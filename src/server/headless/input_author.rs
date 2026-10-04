//! Pane-private JSON input-evidence dispatch. Presentation and media ownership
//! remain separate; socket peer authority is checked by the App handler.

use super::*;

impl HeadlessServer {
    pub(super) fn handle_input_author_api_request(&mut self, msg: api::ApiRequestMessage) {
        let response = self
            .app
            .handle_api_request_after_internal_events_drained_with_context(
                msg.request,
                msg.context,
            );
        let _ = msg.respond_to.send(response);
    }
}

#[cfg(test)]
mod tests {
    use crate::api::{schema::*, ApiRequestContext};
    use crate::app::{App, AppPolicy, Mode};
    use crate::protocol::{ClientKeyCode, ClientKeyKind, ClientPaneInputEvent};
    use crate::server::client_identity::{ClientIdentity, Principal};
    use crate::server::pane_input::{
        apply_client_pane_input_events_with_receipts, apply_terminal_attach_input_with_receipt,
        apply_terminal_attach_scroll_with_receipt,
    };
    use crate::terminal::{TerminalId, TerminalRuntime};

    struct Fixture {
        app: App,
        own: String,
        other: String,
        terminal: TerminalId,
        input: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.app.api_input_log);
        }
    }
    fn fixture(capacity: usize) -> Fixture {
        let (_, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = crate::workspace::Workspace::test_new("author-query");
        let own = workspace.tabs[0].root_pane;
        let other = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.mode = Mode::Terminal;
        let terminal = app
            .state
            .terminal_id_for_pane(0, own)
            .expect("own terminal");
        let other_terminal = app
            .state
            .terminal_id_for_pane(0, other)
            .expect("other terminal");
        let (runtime, input) = TerminalRuntime::test_with_channel_capacity(80, 24, capacity);
        runtime.test_set_child_pid(std::process::id());
        let (other_runtime, _other_input) = TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        app.terminal_runtimes.insert(other_terminal, other_runtime);
        Fixture {
            own: app.public_pane_id(0, own).expect("own id"),
            other: app.public_pane_id(0, other).expect("other id"),
            app,
            terminal,
            input,
        }
    }
    fn mapped_identity() -> ClientIdentity {
        ClientIdentity {
            peer_pid: Some(42),
            uid: Some(1000),
            principal: Some(Principal {
                id: "proof-paul".into(),
                name: "Paul".into(),
            }),
        }
    }
    fn enter() -> ClientPaneInputEvent {
        ClientPaneInputEvent::Key {
            code: ClientKeyCode::Enter,
            modifiers: 0,
            kind: ClientKeyKind::Press,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        }
    }
    fn client_input(fixture: &Fixture, events: &[ClientPaneInputEvent]) -> Result<bool, String> {
        let identity = mapped_identity();
        let runtime = fixture
            .app
            .terminal_runtimes
            .get(&fixture.terminal)
            .expect("runtime");
        apply_client_pane_input_events_with_receipts(runtime, events, |event| {
            fixture
                .app
                .record_client_input(&fixture.terminal, 1, Some(&identity), event);
        })
    }
    fn query(fixture: &mut Fixture, pane: String, peer: Option<u32>) -> serde_json::Value {
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "take".into(),
                method: Method::PaneTakeInputAuthor(PaneTakeInputAuthorParams { pane_id: pane }),
            },
            ApiRequestContext {
                local_peer_pid: peer,
            },
        );
        serde_json::from_str(&response).expect("query JSON")
    }
    fn own_query(fixture: &mut Fixture) -> serde_json::Value {
        let own = fixture.own.clone();
        query(fixture, own, Some(std::process::id()))
    }
    fn send_api(fixture: &mut Fixture, text: &str) -> serde_json::Value {
        let response = fixture.app.handle_api_request_with_context(
            Request {
                id: "send".into(),
                method: Method::PaneSendText(PaneSendTextParams {
                    pane_id: fixture.own.clone(),
                    text: text.into(),
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext {
                local_peer_pid: Some(std::process::id()),
            },
        );
        serde_json::from_str(&response).expect("send JSON")
    }

    #[tokio::test]
    async fn own_peer_can_query_but_other_pane_and_missing_peer_cannot_consume() {
        let mut f = fixture(16);
        client_input(
            &f,
            &[ClientPaneInputEvent::TextCommit("hello".into()), enter()],
        )
        .unwrap();
        let other = f.other.clone();
        assert_eq!(
            query(&mut f, other, Some(std::process::id()))["error"]["code"],
            "input_author_forbidden"
        );
        let own = f.own.clone();
        assert_eq!(
            query(&mut f, own, None)["error"]["code"],
            "input_author_forbidden"
        );
        let result = own_query(&mut f);
        assert_eq!(result["result"]["type"], "pane_input_author");
        assert_eq!(result["result"]["author"]["source"], "client");
        assert_eq!(
            result["result"]["author"]["client"]["principal"]["id"],
            "proof-paul"
        );
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "unknown");
    }

    #[tokio::test]
    async fn api_current_edit_and_next_draft_races_remain_api_after_queries() {
        let mut f = fixture(32);
        client_input(&f, &[ClientPaneInputEvent::TextCommit("A".into()), enter()]).unwrap();
        assert_eq!(send_api(&mut f, "draft B")["result"]["type"], "ok");
        let first = own_query(&mut f);
        assert_eq!(first["result"]["author"]["source"], "api");
        assert_eq!(
            first["result"]["author"]["caller"]["pid"],
            std::process::id()
        );
        assert_eq!(first["result"]["author"]["caller"]["pane"], f.own);
        client_input(&f, &[enter()]).unwrap();
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "api");
        // Even retyping and a second query cannot prove an empty editor baseline.
        client_input(
            &f,
            &[
                ClientPaneInputEvent::TextCommit("clean-looking".into()),
                enter(),
            ],
        )
        .unwrap();
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "api");
    }

    #[tokio::test]
    async fn actual_enqueue_failure_preserves_accepted_prefix_only() {
        let mut f = fixture(1);
        let failed = client_input(
            &f,
            &[
                ClientPaneInputEvent::TextCommit("accepted".into()),
                ClientPaneInputEvent::Paste("rejected\nmultiline".into()),
            ],
        );
        assert!(failed.is_err());
        assert_eq!(
            f.input.try_recv().unwrap(),
            bytes::Bytes::from_static(b"accepted")
        );
        assert!(f.input.try_recv().is_err());
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "client");
        // No receipt for a wholly rejected event, including a rejected Enter.
        f.app
            .terminal_runtimes
            .get(&f.terminal)
            .unwrap()
            .try_send_bytes(bytes::Bytes::from_static(b"fill"))
            .unwrap();
        assert!(client_input(&f, &[enter()]).is_err());
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "unknown");
    }

    #[tokio::test]
    async fn api_multi_key_failure_and_failed_enqueue_cannot_borrow_client_author() {
        let mut f = fixture(2);
        client_input(&f, &[ClientPaneInputEvent::TextCommit("human".into())]).unwrap();
        let response = f.app.handle_api_request_with_context(
            Request {
                id: "partial".into(),
                method: Method::PaneSendKeys(PaneSendKeysParams {
                    pane_id: f.own.clone(),
                    keys: vec!["x".into(), "enter".into()],
                    allow_cross_pane: false,
                }),
            },
            ApiRequestContext {
                local_peer_pid: Some(std::process::id()),
            },
        );
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["error"]["code"], "pane_send_failed");
        assert_eq!(
            f.input.try_recv().unwrap(),
            bytes::Bytes::from_static(b"human")
        );
        assert_eq!(f.input.try_recv().unwrap(), bytes::Bytes::from_static(b"x"));
        assert!(f.input.try_recv().is_err());
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "api");
    }

    #[tokio::test]
    async fn unavailable_audit_log_and_empty_api_input_do_not_create_taint() {
        let mut f = fixture(8);
        client_input(&f, &[ClientPaneInputEvent::TextCommit("human".into())]).unwrap();
        assert_eq!(send_api(&mut f, "")["result"]["type"], "ok");
        let original = f.app.api_input_log.clone();
        f.app.api_input_log = std::env::temp_dir(); // directory, cannot append a private log file
        assert_eq!(
            send_api(&mut f, "refused")["error"]["code"],
            "input_log_unavailable"
        );
        f.app.api_input_log = original;
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "client");
    }

    #[tokio::test]
    async fn direct_raw_receipts_preserve_bytes_and_fail_closed_on_fragmented_sequences() {
        let mut f = fixture(16);
        let identity = mapped_identity();
        for raw in [b"hello\r".as_slice(), b"\x1b[20", b"0~older\r\x1b[201~"] {
            let runtime = f.app.terminal_runtimes.get(&f.terminal).unwrap();
            assert!(
                apply_terminal_attach_input_with_receipt(runtime, raw.to_vec(), |data| {
                    f.app
                        .record_raw_client_input(&f.terminal, 1, Some(&identity), data);
                })
                .unwrap()
            );
            assert_eq!(f.input.try_recv().unwrap().as_ref(), raw);
            let result = own_query(&mut f);
            assert_eq!(
                result["result"]["author"]["source"],
                if raw == b"hello\r" { "client" } else { "api" }
            );
        }
    }

    #[tokio::test]
    async fn multiline_paste_and_history_never_get_a_principal() {
        let mut f = fixture(8);
        client_input(
            &f,
            &[ClientPaneInputEvent::Paste("first\nsecond".into()), enter()],
        )
        .unwrap();
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "api");
        client_input(
            &f,
            &[ClientPaneInputEvent::TextCommit("later".into()), enter()],
        )
        .unwrap();
        assert_eq!(own_query(&mut f)["result"]["author"]["source"], "api");
    }

    #[tokio::test]
    async fn host_only_scrollback_never_issues_child_input_receipt() {
        let f = fixture(8);
        let runtime = f.app.terminal_runtimes.get(&f.terminal).unwrap();
        let mut receipts = 0;
        apply_terminal_attach_scroll_with_receipt(
            runtime,
            crate::protocol::AttachScrollSource::Wheel,
            crate::protocol::AttachScrollDirection::Up,
            1,
            None,
            None,
            0,
            || receipts += 1,
        )
        .unwrap();
        assert_eq!(receipts, 0);
    }

    #[test]
    fn take_schema_requires_pane_id_and_is_not_a_client_shell_method() {
        assert!(serde_json::from_value::<Request>(serde_json::json!({
            "id":"take", "method":"pane.take_input_author", "params":{}
        }))
        .is_err());
        assert!(serde_json::from_value::<Request>(serde_json::json!({
            "id":"take", "method":"pane.take_input_author", "params":{"pane_id":null}
        }))
        .is_err());
        let request: Request = serde_json::from_value(serde_json::json!({
            "id":"take", "method":"pane.take_input_author", "params":{"pane_id":"w1:p1"}
        }))
        .unwrap();
        assert_eq!(
            crate::api::api_method_name(&request.method),
            "pane.take_input_author"
        );
        assert!(!crate::server::client_commands::supports_client_shell_method(&request.method));
    }
}
