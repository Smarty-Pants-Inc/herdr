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
    assert_eq!(capabilities["agent_channel_info"], true);
    assert_eq!(
        capabilities["agent_channel_methods"],
        if supported {
            serde_json::json!([
                "agent.register_self",
                "agent.channel_info",
                "agent.prompt_guarded"
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
