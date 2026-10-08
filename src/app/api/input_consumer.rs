//! Local API transport for actor-owned input consumer operations.
//!
//! This module neither computes receipt intervals nor authenticates tty ancestry.
//! The actor receives the accept-time kernel peer pin and owns those decisions.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::api::schema::{InputConsumerCutKind, Method, Request};
use crate::api::ApiRequestContext;
use crate::app::App;
use crate::pty::input_consumer::{
    ConsumerOperation, ConsumerResponse, CutKind, CutRequest, CutResult,
};

use super::responses::encode_error;

const CONSUMER_REPLY_TIMEOUT: Duration = Duration::from_secs(5);

impl App {
    pub(crate) fn handle_deferred_input_consumer_request(
        &self,
        request: Request,
        context: ApiRequestContext,
        respond_to: Sender<String>,
    ) {
        let id = request.id;
        match self.queue_input_consumer_request(&id, request.method, context) {
            Ok((completion, answer)) => {
                let failure_sender = respond_to.clone();
                let failure_id = id.clone();
                if let Err(err) = std::thread::Builder::new()
                    .name("input-consumer-reply".into())
                    .spawn(move || {
                        let response = match completion.recv_timeout(CONSUMER_REPLY_TIMEOUT) {
                            Ok(response) => encode_consumer_response(id, response, &answer),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                                encode_error(id, "timeout", "input consumer actor reply timed out")
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                encode_error(id, "input_consumer_unavailable", "pty actor closed")
                            }
                        };
                        let _ = respond_to.send(response);
                    })
                {
                    let _ = failure_sender.send(encode_error(
                        failure_id,
                        "input_consumer_unavailable",
                        err.to_string(),
                    ));
                }
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
    }

    fn queue_input_consumer_request(
        &self,
        id: &str,
        method: Method,
        context: ApiRequestContext,
    ) -> Result<(Receiver<ConsumerResponse>, AnswerContext), String> {
        if !crate::platform::input_consumer_supported() {
            return Err(encode_error(
                id.to_owned(),
                "unsupported_platform",
                "input consumer cuts require a Linux server",
            ));
        }
        let (runtime, operation, audit, answer) = match method {
            Method::PaneInputConsumerEnroll(params) => {
                if !is_lower_hex(&params.challenge, 64) {
                    return Err(encode_error(
                        id.to_owned(),
                        "invalid_params",
                        "challenge must be 64 lowercase hex characters",
                    ));
                }
                let Some(peer) = context.local_peer_identity else {
                    return Err(encode_error(
                        id.to_owned(),
                        "peer_unavailable",
                        "enrollment requires a kernel-pinned local socket peer",
                    ));
                };
                let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
                    return Err(encode_error(
                        id.to_owned(),
                        "pane_not_found",
                        "pane not found",
                    ));
                };
                let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
                    return Err(encode_error(
                        id.to_owned(),
                        "pane_not_found",
                        "pane runtime not found",
                    ));
                };
                (
                    runtime,
                    ConsumerOperation::Enroll { peer },
                    None,
                    AnswerContext::Enroll,
                )
            }
            Method::PaneInputConsumerCut(params) => {
                let runtime = self.input_consumer_runtime(id, &params.epoch)?;
                let request = CutRequest {
                    epoch: params.epoch,
                    epoch_key: params.epoch_key,
                    seq: params.seq,
                    token: params.token,
                    cut: params.cut,
                    digest: params.digest,
                    kind: match params.kind {
                        InputConsumerCutKind::Submit => CutKind::Submit,
                        InputConsumerCutKind::Discard => CutKind::Discard,
                    },
                };
                let answer = AnswerContext::Cut(request.clone());
                (
                    runtime,
                    ConsumerOperation::Cut(request),
                    Some(self.input_consumer_audit_sink()),
                    answer,
                )
            }
            Method::PaneInputConsumerRelease(params) => {
                let runtime = self.input_consumer_runtime(id, &params.epoch)?;
                (
                    runtime,
                    ConsumerOperation::Release {
                        epoch: params.epoch,
                        epoch_key: params.epoch_key,
                    },
                    None,
                    AnswerContext::Release,
                )
            }
            _ => {
                return Err(encode_error(
                    id.to_owned(),
                    "invalid_request",
                    "not an input consumer request",
                ))
            }
        };
        runtime
            .queue_input_consumer_operation(operation, audit)
            .map(|completion| (completion, answer))
            .map_err(|err| {
                encode_error(id.to_owned(), "input_consumer_unavailable", err.to_string())
            })
    }

    fn input_consumer_runtime(
        &self,
        id: &str,
        epoch: &str,
    ) -> Result<&crate::terminal::TerminalRuntime, String> {
        // A single bounded scan per API request, never in render/layout loops.
        // The scalar hint selects only the runtime; the actor authenticates the
        // capability and validates the live consumer/incarnation again.
        self.terminal_runtimes
            .values()
            .find(|runtime| runtime.input_consumer_epoch_matches(epoch))
            .ok_or_else(|| {
                encode_error(
                    id.to_owned(),
                    "epoch_not_found",
                    "input consumer epoch is not active",
                )
            })
    }
}

/// What the answer must bind to, taken from the request that produced it.
enum AnswerContext {
    Enroll,
    Cut(CutRequest),
    Release,
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(value.get(i..i + 2)?, 16).ok())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every field is a u32 big-endian byte length plus its UTF-8 bytes (Pi `server-auth.ts`).
fn encode_fields(fields: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
    }
    out
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<_>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The cut answer as Pi reads it, with its HMAC under the epoch key.
fn cut_answer(request: &CutRequest, result: &CutResult) -> Option<serde_json::Value> {
    let (kind, reason, principal) = match result {
        CutResult::Client { principal } => ("client", None, Some(principal.as_ref())),
        CutResult::Api => ("api", None, None),
        CutResult::Mixed => ("mixed", None, None),
        CutResult::Unknown { reason } => ("unknown", Some(reason.as_str()), None),
    };
    let named = principal.flatten();
    let message = encode_fields(&[
        "herdr-cut-v1",
        &request.epoch,
        &request.seq.to_string(),
        &request.token,
        &request.cut.to_string(),
        &request.digest,
        match request.kind {
            CutKind::Submit => "submit",
            CutKind::Discard => "discard",
        },
        kind,
        reason.unwrap_or(""),
        named.map_or("", |p| p.smarty_id.as_str()),
        named.map_or("", |p| p.display_name.as_str()),
    ]);
    let mac = hmac_sha256(&decode_hex(&request.epoch_key)?, &message);
    let mut answer = serde_json::json!({"result": kind, "mac": hex(&mac)});
    if let Some(reason) = reason {
        answer["reason"] = reason.into();
    }
    if let Some(principal) = principal {
        answer["principal"] = serde_json::to_value(principal).ok()?;
    }
    Some(answer)
}

fn encode_consumer_response(
    id: String,
    response: ConsumerResponse,
    answer: &AnswerContext,
) -> String {
    let result = match (response, answer) {
        (
            ConsumerResponse::Enrolled {
                epoch,
                epoch_key,
                nonce,
                tty,
            },
            AnswerContext::Enroll,
        ) => {
            // Server authentication is unavailable in this build (smarty-dev#6690): the
            // answer carries no `sig`, so a consumer labels its turns `terminal`.
            serde_json::json!({
                "epoch": epoch,
                "epoch_key": epoch_key,
                "nonce": nonce,
                "tty": {"dev": tty.0.to_string(), "ino": tty.1.to_string()},
            })
        }
        (ConsumerResponse::Cut(result), AnswerContext::Cut(request)) => {
            match cut_answer(request, &result) {
                Some(answer) => answer,
                None => return encode_error(id, "invalid_epoch", "invalid_epoch"),
            }
        }
        (ConsumerResponse::Released, _) => serde_json::json!({}),
        (ConsumerResponse::Refused { reason }, _) => {
            return encode_error(id, &reason, reason.clone());
        }
        _ => return encode_error(id, "input_consumer_unavailable", "mismatched answer"),
    };
    serde_json::json!({"id": id, "result": result}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::input_consumer::Principal;

    // Vectors computed with Pi's own encoder (pi feat/2636-herdr-attest 092924ef,
    // server-auth.ts) by .local/pi-vector.mjs: Herdr must match those bytes exactly.
    const EPOCH_KEY: &str = "0101010101010101010101010101010101010101010101010101010101010101";

    fn request() -> CutRequest {
        CutRequest {
            epoch: "e1".into(),
            epoch_key: EPOCH_KEY.into(),
            seq: 1,
            token: "00112233445566778899aabbccddeeff".into(),
            cut: 2,
            digest: "d5bb8bd014e448a612372e955d5180bae3fb6d5fc3a1668e754a65c2abc341be".into(),
            kind: CutKind::Submit,
        }
    }

    fn cut(result: CutResult) -> serde_json::Value {
        let response = encode_consumer_response(
            "cut".into(),
            ConsumerResponse::Cut(result),
            &AnswerContext::Cut(request()),
        );
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn input_consumer_cut_answers_match_pi_wire_and_mac() {
        let paul = Principal {
            smarty_id: "paul".into(),
            display_name: "Paul".into(),
        };
        assert_eq!(
            cut(CutResult::Client {
                principal: Some(paul)
            }),
            serde_json::json!({"id":"cut","result":{"result":"client",
                "principal":{"smarty_id":"paul","display_name":"Paul"},
                "mac":"d988bd2d02f6acceb5a437927328de8916258d3a9fee95feff3f6fc87cc9cc38"}})
        );
        assert_eq!(
            cut(CutResult::Client { principal: None }),
            serde_json::json!({"id":"cut","result":{"result":"client","principal":null,
                "mac":"e5c4699bdc1112282eef527224eb194ec831ac9fec11e72aa5c900a4177597c8"}})
        );
        assert_eq!(
            cut(CutResult::Unknown {
                reason: "seq_mismatch".into()
            }),
            serde_json::json!({"id":"cut","result":{"result":"unknown","reason":"seq_mismatch",
                "mac":"4394a953796de6a8179eb143311cfc1d5887620ff4bafe1764dd605adb0526fc"}})
        );
        // A different key gives a different MAC: the answer is bound to the epoch key.
        let mut other = request();
        other.epoch_key = "02".repeat(32);
        let answer = cut_answer(&other, &CutResult::Api).unwrap();
        let original = cut_answer(&request(), &CutResult::Api).unwrap();
        assert_ne!(answer["mac"], original["mac"]);
        assert_eq!(original["result"], "api");
    }

    #[test]
    fn input_consumer_hmac_matches_rfc4231_case_2() {
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn input_consumer_enroll_answer_has_no_signature() {
        let response = encode_consumer_response(
            "enroll".into(),
            ConsumerResponse::Enrolled {
                epoch: "e1".into(),
                epoch_key: EPOCH_KEY.into(),
                nonce: "n".repeat(16),
                tty: (1, 2),
            },
            &AnswerContext::Enroll,
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"id":"enroll","result":{"epoch":"e1","epoch_key":EPOCH_KEY,
                "nonce":"n".repeat(16),"tty":{"dev":"1","ino":"2"}}})
        );
    }

    #[test]
    fn input_consumer_release_and_refusal_envelopes() {
        let response = encode_consumer_response(
            "release".into(),
            ConsumerResponse::Released,
            &AnswerContext::Release,
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            serde_json::json!({"id":"release", "result":{}})
        );
        let response = encode_consumer_response(
            "refused".into(),
            ConsumerResponse::Refused {
                reason: "token_conflict".into(),
            },
            &AnswerContext::Cut(request()),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"],
            "token_conflict"
        );
        assert!(is_lower_hex(&"ab".repeat(32), 64));
        assert!(!is_lower_hex(&"AB".repeat(32), 64));
        assert!(!is_lower_hex(&"ab".repeat(31), 64));
    }
}
