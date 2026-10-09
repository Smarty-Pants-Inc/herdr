//! Regression tests use an actual raw PTY and an executable byte sink, not a test channel.
use super::*;
use crate::api::schema::{Request, SuccessResponse};
use crate::detect::{Agent, AgentState};
use std::time::{Duration, Instant};

// Shared with the native counterexample test: only the exec race with the
// already-established group is harmless. EPERM/ESRCH and a wrong group fail.
const SINK_GROUP_SETUP: &str = r#"
import errno, os
def set_sink_group(pid):
    try:
        os.setpgid(pid, pid)
        return 0
    except OSError as error:
        if error.errno != errno.EACCES or os.getpgid(pid) != pid:
            raise
        return error.errno
"#;

struct Fixture {
    app: App,
    pane: String,
    capture: std::path::PathBuf,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    agent: Agent,
    reporter: crate::platform::ProcessIdentity,
    control: Option<std::path::PathBuf>,
    dir: std::path::PathBuf,
}

impl Fixture {
    fn new(agent: Agent) -> Self {
        Self::with_job_control(agent, false)
    }

    fn with_job_control(agent: Agent, job_control: bool) -> Self {
        Self::with_modes(agent, job_control, false, true)
    }

    fn with_modes(
        agent: Agent,
        job_control: bool,
        socket_reporter: bool,
        initially_detected: bool,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "session-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("private PTY fixture");
        let executable = dir.join(crate::detect::agent_label(agent));
        std::fs::copy(
            if socket_reporter {
                "/usr/bin/python3"
            } else {
                "/bin/cat"
            },
            &executable,
        )
        .expect("byte sink executable");
        let capture = dir.join("bytes");
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("session-guard")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app
            .state
            .terminal_id_for_pane(0, pane_id)
            .expect("terminal");
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("state");
        if initially_detected {
            terminal.set_detected_state(Some(agent), AgentState::Idle);
        }
        terminal.set_agent_name("sink".into());
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("real PTY");
        let control = job_control.then(|| dir.join("control"));
        let mut command = if let Some(control) = &control {
            let mut command = portable_pty::CommandBuilder::new("python3");
            command.arg("-c");
            command.arg(format!(
                "{SINK_GROUP_SETUP}\n{}",
                r#"
import os, select, signal, sys, time, tty
executable, capture, control, directory = sys.argv[1:]
tty.setraw(0)
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
def reap(pid):
    deadline = time.monotonic() + 2
    while os.waitpid(pid, os.WNOHANG)[0] == 0:
        if time.monotonic() >= deadline:
            raise TimeoutError('sink reap')
        time.sleep(0.01)
def sink(program, establish_group=True):
    # Python's pipe FDs are CLOEXEC. EOF forces the formerly racy ordering
    # on every sink, including fallback/replacement, without a timing sleep.
    ready, closed_on_exec = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready)
        if establish_group:
            os.setpgid(0, 0)
        signal.signal(signal.SIGTTOU, signal.SIG_DFL)
        signal.signal(signal.SIGTTIN, signal.SIG_DFL)
        fd = os.open(capture, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        os.dup2(fd, 1)
        os.close(fd)
        os.execv(program, [program])
    os.close(closed_on_exec)
    try:
        if not select.select([ready], [], [], 3)[0]:
            raise TimeoutError('sink exec')
        if os.read(ready, 1) != b'':
            raise RuntimeError('expected CLOEXEC EOF')
        race_errno = set_sink_group(pid)
        with open(directory + '/exec-race', 'a') as file:
            file.write(str(pid) + ' ' + str(race_errno) + '\n')
    except BaseException:
        os.kill(pid, signal.SIGKILL)
        reap(pid)
        raise
    finally:
        os.close(ready)
    return pid
reporter = sink(executable)
os.tcsetpgrp(0, reporter)
with open(directory + '/reporter.pid', 'w') as file:
    file.write(str(reporter))
os.mkfifo(control, 0o600)
with os.fdopen(os.open(control, os.O_RDWR), 'r') as commands:
    fallback = None
    for line in commands:
        action = line.strip()
        if action in ('background', 'replacement'):
            os.kill(reporter, signal.SIGSTOP)
            fallback = sink(executable if action == 'replacement' else '/bin/cat')
            os.tcsetpgrp(0, fallback)
            os.kill(fallback, signal.SIGCONT)
            with open(directory + '/fallback.pid', 'w') as file:
                file.write(str(fallback))
        elif action == 'foreground':
            os.kill(fallback, signal.SIGKILL)
            reap(fallback)
            fallback = None
            os.tcsetpgrp(0, reporter)
            os.kill(reporter, signal.SIGCONT)
        elif action == 'zombie':
            os.kill(reporter, signal.SIGKILL)
            deadline = time.monotonic() + 2
            while os.waitid(os.P_PID, reporter, os.WEXITED | os.WNOWAIT | os.WNOHANG) is None:
                if time.monotonic() >= deadline:
                    raise TimeoutError('sink zombie')
                time.sleep(0.01)
        elif action == 'fallback':
            fallback = sink('/bin/cat')
            os.tcsetpgrp(0, fallback)
            os.kill(fallback, signal.SIGCONT)
        elif action == 'reap':
            reap(reporter)
            reporter = None
        elif action == 'counterexamples':
            # Actual exec-before-parent with the WRONG inherited group must fail.
            try:
                sink('/bin/cat', establish_group=False)
            except OSError as error:
                assert error.errno == errno.EACCES, error
            else:
                raise AssertionError('wrong native group was accepted')
            dead = os.fork()
            if dead == 0:
                os._exit(0)
            reap(dead)
            for pid, expected in [(dead, errno.ESRCH), (os.getpid(), errno.EPERM)]:
                try:
                    set_sink_group(pid)
                except OSError as error:
                    assert error.errno == expected, error
                else:
                    raise AssertionError('unexpected setpgid error was ignored')
        elif action == 'stop':
            for pid in [reporter, fallback]:
                if pid is not None:
                    try:
                        os.kill(pid, signal.SIGKILL)
                        reap(pid)
                    except ProcessLookupError:
                        pass
                    except ChildProcessError:
                        pass
        with open(directory + '/ack', 'w') as file:
            file.write(action)
        if action == 'stop':
            break
"#
            ));
            command.arg(&executable);
            command.arg(&capture);
            command.arg(control);
            command.arg(&dir);
            command
        } else if socket_reporter {
            let mut command = portable_pty::CommandBuilder::new(&executable);
            command.args([
                "-c",
                r#"
import os, socket, sys, time, tty
capture, directory = sys.argv[1:]
tty.setraw(0)
with open(capture, 'wb', buffering=0) as output:
    while not os.path.exists(directory + '/report.json'):
        time.sleep(0.01)
    with open(directory + '/report.json') as file:
        request = file.read()
    with socket.socket(socket.AF_UNIX) as client:
        client.connect(directory + '/report.sock')
        client.sendall(request.encode() + b'\n')
        response = b''
        while not response.endswith(b'\n'):
            response += client.recv(4096)
        with open(directory + '/report.response', 'wb') as file:
            file.write(response)
    while True:
        data = os.read(0, 4096)
        if not data:
            break
        output.write(data)
"#,
            ]);
            command.arg(&capture);
            command.arg(&dir);
            command
        } else {
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args(["-c", "stty raw -echo; exec \"$1\" > \"$2\"", "sh"]);
            command.arg(&executable);
            command.arg(&capture);
            command
        };
        command.env_remove("HERDR_PANE_ID");
        let child = pair.slave.spawn_command(command).expect("PTY byte sink");
        let pid = child.process_id().expect("sink PID");
        let master_fd = unsafe { libc::dup(pair.master.as_raw_fd().expect("PTY fd")) };
        assert!(master_fd >= 0);
        let (events, _events_rx) = tokio::sync::mpsc::channel(32);
        let mut runtime = crate::terminal::TerminalRuntime::from_handoff_fd(
            crate::handoff_runtime::ImportedHandoffRuntime {
                master_fd,
                state: serde_json::from_value(serde_json::json!({
                    "pane_id": pane_id.raw(), "child_pid": pid, "rows": 24, "cols": 80,
                    "cell_width_px": 0, "cell_height_px": 0,
                }))
                .expect("import state"),
            },
            4096,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .expect("production PTY runtime");
        runtime.assume_handoff_ownership();
        runtime.set_handoff_reader_paused(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !capture.exists()
            || !crate::app::agents::runtime_hosts_agent(&runtime, agent)
            || (job_control
                && (!dir.join("reporter.pid").exists()
                    || !control.as_ref().is_some_and(|path| path.exists())))
        {
            assert!(Instant::now() < deadline, "PTY sink ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let reporter_pid = if job_control {
            std::fs::read_to_string(dir.join("reporter.pid"))
                .expect("reporter PID")
                .parse()
                .expect("numeric reporter PID")
        } else {
            pid
        };
        let reporter =
            crate::platform::process_identity(reporter_pid).expect("native PTY reporter identity");
        assert_ne!(
            reporter.pid,
            std::process::id(),
            "never bind the test runner"
        );
        app.state.insert_test_runtime(pane_id, runtime);
        let pane = app.public_pane_id(0, pane_id).expect("public pane");
        Self {
            app,
            pane,
            capture,
            child,
            agent,
            reporter,
            control,
            dir,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> String {
        let context = if method.starts_with("pane.report_agent") {
            crate::api::ApiRequestContext::capture(Some(self.reporter))
        } else {
            Default::default()
        };
        self.request_with_context(method, params, context)
    }

    fn request_with_context(
        &mut self,
        method: &str,
        mut params: serde_json::Value,
        context: crate::api::ApiRequestContext,
    ) -> String {
        params["allow_cross_pane"] = true.into();
        let request: Request = serde_json::from_value(serde_json::json!({
            "id": "session-guard", "method": method, "params": params,
        }))
        .expect("request schema");
        if method.starts_with("agent.prompt") {
            let (tx, rx) = std::sync::mpsc::channel();
            assert!(self
                .app
                .handle_deferred_agent_api_request(request, Default::default(), tx));
            rx.recv_timeout(Duration::from_secs(3))
                .expect("deferred prompt must respond")
        } else {
            // Deliberately do not consume ProcessExited/detection events. Ownership
            // must be checked against the OS even while App's cache still says Pi.
            self.app
                .handle_api_request_after_internal_events_drained_with_context(request, context)
        }
    }

    fn control(&self, action: &str) {
        self.try_control(action, Duration::from_secs(3))
            .unwrap_or_else(|error| panic!("control {action}: {error}"));
    }

    fn try_control(&self, action: &str, timeout: Duration) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind, Write};
        use std::os::unix::fs::OpenOptionsExt;
        let deadline = Instant::now() + timeout;
        let path = self
            .control
            .as_ref()
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "not a job-controlled fixture"))?;
        let ack = self.dir.join("ack");
        match std::fs::remove_file(&ack) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // A dead controller leaves a FIFO with no reader: blocking open would
        // hang before the ACK deadline, including during panic unwinding.
        let mut commands = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)?;
        let command = format!("{action}\n");
        if command.len() > 512 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "control exceeds PIPE_BUF",
            ));
        }
        // One atomic, nonblocking write. No write_all retry loop or FIFO reads.
        if commands.write(command.as_bytes())? != command.len() {
            return Err(Error::new(ErrorKind::WriteZero, "partial control command"));
        }
        loop {
            match std::fs::read_to_string(&ack) {
                Ok(value) if value == action => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::TimedOut,
                    format!("control ack: {action}"),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_cached_session(&mut self, expected: &str) {
        let (ws_idx, pane_id) = self.app.parse_pane_id(&self.pane).expect("pane");
        let terminal_id = self
            .app
            .state
            .terminal_id_for_pane(ws_idx, pane_id)
            .expect("cached terminal ID");
        let terminal = self
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("cached terminal");
        assert_eq!(terminal.detected_agent, Some(self.agent));
        assert_eq!(terminal.reported_agent_session_id(), Some(expected));
    }

    fn report(&mut self, id: Option<&str>, path: Option<&str>, seq: u64) {
        let response = self.request("pane.report_agent_session", serde_json::json!({
            "pane_id": self.pane, "source": format!("herdr:{}", crate::detect::agent_label(self.agent)),
            "agent": crate::detect::agent_label(self.agent), "seq": seq,
            "agent_session_id": id, "agent_session_path": path, "session_start_source": "new",
        }));
        assert_ok(&response);
    }

    fn bytes(&self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let actual = std::fs::read(&self.capture).expect("actual PTY bytes");
            if actual == expected {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY bytes: {actual:?}, expected {expected:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Allow actor submissions to flush; a rejected request must enqueue no delayed bytes either.
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(
            std::fs::read(&self.capture).expect("settled bytes"),
            expected
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.control.is_some() {
            // Drop must not double-panic or wait forever for a dead/stopped peer.
            if let Err(error) = self.try_control("stop", Duration::from_millis(250)) {
                tracing::warn!(%error, "fixture stop failed; terminating owned PTY session");
            }
        }
        // Runtime shutdown wakes the actor, then signals the owned session with
        // three 250ms grace periods. It does not join a PTY reader thread.
        for (_, runtime) in self.app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        // portable_pty's Unix kill has at most 200ms of SIGHUP grace before
        // SIGKILL; the child handle has not been reaped before session shutdown.
        let _ = self.child.kill();
        // Never call blocking Child::wait, even if kill failed during unwinding.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    tracing::warn!("fixture child reap deadline exceeded");
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_fixture_exec_before_parent_and_native_counterexamples() {
    let mut f = Fixture::with_job_control(Agent::Pi, true);
    assert_eq!(
        std::fs::read_to_string(f.dir.join("exec-race")).expect("exec barrier"),
        format!("{} {}\n", f.reporter.pid, libc::EACCES)
    );
    assert_eq!(
        unsafe { libc::getpgid(f.reporter.pid as i32) },
        f.reporter.pid as i32
    );
    f.control("counterexamples");
    f.report(Some("private-native-exec"), None, 1);
    assert_ok(&f.request(
        "pane.send_text_session_checked",
        serde_json::json!({
            "pane_id": f.pane, "text": "exec", "expected_agent_session_id": "private-native-exec",
        }),
    ));
    f.bytes(b"exec");
    f.control("background");
    assert_error(&f.request("pane.send_text_session_checked", serde_json::json!({
        "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "private-native-exec",
    })), "agent_session_unknown");
    f.bytes(b"exec");
    f.control("foreground");
    assert_ok(&f.request("pane.send_text_session_checked", serde_json::json!({
        "pane_id": f.pane, "text": "restored", "expected_agent_session_id": "private-native-exec",
    })));
    f.bytes(b"execrestored");
    let controller = f.child.process_id().expect("controller PID");
    let reporter = f.reporter.pid;
    let fallback: u32 = std::fs::read_to_string(f.dir.join("fallback.pid"))
        .expect("native fallback PID")
        .parse()
        .expect("numeric fallback PID");
    let dir = f.dir.clone();
    drop(f);
    for pid in [controller, reporter, fallback] {
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    assert!(!dir.exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_fixture_dead_controller_unwind_is_bounded() {
    let f = Fixture::with_job_control(Agent::Pi, true);
    // Reap the owned reporter before killing its controller, so the test does
    // not delegate orphan reaping to the host's init process.
    f.control("zombie");
    f.control("reap");
    let reporter = f.reporter;
    let controller = f.child.process_id().expect("controller PID");
    let dir = f.dir.clone();
    let identity = crate::platform::process_identity(controller).expect("owned controller");
    assert_eq!(
        crate::platform::process_identity(controller),
        Some(identity)
    );
    // Keep the controller unreaped until runtime shutdown has signalled its
    // session. That pins its PID even while Drop runs during unwinding.
    assert_eq!(unsafe { libc::kill(controller as i32, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(1);
    while crate::platform::process_identity(controller).is_some() {
        assert!(Instant::now() < deadline, "controller exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    let start = Instant::now();
    let error = f
        .try_control("foreground", Duration::from_millis(100))
        .expect_err("dead FIFO reader must fail, not block opening");
    assert_eq!(error.raw_os_error(), Some(libc::ENXIO));
    assert!(start.elapsed() < Duration::from_millis(100));
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // The assertion path must fail visibly; Drop during unwinding must not
        // panic a second time or block opening the readerless FIFO.
        f.control("foreground");
    }));
    assert!(unwind.is_err());
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "bounded complete Drop"
    );
    assert!(!dir.exists());
    assert!(!std::path::Path::new(&format!("/proc/{controller}")).exists());
    assert!(!std::path::Path::new(&format!("/proc/{}", reporter.pid)).exists());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_fixture_stalled_controller_and_full_fifo_are_bounded() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let f = Fixture::with_job_control(Agent::Pi, true);
    f.control("zombie");
    f.control("reap");
    let controller = f.child.process_id().expect("controller PID");
    let identity = crate::platform::process_identity(controller).expect("owned controller");
    assert_eq!(
        crate::platform::process_identity(controller),
        Some(identity)
    );
    assert_eq!(unsafe { libc::kill(controller as i32, libc::SIGSTOP) }, 0);
    let start = Instant::now();
    let error = f
        .try_control("foreground", Duration::from_millis(100))
        .expect_err("live reader without ACK must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(1));
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(f.control.as_ref().expect("FIFO"))
        .expect("live stopped reader");
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match fifo.write(&[b'x'; 512]) {
            Ok(_) => assert!(Instant::now() < deadline, "bounded FIFO fill"),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill FIFO: {error}"),
        }
    }
    let error = f
        .try_control("stop", Duration::from_millis(100))
        .expect_err("full FIFO must fail its nonblocking write");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    drop(fifo);
    let dir = f.dir.clone();
    let start = Instant::now();
    drop(f);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "bounded stalled-peer Drop"
    );
    assert!(!dir.exists());
    assert!(!std::path::Path::new(&format!("/proc/{controller}")).exists());
}

fn assert_ok(response: &str) {
    assert!(
        serde_json::from_str::<SuccessResponse>(response).is_ok(),
        "{response}"
    );
}
fn assert_error(response: &str, code: &str) {
    let json: serde_json::Value = serde_json::from_str(response).expect("error JSON");
    assert_eq!(json["error"]["code"], code, "{response}");
    assert!(
        json.get("result").is_none(),
        "an error must not include agent info: {response}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_agent_session_real_pty_socket_report_before_initial_detection_keeps_native_owner()
{
    use interprocess::local_socket::traits::Listener as _;
    use std::sync::{atomic::AtomicBool, Arc};
    let mut f = Fixture::with_modes(Agent::Pi, false, true, false);
    let (ws_idx, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
    let terminal_id = f
        .app
        .state
        .terminal_id_for_pane(ws_idx, pane_id)
        .expect("terminal");
    assert_eq!(f.app.state.terminals[&terminal_id].detected_agent, None);
    let listener =
        crate::ipc::bind_local_listener(&f.dir.join("report.sock")).expect("report socket");
    let (api_tx, mut api_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
    std::fs::write(f.dir.join("report.json"), serde_json::json!({
        "id": "native-report", "method": "pane.report_agent_session", "params": {
            "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "seq": 1,
            "agent_session_id": "private-session-socket", "agent_session_path": "/tmp/socket-owner.jsonl",
            "session_start_source": "new",
        },
    }).to_string()).expect("release actual socket reporter");
    let server = listener.accept().expect("child reporter connection");
    let handle = std::thread::spawn(move || {
        crate::api::test_handle_connection(
            server,
            &api_tx,
            &crate::api::EventHub::default(),
            &Arc::new(AtomicBool::new(true)),
            None,
        )
    });
    let message = tokio::time::timeout(Duration::from_secs(3), api_rx.recv())
        .await
        .expect("bounded report dispatch")
        .expect("report message");
    assert_eq!(
        message.context.local_peer_identity,
        Some(f.reporter),
        "kernel peer must be the PTY owner, not Main"
    );
    let response = f
        .app
        .handle_api_request_after_internal_events_drained_with_context(
            message.request,
            message.context,
        );
    assert_ok(&response);
    message.respond_to.send(response).expect("report response");
    handle
        .join()
        .expect("server thread")
        .expect("report socket response");
    let deadline = Instant::now() + Duration::from_secs(3);
    while !f.dir.join("report.response").exists() {
        assert!(
            Instant::now() < deadline,
            "reporter must resume its input loop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The official startup report arrives from the actual PTY owner BEFORE the
    // first process-detection event. Pi's full-lifecycle gate buffers this report
    // until detection; pre-detecting Pi would mask that startup ordering.
    let terminal = &f.app.state.terminals[&terminal_id];
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(terminal.reported_agent_session_id(), None);
    assert_eq!(terminal.reported_agent_session_reporter(), None);
    let accepted_revision = terminal.accepted_session_report_revision();
    f.app
        .handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: Instant::now(),
        });
    f.assert_cached_session("private-session-socket");
    let terminal = &f.app.state.terminals[&terminal_id];
    assert_eq!(
        terminal.accepted_session_report_revision(),
        accepted_revision.wrapping_add(1)
    );
    assert_eq!(terminal.reported_agent_session_reporter(), Some(f.reporter));
    for (method, params, field) in [
        ("pane.get", serde_json::json!({"pane_id": f.pane}), "pane"),
        ("agent.get", serde_json::json!({"target": f.pane}), "agent"),
    ] {
        let response = f.request(method, params);
        assert_ok(&response);
        let json: serde_json::Value = serde_json::from_str(&response).expect("discovery");
        assert_eq!(json["result"][field]["agent_session"]["kind"], "path");
        assert_eq!(
            json["result"][field]["agent_session"]["value"],
            "/tmp/socket-owner.jsonl"
        );
        assert_eq!(
            json["result"][field]["agent_session_id"],
            "private-session-socket"
        );
    }
    assert_ok(&f.request("pane.send_text_session_checked", serde_json::json!({
        "pane_id": f.pane, "text": "native", "expected_agent_session_id": "private-session-socket",
    })));
    f.bytes(b"native");
}

#[tokio::test]
async fn expected_agent_session_real_pty_accepted_report_before_initial_detection_keeps_native_owner(
) {
    let mut f = Fixture::with_modes(Agent::Claude, false, false, false);
    f.report(Some("private-startup-session"), None, 1);
    let (ws_idx, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
    let terminal_id = f
        .app
        .state
        .terminal_id_for_pane(ws_idx, pane_id)
        .expect("terminal");
    let terminal = &f.app.state.terminals[&terminal_id];
    assert_eq!(terminal.detected_agent, None);
    assert_eq!(
        terminal.reported_agent_session_id(),
        Some("private-startup-session")
    );
    assert_eq!(terminal.reported_agent_session_reporter(), Some(f.reporter));
    let revision = terminal.accepted_session_report_revision();
    f.app
        .handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Claude,
            observed_at: Instant::now(),
        });
    f.assert_cached_session("private-startup-session");
    let terminal = &f.app.state.terminals[&terminal_id];
    assert_eq!(terminal.accepted_session_report_revision(), revision);
    assert_eq!(terminal.reported_agent_session_reporter(), Some(f.reporter));
    assert_ok(&f.request("pane.send_text_session_checked", serde_json::json!({
        "pane_id": f.pane, "text": "startup", "expected_agent_session_id": "private-startup-session",
    })));
    f.bytes(b"startup");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_initial_matching_label_different_reporter_fails_closed() {
    let mut f = Fixture::with_modes(Agent::Claude, true, false, false);
    f.report(Some("private-startup-session"), None, 1);
    f.control("replacement");
    let replacement_pid = std::fs::read_to_string(f.dir.join("fallback.pid"))
        .expect("replacement PID")
        .parse()
        .expect("numeric replacement PID");
    let replacement = crate::platform::process_identity(replacement_pid).expect("live replacement");
    assert_ne!(replacement, f.reporter);
    assert_eq!(
        crate::platform::process_identity(f.reporter.pid),
        Some(f.reporter)
    );
    let (ws_idx, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
    let runtime = f
        .app
        .lookup_runtime_sender(ws_idx, pane_id)
        .expect("runtime");
    assert!(crate::app::agents::runtime_hosts_agent(
        runtime,
        Agent::Claude
    ));
    f.app
        .handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Claude,
            observed_at: Instant::now(),
        });
    // The matching label cannot reassign the report to this different process.
    // Its original binding remains a scalar fact, not foreground authorization.
    f.assert_cached_session("private-startup-session");
    for method in [
        "pane.send_text_session_checked",
        "pane.send_keys_session_checked",
        "agent.prompt_session_checked",
    ] {
        assert_error(
            &f.request(
                method,
                serde_json::json!({
                    "pane_id": f.pane, "target": f.pane, "text": "bad", "keys": ["enter"],
                    "expected_agent_session_id": "private-startup-session",
                }),
            ),
            "agent_session_unknown",
        );
    }
    f.bytes(b"");
    f.control("foreground");
    f.assert_cached_session("private-startup-session");
    assert_ok(&f.request("pane.send_text_session_checked", serde_json::json!({
        "pane_id": f.pane, "text": "original", "expected_agent_session_id": "private-startup-session",
    })));
    f.bytes(b"original");
}

#[tokio::test]
async fn expected_agent_session_real_pty_initial_different_agent_and_exit_clear_binding() {
    for (agent, process_exited) in [(Agent::Codex, false), (Agent::Claude, true)] {
        let mut f = Fixture::with_modes(Agent::Claude, false, false, false);
        f.report(Some("private-startup-session"), None, 1);
        let (_, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
        f.app
            .handle_internal_event(crate::events::AppEvent::StateChanged {
                pane_id,
                agent: Some(agent),
                state: AgentState::Unknown,
                visible_blocker: false,
                visible_working: false,
                process_exited,
                observed_at: Instant::now(),
            });
        for method in [
            "pane.send_text_session_checked",
            "pane.send_keys_session_checked",
            "agent.prompt_session_checked",
        ] {
            assert_error(
                &f.request(
                    method,
                    serde_json::json!({
                        "pane_id": f.pane, "target": f.pane, "text": "bad", "keys": ["enter"],
                        "expected_agent_session_id": "private-startup-session",
                    }),
                ),
                "agent_session_unknown",
            );
        }
        f.bytes(b"");
    }
}

#[tokio::test]
async fn expected_agent_session_real_pty_buffered_receipts_reject_same_key_duplicates() {
    for hook in [false, true] {
        let mut f = Fixture::with_modes(Agent::Pi, false, false, false);
        f.report(Some("private-buffered"), Some("/tmp/buffered.jsonl"), 1);
        let method = if hook {
            "pane.report_agent"
        } else {
            "pane.report_agent_session"
        };
        let seq = if hook { 2 } else { 1 };
        let params = serde_json::json!({
            "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "state": "idle", "seq": seq,
            "agent_session_id": "private-buffered", "agent_session_path": "/tmp/buffered.jsonl",
            "session_start_source": "startup",
        });
        if hook {
            assert_ok(&f.request(method, params.clone()));
        }
        let (ws_idx, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
        let terminal_id = f
            .app
            .state
            .terminal_id_for_pane(ws_idx, pane_id)
            .expect("terminal");
        let before = f.app.state.terminals[&terminal_id].session_report_marker();
        let mut rejected = params.clone();
        rejected["agent_session_id"] = "private-duplicate".into();
        // Same source/ref/seq, a different live native process in request context.
        // An ignored duplicate must not attach this ID or peer to the queue.
        assert_ok(&f.request_with_context(
            method,
            rejected,
            crate::api::ApiRequestContext::capture(crate::platform::process_identity(
                std::process::id(),
            )),
        ));
        let after = f.app.state.terminals[&terminal_id].session_report_marker();
        assert_eq!(before, after);
        f.app
            .handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
                pane_id,
                agent: Agent::Pi,
                observed_at: Instant::now(),
            });
        f.assert_cached_session("private-buffered");
        assert_eq!(
            f.app.state.terminals[&terminal_id].reported_agent_session_reporter(),
            Some(f.reporter)
        );
        assert_error(&f.request("pane.send_text_session_checked", serde_json::json!({
            "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "private-duplicate",
        })), "agent_session_mismatch");
        assert_ok(&f.request("pane.send_text_session_checked", serde_json::json!({
            "pane_id": f.pane, "text": "queued", "expected_agent_session_id": "private-buffered",
        })));
        f.bytes(b"queued");
    }
}

#[tokio::test]
async fn expected_agent_session_real_pty_buffered_new_path_only_or_unbound_report_fails_closed() {
    for (hook, unbound) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut f = Fixture::with_modes(Agent::Pi, false, false, false);
        f.report(Some("private-buffered"), Some("/tmp/buffered.jsonl"), 1);
        let method = if hook {
            "pane.report_agent"
        } else {
            "pane.report_agent_session"
        };
        let params = serde_json::json!({
            "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "state": "idle", "seq": 2,
            "agent_session_id": if unbound { Some("private-buffered") } else { None },
            "agent_session_path": "/tmp/buffered.jsonl", "session_start_source": "startup",
        });
        if unbound {
            assert_ok(&f.request_with_context(method, params, Default::default()));
        } else {
            assert_ok(&f.request(method, params));
        }
        // Nor may an ignored duplicate retroactively add transport metadata to
        // the actually queued path-only/unbound report with this exact key.
        assert_ok(&f.request(
            method,
            serde_json::json!({
                "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "state": "idle", "seq": 2,
                "agent_session_id": "private-buffered", "agent_session_path": "/tmp/buffered.jsonl",
                "session_start_source": "startup",
            }),
        ));
        let (_, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
        f.app
            .handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
                pane_id,
                agent: Agent::Pi,
                observed_at: Instant::now(),
            });
        for method in [
            "pane.send_text_session_checked",
            "pane.send_keys_session_checked",
            "agent.prompt_session_checked",
        ] {
            assert_error(
                &f.request(
                    method,
                    serde_json::json!({
                        "pane_id": f.pane, "target": f.pane, "text": "bad", "keys": ["enter"],
                        "expected_agent_session_id": "private-buffered",
                    }),
                ),
                "agent_session_unknown",
            );
        }
        f.bytes(b"");
    }
}

#[tokio::test]
async fn expected_agent_session_real_pty_unbound_and_reused_reporter_fail_closed() {
    let mut f = Fixture::new(Agent::Pi);
    for (seq, context) in [
        (1, crate::api::ApiRequestContext::default()),
        (
            2,
            crate::api::ApiRequestContext::capture(Some(crate::platform::ProcessIdentity {
                start_time: f.reporter.start_time.wrapping_add(1),
                ..f.reporter
            })),
        ),
    ] {
        assert_ok(&f.request_with_context(
            "pane.report_agent_session",
            serde_json::json!({
                "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "seq": seq,
                "agent_session_id": "private-session-a", "agent_session_path": "/tmp/owner.jsonl",
                "session_start_source": "new",
            }),
            context,
        ));
        for expected in ["private-session-a", "not-current"] {
            let response = f.request(
                "pane.send_text",
                serde_json::json!({
                    "pane_id": f.pane, "text": "bad", "expected_agent_session_id": expected,
                }),
            );
            assert_error(&response, "agent_session_unknown");
            assert!(response.contains(expected));
            if expected != "private-session-a" {
                assert!(!response.contains("private-session-a"));
            }
        }
        f.bytes(b"");
    }
    f.report(Some("private-session-a"), Some("/tmp/owner.jsonl"), 3);
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "live", "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"live");
}

#[tokio::test]
async fn expected_agent_session_real_pty_lifecycle_report_binds_and_clears_owner() {
    let mut f = Fixture::new(Agent::Claude);
    for (seq, context, expected) in [
        (
            1,
            crate::api::ApiRequestContext::capture(Some(f.reporter)),
            "agent_session_mismatch",
        ),
        (
            2,
            crate::api::ApiRequestContext::default(),
            "agent_session_unknown",
        ),
        (
            3,
            crate::api::ApiRequestContext::capture(Some(f.reporter)),
            "agent_session_mismatch",
        ),
    ] {
        assert_ok(&f.request_with_context(
            "pane.report_agent",
            serde_json::json!({
                "pane_id": f.pane, "source": "herdr:claude", "agent": "claude", "state": "idle",
                "seq": seq, "agent_session_id": "private-lifecycle",
            }),
            context,
        ));
        let response = f.request(
            "pane.send_keys",
            serde_json::json!({
                "pane_id": f.pane, "keys": ["enter"], "expected_agent_session_id": "not-current",
            }),
        );
        assert_error(&response, expected);
        assert!(!response.contains("private-lifecycle"));
    }
    // A stale unbound report must not erase the currently accepted binding.
    assert_ok(&f.request_with_context(
        "pane.report_agent",
        serde_json::json!({
            "pane_id": f.pane, "source": "herdr:claude", "agent": "claude", "state": "idle",
            "seq": 2, "agent_session_id": "private-lifecycle",
        }),
        Default::default(),
    ));
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "hook", "expected_agent_session_id": "private-lifecycle",
        }),
    ));
    f.bytes(b"hook");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_reaped_owner_same_pty_shell_before_exit_event() {
    let mut f = Fixture::with_job_control(Agent::Pi, true);
    f.report(
        Some("private-session-a"),
        Some("/tmp/reaped-owner.jsonl"),
        1,
    );
    f.control("zombie");
    f.control("fallback");
    f.control("reap");
    assert!(crate::platform::process_identity(f.reporter.pid).is_none());
    f.assert_cached_session("private-session-a");
    for method in [
        "pane.send_text",
        "pane.send_keys",
        "agent.prompt_session_checked",
    ] {
        let response = f.request(
            method,
            serde_json::json!({
                "pane_id": f.pane, "target": f.pane, "text": "bad", "keys": ["enter"],
                "expected_agent_session_id": "private-session-a",
            }),
        );
        assert_error(&response, "agent_session_unknown");
    }
    f.bytes(b"");
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({"pane_id": f.pane, "text": "shell"}),
    ));
    f.bytes(b"shell");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_zombie_then_reaped_owner_before_exit_event() {
    let mut f = Fixture::with_job_control(Agent::Pi, true);
    f.report(Some("private-session-a"), Some("/tmp/dead-owner.jsonl"), 1);
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "live", "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"live");
    f.control("zombie");
    // The reporter is a zombie, not merely a stale cached ProcessExited event.
    assert!(crate::platform::process_identity(f.reporter.pid).is_none());
    assert!(std::path::Path::new(&format!("/proc/{}", f.reporter.pid)).exists());
    for phase in ["zombie", "fallback", "reap"] {
        if phase != "zombie" {
            f.control(phase);
        }
        f.assert_cached_session("private-session-a");
        for expected in ["private-session-a", "not-current"] {
            let response = f.request(
                "pane.send_text",
                serde_json::json!({
                    "pane_id": f.pane, "text": "bad", "expected_agent_session_id": expected,
                }),
            );
            assert_error(&response, "agent_session_unknown");
        }
        f.bytes(b"live");
    }
    // The very same PTY now has an ordinary byte sink (standing in for shell).
    // No guard preserves legacy input and proves it could have received bytes.
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({"pane_id": f.pane, "text": "shell"}),
    ));
    f.bytes(b"liveshell");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn expected_agent_session_real_pty_foreground_handoff_and_restore_without_detection() {
    let mut f = Fixture::with_job_control(Agent::Pi, true);
    f.report(
        Some("private-session-a"),
        Some("/tmp/foreground-owner.jsonl"),
        1,
    );
    f.control("background");
    assert_eq!(
        crate::platform::process_identity(f.reporter.pid),
        Some(f.reporter)
    );
    f.assert_cached_session("private-session-a");
    for method in [
        "pane.send_text",
        "pane.send_keys_session_checked",
        "agent.prompt_session_checked",
    ] {
        let response = f.request(
            method,
            serde_json::json!({
                "pane_id": f.pane, "target": f.pane, "text": "bad", "keys": ["enter"],
                "expected_agent_session_id": "private-session-a",
            }),
        );
        assert_error(&response, "agent_session_unknown");
    }
    f.bytes(b"");
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({"pane_id": f.pane, "text": "shell"}),
    ));
    f.bytes(b"shell");
    f.control("foreground");
    f.assert_cached_session("private-session-a");
    assert_ok(&f.request(
        "pane.send_text_session_checked",
        serde_json::json!({
            "pane_id": f.pane, "text": "restored", "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"shellrestored");
}

#[tokio::test]
async fn expected_agent_session_real_pty_match_mismatch_unknown_absent_and_transition() {
    let mut f = Fixture::new(Agent::Pi);
    f.report(Some("private-session-a"), Some("/tmp/pi-a.jsonl"), 1);
    // Failing-first: old server ignores this guard and writes into the real PTY.
    let response = f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "not-current",
        }),
    );
    f.bytes(b"");
    assert_error(&response, "agent_session_mismatch");
    assert!(
        response.contains("not-current"),
        "expected ID must be included"
    );
    assert!(
        !response.contains("private-session-a"),
        "actual ID must not be exposed"
    );
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "one", "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"one");
    assert_ok(&f.request(
        "pane.send_keys",
        serde_json::json!({
            "pane_id": f.pane, "keys": ["enter"], "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"one\r");
    f.report(Some("private-session-b"), Some("/tmp/pi-b.jsonl"), 2);
    let mismatch = f.request(
        "pane.send_keys",
        serde_json::json!({
            "pane_id": f.pane, "keys": ["enter"], "expected_agent_session_id": "private-session-a",
        }),
    );
    assert_error(&mismatch, "agent_session_mismatch");
    assert!(
        mismatch.contains("private-session-a"),
        "caller expectation is included"
    );
    assert!(
        !mismatch.contains("private-session-b"),
        "different actual ID stays private"
    );
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({
            "pane_id": f.pane, "text": "two", "expected_agent_session_id": "private-session-b",
        }),
    ));
    f.report(None, Some("/tmp/pi-path-only.jsonl"), 3);
    let unknown = f.request("pane.send_text", serde_json::json!({
        "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "/tmp/pi-path-only.jsonl",
    }));
    assert_error(&unknown, "agent_session_unknown");
    assert!(
        unknown.contains("/tmp/pi-path-only.jsonl"),
        "unknown error includes caller expectation"
    );
    assert!(
        !unknown.contains("private-session-b"),
        "previous ID stays private"
    );
    assert_ok(&f.request(
        "pane.send_text",
        serde_json::json!({"pane_id": f.pane, "text": "old"}),
    ));
    f.bytes(b"one\rtwoold");
}

#[tokio::test]
async fn expected_agent_session_real_pty_prompt_aliases_require_guards_and_pin_pane() {
    let mut f = Fixture::new(Agent::Pi);
    f.report(
        Some("private-session-a"),
        Some("/tmp/alias-session.jsonl"),
        1,
    );
    for method in ["agent.prompt", "agent.prompt_session_checked"] {
        assert_error(
            &f.request(
                method,
                serde_json::json!({
                    "target": "sink", "text": "bad", "expected_agent_session_id": "not-current",
                }),
            ),
            "agent_session_mismatch",
        );
        assert_error(
            &f.request(
                method,
                serde_json::json!({
                    "target": "sink", "text": "bad", "expected_pane_id": "w999:p999",
                }),
            ),
            "expected_pane_mismatch",
        );
    }
    for wait in [
        serde_json::Value::Null,
        serde_json::json!({"timeout_ms": 100}),
    ] {
        assert_error(
            &f.request(
                "agent.prompt_session_checked",
                serde_json::json!({
                    "target": "sink", "text": "bad", "wait": wait,
                }),
            ),
            "invalid_request",
        );
    }
    for method in [
        "pane.send_text_session_checked",
        "pane.send_keys_session_checked",
    ] {
        assert_error(
            &f.request(
                method,
                serde_json::json!({
                    "pane_id": f.pane, "text": "bad", "keys": ["enter"],
                }),
            ),
            "invalid_request",
        );
    }
    f.bytes(b"");
    assert_ok(&f.request(
        "agent.prompt_session_checked",
        serde_json::json!({
            "target": "sink", "text": "match", "expected_pane_id": f.pane,
            "expected_agent_session_id": "private-session-a",
        }),
    ));
    f.bytes(b"match\r");
    assert_ok(&f.request(
        "agent.prompt_session_checked",
        serde_json::json!({
            "target": "sink", "text": "pane", "expected_pane_id": f.pane,
        }),
    ));
    f.bytes(b"match\rpane\r");
    assert_ok(&f.request(
        "agent.prompt",
        serde_json::json!({"target": "sink", "text": "old"}),
    ));
    f.bytes(b"match\rpane\rold\r");
    f.report(None, Some("/tmp/unknown-session.jsonl"), 2);
    assert_error(
        &f.request(
            "agent.prompt_session_checked",
            serde_json::json!({
                "target": f.pane, "text": "bad", "expected_agent_session_id": "not-current",
            }),
        ),
        "agent_session_unknown",
    );
    f.bytes(b"match\rpane\rold\r");
}

#[tokio::test]
async fn expected_agent_session_real_pty_same_path_stale_rejected_clear_and_discovery() {
    let mut f = Fixture::new(Agent::Pi);
    f.report(
        Some("private-session-a"),
        Some("/tmp/same-session.jsonl"),
        1,
    );
    let discovery = |f: &mut Fixture| {
        let response = f.request("pane.get", serde_json::json!({"pane_id": f.pane}));
        let json: serde_json::Value = serde_json::from_str(&response).expect("pane info");
        assert_eq!(json["result"]["pane"]["agent_session"]["kind"], "path");
        assert_eq!(
            json["result"]["pane"]["agent_session"]["value"],
            "/tmp/same-session.jsonl"
        );
        json["result"]["pane"]["agent_session_id"].clone()
    };
    assert_eq!(discovery(&mut f), "private-session-a");
    f.report(
        Some("private-session-stale"),
        Some("/tmp/same-session.jsonl"),
        1,
    );
    assert_eq!(discovery(&mut f), "private-session-a");
    // A newer but unauthorized different agent report with the SAME preferred path
    // must not exploit the old session_report_applied equality heuristic.
    assert_ok(&f.request("pane.report_agent_session", serde_json::json!({
        "pane_id": f.pane, "source": "herdr:omp", "agent": "omp", "seq": 2,
        "agent_session_id": "private-session-rejected", "agent_session_path": "/tmp/same-session.jsonl",
        "session_start_source": "new",
    })));
    assert_eq!(discovery(&mut f), "private-session-a");
    f.report(
        Some("private-session-b"),
        Some("/tmp/same-session.jsonl"),
        2,
    );
    assert_eq!(discovery(&mut f), "private-session-b");
    assert_error(
        &f.request(
            "pane.send_text_session_checked",
            serde_json::json!({
                "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "private-session-a",
            }),
        ),
        "agent_session_mismatch",
    );
    assert_ok(&f.request(
        "pane.send_text_session_checked",
        serde_json::json!({
            "pane_id": f.pane, "text": "b", "expected_agent_session_id": "private-session-b",
        }),
    ));
    assert_ok(&f.request("pane.report_agent", serde_json::json!({
        "pane_id": f.pane, "source": "herdr:pi", "agent": "pi", "state": "idle", "seq": 3,
        "agent_session_id": "private-session-b", "agent_session_path": "/tmp/same-session.jsonl",
    })));
    assert_ok(&f.request(
        "pane.clear_agent_authority",
        serde_json::json!({
            "pane_id": f.pane, "source": "herdr:pi", "seq": 4,
        }),
    ));
    assert_error(
        &f.request(
            "pane.send_text",
            serde_json::json!({
                "pane_id": f.pane, "text": "bad", "expected_agent_session_id": "private-session-b",
            }),
        ),
        "agent_session_unknown",
    );
    f.bytes(b"b");
    let mut id_only = Fixture::new(Agent::Pi);
    id_only.report(Some("private-session-id-only"), None, 1);
    assert_ok(&id_only.request("pane.send_keys_session_checked", serde_json::json!({
        "pane_id": id_only.pane, "keys": ["enter"], "expected_agent_session_id": "private-session-id-only",
    })));
    id_only.bytes(b"\r");
}

#[tokio::test]
async fn expected_agent_session_real_pty_guard_precedes_copilot_focus_event() {
    let mut f = Fixture::new(Agent::GithubCopilot);
    f.report(Some("private-session-copilot"), None, 1);
    assert_error(
        &f.request(
            "agent.prompt_session_checked",
            serde_json::json!({
                "target": f.pane, "text": "bad", "expected_agent_session_id": "not-current",
            }),
        ),
        "agent_session_mismatch",
    );
    assert_error(
        &f.request(
            "agent.prompt_session_checked",
            serde_json::json!({
                "target": f.pane, "text": "bad", "expected_pane_id": "w999:p999",
            }),
        ),
        "expected_pane_mismatch",
    );
    f.bytes(b"");
    assert_ok(&f.request(
        "agent.prompt_session_checked",
        serde_json::json!({
            "target": f.pane, "text": "yes", "expected_agent_session_id": "private-session-copilot",
        }),
    ));
    f.bytes(b"\x1b[Iyes\r");
    // Official release reports do not override process/detection authority.
    let (_, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
    f.app
        .handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::GithubCopilot),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: Instant::now(),
        });
    assert_error(&f.request("agent.prompt_session_checked", serde_json::json!({
        "target": f.pane, "text": "bad", "expected_agent_session_id": "private-session-copilot",
    })), "agent_session_unknown");
    f.bytes(b"\x1b[Iyes\r");
}

#[tokio::test]
async fn expected_agent_session_real_pty_omp_keeps_path_resume_and_actual_id() {
    let mut f = Fixture::new(Agent::Omp);
    f.report(
        Some("private-session-omp"),
        Some("/tmp/omp-session.jsonl"),
        1,
    );
    let response = f.request("agent.get", serde_json::json!({"target": f.pane}));
    let json: serde_json::Value = serde_json::from_str(&response).expect("agent info");
    assert_eq!(json["result"]["agent"]["agent_session"]["kind"], "path");
    assert_eq!(
        json["result"]["agent"]["agent_session_id"],
        "private-session-omp"
    );
    assert_ok(&f.request(
        "pane.send_text_session_checked",
        serde_json::json!({
            "pane_id": f.pane, "text": "omp", "expected_agent_session_id": "private-session-omp",
        }),
    ));
    assert_error(&f.request("pane.send_keys_session_checked", serde_json::json!({
        "pane_id": f.pane, "keys": ["enter"], "expected_agent_session_id": "/tmp/omp-session.jsonl",
    })), "agent_session_mismatch");
    f.bytes(b"omp");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expected_agent_session_real_pty_wait_route_preserves_alias_and_checks_after_preflight() {
    use interprocess::local_socket::traits::Listener as _;
    use std::io::{BufRead, Write};
    use std::sync::{atomic::AtomicBool, Arc};
    let mut f = Fixture::with_job_control(Agent::Pi, cfg!(target_os = "linux"));
    f.report(
        Some("private-session-a"),
        Some("/tmp/wait-session.jsonl"),
        1,
    );
    for (index, wait, expected, transition) in [
        (0, false, None, false),
        (1, true, None, false),
        (2, true, Some("private-session-a"), true),
        (4, true, Some("private-session-b"), false),
        (3, true, Some("private-session-b"), false),
        (5, true, Some("private-session-b"), false),
    ] {
        if index == 5 && !cfg!(target_os = "linux") {
            continue;
        }
        let path = f.dir.join(format!("api-{index}.sock"));
        let listener = crate::ipc::bind_local_listener(&path).expect("socket");
        let mut client = crate::ipc::connect_local_stream(&path).expect("client");
        let server = listener.accept().expect("server connection");
        let (api_tx, mut api_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::api::ApiRequestMessage>();
        let mut params = serde_json::json!({
            "target": f.pane, "text": "wait", "allow_cross_pane": true,
        });
        if let Some(expected) = expected {
            params["expected_agent_session_id"] = expected.into();
        }
        if index == 4 {
            params["target"] = "sink".into();
            params["expected_pane_id"] = f.pane.clone().into();
        }
        if wait {
            params["wait"] = serde_json::json!({"until": ["working"], "timeout_ms": 500});
        }
        writeln!(
            client,
            "{}",
            serde_json::json!({
                "id": "wait-guard", "method": "agent.prompt_session_checked", "params": params,
            })
        )
        .expect("send request");
        client.flush().expect("flush request");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = crate::api::test_handle_connection(
                server,
                &api_tx,
                &crate::api::EventHub::default(),
                &Arc::new(AtomicBool::new(true)),
                None,
            );
            let _ = done_tx.send(result);
        });
        if expected.is_some() {
            // agent.get is wait preparation, not the atomic effect-time check.
            let get = tokio::time::timeout(Duration::from_secs(2), api_rx.recv())
                .await
                .expect("bounded get dispatch")
                .expect("get message");
            assert!(matches!(
                get.request.method,
                crate::api::schema::Method::AgentGet(_)
            ));
            let response = f.app.handle_api_request(get.request);
            get.respond_to.send(response).expect("preflight response");
            if index == 4 {
                let (_, original_pane) = f.app.parse_pane_id(&f.pane).expect("original pane");
                let original_id = f
                    .app
                    .state
                    .terminal_id_for_pane(0, original_pane)
                    .expect("terminal");
                f.app
                    .state
                    .terminals
                    .get_mut(&original_id)
                    .expect("original state")
                    .clear_agent_name();
                let replacement_pane =
                    f.app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
                f.app.state.ensure_test_terminals();
                let replacement_id = f
                    .app
                    .state
                    .terminal_id_for_pane(0, replacement_pane)
                    .expect("replacement terminal");
                let replacement = f
                    .app
                    .state
                    .terminals
                    .get_mut(&replacement_id)
                    .expect("replacement state");
                replacement.set_detected_state(Some(Agent::Pi), AgentState::Idle);
                replacement.set_agent_name("sink".into());
            } else if transition {
                f.report(
                    Some("private-session-b"),
                    Some("/tmp/wait-session.jsonl"),
                    2,
                );
            } else if index == 5 {
                // A successful wait preflight must not authorize a later write
                // after the pinned report owner exits, even with stale App state.
                f.control("zombie");
                f.control("fallback");
                f.control("reap");
                f.assert_cached_session("private-session-b");
            } else {
                let (_, pane_id) = f.app.parse_pane_id(&f.pane).expect("pane");
                let id = f
                    .app
                    .state
                    .terminal_id_for_pane(0, pane_id)
                    .expect("terminal");
                f.app
                    .state
                    .terminals
                    .get_mut(&id)
                    .expect("state")
                    .set_detected_state(Some(Agent::Pi), AgentState::Working);
            }
            let prompt = tokio::time::timeout(Duration::from_secs(2), api_rx.recv())
                .await
                .expect("bounded prompt dispatch")
                .expect("prompt message");
            assert!(
                matches!(
                    prompt.request.method,
                    crate::api::schema::Method::AgentPromptSessionChecked(_)
                ),
                "wait must preserve checked intent"
            );
            assert!(f.app.handle_deferred_agent_api_request(
                prompt.request,
                prompt.context,
                prompt.respond_to
            ));
        }
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("socket route must not hang")
            .expect("socket response");
        handle.join().expect("server thread");
        let mut line = String::new();
        std::io::BufReader::new(client)
            .read_line(&mut line)
            .expect("response line");
        if expected.is_none() {
            assert_error(&line, "invalid_request");
            assert!(
                api_rx.try_recv().is_err(),
                "malformed aliases must not reach app preparation"
            );
        } else if transition {
            assert_error(&line, "agent_session_mismatch");
        } else if index == 5 {
            assert_error(&line, "agent_session_unknown");
        } else if index == 4 {
            assert_error(&line, "expected_pane_mismatch");
            let (_, original_pane) = f.app.parse_pane_id(&f.pane).expect("original pane");
            let original_id = f
                .app
                .state
                .terminal_id_for_pane(0, original_pane)
                .expect("terminal");
            for terminal in f.app.state.terminals.values_mut() {
                terminal.clear_agent_name();
            }
            f.app
                .state
                .terminals
                .get_mut(&original_id)
                .expect("original state")
                .set_agent_name("sink".into());
        } else {
            assert_ok(&line);
        }
        f.bytes(if index == 3 || index == 5 {
            b"wait\r"
        } else {
            b""
        });
    }
}
