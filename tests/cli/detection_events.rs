//! Real detector regressions: private manifests, foreground processes, PTY output,
//! and the public socket. These markers are a synthetic protocol, not agent UI.
use super::harness::*;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

// The normal detector cadence is 300ms on Unix (500ms on Windows). Allow
// another second for loaded CI scheduling and socket fanout, not startup grace.
const OUTPUT_DEADLINE: Duration = Duration::from_millis(1500);
const SETUP_DEADLINE: Duration = Duration::from_secs(8);

fn synthetic_manifest(agent: &str) -> String {
    format!(
        r#"id = "{agent}"

[[rules]]
id = "synthetic-working"
state = "working"
region = "bottom_non_empty_lines(1)"
visible_working = true
all = [{{ contains = ["E2E:"] }}]
any = [{{ line_regex = ['^E2E:WORK$'] }}]

[[rules]]
id = "synthetic-blocked"
state = "blocked"
region = "bottom_non_empty_lines(1)"
visible_blocker = true
line_regex = ['^E2E:BLOCK$']

[[rules]]
id = "synthetic-ready"
state = "idle"
region = "bottom_non_empty_lines(1)"
visible_idle = true
line_regex = ['^E2E:READY$']

[[rules]]
id = "synthetic-plain-idle"
state = "idle"
region = "bottom_non_empty_lines(1)"
line_regex = ['^E2E:PLAIN$']
"#
    )
}

struct DetectorFixture {
    base: PathBuf,
    socket: PathBuf,
    pane: String,
    control: fs::File,
    server: Option<SpawnedHerdr>,
}

impl DetectorFixture {
    fn new() -> Self {
        let base = unique_test_dir();
        let config_home = base.join("config");
        let runtime_dir = base.join("runtime");
        let socket = runtime_dir.join("herdr.sock");
        let manifests = config_home.join(app_dir_name()).join("agent-detection");
        fs::create_dir_all(&manifests).unwrap();
        for agent in ["pi", "codex"] {
            fs::write(
                manifests.join(format!("{agent}.toml")),
                synthetic_manifest(agent),
            )
            .unwrap();
        }

        let fifo = base.join("control");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        // Keep both ends open so dispatch cannot block on an agent startup/exit.
        let control = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let script = base.join("synthetic-agent.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
exec 3<"$1"
printf '%s\n' "$$" > "$2/agent.pid"
printf '%s\n' "$3" > "$2/identity"
marker() { printf '\033[2J\033[HE2E:%s' "$1"; }
if [ "$4" != silent ]; then marker READY; fi
while IFS= read -r command <&3; do
    case "$command" in
        work) marker WORK ;;
        block) marker BLOCK ;;
        ready) marker READY ;;
        plain) marker PLAIN ;;
        burst)
            marker PLAIN
            i=0
            while [ "$i" -lt 30 ]; do
                # Separate PTY writes without changing the bottom-buffer text.
                printf '\033[H'
                /bin/sleep 0.005
                i=$((i + 1))
            done
            ;;
        replace)
            HERDR_AGENT=codex exec /bin/sh "$0" "$1" "$2" codex silent
            ;;
        quit)
            printf '%s\n' "$command" > "$2/receipt"
            exit 0
            ;;
        *) exit 2 ;;
    esac
    printf '%s\n' "$command" > "$2/receipt"
done
"#,
        )
        .unwrap();

        let server = spawn_herdr(&config_home, &runtime_dir, &socket);
        // Construct the guard before assertions so failures clean up the server.
        let mut fixture = Self {
            base,
            socket,
            pane: String::new(),
            control,
            server: Some(server),
        };
        wait_for_socket(&fixture.socket, SETUP_DEADLINE);
        let created = run_cli_json(
            &fixture.socket,
            &[
                "workspace",
                "create",
                "--cwd",
                fixture.base.to_str().unwrap(),
            ],
        );
        fixture.pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        // A real shell foreground subprocess, with its initial environment hint
        // visible to the process probe. No report-agent or other lifecycle hooks.
        // Suppress shell echo/prompts so quit and exec replacement are PTY-silent.
        let launch = format!(
            "stty -echo; PS1=; PS2=; HERDR_AGENT=pi /bin/sh '{}' '{}' '{}' pi",
            script.display(),
            fifo.display(),
            fixture.base.display()
        );
        // `pane run` acknowledges success quietly; only query commands return JSON.
        let launched = run_cli(&fixture.socket, &["pane", "run", &fixture.pane, &launch]);
        assert!(launched.status.success(), "{launched:?}");
        assert!(launched.stdout.is_empty(), "{launched:?}");
        assert!(launched.stderr.is_empty(), "{launched:?}");
        fixture.wait_for_agent("pi", "idle", SETUP_DEADLINE);
        // Reaching idle through screen detection proves the 3s acquisition grace
        // has expired. Transition deadlines below start only after this point.
        let explain = run_cli_json(
            &fixture.socket,
            &["agent", "explain", &fixture.pane, "--json"],
        );
        assert_eq!(
            explain["manifest_source"],
            manifests.join("pi.toml").to_str().unwrap(),
            "{explain}"
        );
        assert_eq!(
            explain["matched_rule"]["id"], "synthetic-ready",
            "{explain}"
        );
        fixture
    }

    fn pane_info(&self) -> serde_json::Value {
        run_cli_json(&self.socket, &["pane", "get", &self.pane])["result"]["pane"].clone()
    }

    fn wait_for_agent(&self, agent: &str, state: &str, timeout: Duration) {
        let mut last = serde_json::Value::Null;
        assert!(
            wait_until(timeout, Duration::from_millis(25), || {
                last = self.pane_info();
                last["agent"] == agent && status_matches(&last["agent_status"], state)
            }),
            "expected {agent}/{state}, last pane: {last}"
        );
    }

    fn dispatch(&mut self, command: &str) -> Instant {
        let started = Instant::now();
        writeln!(self.control, "{command}").unwrap();
        self.control.flush().unwrap();
        started
    }

    fn detection_text(&self) -> Vec<u8> {
        let output = run_cli(
            &self.socket,
            &[
                "pane",
                "read",
                &self.pane,
                "--source",
                "detection",
                "--format",
                "text",
            ],
        );
        assert!(output.status.success(), "{:?}", output);
        output.stdout
    }

    fn receipt(&self, expected: &str) {
        assert!(
            wait_until(OUTPUT_DEADLINE, Duration::from_millis(10), || {
                fs::read_to_string(self.base.join("receipt"))
                    .is_ok_and(|receipt| receipt.trim() == expected)
            }),
            "synthetic producer did not complete {expected}"
        );
    }
}

impl Drop for DetectorFixture {
    fn drop(&mut self) {
        drop(self.server.take());
        cleanup_test_base(&self.base);
    }
}

fn status_matches(value: &serde_json::Value, expected: &str) -> bool {
    value == expected || (expected == "idle" && value == "done")
}

struct DetectionEvents {
    reader: BufReader<UnixStream>,
    pane: String,
}

impl DetectionEvents {
    fn subscribe(fixture: &DetectorFixture) -> Self {
        let mut stream = UnixStream::connect(&fixture.socket).unwrap();
        stream.set_read_timeout(Some(SETUP_DEADLINE)).unwrap();
        writeln!(
            stream,
            "{}",
            serde_json::json!({
                "id": "synthetic-detector-events",
                "method": "events.subscribe",
                "params": {"subscriptions": [
                    {"type": "pane.agent_status_changed", "pane_id": fixture.pane},
                    {"type": "pane.agent_detected"}
                ]}
            })
        )
        .unwrap();
        stream.flush().unwrap();
        let mut events = Self {
            reader: BufReader::new(stream),
            pane: fixture.pane.clone(),
        };
        let response = events.next(Instant::now() + SETUP_DEADLINE);
        assert_eq!(
            response["result"]["type"], "subscription_started",
            "{response}"
        );
        events
    }

    fn next(&mut self, deadline: Instant) -> serde_json::Value {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("detector event deadline expired");
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .unwrap();
        let mut line = String::new();
        let count = self.reader.read_line(&mut line).unwrap_or_else(|error| {
            panic!("detector event not received: {error}; partial={line:?}")
        });
        assert_ne!(count, 0, "detector event socket closed");
        serde_json::from_str(&line).unwrap()
    }

    fn wait_status(&mut self, expected: &str, deadline: Instant) -> serde_json::Value {
        loop {
            let event = self.next(deadline);
            if event["event"] == "pane.agent_status_changed"
                && event["data"]["pane_id"] == self.pane
                && status_matches(&event["data"]["agent_status"], expected)
            {
                return event;
            }
        }
    }
}

#[test]
fn detection_events_output_only_markers_wake_a_settled_detector() {
    let mut fixture = DetectorFixture::new();
    let mut events = DetectionEvents::subscribe(&fixture);
    // No pane input, process change, reload, or API mutation after acquisition:
    // only the foreground producer's FIFO-driven PTY output changes the state.
    for (command, expected) in [("work", "working"), ("block", "blocked"), ("ready", "idle")] {
        let started = fixture.dispatch(command);
        let event = events.wait_status(expected, started + OUTPUT_DEADLINE);
        assert_eq!(event["data"]["agent"], "pi", "{event}");
        fixture.receipt(command);
        fixture.wait_for_agent("pi", expected, OUTPUT_DEADLINE);
    }
}

#[test]
fn detection_events_plain_idle_confirms_without_more_output() {
    let mut fixture = DetectorFixture::new();
    let mut events = DetectionEvents::subscribe(&fixture);
    let started = fixture.dispatch("work");
    events.wait_status("working", started + OUTPUT_DEADLINE);
    let started = fixture.dispatch("plain");
    events.wait_status("idle", started + OUTPUT_DEADLINE);
    fixture.receipt("plain");
    // The producer is now blocked on the FIFO. No output is available to wake
    // confirmation scans; the unchanged bottom buffer must still publish idle.
    fixture.wait_for_agent("pi", "idle", OUTPUT_DEADLINE);
}

#[test]
fn detection_events_burst_cannot_spend_plain_idle_confirmations() {
    let mut fixture = DetectorFixture::new();
    let mut events = DetectionEvents::subscribe(&fixture);
    let started = fixture.dispatch("work");
    events.wait_status("working", started + OUTPUT_DEADLINE);
    let started = fixture.dispatch("burst");
    events.wait_status("idle", started + OUTPUT_DEADLINE);
    // Three 100ms confirmation rechecks must not become three raw output wakes.
    // This is only a generous LOWER bound (300ms minus 100ms), so host slowness
    // cannot fail it. The socket stays subscribed while separate writes arrive.
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "burst output prematurely confirmed plain idle after {:?}",
        started.elapsed()
    );
    fixture.receipt("burst");
    fixture.wait_for_agent("pi", "idle", OUTPUT_DEADLINE);
}

#[test]
fn detection_events_silent_foreground_exit_releases_stale_working_marker() {
    let mut fixture = DetectorFixture::new();
    let mut events = DetectionEvents::subscribe(&fixture);
    let started = fixture.dispatch("work");
    events.wait_status("working", started + OUTPUT_DEADLINE);
    let before = fixture.detection_text();
    let pid: u32 = fs::read_to_string(fixture.base.join("agent.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let started = fixture.dispatch("quit");
    events.wait_status("idle", started + OUTPUT_DEADLINE);
    fixture.receipt("quit");
    assert!(wait_for_pid_exit(pid, OUTPUT_DEADLINE));
    assert_eq!(
        fixture.detection_text(),
        before,
        "exit must not emit PTY output"
    );
    assert!(
        wait_until(OUTPUT_DEADLINE, Duration::from_millis(25), || {
            fixture.pane_info()["agent"].is_null()
        }),
        "silent foreground exit retained the agent identity"
    );
}

#[test]
fn detection_events_silent_exec_replacement_rescans_unchanged_idle_buffer() {
    let mut fixture = DetectorFixture::new();
    let mut events = DetectionEvents::subscribe(&fixture);
    let before = fixture.detection_text();
    let pid_before = fs::read_to_string(fixture.base.join("agent.pid")).unwrap();
    let started = fixture.dispatch("replace");
    // exec preserves the foreground PGID. The existing identified-process
    // safety probe is 5s, unlike output/group-change wakes; give it 2s CI slack.
    loop {
        let event = events.next(started + Duration::from_secs(7));
        if event["event"] == "pane_agent_detected"
            && event["data"]["pane_id"] == fixture.pane
            && event["data"]["agent"] == "codex"
        {
            break;
        }
    }
    assert_eq!(
        fs::read_to_string(fixture.base.join("identity"))
            .unwrap()
            .trim(),
        "codex"
    );
    assert_eq!(
        fs::read_to_string(fixture.base.join("agent.pid")).unwrap(),
        pid_before
    );
    assert_eq!(
        fixture.detection_text(),
        before,
        "replacement must not emit PTY output"
    );
    // Replacement acquisition gets its own 3s grace, then must scan the same
    // marker under the new manifest instead of reusing the old idle scan skip.
    let event = events.wait_status("idle", Instant::now() + Duration::from_secs(5));
    assert_eq!(event["data"]["agent"], "codex", "{event}");
    fixture.wait_for_agent("codex", "idle", OUTPUT_DEADLINE);
}

#[test]
fn detection_events_agent_exec_after_acquisition_window_is_identified() {
    // #3261: a launcher keeps writing output past the acquisition window, then
    // execs the agent in place (same PID and PGID, like a wrapper that becomes
    // Pi). The agent keeps rendering changed frames, like a real Pi TUI.
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket = runtime_dir.join("herdr.sock");
    let manifests = config_home.join(app_dir_name()).join("agent-detection");
    fs::create_dir_all(&manifests).unwrap();
    fs::write(manifests.join("pi.toml"), synthetic_manifest("pi")).unwrap();
    let launcher = base.join("launcher.sh");
    fs::write(
        &launcher,
        r#"#!/bin/sh
i=0
while [ ! -e "$1/go" ]; do
    printf 'launcher tick %s\n' "$i"
    i=$((i + 1))
    /bin/sleep 0.2
done
HERDR_AGENT=pi exec /bin/sh -c 'i=0; while :; do printf "\033[2J\033[Hagent tick %s\nE2E:READY" "$i"; i=$((i + 1)); /bin/sleep 0.2; done'
"#,
    )
    .unwrap();

    let server = spawn_herdr(&config_home, &runtime_dir, &socket);
    struct Cleanup(Option<SpawnedHerdr>, PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            drop(self.0.take());
            cleanup_test_base(&self.1);
        }
    }
    let _cleanup = Cleanup(Some(server), base.clone());
    wait_for_socket(&socket, SETUP_DEADLINE);
    let created = run_cli_json(
        &socket,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let launch = format!(
        "stty -echo; PS1=; PS2=; /bin/sh '{}' '{}'",
        launcher.display(),
        base.display()
    );
    let launched = run_cli(&socket, &["pane", "run", &pane, &launch]);
    assert!(launched.status.success(), "{launched:?}");
    let pane_info = || run_cli_json(&socket, &["pane", "get", &pane])["result"]["pane"].clone();

    // Outlast the 8s acquisition window with output that never pauses for 2s.
    let launched_at = Instant::now();
    while launched_at.elapsed() < Duration::from_secs(11) {
        let info = pane_info();
        assert!(info["agent"].is_null(), "launcher misidentified: {info}");
        std::thread::sleep(Duration::from_millis(250));
    }
    fs::write(base.join("go"), "").unwrap();

    // Rendering wakes the rate-limited 5s recheck; allow 3s CI slack, no timer.
    let mut last = serde_json::Value::Null;
    assert!(
        wait_until(Duration::from_secs(8), Duration::from_millis(50), || {
            last = pane_info();
            last["agent"] == "pi"
        }),
        "agent exec'd after the acquisition window was never identified: {last}"
    );
    // Identification starts the 3s startup grace before the screen is read.
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            last = pane_info();
            last["agent"] == "pi" && status_matches(&last["agent_status"], "idle")
        }),
        "identified agent never reached idle: {last}"
    );
}
