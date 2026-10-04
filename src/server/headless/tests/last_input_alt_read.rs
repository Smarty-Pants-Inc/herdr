use super::last_input_tests::{
    assert_media_owner_and_age, connect, last_input, setup_with_channel_capacity, type_into,
};
use super::*;

const ALICE: u64 = 31;
const CAPACITY: usize = 2;
type InputReceiver = tokio::sync::mpsc::Receiver<Bytes>;
type ResponseReceiver = std::sync::mpsc::Receiver<String>;

fn draw(lines: &[&str]) -> Vec<u8> {
    let mut bytes = b"\x1b[2J\x1b[H".to_vec();
    bytes.extend_from_slice(lines.join("\r\n").as_bytes());
    bytes
}

fn setup() -> (
    HeadlessServer,
    String,
    crate::terminal::TerminalId,
    InputReceiver,
) {
    let (mut server, pane, terminal, _old_input) = setup_with_channel_capacity(CAPACITY);
    let _alice = connect(&mut server, ALICE, Some("Alice"));
    let (runtime, input) =
        crate::terminal::TerminalRuntime::test_with_channel_capacity(20, 5, CAPACITY);
    runtime.test_process_pty_bytes(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    runtime.test_process_pty_bytes(&draw(&["16", "17", "18", "19", "20"]));
    server
        .app
        .terminal_runtimes
        .insert(terminal.clone(), runtime)
        .unwrap()
        .shutdown();
    let state = server.app.state.terminals.get_mut(&terminal).unwrap();
    state.detected_agent = Some(crate::detect::Agent::Claude);
    state.state = crate::detect::AgentState::Idle;
    assert!(server.terminal_attach_owners.is_empty());
    (server, pane, terminal, input)
}

fn named_input(server: &mut HeadlessServer, pane: &str, input: &mut InputReceiver) -> Instant {
    type_into(server, ALICE, pane);
    assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"hello"));
    let (_, runtime_pane) = server.app.parse_pane_id(pane).unwrap();
    let at = Instant::now() - Duration::from_secs(9);
    server.media.note_pane_input(ALICE, runtime_pane, pane, at);
    assert_eq!(last_input(server, pane)["user"], "Alice");
    at
}

fn read_method(pane: &str, agent: bool) -> api::schema::Method {
    if agent {
        api::schema::Method::AgentRead(api::schema::AgentReadParams {
            target: pane.into(),
            source: api::schema::ReadSource::Recent,
            lines: Some(8),
            format: api::schema::ReadFormat::Text,
            strip_ansi: true,
        })
    } else {
        api::schema::Method::PaneRead(api::schema::PaneReadParams {
            pane_id: pane.into(),
            source: api::schema::ReadSource::Recent,
            lines: Some(8),
            format: api::schema::ReadFormat::Text,
            strip_ansi: true,
            intent: api::schema::ReadIntent::Interactive,
        })
    }
}

fn request(server: &mut HeadlessServer, method: api::schema::Method) -> ResponseReceiver {
    let (respond_to, response) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        context: crate::api::ApiRequestContext::default(),
        request: api::schema::Request {
            id: "alt-attribution".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    response
}

fn start_read(server: &mut HeadlessServer, pane: &str, agent: bool) -> (ResponseReceiver, Instant) {
    let response = request(server, read_method(pane, agent));
    assert!(
        response.try_recv().is_err(),
        "interactive read must actually be pending"
    );
    assert_eq!(server.pending_alt_screen_reads.len(), 1);
    let deadline = server.pending_alt_screen_reads[0].next_deadline();
    (response, deadline)
}

fn wheel(input: &mut InputReceiver, down: bool) {
    let event = if down {
        "\x1b[<65;10;3M"
    } else {
        "\x1b[<64;10;3M"
    };
    assert_eq!(
        input.try_recv().expect("real wheel enqueue").as_ref(),
        event.repeat(3).as_bytes()
    );
    assert!(input.try_recv().is_err(), "exactly one wheel batch");
}

fn response(response: &ResponseReceiver) -> serde_json::Value {
    serde_json::from_str(&response.try_recv().expect("read completed")).unwrap()
}

#[tokio::test]
async fn agent_and_interactive_pane_read_probe_harvest_restore_clear_display_only() {
    for agent in [true, false] {
        let (mut server, pane, terminal, mut input) = setup();
        let mut owner_at = named_input(&mut server, &pane, &mut input);
        let (reply, probe_at) = start_read(&mut server, &pane, agent);
        let before = last_input(&mut server, &pane);
        server.poll_pending_alt_screen_reads(probe_at - Duration::from_millis(1));
        assert_eq!(
            last_input(&mut server, &pane),
            before,
            "not yet due means no receipt"
        );
        assert!(input.try_recv().is_err());
        server.poll_pending_alt_screen_reads(probe_at);
        wheel(&mut input, true);
        assert!(last_input(&mut server, &pane).is_null());
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);

        // Re-establish a sender between polls: each enqueue must issue a fresh receipt.
        owner_at = named_input(&mut server, &pane, &mut input);
        let harvest_at = server.pending_alt_screen_reads[0].next_deadline();
        server.poll_pending_alt_screen_reads(harvest_at);
        wheel(&mut input, false);
        assert!(last_input(&mut server, &pane).is_null());
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);

        owner_at = named_input(&mut server, &pane, &mut input);
        let before_restore = last_input(&mut server, &pane);
        server
            .app
            .terminal_runtimes
            .get(&terminal)
            .unwrap()
            .test_process_pty_bytes(&draw(&["13", "14", "15", "16", "17"]));
        let redraw_at = harvest_at + Duration::from_millis(1);
        server.poll_pending_alt_screen_reads(redraw_at);
        assert_eq!(
            last_input(&mut server, &pane),
            before_restore,
            "coalescing writes nothing"
        );
        assert!(input.try_recv().is_err());
        server.poll_pending_alt_screen_reads(redraw_at + Duration::from_millis(10));
        wheel(&mut input, true);
        assert!(last_input(&mut server, &pane).is_null());
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);

        // Completion without another enqueue must not replay an earlier receipt.
        named_input(&mut server, &pane, &mut input);
        let before_completion = last_input(&mut server, &pane);
        server
            .app
            .terminal_runtimes
            .get(&terminal)
            .unwrap()
            .test_process_pty_bytes(&draw(&["16", "17", "18", "19", "20"]));
        server.poll_pending_alt_screen_reads(redraw_at + Duration::from_millis(11));
        server.poll_pending_alt_screen_reads(redraw_at + Duration::from_millis(21));
        assert!(server.pending_alt_screen_reads.is_empty());
        assert_eq!(
            response(&reply)["result"]["read"]["text"],
            "13\n14\n15\n16\n17\n18\n19\n20\n"
        );
        assert_eq!(last_input(&mut server, &pane), before_completion);
        assert!(input.try_recv().is_err());
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn interactive_read_without_prior_owner_does_not_create_attribution_state() {
    let (mut server, pane, _terminal, mut input) = setup();
    assert!(!server.media.has_input_owners());
    let (reply, at) = start_read(&mut server, &pane, true);
    server.poll_pending_alt_screen_reads(at);
    wheel(&mut input, true);
    assert!(last_input(&mut server, &pane).is_null());
    assert!(!server.media.has_input_owners());
    // The expired bottom probe completes without harvesting or creating history.
    server.poll_pending_alt_screen_reads(at + Duration::from_secs(16));
    assert!(server.pending_alt_screen_reads.is_empty());
    assert_eq!(response(&reply)["result"]["type"], "pane_read");
    assert!(!server.media.has_input_owners());
    assert!(input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn rejected_and_no_write_alt_read_polls_preserve_complete_prior_sender() {
    for case in [
        "full",
        "closed",
        "absent",
        "no-report",
        "primary",
        "abort-before-probe",
    ] {
        let (mut server, pane, terminal, mut input) = setup();
        let owner_at = named_input(&mut server, &pane, &mut input);
        let before = last_input(&mut server, &pane);
        let (reply, at) = start_read(&mut server, &pane, true);
        match case {
            "full" => {
                let runtime = server.app.terminal_runtimes.get(&terminal).unwrap();
                for _ in 0..CAPACITY {
                    runtime.try_send_bytes(Bytes::from_static(b"fill")).unwrap();
                }
            }
            "closed" => input.close(),
            "absent" => server
                .app
                .terminal_runtimes
                .remove(&terminal)
                .unwrap()
                .shutdown(),
            "no-report" => server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .test_process_pty_bytes(b"\x1b[?1000l"),
            "primary" => server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .test_process_pty_bytes(b"\x1b[?1049l"),
            "abort-before-probe" => {
                server.app.state.terminals.get_mut(&terminal).unwrap().state =
                    crate::detect::AgentState::Working;
            }
            _ => unreachable!(),
        }
        server.poll_pending_alt_screen_reads(at);
        // Mode changes are output and may require one quiet/coalescing turn.
        server.poll_pending_alt_screen_reads(at + Duration::from_millis(10));
        server.poll_pending_alt_screen_reads(at + Duration::from_millis(20));
        assert!(server.pending_alt_screen_reads.is_empty(), "{case}");
        assert_eq!(response(&reply)["result"]["type"], "pane_read", "{case}");
        assert_eq!(last_input(&mut server, &pane), before, "{case}");
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);
        if case == "full" {
            for _ in 0..CAPACITY {
                assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
            }
        }
        assert!(
            input.try_recv().is_err(),
            "{case}: no anonymous bytes accepted"
        );
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn passive_and_suppressed_read_requests_preserve_complete_prior_sender() {
    for case in [
        "passive",
        "visible",
        "short",
        "attached",
        "working",
        "working-agent",
        "unknown",
        "no-report",
        "primary",
        "absent",
    ] {
        let (mut server, pane, terminal, mut input) = setup();
        let owner_at = named_input(&mut server, &pane, &mut input);
        let before = last_input(&mut server, &pane);
        let api::schema::Method::PaneRead(mut params) = read_method(&pane, false) else {
            unreachable!()
        };
        match case {
            "passive" => params.intent = api::schema::ReadIntent::Passive,
            "visible" => params.source = api::schema::ReadSource::Visible,
            "short" => params.lines = Some(5),
            "attached" => {
                server
                    .terminal_attach_owners
                    .insert(terminal.as_str().into(), 90);
            }
            "working" | "working-agent" => {
                server.app.state.terminals.get_mut(&terminal).unwrap().state =
                    crate::detect::AgentState::Working
            }
            "unknown" => {
                server
                    .app
                    .state
                    .terminals
                    .get_mut(&terminal)
                    .unwrap()
                    .detected_agent = None
            }
            "no-report" => server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .test_process_pty_bytes(b"\x1b[?1000l"),
            "primary" => server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .test_process_pty_bytes(b"\x1b[?1049l"),
            "absent" => server
                .app
                .terminal_runtimes
                .remove(&terminal)
                .unwrap()
                .shutdown(),
            _ => unreachable!(),
        }
        let method = if case == "working-agent" {
            read_method(&pane, true)
        } else {
            api::schema::Method::PaneRead(params)
        };
        let reply = request(&mut server, method);
        assert!(
            server.pending_alt_screen_reads.is_empty(),
            "{case}: traversal suppressed"
        );
        let result = response(&reply);
        if case == "absent" {
            assert!(result.get("error").is_some());
        } else if case == "working-agent" {
            assert_eq!(result["error"]["code"], "agent_not_idle");
        } else {
            assert_eq!(result["result"]["type"], "pane_read", "{case}");
        }
        server.poll_pending_alt_screen_reads(Instant::now() + Duration::from_secs(1));
        assert_eq!(last_input(&mut server, &pane), before, "{case}");
        assert!(input.try_recv().is_err(), "{case}");
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn accepted_probe_prefix_survives_later_queue_rejection_without_replaying_receipts() {
    for new_sender in [false, true] {
        let (mut server, pane, terminal, mut input) = setup();
        let mut owner_at = named_input(&mut server, &pane, &mut input);
        let (reply, at) = start_read(&mut server, &pane, false);
        server.poll_pending_alt_screen_reads(at);
        wheel(&mut input, true);
        assert!(last_input(&mut server, &pane).is_null());
        if new_sender {
            owner_at = named_input(&mut server, &pane, &mut input);
        }
        let before_rejection = last_input(&mut server, &pane);
        for _ in 0..CAPACITY {
            server
                .app
                .terminal_runtimes
                .get(&terminal)
                .unwrap()
                .try_send_bytes(Bytes::from_static(b"fill"))
                .unwrap();
        }
        let next = server.pending_alt_screen_reads[0].next_deadline();
        server.poll_pending_alt_screen_reads(next);
        assert!(server.pending_alt_screen_reads.is_empty());
        assert_eq!(response(&reply)["result"]["type"], "pane_read");
        assert_eq!(last_input(&mut server, &pane), before_rejection);
        assert_media_owner_and_age(&mut server, &pane, ALICE, owner_at);
        for _ in 0..CAPACITY {
            assert_eq!(input.try_recv().unwrap(), Bytes::from_static(b"fill"));
        }
        assert!(input.try_recv().is_err());
        shutdown_test_runtimes(&mut server);
    }
}

#[tokio::test]
async fn abort_restore_invalidates_unique_current_attachment_not_request_time_pane() {
    let (mut server, pane, terminal, mut input) = setup();
    named_input(&mut server, &pane, &mut input);
    let (reply, at) = start_read(&mut server, &pane, true);
    server.poll_pending_alt_screen_reads(at);
    wheel(&mut input, true);
    let harvest_at = server.pending_alt_screen_reads[0].next_deadline();
    server.poll_pending_alt_screen_reads(harvest_at);
    wheel(&mut input, false);
    let old_owner_at = named_input(&mut server, &pane, &mut input);
    let old_before = last_input(&mut server, &pane);

    let other = crate::workspace::Workspace::test_new("rebound-read");
    let other_pane = other.tabs[0].root_pane;
    let other_terminal = other.terminal_id(other_pane).unwrap().clone();
    server.app.state.workspaces.push(other);
    server.app.state.ensure_test_terminals();
    let other_ref = server.app.public_pane_id(1, other_pane).unwrap();
    let new_owner_at = Instant::now() - Duration::from_secs(9);
    server
        .media
        .note_pane_input(ALICE, other_pane, &other_ref, new_owner_at);
    let (_, old_pane) = server.app.parse_pane_id(&pane).unwrap();
    server.app.state.workspaces[0].tabs[0]
        .panes
        .get_mut(&old_pane)
        .unwrap()
        .attached_terminal_id = other_terminal;
    server.app.state.workspaces[1].tabs[0]
        .panes
        .get_mut(&other_pane)
        .unwrap()
        .attached_terminal_id = terminal.clone();
    server.app.state.workspaces.swap(0, 1);
    // An attachment arriving mid-harvest aborts the read, but its restoration
    // still sends anonymous wheel bytes to the terminal's NEW current host.
    server
        .terminal_attach_owners
        .insert(terminal.as_str().into(), 90);
    server.poll_pending_alt_screen_reads(harvest_at + Duration::from_millis(1));
    wheel(&mut input, true);
    assert_eq!(last_input(&mut server, &pane), old_before);
    assert!(last_input(&mut server, &other_ref).is_null());
    assert_media_owner_and_age(&mut server, &pane, ALICE, old_owner_at);
    assert_media_owner_and_age(&mut server, &other_ref, ALICE, new_owner_at);
    // Already-restored snapshot completes fallback without another receipt.
    server.poll_pending_alt_screen_reads(harvest_at + Duration::from_millis(121));
    assert!(server.pending_alt_screen_reads.is_empty());
    assert_eq!(
        response(&reply)["result"]["read"]["text"],
        "16\n17\n18\n19\n20\n"
    );
    assert_eq!(last_input(&mut server, &pane), old_before);
    assert!(input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}
