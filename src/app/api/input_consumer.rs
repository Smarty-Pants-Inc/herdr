//! Local API transport for actor-owned input consumer operations.
//!
//! This module neither computes receipt intervals nor authenticates tty ancestry.
//! The actor receives the accept-time kernel peer pin and owns those decisions.

use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::api::schema::{InputConsumerCutKind, Method, Request};
use crate::api::ApiRequestContext;
use crate::app::App;
use crate::pty::input_consumer::{ConsumerOperation, ConsumerResponse, CutKind, CutRequest};

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
            Ok(completion) => {
                let failure_sender = respond_to.clone();
                let failure_id = id.clone();
                if let Err(err) = std::thread::Builder::new()
                    .name("input-consumer-reply".into())
                    .spawn(move || {
                        let response = match completion.recv_timeout(CONSUMER_REPLY_TIMEOUT) {
                            Ok(response) => encode_consumer_response(id, response),
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
    ) -> Result<Receiver<ConsumerResponse>, String> {
        if !crate::platform::input_consumer_supported() {
            return Err(encode_error(
                id.to_owned(),
                "unsupported_platform",
                "input consumer cuts require a Linux server",
            ));
        }
        let (runtime, operation, audit) = match method {
            Method::PaneInputConsumerEnroll(params) => {
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
                (runtime, ConsumerOperation::Enroll { peer }, None)
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
                (
                    runtime,
                    ConsumerOperation::Cut(request),
                    Some(self.input_consumer_audit_sink()),
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

fn encode_consumer_response(id: String, response: ConsumerResponse) -> String {
    let result = match response {
        ConsumerResponse::Enrolled {
            epoch,
            epoch_key,
            nonce,
        } => {
            serde_json::json!({"epoch": epoch, "epoch_key": epoch_key, "nonce": nonce})
        }
        ConsumerResponse::Cut(result) => match serde_json::to_value(result) {
            Ok(result) => result,
            Err(err) => return encode_error(id, "serialization_error", err.to_string()),
        },
        ConsumerResponse::Released => serde_json::json!({}),
        ConsumerResponse::Refused { reason } => {
            return encode_error(id, &reason, reason.clone());
        }
    };
    serde_json::json!({"id": id, "result": result}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::input_consumer::CutResult;

    #[test]
    fn input_consumer_response_envelopes_are_flat_and_unmapped_is_explicit() {
        let response = encode_consumer_response(
            "cut".into(),
            ConsumerResponse::Cut(CutResult::Client { principal: None }),
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            serde_json::json!({"id":"cut", "result":{"source":"client", "principal":null}})
        );
        let response = encode_consumer_response("release".into(), ConsumerResponse::Released);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap(),
            serde_json::json!({"id":"release", "result":{}})
        );
        let response = encode_consumer_response(
            "refused".into(),
            ConsumerResponse::Refused {
                reason: "token_conflict".into(),
            },
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&response).unwrap()["error"]["code"],
            "token_conflict"
        );
    }
}
