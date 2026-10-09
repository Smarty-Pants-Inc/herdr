//! Native PTY regressions. The runner is deliberately paused by the test at
//! actual write boundaries; only real PTY input reaches the executable sinks.
use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const CONTROLLER: &str = r#"
import os, select, signal, sys, time, tty
capture, directory = sys.argv[1:]
tty.setraw(0)
signal.signal(signal.SIGTTOU, signal.SIG_IGN)
children = set()
def terminate(signum, frame):
    raise SystemExit(0)
signal.signal(signal.SIGTERM, terminate)
def reap(pid):
    deadline = time.monotonic() + 2
    while os.waitpid(pid, os.WNOHANG)[0] == 0:
        if time.monotonic() >= deadline:
            raise TimeoutError('reap sink')
        time.sleep(0.005)
    children.discard(pid)
def sink():
    ready, on_exec = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(ready)
        os.setpgid(0, 0)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        signal.signal(signal.SIGTTIN, signal.SIG_DFL)
        signal.signal(signal.SIGTTOU, signal.SIG_DFL)
        output = os.open(capture, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        os.dup2(output, 1)
        os.close(output)
        os.execv('/bin/cat', ['/bin/cat'])
    children.add(pid)
    os.close(on_exec)
    try:
        if not select.select([ready], [], [], 2)[0] or os.read(ready, 1) != b'':
            raise TimeoutError('exec sink')
    finally:
        os.close(ready)
    return pid
def probe():
    pid = os.fork()
    if pid == 0:
        os.setpgid(0, 0)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        signal.signal(signal.SIGTTIN, signal.SIG_DFL)
        signal.signal(signal.SIGTTOU, signal.SIG_DFL)
        # Do not read until the controller has made this process foreground.
        os.kill(os.getpid(), signal.SIGSTOP)
        os.set_blocking(0, False)
        try:
            data = os.read(0, 4 * 1024 * 1024)
        except BlockingIOError:
            data = b''
        with open(directory + '/probe-bytes', 'wb') as f:
            f.write(data)
        os._exit(0)
    children.add(pid)
    os.waitpid(pid, os.WUNTRACED)
    os.tcsetpgrp(0, pid)
    os.kill(pid, signal.SIGCONT)
    reap(pid)
try:
    reporter = sink()
    os.tcsetpgrp(0, reporter)
    os.kill(reporter, signal.SIGCONT)
    fifo = directory + '/control'
    os.mkfifo(fifo, 0o600)
    control = os.open(fifo, os.O_RDWR | os.O_NONBLOCK)
    with open(directory + '/ready', 'w') as f:
        f.write(str(reporter))
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        if not select.select([control], [], [], 0.1)[0]:
            continue
        action = os.read(control, 4096).decode().strip()
        if action == 'pause':
            os.kill(reporter, signal.SIGSTOP)
            os.waitpid(reporter, os.WUNTRACED)
        elif action in ('dead', 'switch') or action.startswith('dead-drain:'):
            if action != 'switch':
                os.kill(reporter, signal.SIGKILL)
                reap(reporter)
            else:
                os.kill(reporter, signal.SIGSTOP)
                os.waitpid(reporter, os.WUNTRACED)
            fallback = sink()
            os.tcsetpgrp(0, fallback)
            os.kill(fallback, signal.SIGCONT)
            if action.startswith('dead-drain:'):
                # These pre-existing tests let the fallback consume the accepted
                # prefix before loss is observed; do not race the input flush.
                expected = int(action.split(':')[1])
                drained_by = time.monotonic() + 2
                while os.stat(capture).st_size < expected:
                    if time.monotonic() >= drained_by:
                        raise TimeoutError('fallback drain')
                    time.sleep(0.005)
        elif action == 'park':
            # The controller does not read the tty. Change native foreground
            # ownership while preserving every staged byte until loss cleanup.
            os.tcsetpgrp(0, os.getpgrp())
        elif action == 'probe':
            probe()
        elif action == 'stop':
            break
        else:
            raise RuntimeError('unknown control: ' + action)
        with open(directory + '/ack', 'w') as f:
            f.write(action)
finally:
    for pid in list(children):
        try:
            os.kill(pid, signal.SIGKILL)
            reap(pid)
        except (ProcessLookupError, ChildProcessError):
            pass
    with open(directory + '/clean', 'w') as f:
        f.write('reaped')
"#;

struct NativePty {
    directory: PathBuf,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    reporter: crate::platform::ProcessIdentity,
    binding_validity: Arc<AtomicBool>,
}

impl NativePty {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "actor-guard-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).expect("private fixture directory");
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("native PTY");
        let mut command = portable_pty::CommandBuilder::new("python3");
        command.args(["-c", CONTROLLER]);
        command.arg(directory.join("bytes"));
        command.arg(&directory);
        command.env_remove("HERDR_PANE_ID");
        let child = pair.slave.spawn_command(command).expect("controller");
        drop(pair.slave);
        let mut fixture = Self {
            directory,
            master: pair.master,
            child,
            reporter: crate::platform::ProcessIdentity {
                pid: 0,
                start_time: 0,
            },
            binding_validity: Arc::new(AtomicBool::new(true)),
        };
        fixture.wait_for("ready", |value| !value.is_empty());
        let pid = std::fs::read_to_string(fixture.directory.join("ready"))
            .expect("reporter PID")
            .parse()
            .expect("native PID");
        fixture.reporter = crate::platform::process_identity(pid).expect("native generation");
        assert_ne!(pid, std::process::id());
        assert!(crate::platform::session_reporter_is_foreground(
            fixture.reporter,
            || {
                crate::platform::foreground_process_group_id_for_tty_fd(
                    fixture.master.as_raw_fd().expect("master"),
                )
            }
        ));
        fixture
    }

    fn wait_for(&self, name: &str, matches: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if std::fs::read_to_string(self.directory.join(name)).is_ok_and(|value| matches(&value))
            {
                return;
            }
            assert!(Instant::now() < deadline, "native fixture {name}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn control(&self, action: &str) {
        Self::control_at(&self.directory, action);
    }

    fn control_at(directory: &std::path::Path, action: &str) {
        use std::os::unix::fs::OpenOptionsExt;
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(directory.join("control"))
            .expect("control FIFO");
        writeln!(fifo, "{action}").expect("native command");
        if action != "stop" {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !std::fs::read_to_string(directory.join("ack")).is_ok_and(|ack| ack == action) {
                assert!(Instant::now() < deadline, "native control ack: {action}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn guard(&self) -> SessionInputGuard {
        SessionInputGuard {
            reporter: self.reporter,
            expected_agent_session_id: "native-session".into(),
            expected_agent_status: None,
            agent_status: std::sync::Weak::new(),
            status_mismatch: Arc::new(AtomicBool::new(false)),
            binding_validity: Arc::downgrade(&self.binding_validity),
        }
    }

    fn runner(&self) -> PtyIoActorRunner {
        let (mut runner, peer) = super::tests::actor_runner_for_unit_test();
        drop(peer);
        let fd =
            fd::duplicate_cloexec_fd(self.master.as_raw_fd().expect("master")).expect("duplicate");
        use std::os::fd::FromRawFd;
        runner.file = ActorPtyFile::new(unsafe { std::fs::File::from_raw_fd(fd) });
        fd::set_nonblocking(runner.file.as_raw_fd()).expect("nonblocking");
        runner
    }

    fn queue_handle(&self, runner: &mut PtyIoActorRunner) -> PtyIoActorHandle {
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake = fd::create_wake_pipe().expect("wake pipe");
        runner.data_rx = data_rx;
        runner.control_rx = control_rx;
        runner.wake_read_fd = wake.read_fd;
        PtyIoActorHandle {
            input_poisoned: Arc::clone(&runner.input_poisoned),
            data_tx,
            control_tx,
            wake: wake.writer,
            user_writes: Arc::clone(&runner.user_writes),
            controls: Arc::clone(&runner.controls),
            response_order: Arc::clone(&runner.response_order),
            foreground_fd: PtyForegroundObserver(Arc::downgrade(&runner.file.foreground)),
            consumer_epoch: Arc::clone(&runner.consumer_epoch),
        }
    }

    #[cfg(target_os = "linux")]
    fn probe_bytes(&self) -> Vec<u8> {
        self.control("probe");
        std::fs::read(self.directory.join("probe-bytes")).expect("one foreground probe read")
    }

    fn bytes(&self) -> Vec<u8> {
        std::fs::read(self.directory.join("bytes")).expect("real PTY byte capture")
    }

    fn captured(&self, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let bytes = self.bytes();
            if bytes.len() >= expected.len() {
                assert_eq!(bytes, expected, "exact native PTY input");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "sink capture: {} of {}",
                bytes.len(),
                expected.len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // No pending input may materialize after the exact prefix is observed.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(self.bytes(), expected);
    }
}

impl Drop for NativePty {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut fifo) = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(self.directory.join("control"))
        {
            let _ = fifo.write_all(b"stop\n");
        } else if let Some(pid) = self.child.process_id() {
            // Only this fixture's controller, whose SIGTERM handler reaps its sinks.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.child.try_wait().expect("controller status").is_none() {
            assert!(Instant::now() < deadline, "fixture controller cleanup");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            std::fs::read_to_string(self.directory.join("clean")).expect("all sinks reaped"),
            "reaped"
        );
        // Exactly the directory allocated by this fixture; never inherited TMPDIR.
        std::fs::remove_dir_all(&self.directory).expect("remove owned fixture");
    }
}

fn submit(
    runner: &mut PtyIoActorRunner,
    text: Bytes,
    enter: Bytes,
    delay: Duration,
    guard: Option<SessionInputGuard>,
) -> std_mpsc::Receiver<std::io::Result<()>> {
    let (reply, receipt) = std_mpsc::channel();
    assert!(
        !runner.handle_data_command(PtyIoDataCommand::SubmitUserInput {
            source: InputSource::Api,
            text,
            enter,
            delay,
            guard,
            reply,
        })
    );
    receipt
}

fn pump(runner: &mut PtyIoActorRunner) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !runner.pending_writes.is_empty() || runner.active_submission.is_some() {
        if let Some(boundary) = runner.flush_pending_writes_once().expect("native write") {
            runner.complete_submission_boundary(boundary);
        }
        runner.schedule_submission_enter();
        assert!(Instant::now() < deadline, "bounded actor submission");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn lost(receipt: std_mpsc::Receiver<std::io::Result<()>>, partial_text_consumed: bool) {
    let error = receipt
        .recv_timeout(Duration::from_secs(1))
        .expect("completion")
        .expect_err("guard loss returns typed lost");
    assert!(super::super::is_agent_session_lost(&error));
    assert_eq!(
        error.to_string(),
        format!(
            "agent session ownership was lost: native-session; partial_text_consumed={partial_text_consumed}; flush_failed=false"
        )
    );
    assert_eq!(
        super::super::agent_session_loss_partial_text_consumed(&error),
        Some(partial_text_consumed),
        "typed loss must report whether text reached the PTY"
    );
    assert!(!super::super::is_agent_session_lost(&std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "unrelated permission error"
    )));
}

#[test]
fn guarded_native_dead_before_write_drops_focus_text_enter_only() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    runner.enqueue_write(Bytes::from_static(b"before"));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"\x1b[Iprompt"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    // This ordered consumer marker is not a submission-owned write.
    runner.pending_writes.push_back(PendingWrite {
        source: None,
        bytes: Bytes::from_static(b"marker"),
        boundary: Some(SubmissionBoundary::Marker),
    });
    fixture.control("dead"); // deterministic pause: no actor syscall has run yet
    assert!(crate::platform::process_identity(fixture.reporter.pid).is_none());
    pump(&mut runner);
    lost(receipt, false);
    fixture.captured(b"beforeresponsemarker");
    assert_eq!(runner.state, ActorState::Running);
    runner.enqueue_write(Bytes::from_static(b"after"));
    pump(&mut runner);
    fixture.captured(b"beforeresponsemarkerafter");
}

#[test]
fn guarded_native_foreground_switch_during_enter_deadline_never_enters() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let delay = Duration::from_millis(300);
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"prompt"),
        Bytes::from_static(b"\r"),
        delay,
        Some(fixture.guard()),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    fixture.captured(b"prompt");
    let SubmissionPhase::WaitingUntil(deadline) =
        runner.active_submission.as_ref().expect("active").phase
    else {
        panic!("text must be complete with Enter deadline known");
    };
    fixture.control("switch");
    assert!(
        Instant::now() < deadline,
        "foreground switched during the real 300ms Enter delay"
    );
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    pump(&mut runner);
    lost(receipt, true);
    fixture.captured(b"promptresponse");
}

#[test]
fn guarded_native_live_prompt_and_legacy_submission_exactly_once() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let local_offsets = Arc::new(Mutex::new(Vec::new()));
    let local = Arc::clone(&local_offsets);
    runner.file.before_write = Some(Box::new(move |offset| {
        local.lock().expect("local hook").push(offset);
    }));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let installed = Arc::clone(&attempts);
    let (reply, acknowledged) = std_mpsc::channel();
    assert!(
        !runner.handle_control_command(PtyIoControlCommand::SetBeforeWrite {
            callback: Box::new(move |number| {
                installed.lock().expect("installed hook").push(number);
            }),
            reply,
        })
    );
    acknowledged
        .recv_timeout(Duration::from_secs(1))
        .expect("hook install ACK");
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"\x1b[Iprompt"),
        Bytes::from_static(b"\r"),
        Duration::from_millis(20),
        Some(fixture.guard()),
    );
    pump(&mut runner);
    receipt
        .recv_timeout(Duration::from_secs(1))
        .expect("guarded completion")
        .expect("live guard");
    fixture.captured(b"\x1b[Iprompt\r");
    let handle = fixture.queue_handle(&mut runner);
    let pane = handle
        .queue_guarded_user_input_submission_with_source(
            Bytes::from_static(b"pane"),
            Bytes::new(),
            Duration::ZERO,
            fixture.guard(),
            InputSource::Api,
        )
        .expect("live text-only queue");
    assert!(!runner.drain_data_commands());
    pump(&mut runner);
    pane.recv_timeout(Duration::from_secs(1))
        .expect("pane completion")
        .expect("live pane guard");
    fixture.captured(b"\x1b[Iprompt\rpane");
    let text_only = submit(
        &mut runner,
        Bytes::from_static(b"done"),
        Bytes::new(),
        Duration::from_millis(50),
        Some(fixture.guard()),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text-only write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    fixture.captured(b"\x1b[Iprompt\rpanedone");
    fixture.control("dead");
    pump(&mut runner);
    text_only
        .recv_timeout(Duration::from_secs(1))
        .expect("text-only completion")
        .expect("fully written text with no Enter succeeds even after later loss");
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"legacy"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        None,
    );
    pump(&mut runner);
    receipt
        .recv_timeout(Duration::from_secs(1))
        .expect("legacy completion")
        .expect("legacy unaffected");
    fixture.captured(b"\x1b[Iprompt\rpanedonelegacy\r");
    assert_eq!(*attempts.lock().expect("attempts"), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        *local_offsets.lock().expect("local offsets"),
        vec![0; 6],
        "installed barrier must preserve the runner-local hook"
    );
}

#[test]
fn guarded_native_partial_wouldblock_retry_discards_only_remaining_chunk() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause"); // no sink consumption: force actual native EAGAIN
    let length = 4 * 1024 * 1024;
    let receipt = submit(
        &mut runner,
        Bytes::from(vec![b'x'; length]),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    assert_eq!(
        runner.flush_pending_writes_once().expect("partial write"),
        None
    );
    let accepted = runner.current_write_offset;
    assert!(
        accepted > 0 && accepted < length,
        "native partial write followed by WouldBlock"
    );
    assert_eq!(runner.pending_writes.len(), 1);
    fixture.control(&format!("dead-drain:{accepted}")); // fallback consumes the accepted prefix
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    pump(&mut runner); // retry must re-prove before writing ANY remaining byte
    lost(receipt, true);
    let mut expected = vec![b'x'; accepted];
    expected.extend_from_slice(b"response");
    fixture.captured(&expected);
    assert_eq!(runner.current_write_offset, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_native_flush_failure_poison_blocks_all_input_until_operator_clear() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let handle = fixture.queue_handle(&mut runner);
    fixture.control("pause");
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"staged"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("boundary");
    runner.complete_submission_boundary(boundary);
    runner.file.force_flush_failure = true;
    fixture.binding_validity.store(false, Ordering::Release);
    pump(&mut runner);
    let loss = receipt.recv().expect("completion").expect_err("flush loss");
    assert_eq!(
        super::super::agent_session_loss_details(&loss),
        Some((None, true))
    );
    assert!(handle.input_is_poisoned());
    assert!(super::super::is_pane_input_poisoned(
        &runner
            .begin_handoff()
            .expect_err("handoff must not clear poison")
    ));
    assert_eq!(
        crate::platform::process_identity(fixture.reporter.pid),
        Some(fixture.reporter)
    );
    assert!(
        runner.file.as_raw_fd() >= 0,
        "poison must not close the PTY"
    );
    let api = handle
        .queue_user_input_submission(
            Bytes::from_static(b"api"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        )
        .expect_err("API refused");
    assert!(super::super::is_pane_input_poisoned(&api));
    assert!(
        handle
            .try_write_user_input_with_source(
                Bytes::from_static(b"\r"),
                InputSource::Client {
                    connection_id: 42,
                    principal: None
                }
            )
            .is_err(),
        "client byte entry uses same poisoned gate"
    );
    runner.enqueue_write(Bytes::from_static(b"queued-unguarded"));
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    runner
        .flush_pending_writes_once()
        .expect("poison refuses every write");
    assert!(runner.pending_writes.is_empty());
    // Even a later successful flush must not clear poison.
    runner.file.force_flush_failure = false;
    crate::platform::flush_pty_input(runner.file.as_raw_fd())
        .expect("operator inspected/discarded staged input");
    assert!(handle.input_is_poisoned());
    let (reply, cleared) = std_mpsc::channel();
    runner.handle_control_command(PtyIoControlCommand::ClearInputPoison(reply));
    cleared
        .recv()
        .expect("clear receipt")
        .expect("operator clear");
    assert!(!handle.input_is_poisoned());
    handle
        .try_write_user_input(Bytes::from_static(b"healthy"))
        .expect("explicit clear restores input");
    runner.drain_data_commands();
    pump(&mut runner);
    assert_eq!(fixture.probe_bytes(), b"healthy");
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_native_loss_after_partial_write_discards_staged_bytes_before_fallback_read() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause"); // retain all input on the slave, without a reader
    let length = 4 * 1024 * 1024;
    let receipt = submit(
        &mut runner,
        Bytes::from(vec![b'x'; length]),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    assert_eq!(
        runner.flush_pending_writes_once().expect("partial write"),
        None
    );
    let accepted = runner.current_write_offset;
    assert!(accepted > 0 && accepted < length, "real accepted prefix");
    assert_eq!(
        runner
            .file
            .write(b"x")
            .expect_err("real native EAGAIN")
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(runner.pending_writes.len(), 1);
    fixture.captured(b"");

    // Move foreground ownership to the non-reading controller before the actor
    // observes loss. Only after cleanup admit a new foreground reader.
    fixture.control("park");
    pump(&mut runner);
    lost(receipt, true);
    assert!(runner.pending_writes.is_empty());
    assert!(runner.active_submission.is_none());
    assert_eq!(runner.current_write_offset, 0);
    let bytes = fixture.probe_bytes();
    assert!(!bytes.contains(&b'\r'), "Enter must never reach the probe");
    assert!(
        bytes.is_empty(),
        "staged prefix reached the probe: {} bytes",
        bytes.len()
    );
    fixture.captured(b"");
}

#[cfg(target_os = "linux")]
#[test]
fn guarded_native_loss_before_enter_discards_staged_text_before_fallback_read() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause"); // stop consumption BEFORE delivering any text
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"staged prompt"),
        Bytes::from_static(b"\r"),
        Duration::from_millis(300),
        Some(fixture.guard()),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("staged text write")
        .expect("text boundary");
    assert_eq!(boundary, SubmissionBoundary::Text);
    runner.complete_submission_boundary(boundary);
    assert!(matches!(
        runner.active_submission.as_ref().expect("active").phase,
        SubmissionPhase::WaitingUntil(_)
    ));
    fixture.captured(b"");
    fixture.control("park");
    pump(&mut runner); // reject Enter and flush while the replacement is not reading
    lost(receipt, true);
    assert!(runner.pending_writes.is_empty());
    assert!(runner.active_submission.is_none());
    let bytes = fixture.probe_bytes();
    assert!(!bytes.contains(&b'\r'), "Enter must never reach the probe");
    assert!(
        bytes.is_empty(),
        "staged text reached the probe: {} bytes",
        bytes.len()
    );
    fixture.captured(b"");
}

#[test]
fn guarded_native_rechecks_between_partial_syscalls_in_one_flush() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause"); // retain every accepted byte in the kernel for the fallback
    let directory = fixture.directory.clone();
    let accepted = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&accepted);
    let mut attempts = 0;
    runner.file.before_write = Some(Box::new(move |offset| {
        attempts += 1;
        if attempts == 2 {
            observed.store(offset, Ordering::Relaxed);
            NativePty::control_at(&directory, &format!("dead-drain:{offset}"));
        }
    }));
    let length = 4 * 1024 * 1024;
    let receipt = submit(
        &mut runner,
        Bytes::from(vec![b'x'; length]),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    // First syscall writes a real partial chunk; the next attempt kills and
    // reaps the reporter BEFORE re-proof, within the very same flush call.
    assert_eq!(
        runner.flush_pending_writes_once().expect("single flush"),
        None
    );
    lost(receipt, true);
    let accepted = accepted.load(Ordering::Relaxed);
    assert!(accepted > 0 && accepted < length);
    fixture.captured(&vec![b'x'; accepted]);
    assert!(runner.pending_writes.is_empty());
}

#[test]
fn guarded_native_queue_completion_text_only_fifo_and_handoff_survive_loss() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let handle = fixture.queue_handle(&mut runner);
    handle
        .try_write_user_input(Bytes::from_static(b"before"))
        .expect("ordinary input");
    let guarded = handle
        .queue_guarded_user_input_submission_with_source(
            Bytes::from_static(b"guarded"),
            Bytes::new(),
            Duration::ZERO,
            fixture.guard(),
            InputSource::Api,
        )
        .expect("guarded text-only queue");
    handle
        .try_write_user_input(Bytes::from_static(b"after"))
        .expect("later ordinary input");
    let legacy = handle
        .queue_user_input_submission(
            Bytes::from_static(b"legacy"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        )
        .expect("later submission");
    fixture.control("dead");
    let (reply, handoff) = std_mpsc::channel();
    runner.defer_or_begin_handoff(reply); // accepted work is drained, not forgotten
    assert!(runner.pending_handoff.is_some());
    pump(&mut runner);
    lost(guarded, false);
    assert!(!runner.drain_commands()); // starts the later legacy submission
    pump(&mut runner);
    legacy
        .recv_timeout(Duration::from_secs(1))
        .expect("legacy completion")
        .expect("legacy survives");
    assert!(!runner.drain_commands()); // finishes deferred handoff with ordinary FIFO intact
    handoff
        .recv_timeout(Duration::from_secs(1))
        .expect("handoff completion")
        .expect("handoff succeeds");
    fixture.captured(b"beforeafterlegacy\r");
    assert_eq!(runner.state, ActorState::Quiesced);
}

#[test]
fn guarded_native_false_and_expired_binding_drop_input_with_live_reporter() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.binding_validity.store(false, Ordering::Release);
    let false_guard = fixture.guard();
    let mut expired_guard = fixture.guard();
    expired_guard.binding_validity = Arc::downgrade(&Arc::new(AtomicBool::new(true)));
    for guard in [false_guard, expired_guard] {
        let receipt = submit(
            &mut runner,
            // Leave a reserved-introducer partial match in the sanitizer.
            // It holds no bytes: this cancelled prefix must never reappear
            // when unrelated input follows.
            Bytes::from_static(b"forbidden\x1b_he"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
            Some(guard),
        );
        pump(&mut runner);
        lost(receipt, false);
        fixture.captured(b"");
    }
    assert!(
        crate::platform::session_reporter_is_foreground(fixture.reporter, || {
            crate::platform::foreground_process_group_id_for_tty_fd(runner.file.as_raw_fd())
        }),
        "binding loss must be independent of the still-live native owner"
    );
    runner.enqueue_write(Bytes::from_static(b"legacy"));
    pump(&mut runner);
    fixture.captured(b"legacy");
}

#[test]
fn guarded_native_rechecks_binding_after_proof_without_retaining_root() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let root = Arc::new(AtomicBool::new(true));
    let weak = Arc::downgrade(&root);
    let mut guard = fixture.guard();
    guard.binding_validity = weak.clone();
    let mut sole_root = Some(root);
    runner.file.during_guard_proof = Some(Box::new(move || {
        drop(sole_root.take());
    }));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"forbidden"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    pump(&mut runner);
    lost(receipt, false);
    assert!(
        weak.upgrade().is_none(),
        "no strong binding root may cross native proof"
    );
    fixture.captured(b"");

    // A still-owned binding root must also be reloaded AFTER native proof:
    // existence alone cannot turn an explicitly invalidated cell back valid.
    let root = Arc::clone(&fixture.binding_validity);
    runner.file.during_guard_proof = Some(Box::new(move || {
        root.store(false, Ordering::Release);
    }));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"forbidden"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(fixture.guard()),
    );
    pump(&mut runner);
    lost(receipt, false);
    fixture.captured(b"");
}

#[test]
fn guarded_native_binding_invalidated_during_enter_delay_never_enters() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"prompt"),
        Bytes::from_static(b"\r"),
        Duration::from_millis(300),
        Some(fixture.guard()),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    fixture.captured(b"prompt");
    fixture.binding_validity.store(false, Ordering::Release);
    pump(&mut runner);
    lost(receipt, true);
    fixture.captured(b"prompt");
}

fn status_guard(fixture: &NativePty) -> (SessionInputGuard, crate::terminal::TerminalState) {
    let mut terminal =
        crate::terminal::TerminalState::new(crate::terminal::TerminalId::alloc(), "/tmp".into());
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Idle,
    );
    let mut guard = fixture.guard();
    guard.expected_agent_status = Some(crate::api::schema::AgentStatus::Idle);
    guard.agent_status = terminal.agent_status_cell();
    (guard, terminal)
}

fn status_mismatch(
    receipt: std_mpsc::Receiver<std::io::Result<()>>,
    partial_text_consumed: Option<bool>,
    flush_failed: bool,
) {
    let error = receipt
        .recv_timeout(Duration::from_secs(1))
        .expect("completion")
        .expect_err("status guard refuses changed status");
    assert!(
        !super::super::is_agent_session_lost(&error),
        "status refusal must not be reported as session loss: {error}"
    );
    assert!(super::super::is_expected_status_mismatch(&error));
    assert!(error.to_string().contains("detected agent status"));
    assert_eq!(
        super::super::agent_session_loss_details(&error),
        Some((partial_text_consumed, flush_failed)),
        "status refusal must preserve the shared flush metadata"
    );
}

#[test]
fn guarded_native_status_latch_preserves_error_type_after_change_back() {
    let fixture = NativePty::new();
    let (guard, mut terminal) = status_guard(&fixture);
    let clone = guard.clone();
    assert!(guard.status_is_current());
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Working,
    );
    assert!(!guard.status_is_current());
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Idle,
    );
    assert!(
        !clone.status_is_current(),
        "refusal is sticky across clones"
    );
    for partial in [false, true] {
        let error = super::super::agent_session_lost(&clone, partial);
        assert!(super::super::is_expected_status_mismatch(&error));
        assert!(!super::super::is_agent_session_lost(&error));
        assert_eq!(
            super::super::agent_session_loss_details(&error),
            Some((Some(partial), false))
        );
    }
    let error = super::super::agent_session_flush_failed(&clone);
    assert!(super::super::is_expected_status_mismatch(&error));
    assert!(!super::super::is_agent_session_lost(&error));
    assert_eq!(
        super::super::agent_session_loss_details(&error),
        Some((None, true))
    );
    let (matching, _terminal) = status_guard(&fixture);
    for partial in [false, true] {
        let error = super::super::agent_session_lost(&matching, partial);
        assert!(super::super::is_agent_session_lost(&error));
        assert!(!super::super::is_expected_status_mismatch(&error));
        assert_eq!(
            super::super::agent_session_loss_details(&error),
            Some((Some(partial), false))
        );
    }
    let error = super::super::agent_session_flush_failed(&matching);
    assert!(super::super::is_agent_session_lost(&error));
    assert!(!super::super::is_expected_status_mismatch(&error));
    assert_eq!(
        super::super::agent_session_loss_details(&error),
        Some((None, true))
    );
    let generic = std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "expected_status_mismatch",
    );
    assert!(!super::super::is_expected_status_mismatch(&generic));
    assert!(!super::super::is_agent_session_lost(&generic));
    assert_eq!(super::super::agent_session_loss_details(&generic), None);
}

#[test]
fn guarded_native_matching_status_delivers_once_and_omitted_status_is_unchanged() {
    use crate::api::schema::AgentStatus;
    use crate::detect::{Agent, AgentState};
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let (guard, mut terminal) = status_guard(&fixture);
    let mut expected = Vec::new();
    for (state, status, text) in [
        (AgentState::Idle, AgentStatus::Idle, "idle"),
        (AgentState::Working, AgentStatus::Working, "working"),
        (AgentState::Blocked, AgentStatus::Blocked, "blocked"),
        (AgentState::Unknown, AgentStatus::Unknown, "unknown"),
    ] {
        terminal.set_detected_state(Some(Agent::Pi), state);
        let mut guard = guard.clone();
        guard.expected_agent_status = Some(status);
        let receipt = submit(
            &mut runner,
            Bytes::from(text),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
            Some(guard),
        );
        pump(&mut runner);
        receipt
            .recv_timeout(Duration::from_secs(1))
            .expect("completion")
            .expect("matching status");
        expected.extend_from_slice(text.as_bytes());
        expected.push(b'\r');
        fixture.captured(&expected);
    }
    let mut guard = guard;
    guard.expected_agent_status = None;
    drop(terminal); // A status-less guard must not require a live status cell.
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"legacy"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    pump(&mut runner);
    receipt
        .recv_timeout(Duration::from_secs(1))
        .expect("completion")
        .expect("session-only unchanged");
    expected.extend_from_slice(b"legacy\r");
    fixture.captured(&expected);
}

#[test]
fn guarded_native_status_flip_before_write_preserves_unrelated_fifo() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let (guard, mut terminal) = status_guard(&fixture);
    runner.enqueue_write(Bytes::from_static(b"before"));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"\x1b[Iforbidden\x1b_he"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    runner.pending_writes.push_back(PendingWrite {
        source: None,
        bytes: Bytes::from_static(b"marker"),
        boundary: Some(SubmissionBoundary::Marker),
    });
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Working,
    );
    pump(&mut runner);
    status_mismatch(receipt, Some(false), false);
    fixture.captured(b"beforeresponsemarker");
    assert_eq!(runner.state, ActorState::Running);
    runner.enqueue_write(Bytes::from_static(b"after"));
    pump(&mut runner);
    fixture.captured(b"beforeresponsemarkerafter");
    assert!(fixture.binding_validity.load(Ordering::Acquire));
}

#[test]
fn guarded_native_status_rechecked_after_proof_without_retaining_root() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let (guard, terminal) = status_guard(&fixture);
    let terminal = Arc::new(Mutex::new(terminal));
    let during_proof = Arc::clone(&terminal);
    runner.file.during_guard_proof = Some(Box::new(move || {
        during_proof.lock().expect("terminal").set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Blocked,
        );
    }));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"forbidden"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    pump(&mut runner);
    status_mismatch(receipt, Some(false), false);
    fixture.captured(b"");

    let (guard, terminal) = status_guard(&fixture);
    let weak = guard.agent_status.clone();
    let mut sole_root = Some(terminal);
    runner.file.during_guard_proof = Some(Box::new(move || {
        drop(sole_root.take());
    }));
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"forbidden"),
        Bytes::new(),
        Duration::ZERO,
        Some(guard),
    );
    pump(&mut runner);
    status_mismatch(receipt, Some(false), false);
    assert!(
        weak.upgrade().is_none(),
        "no status root crosses native proof"
    );
    fixture.captured(b"");
}

#[test]
fn guarded_native_status_flip_during_enter_delay_never_enters() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let (guard, mut terminal) = status_guard(&fixture);
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"prompt"),
        Bytes::from_static(b"\r"),
        Duration::from_millis(300),
        Some(guard),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    fixture.captured(b"prompt");
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Working,
    );
    pump(&mut runner);
    status_mismatch(receipt, Some(true), false);
    fixture.captured(b"prompt");
}

#[test]
fn guarded_native_status_flip_before_enter_discards_staged_text() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause");
    let (guard, mut terminal) = status_guard(&fixture);
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"staged prompt"),
        Bytes::from_static(b"\r"),
        Duration::from_millis(300),
        Some(guard),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    fixture.captured(b"");
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Working,
    );
    pump(&mut runner);
    status_mismatch(receipt, Some(true), false);
    assert!(runner.pending_writes.is_empty());
    assert!(runner.active_submission.is_none());
    assert!(
        fixture.probe_bytes().is_empty(),
        "no staged text or Enter remains"
    );
    fixture.captured(b"");
}

#[test]
fn guarded_native_status_flush_failure_stays_typed_and_poisons_input() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    let handle = fixture.queue_handle(&mut runner);
    fixture.control("pause");
    let (guard, mut terminal) = status_guard(&fixture);
    let receipt = submit(
        &mut runner,
        Bytes::from_static(b"staged"),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    let boundary = runner
        .flush_pending_writes_once()
        .expect("text write")
        .expect("text boundary");
    runner.complete_submission_boundary(boundary);
    runner.file.force_flush_failure = true;
    terminal.set_detected_state(
        Some(crate::detect::Agent::Pi),
        crate::detect::AgentState::Working,
    );
    pump(&mut runner);
    status_mismatch(receipt, None, true);
    assert!(handle.input_is_poisoned());
    assert!(runner.pending_writes.is_empty());
    assert!(runner.active_submission.is_none());
    let refused = handle
        .queue_user_input_submission(
            Bytes::from_static(b"forbidden"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        )
        .expect_err("poison blocks later submissions");
    assert!(super::super::is_pane_input_poisoned(&refused));
    assert!(handle
        .try_write_user_input(Bytes::from_static(b"\r"))
        .is_err());
    runner.enqueue_terminal_responses(vec![Bytes::from_static(b"response")]);
    runner
        .flush_pending_writes_once()
        .expect("poison refuses writes");
    assert!(runner.pending_writes.is_empty());
    runner.file.force_flush_failure = false;
    crate::platform::flush_pty_input(runner.file.as_raw_fd()).expect("operator discards text");
    assert!(
        handle.input_is_poisoned(),
        "successful flush alone never clears poison"
    );
    let (reply, cleared) = std_mpsc::channel();
    runner.handle_control_command(PtyIoControlCommand::ClearInputPoison(reply));
    cleared
        .recv()
        .expect("clear receipt")
        .expect("operator clear");
    assert!(!handle.input_is_poisoned());
    handle
        .try_write_user_input(Bytes::from_static(b"healthy"))
        .expect("explicit clear restores input");
    runner.drain_data_commands();
    pump(&mut runner);
    assert_eq!(fixture.probe_bytes(), b"healthy");
}

#[test]
fn guarded_native_status_rechecked_between_partial_syscalls() {
    let fixture = NativePty::new();
    let mut runner = fixture.runner();
    fixture.control("pause");
    let (guard, terminal) = status_guard(&fixture);
    let terminal = Arc::new(Mutex::new(terminal));
    let during_write = Arc::clone(&terminal);
    let accepted = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&accepted);
    let mut attempts = 0;
    runner.file.before_write = Some(Box::new(move |offset| {
        attempts += 1;
        if attempts == 2 {
            observed.store(offset, Ordering::Relaxed);
            during_write.lock().expect("terminal").set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Working,
            );
        }
    }));
    let length = 4 * 1024 * 1024;
    let receipt = submit(
        &mut runner,
        Bytes::from(vec![b'x'; length]),
        Bytes::from_static(b"\r"),
        Duration::ZERO,
        Some(guard),
    );
    assert_eq!(
        runner.flush_pending_writes_once().expect("single flush"),
        None
    );
    status_mismatch(receipt, Some(true), false);
    let accepted = accepted.load(Ordering::Relaxed);
    assert!(accepted > 0 && accepted < length);
    assert!(runner.pending_writes.is_empty());
    assert!(runner.active_submission.is_none());
    assert_eq!(runner.current_write_offset, 0);
    assert!(crate::platform::session_reporter_is_foreground(
        fixture.reporter,
        || crate::platform::foreground_process_group_id_for_tty_fd(runner.file.as_raw_fd())
    ));
    assert!(
        fixture.probe_bytes().is_empty(),
        "TCIFLUSH discards staged text"
    );
    fixture.captured(b"");
}

#[test]
fn guarded_native_stale_generation_and_wrong_tty_are_not_admitted() {
    let fixture = NativePty::new();
    let other = NativePty::new();
    let mut runner = fixture.runner();
    let mut stale = fixture.guard();
    stale.reporter.start_time = stale.reporter.start_time.wrapping_add(1);
    for guard in [stale, other.guard()] {
        let receipt = submit(
            &mut runner,
            Bytes::from_static(b"forbidden"),
            Bytes::new(),
            Duration::ZERO,
            Some(guard),
        );
        pump(&mut runner);
        lost(receipt, false);
    }
    fixture.captured(b"");
}
