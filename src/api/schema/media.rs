use serde::{Deserialize, Serialize};

// Client-local media sessions (see `crate::protocol::media`). Correlation tokens are
// preserved by these DTOs and validated at media admission before any client work.
pub use crate::protocol::media::MediaEndOrigin;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaOpenParams {
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaCloseParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// A validated completion with binding supplied only by the server's session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaEndedReceipt {
    pub session_id: String,
    pub pane_id: String,
    pub client: u64,
    pub generation: String,
    pub attempt: String,
    pub origin: MediaEndOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub acquired: bool,
}

/// Diagnostic only: this is never proof of completed teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MediaTeardownStuckEvent {
    pub session_id: String,
    pub pane_id: String,
    pub client: u64,
    pub generation: String,
    pub attempt: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaSessionTarget {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaAnswerParams {
    pub session_id: String,
    pub sdp: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MediaMuteParams {
    pub session_id: String,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MediaSessionState {
    Opening,
    Offered,
    Connecting,
    Connected,
    Failed,
    Closed,
}

#[cfg(test)]
mod receipt_api_red {
    use crate::api::schema::{Method, PaneTarget, Request};
    use serde_json::{json, Value};

    fn request_json(method: &str, params: Value) -> Value {
        json!({"id": "receipt-api-red", "method": method, "params": params})
    }

    fn round_trip(method: &str, params: Value) -> Value {
        let input = request_json(method, params);
        // Go through the same runtime JSON/Request boundary as API callers, not
        // just the params DTO or the generated schema.
        let request: Request = serde_json::from_str(&input.to_string())
            .unwrap_or_else(|err| panic!("{method} must be registered: {err}"));
        assert_eq!(crate::api::api_method_name(&request.method), method);
        let output = serde_json::to_value(&request).unwrap();
        assert_eq!(output["id"], input["id"]);
        assert_eq!(output["method"], input["method"]);
        assert_eq!(
            serde_json::from_value::<Request>(output.clone()).unwrap(),
            request
        );
        output
    }

    fn valid_tokens() -> Vec<String> {
        vec!["A".into(), "Az09._:-".into(), "z".repeat(128)]
    }

    fn assert_rejected_or_preserved(method: &str, params: Value) {
        let input = request_json(method, params.clone());
        let Ok(request) = serde_json::from_str::<Request>(&input.to_string()) else {
            return;
        };
        // Bounds/pair validation may live in headless dispatch rather than
        // serde. If decoding succeeds, it MUST retain the supplied identity so
        // that boundary can reject it; silently dropping it runs a legacy call.
        // This checks DTO preservation, not headless rejection or admission.
        assert_eq!(crate::api::api_method_name(&request.method), method);
        let output = serde_json::to_value(&request).unwrap();
        for (field, expected) in params.as_object().unwrap() {
            assert_eq!(
                output["params"].get(field),
                Some(expected),
                "{method} silently discarded or changed {field}: {input} -> {output}"
            );
        }
    }

    #[test]
    fn pane_media_preflight_accepts_pane_target_and_is_registered() {
        let params = serde_json::to_value(PaneTarget {
            pane_id: "w1:p2".into(),
        })
        .unwrap();
        let output = round_trip("pane.media_preflight", params.clone());
        assert_eq!(output["params"], params);
        assert_eq!(
            serde_json::from_value::<PaneTarget>(output["params"].clone())
                .unwrap()
                .pane_id,
            "w1:p2"
        );
    }

    #[test]
    fn legacy_media_methods_still_accept_targets_without_receipt_metadata() {
        let open = round_trip("pane.media_open", json!({"pane_id": "w1:p2"}));
        let request: Request = serde_json::from_value(open.clone()).unwrap();
        assert!(matches!(request.method, Method::PaneMediaOpen(_)));
        assert_eq!(open["params"]["pane_id"], "w1:p2");
        assert!(open["params"]["generation"].is_null());
        assert!(open["params"]["attempt"].is_null());

        let close = round_trip("media.close", json!({"session_id": "media_1"}));
        let request: Request = serde_json::from_value(close.clone()).unwrap();
        assert!(matches!(request.method, Method::MediaClose(_)));
        assert_eq!(close["params"]["session_id"], "media_1");
        assert!(close["params"]["request_id"].is_null());
    }

    #[test]
    fn pane_media_open_round_trip_preserves_generation_and_attempt() {
        for token in valid_tokens() {
            // Exercise each field at both boundaries with distinct values so a
            // copy/paste mapping cannot replace one identity with the other.
            for (generation, attempt) in [
                (token.as_str(), "attempt-7"),
                ("generation-9", token.as_str()),
            ] {
                let output = round_trip(
                    "pane.media_open",
                    json!({"pane_id": "w1:p2", "generation": generation, "attempt": attempt}),
                );
                assert_eq!(output["params"]["pane_id"], "w1:p2");
                assert_eq!(output["params"]["generation"], generation);
                assert_eq!(output["params"]["attempt"], attempt);
            }
        }
    }

    #[test]
    fn pane_media_open_incomplete_pair_is_rejected_or_preserved_for_boundary_validation() {
        for field in ["generation", "attempt"] {
            let mut params = json!({"pane_id": "w1:p2"});
            params[field] = json!("opaque-1");
            assert_rejected_or_preserved("pane.media_open", params);
        }
    }

    fn check_open_tokens(tokens: &[String]) {
        for field in ["generation", "attempt"] {
            for token in tokens {
                let mut params = json!({"pane_id": "w1:p2", "generation": "g1", "attempt": "a1"});
                params[field] = json!(token);
                assert_rejected_or_preserved("pane.media_open", params);
            }
        }
    }

    #[test]
    fn pane_media_open_empty_tokens_are_rejected_or_preserved_for_boundary_validation() {
        check_open_tokens(&[String::new()]);
    }

    #[test]
    fn pane_media_open_overlong_tokens_are_rejected_or_preserved_for_boundary_validation() {
        check_open_tokens(&["a".repeat(129)]);
    }

    #[test]
    fn pane_media_open_invalid_ascii_is_rejected_or_preserved_for_boundary_validation() {
        check_open_tokens(&[
            "bad token".into(),
            "bad/token".into(),
            "bad+token".into(),
            "bad@token".into(),
            "bad\nline".into(),
            "bad\0token".into(),
            "bad\u{7f}token".into(),
        ]);
    }

    #[test]
    fn pane_media_open_non_ascii_is_rejected_or_preserved_for_boundary_validation() {
        check_open_tokens(&["génération".into(), "attempt🔒".into()]);
    }

    #[test]
    fn media_close_round_trip_preserves_request_id() {
        for token in valid_tokens() {
            let output = round_trip(
                "media.close",
                json!({"session_id": "media_1", "request_id": token}),
            );
            assert_eq!(output["params"]["session_id"], "media_1");
            assert_eq!(output["params"]["request_id"], token);
        }
    }

    fn check_close_tokens(tokens: &[String]) {
        for token in tokens {
            assert_rejected_or_preserved(
                "media.close",
                json!({"session_id": "media_1", "request_id": token}),
            );
        }
    }

    #[test]
    fn media_close_empty_request_id_is_rejected_or_preserved_for_boundary_validation() {
        check_close_tokens(&[String::new()]);
    }

    #[test]
    fn media_close_overlong_request_id_is_rejected_or_preserved_for_boundary_validation() {
        check_close_tokens(&["r".repeat(129)]);
    }

    #[test]
    fn media_close_invalid_ascii_is_rejected_or_preserved_for_boundary_validation() {
        check_close_tokens(&[
            "bad token".into(),
            "bad/token".into(),
            "bad+token".into(),
            "bad@token".into(),
            "bad\nline".into(),
            "bad\0token".into(),
            "bad\u{7f}token".into(),
        ]);
    }

    #[test]
    fn media_close_non_ascii_is_rejected_or_preserved_for_boundary_validation() {
        check_close_tokens(&["requête".into(), "request🔒".into()]);
    }
}
