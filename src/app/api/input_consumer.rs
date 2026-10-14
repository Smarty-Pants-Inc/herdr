//! Local API transport for actor-owned input consumer operations.
//!
//! This module neither computes receipt intervals nor authenticates tty ancestry.
//! The actor receives the accept-time kernel peer pin and owns those decisions.
//! This module signs what the actor answers (smarty-dev#2636 E′, message contract v2):
//! the enroll transcript and every cut answer, under the server key. Nothing here
//! is a secret the consumer holds; there is no HMAC or epoch key.

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

/// Domain label of the enroll transcript `T`.
const ENROLL_LABEL: &str = "herdr-enroll-v2";
/// Domain label of a signed cut answer.
const CUT_ANSWER_LABEL: &str = "herdr-cut-answer-v2";

impl App {
    pub(crate) fn handle_deferred_input_consumer_request(
        &self,
        request: Request,
        context: ApiRequestContext,
        peer_pidfd: Option<crate::platform::PeerPidfd>,
        respond_to: Sender<String>,
    ) {
        let id = request.id;
        match self.queue_input_consumer_request(&id, request.method, context, peer_pidfd) {
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
        peer_pidfd: Option<crate::platform::PeerPidfd>,
    ) -> Result<(Receiver<ConsumerResponse>, AnswerContext), String> {
        if !crate::platform::input_consumer_supported() {
            return Err(encode_error(
                id.to_owned(),
                "unsupported_platform",
                "input consumer cuts require a Linux server",
            ));
        }
        // Fail closed: no attestation key means no epoch, never an unsigned one.
        if !crate::platform::server_key_available() {
            return Err(encode_error(
                id.to_owned(),
                "server_key_unavailable",
                "this server has no attestation key",
            ));
        }
        let peer_ino = peer_pidfd.as_ref().map(|peer| peer.ino);
        let (runtime, operation, audit, answer) = match method {
            Method::PaneInputConsumerEnroll(params) => {
                if !is_lower_hex(&params.challenge, 64) {
                    return Err(encode_error(
                        id.to_owned(),
                        "invalid_params",
                        "challenge must be 64 lowercase hex characters",
                    ));
                }
                if context.local_peer_identity.is_none() {
                    return Err(encode_error(
                        id.to_owned(),
                        "peer_unavailable",
                        "enrollment requires a kernel-pinned local socket peer",
                    ));
                }
                // No pid or start-time fallback: without SO_PEERPIDFD and pidfs
                // there is no epoch (design "Round 2 answers" item 4).
                let Some(peer) = peer_pidfd else {
                    return Err(encode_error(
                        id.to_owned(),
                        "peer_identity_unsupported",
                        "enrollment requires SO_PEERPIDFD and pidfs",
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
                let answer = AnswerContext::Enroll {
                    pane_id: params.pane_id,
                    challenge: params.challenge,
                };
                (runtime, ConsumerOperation::Enroll { peer }, None, answer)
            }
            Method::PaneInputConsumerCut(params) => {
                let runtime = self.input_consumer_runtime(id, &params.epoch)?;
                let request = CutRequest {
                    epoch: params.epoch,
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
                    ConsumerOperation::Cut { request, peer_ino },
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
                        peer_ino,
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
        // The scalar hint selects only the runtime; the actor pins the peer and
        // validates the live consumer/incarnation again.
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
    Enroll { pane_id: String, challenge: String },
    Cut(CutRequest),
    Release,
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `F`: every field is a u32 big-endian byte length plus its UTF-8 bytes
/// (Pi `encodeFields`). Injective for a fixed field count.
fn encode_fields(fields: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(&(field.len() as u32).to_be_bytes());
        out.extend_from_slice(field.as_bytes());
    }
    out
}

/// The public facts an enroll answer signs.
struct Enrollment<'a> {
    challenge: &'a str,
    epoch: &'a str,
    nonce: &'a str,
    pane_id: &'a str,
    tty: (u64, u64),
    peer_pid: u32,
    pidns: u64,
}

/// `T = F("herdr-enroll-v2", challenge, epoch, nonce, pane_id, dec(tty.dev),
/// dec(tty.ino), dec(peer_pid), dec(pidns))`.
fn enroll_transcript(e: &Enrollment<'_>) -> Vec<u8> {
    encode_fields(&[
        ENROLL_LABEL,
        e.challenge,
        e.epoch,
        e.nonce,
        e.pane_id,
        &e.tty.0.to_string(),
        &e.tty.1.to_string(),
        &e.peer_pid.to_string(),
        &e.pidns.to_string(),
    ])
}

fn enroll_answer(
    e: &Enrollment<'_>,
    sign: impl Fn(&[u8]) -> Option<Vec<u8>>,
) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let sig = sign(&enroll_transcript(e))?;
    Some(serde_json::json!({
        "epoch": e.epoch,
        "nonce": e.nonce,
        "tty": {"dev": e.tty.0.to_string(), "ino": e.tty.1.to_string()},
        "peer": {"pid": e.peer_pid.to_string(), "pidns": e.pidns.to_string()},
        "sig": base64::engine::general_purpose::STANDARD.encode(sig),
    }))
}

fn cut_parts(
    result: &CutResult,
) -> (
    &'static str,
    Option<&str>,
    Option<Option<&crate::pty::input_consumer::Principal>>,
) {
    match result {
        CutResult::Client { principal } => ("client", None, Some(principal.as_ref())),
        CutResult::Api => ("api", None, None),
        CutResult::Mixed => ("mixed", None, None),
        CutResult::Unknown { reason } => ("unknown", Some(reason.as_str()), None),
    }
}

/// `F("herdr-cut-answer-v2", epoch, dec(seq), token, dec(cut), digest, kind,
/// result, reason ?? "", smarty_id ?? "", display_name ?? "")`.
fn cut_answer_message(request: &CutRequest, result: &CutResult) -> Vec<u8> {
    let (kind, reason, principal) = cut_parts(result);
    let named = principal.flatten();
    encode_fields(&[
        CUT_ANSWER_LABEL,
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
    ])
}

/// The cut answer as Pi reads it, signed under the server key.
fn cut_answer(
    request: &CutRequest,
    result: &CutResult,
    sign: impl Fn(&[u8]) -> Option<Vec<u8>>,
) -> Option<serde_json::Value> {
    use base64::Engine as _;
    let (kind, reason, principal) = cut_parts(result);
    let sig = sign(&cut_answer_message(request, result))?;
    let mut answer = serde_json::json!({
        "result": kind,
        "sig": base64::engine::general_purpose::STANDARD.encode(sig),
    });
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
    let unsigned = |id| {
        encode_error(
            id,
            "server_key_unavailable",
            "this server has no attestation key",
        )
    };
    let result = match (response, answer) {
        (
            ConsumerResponse::Enrolled {
                epoch,
                nonce,
                tty,
                peer_pid,
                pidns,
            },
            AnswerContext::Enroll { pane_id, challenge },
        ) => {
            let enrollment = Enrollment {
                challenge,
                epoch: &epoch,
                nonce: &nonce,
                pane_id,
                tty,
                peer_pid,
                pidns,
            };
            match enroll_answer(&enrollment, crate::platform::server_key_sign) {
                Some(answer) => answer,
                None => return unsigned(id),
            }
        }
        // Fresh, stored replay and `refuse_logged` unknown answers alike.
        (ConsumerResponse::Cut(result), AnswerContext::Cut(request)) => {
            match cut_answer(request, &result, crate::platform::server_key_sign) {
                Some(answer) => answer,
                None => return unsigned(id),
            }
        }
        // Unsigned: Pi ignores the release answer.
        (ConsumerResponse::Released, _) => serde_json::json!({}),
        (ConsumerResponse::Refused { reason }, _) => {
            return encode_error(id, &reason, reason.clone());
        }
        _ => return encode_error(id, "input_consumer_unavailable", "mismatched answer"),
    };
    serde_json::json!({"id": id, "result": result}).to_string()
}

#[cfg(test)]
#[path = "input_consumer_tests.rs"]
mod tests;
