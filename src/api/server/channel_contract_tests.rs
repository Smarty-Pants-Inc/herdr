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
