use super::*;

#[test]
fn channel_methods_have_distinct_json_shapes_and_no_transport_authority() {
    let register: Request = serde_json::from_value(serde_json::json!({"id":"r","method":"agent.register_self", "params":{
        "pane_id":"w1:p1", "session_generation":"session", "transport":{"forged":true}, "pid":123
    }})).unwrap();
    let Method::AgentRegisterSelf(params) = &register.method else {
        panic!("registration method");
    };
    assert!(params.transport.is_none());
    let value = serde_json::to_value(&register).unwrap();
    assert!(value["params"].get("transport").is_none());
    assert!(value["params"].get("pid").is_none());
    let mut guarded = serde_json::json!({"id":"r","method":"agent.prompt_guarded", "params":{
        "target":"undetected-terminal", "text":"; shell literal\n", "expected_terminal":"term",
        "expected_registration_epoch":"epoch", "request_id":"request", "allow_cross_pane":true
    }});
    assert!(serde_json::from_value::<Request>(guarded.clone()).is_ok());
    for field in [
        "expected_terminal",
        "expected_registration_epoch",
        "request_id",
    ] {
        let mut absent = guarded.clone();
        absent["params"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<Request>(absent).is_err());
        let mut null = guarded.clone();
        null["params"][field] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Request>(null).is_err());
    }
    let mut null_timeout = guarded.clone();
    null_timeout["params"]["timeout_ms"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<Request>(null_timeout).is_err());
    guarded["method"] = "agent.prompt".into();
    let old: Request = serde_json::from_value(guarded).unwrap();
    assert!(matches!(old.method, Method::AgentPrompt(_))); // Methods never reinterpret one another.
}
#[test]
fn channel_draft_guard_json_contract_is_explicit_and_query_is_target_only() {
    let registration: Request = serde_json::from_value(serde_json::json!({
        "id":"r", "method":"agent.register_self", "params":{
            "session_generation":"session", "draft_guard":true
        }
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(registration).unwrap()["params"]["draft_guard"],
        true
    );
    for value in [serde_json::Value::Null, "true".into(), 1.into()] {
        assert!(serde_json::from_value::<Request>(serde_json::json!({
            "id":"r", "method":"agent.register_self", "params":{
                "session_generation":"session", "draft_guard":value
            }
        }))
        .is_err());
    }
    let guarded: Request = serde_json::from_value(serde_json::json!({
        "id":"r", "method":"agent.prompt_guarded", "params":{
            "target":"pane", "text":"literal", "expected_terminal":"term",
            "expected_registration_epoch":"epoch", "request_id":"request", "if_draft_empty":true
        }
    }))
    .unwrap();
    let mut guarded = serde_json::to_value(guarded).unwrap();
    assert_eq!(guarded["params"]["if_draft_empty"], true);
    for value in [serde_json::Value::Null, "true".into(), 1.into()] {
        let mut invalid = guarded.clone();
        invalid["params"]["if_draft_empty"] = value;
        assert!(serde_json::from_value::<Request>(invalid).is_err());
    }
    guarded["params"]
        .as_object_mut()
        .unwrap()
        .remove("if_draft_empty");
    let Method::AgentPromptGuarded(unguarded) =
        serde_json::from_value::<Request>(guarded).unwrap().method
    else {
        panic!("prompt method");
    };
    assert!(!unguarded.if_draft_empty);
    let query: Request = serde_json::from_value(serde_json::json!({
        "id":"query", "method":"agent.draft_state", "params":{"target":"pane"}
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(&query).unwrap()["params"],
        serde_json::json!({"target":"pane"})
    );
    assert!(!crate::api::request_changes_ui(&query));
    assert_eq!(api_method_name(&query.method), "agent.draft_state");
    for field in [
        "text",
        "empty",
        "chars",
        "hold",
        "if_draft_empty",
        "request_id",
        "timeout_ms",
    ] {
        let mut invalid = serde_json::to_value(&query).unwrap();
        invalid["params"][field] = true.into();
        assert!(
            serde_json::from_value::<Request>(invalid).is_err(),
            "query accepts only target and the caller opt-in: {field}"
        );
    }
    // The same explicit cross-pane opt-in as agent.prompt; never implied.
    let mut allowed = serde_json::to_value(&query).unwrap();
    allowed["params"]["allow_cross_pane"] = true.into();
    let parsed: Request = serde_json::from_value(allowed.clone()).unwrap();
    assert!(matches!(&parsed.method, Method::AgentDraftState(params) if params.allow_cross_pane));
    assert_eq!(serde_json::to_value(&parsed).unwrap(), allowed);
    for invalid in [serde_json::Value::Null, "true".into(), 1.into()] {
        allowed["params"]["allow_cross_pane"] = invalid;
        assert!(serde_json::from_value::<Request>(allowed.clone()).is_err());
    }
}

fn draft_query_request() -> Request {
    Request {
        id: "draft-query".into(),
        method: Method::AgentDraftState(crate::api::schema::AgentDraftStateParams {
            target: "pane".into(),
            allow_cross_pane: false,
        }),
    }
}

#[test]
fn channel_draft_query_app_deadline_is_bounded_and_typed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let started = Instant::now();
    let response = dispatch_draft_state_with_timeout(
        draft_query_request(),
        &tx,
        ApiRequestContext::default(),
        None,
        Duration::ZERO,
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response,
        serde_json::json!({"id":"draft-query", "result":{"status":"unknown", "reason":"timeout"}})
    );
    assert!(matches!(
        rx.try_recv().unwrap().request.method,
        Method::AgentDraftState(_)
    ));
}

#[test]
fn channel_draft_query_app_receiver_and_response_loss_are_unknown() {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    drop(rx);
    let response = dispatch_draft_state_with_timeout(
        draft_query_request(),
        &tx,
        ApiRequestContext::default(),
        None,
        Duration::from_secs(1),
    );
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response["result"],
        serde_json::json!({"status":"unknown", "reason":"unknown"})
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApiRequestMessage>();
    let app = std::thread::spawn(move || {
        drop(rx.blocking_recv().unwrap());
    });
    let response = dispatch_draft_state_with_timeout(
        draft_query_request(),
        &tx,
        ApiRequestContext::default(),
        None,
        Duration::from_secs(1),
    );
    app.join().unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(
        response["result"],
        serde_json::json!({"status":"unknown", "reason":"unknown"})
    );
}

#[test]
fn channel_draft_query_app_results_and_target_errors_are_preserved() {
    for response in [
        serde_json::json!({"id":"draft-query", "result":{"status":"known", "empty":false, "hold":"editor"}}),
        serde_json::json!({"id":"draft-query", "error":{"code":"agent_not_found", "message":"target missing"}}),
        // Caller-policy refusals stay errors; they never become an observation.
        serde_json::json!({"id":"draft-query", "error":{"code":"cross_pane_input_denied", "message":"denied"}}),
        serde_json::json!({"id":"draft-query", "error":{"code":"input_origin_unknown", "message":"unknown"}}),
    ] {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ApiRequestMessage>();
        let expected = response.to_string();
        let reply = expected.clone();
        let app = std::thread::spawn(move || {
            let request = rx.blocking_recv().unwrap();
            assert!(matches!(request.request.method, Method::AgentDraftState(_)));
            request.respond_to.send(reply).unwrap();
        });
        let actual = dispatch_draft_state_with_timeout(
            draft_query_request(),
            &tx,
            ApiRequestContext::default(),
            None,
            Duration::from_secs(1),
        );
        app.join().unwrap();
        assert_eq!(actual, expected);
    }
}

#[test]
fn channel_capabilities_are_optional_json_and_windows_policy_is_false() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let response = handle_request_with_context(
        Request {
            id: "ping".into(),
            method: Method::Ping(Default::default()),
        },
        ApiRequestContext::default(),
        &tx,
        default_capabilities(),
        None,
        None,
        None,
    );
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    let capabilities = &response["result"]["capabilities"];
    let supported = crate::platform::capabilities().registered_agent_channel;
    assert_eq!(capabilities["agent_registration_channel"], supported);
    assert_eq!(capabilities["guarded_agent_prompt"], supported);
    assert_eq!(
        capabilities["guarded_agent_prompt_if_draft_empty"],
        supported
    );
    assert_eq!(capabilities["agent_channel_info"], true);
    assert_eq!(capabilities["agent_draft_state"], supported);
    assert_eq!(capabilities["media_open_cancel"], true);
    assert!(!crate::server::client_commands::supports_client_shell_method_name("pane.media_open"));
    assert!(!crate::server::client_commands::supports_client_shell_method_name("media.close"));
    assert_eq!(
        capabilities["agent_channel_methods"],
        if supported {
            serde_json::json!([
                "agent.register_self",
                "agent.channel_info",
                "agent.prompt_guarded",
                "agent.draft_state"
            ])
        } else {
            serde_json::json!(["agent.channel_info"])
        }
    );
    #[cfg(windows)]
    assert!(!supported);
    // Old typed ping consumers ignore these optional JSON additions.
    assert!(serde_json::from_value::<SuccessResponse>(response).is_ok());
    assert!(
        crate::server::client_commands::supports_client_shell_method_name("agent.channel_info")
    );
    assert!(
        crate::server::client_commands::supports_client_shell_method_name("agent.prompt_guarded")
    );
    assert!(
        !crate::server::client_commands::supports_client_shell_method_name("agent.register_self")
    );
}

// These exercise the real API message/response channels and broker effects. Keep
// them for the microphone-release failure path, which needs a disappearing caller.
pub(super) struct MediaHarness {
    pub(super) broker: crate::server::media::MediaBroker,
    pub(super) controls: Vec<crate::protocol::media::MediaControl>,
    pub(super) now: Instant,
}

impl MediaHarness {
    pub(super) fn new() -> Self {
        let now = Instant::now();
        let mut broker = crate::server::media::MediaBroker::new();
        broker.client_connected(1, true);
        broker.note_pane_input(1, crate::layout::PaneId::from_raw(7), "w1:p7", now);
        Self {
            broker,
            controls: Vec::new(),
            now,
        }
    }

    pub(super) fn service(&mut self, message: ApiRequestMessage) {
        let Method::PaneMediaOpen(params) = message.request.method else {
            panic!("expected media open");
        };
        assert_eq!(params.pane_id, "w1:p7");
        let actions = self.broker.open(
            message.request.id,
            message.respond_to,
            crate::layout::PaneId::from_raw(7),
            params.open_id,
            message.response_write_complete,
            |_| true,
            self.now,
        );
        self.perform(actions);
    }

    pub(super) fn perform(&mut self, actions: Vec<crate::server::media::MediaAction>) {
        use crate::server::media::MediaAction;
        for action in actions {
            match action {
                MediaAction::Publish { .. } => {}
                MediaAction::Send { client_id, control } => {
                    assert_eq!(client_id, 1);
                    self.controls.push(control);
                }
                MediaAction::Respond {
                    respond_to,
                    response,
                    session_id,
                } => {
                    // Match the headless executor: failed channel delivery must
                    // retire the actual live session, not merely lose the reply.
                    if respond_to.send(response).is_err() {
                        if let Some(id) = session_id {
                            let follow_up = self.broker.response_delivery_failed(&id, self.now);
                            self.perform(follow_up);
                        }
                    }
                }
            }
        }
    }

    pub(super) fn opened_session(&self) -> String {
        self.controls
            .iter()
            .rev()
            .find_map(|control| match control {
                crate::protocol::media::MediaControl::Open(open) => Some(open.session_id.clone()),
                _ => None,
            })
            .expect("a media.open reached the client")
    }

    pub(super) fn assert_unprotected_open_pair(&self, session_id: &str) {
        use crate::protocol::media::MediaControl;

        assert_eq!(
            self.controls
                .iter()
                .filter(|control| matches!(control, MediaControl::OpenInitial(_)))
                .count(),
            1,
            "one complete initial open is sent"
        );
        assert_eq!(
            self.controls
                .iter()
                .filter(|control| matches!(control, MediaControl::Open(_)))
                .count(),
            1,
            "the v1 alternate is retained"
        );
        assert!(matches!(
            self.controls.as_slice(),
            [MediaControl::OpenInitial(initial), MediaControl::Open(open), ..]
                if initial.session_id == session_id
                    && initial.pane_id == "w1:p7"
                    && !initial.input_muted
                    && !initial.output_suppressed
                    && open.session_id == initial.session_id
                    && open.pane_id == initial.pane_id
        ), "the complete false/false initial open is immediately followed by its same-ID, same-pane alternate"
        );
        assert_eq!(
            self.controls[0].kind(),
            crate::protocol::media::MEDIA_OPEN_INITIAL_KIND
        );
        assert_eq!(
            self.controls[1].kind(),
            crate::protocol::media::MEDIA_OPEN_KIND
        );
    }

    pub(super) fn offer(&mut self, session_id: &str) {
        let actions = self.broker.client_control(
            1,
            crate::protocol::media::MediaControl::Offer(crate::protocol::media::MediaSdp {
                session_id: session_id.into(),
                sdp: "v=0 offer".into(),
            }),
            self.now,
        );
        self.perform(actions);
    }

    pub(super) fn expire_at(&mut self, now: Instant) {
        let actions = self.broker.expire(now, |_| true);
        self.perform(actions);
    }

    pub(super) fn expire(&mut self) {
        self.expire_at(self.now);
    }

    pub(super) fn assert_abandoned(&self, session_id: &str) {
        use crate::protocol::media::{close_code, MediaControl};
        assert!(!self.broker.has_sessions());
        assert_eq!(
            self.broker.state(session_id).unwrap().code.as_deref(),
            Some(close_code::ABANDONED)
        );
        assert!(self.controls.iter().any(|control| matches!(control,
            MediaControl::Close(close)
                if close.session_id == session_id
                    && close.code.as_deref() == Some(close_code::ABANDONED))));
    }
}

pub(super) fn media_request() -> Request {
    Request {
        id: "media-open".into(),
        method: Method::PaneMediaOpen(crate::api::schema::MediaOpenParams {
            pane_id: "w1:p7".into(),
            open_id: Some("voice-open".into()),
        }),
    }
}

fn channel_media_open(
    api_tx: &ApiRequestSender,
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) -> std::sync::mpsc::Receiver<String> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    api_tx
        .send(ApiRequestMessage {
            request: media_request(),
            context: ApiRequestContext::default(),
            peer_pidfd: None,
            respond_to,
            response_write_complete,
        })
        .unwrap();
    response_rx
}

#[test]
fn media_delivery_receipt_is_failure_by_default_and_drop_only_on_success() {
    let (delivery, receipt) = MediaResponseDelivery::new();
    drop(delivery);
    assert_eq!(receipt.try_recv(), Ok(()));
    assert_eq!(
        receipt.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );

    let (delivery, receipt) = MediaResponseDelivery::new();
    delivery.delivered();
    assert_eq!(
        receipt.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Disconnected)
    );
}

#[test]
fn channel_media_pending_disconnect_abandons_before_a_late_offer() {
    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
    let (delivery, receipt) = MediaResponseDelivery::new();
    let response_rx = channel_media_open(&api_tx, Some(receipt));
    let mut media = MediaHarness::new();
    media.service(api_rx.try_recv().unwrap());
    let id = media.opened_session();
    assert!(
        response_rx.try_recv().is_err(),
        "open waits for the client's offer"
    );

    drop(response_rx);
    drop(delivery);
    // Real main-loop ordering: client events are handled before maintenance.
    media.offer(&id);
    media.assert_abandoned(&id);
    media.expire();
    media.assert_unprotected_open_pair(&id);
    assert!(
        matches!(
            media.controls.as_slice(),
            [
                crate::protocol::media::MediaControl::OpenInitial(_),
                crate::protocol::media::MediaControl::Open(_),
                crate::protocol::media::MediaControl::Close(close)
            ] if close.session_id == id
                && close.code.as_deref() == Some(crate::protocol::media::close_code::ABANDONED)
        ),
        "pre-delivery abandonment preserves the open pair and sends only its expected close"
    );
}

#[test]
fn channel_media_pending_receipt_owner_loss_is_abandoned() {
    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
    let (receipt_owner, receipt) = std::sync::mpsc::channel();
    let response_rx = channel_media_open(&api_tx, Some(receipt));
    let mut media = MediaHarness::new();
    media.service(api_rx.try_recv().unwrap());
    let id = media.opened_session();
    drop(response_rx);
    drop(receipt_owner);
    media.expire();
    media.assert_abandoned(&id);
}

#[test]
fn channel_media_cancelled_before_app_service_never_opens_microphone() {
    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
    let (delivery, receipt) = MediaResponseDelivery::new();
    let response_rx = channel_media_open(&api_tx, Some(receipt));
    drop(response_rx);
    drop(delivery);
    let mut media = MediaHarness::new();
    media.service(api_rx.try_recv().unwrap());
    assert!(media.controls.is_empty());
    assert!(!media.broker.has_sessions());
}

#[test]
fn channel_media_failed_offer_delivery_abandons_without_a_socket_receipt() {
    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
    let response_rx = channel_media_open(&api_tx, None);
    let mut media = MediaHarness::new();
    media.service(api_rx.try_recv().unwrap());
    let id = media.opened_session();
    drop(response_rx);
    media.offer(&id);
    media.assert_abandoned(&id);
}

#[test]
fn channel_media_no_receipt_callers_retire_at_open_deadline() {
    for caller_dropped in [true, false] {
        let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
        let response_rx = channel_media_open(&api_tx, None);
        let mut media = MediaHarness::new();
        media.service(api_rx.try_recv().unwrap());
        let id = media.opened_session();
        let response_rx = if caller_dropped {
            drop(response_rx);
            None
        } else {
            assert!(response_rx.try_recv().is_err(), "caller is still waiting");
            Some(response_rx)
        };

        let deadline = media.now + crate::server::media::MEDIA_OPEN_TIMEOUT;
        assert_eq!(media.broker.next_deadline(media.now), Some(deadline));
        media.expire_at(deadline - Duration::from_nanos(1));
        assert!(
            media.broker.has_sessions(),
            "the open remains live before its deadline"
        );
        assert!(
            !media
                .controls
                .iter()
                .any(|control| matches!(control, crate::protocol::media::MediaControl::Close(_))),
            "no close is sent before the open deadline"
        );

        media.expire_at(deadline);
        assert!(
            !media.broker.has_sessions(),
            "the deadline retires the open"
        );
        assert_eq!(
            media.broker.state(&id).unwrap().code.as_deref(),
            Some(crate::protocol::media::close_code::TIMEOUT)
        );
        assert!(
            media.controls.iter().any(|control| matches!(control,
            crate::protocol::media::MediaControl::Close(close)
                if close.session_id == id
                    && close.code.as_deref() == Some(crate::protocol::media::close_code::TIMEOUT)))
        );
        if let Some(response_rx) = response_rx {
            let response: serde_json::Value =
                serde_json::from_str(&response_rx.try_recv().unwrap()).unwrap();
            assert_eq!(response["error"]["code"], "media_timeout");
        }

        let controls_before_late_offer = media.controls.len();
        media.offer(&id);
        assert_eq!(
            media.broker.state(&id).unwrap().code.as_deref(),
            Some(crate::protocol::media::close_code::TIMEOUT),
            "a late offer cannot resurrect a timed-out open"
        );
        assert_eq!(media.controls.len(), controls_before_late_offer);
    }
}

#[test]
fn channel_media_successful_offer_survives_response_channel_and_receipt_drop() {
    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
    let (delivery, receipt) = MediaResponseDelivery::new();
    let response_rx = channel_media_open(&api_tx, Some(receipt));
    let mut media = MediaHarness::new();
    media.service(api_rx.try_recv().unwrap());
    let id = media.opened_session();
    assert!(response_rx.try_recv().is_err());
    media.offer(&id);
    let response: serde_json::Value =
        serde_json::from_str(&response_rx.try_recv().unwrap()).unwrap();
    assert_eq!(response["result"]["session_id"], id);
    drop(response_rx);
    delivery.delivered();
    media.expire();
    assert!(media.broker.has_sessions());
    assert_eq!(
        media.broker.state(&id).unwrap().state,
        crate::api::schema::MediaSessionState::Offered
    );
    media.assert_unprotected_open_pair(&id);
    assert!(
        matches!(
            media.controls.as_slice(),
            [
                crate::protocol::media::MediaControl::OpenInitial(_),
                crate::protocol::media::MediaControl::Open(_)
            ]
        ),
        "a successful response leaves only the compatibility open pair, with no close"
    );
}
