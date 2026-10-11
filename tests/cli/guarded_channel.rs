//! Registered-channel transport against the real server, real pane PTYs and an
//! independently running socket peer. This deliberately does not impersonate Pi
//! admission: the tiny receiver records each delivery and supplies typed receipts.
//! It never reports an agent name/state or uses screen/detection authority.
use super::harness::*;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;

const DEADLINE: Duration = Duration::from_secs(10);
const SESSION: &str = "guarded-channel";
const GENERATION: &str = "test-session-1";

// The launcher puts the *real pane's* stdin in raw mode before creating the
// registrant. On exit it reaps that exact child, then invokes a real /bin/sh
// successor running the byte recorder. The launcher remains alive throughout.
// After registration no flush/reset occurs: partial text, newline/Enter and escape bytes
// left behind by a broken PTY fallback all survive into shell.bytes. The
// recorder's control barrier drains stdin before replying, including on a PTY
// with no newline. A positive control tests this instrumentation separately.
const PROCESS: &str = r#"
import json, os, pathlib, select, socket, subprocess, sys, threading, time, tty

role = sys.argv[1]
directory = pathlib.Path(sys.argv[2])
directory.mkdir(parents=True, exist_ok=True)
script = str(pathlib.Path(__file__).resolve())

def publish(name, value):
    path = directory / name
    staged = path.with_name(path.name + '.tmp')
    staged.write_text(json.dumps(value))
    staged.replace(path)

def append(name, value):
    with (directory / name).open('a') as f:
        f.write(json.dumps(value) + '\n')
        f.flush()

def listen(name):
    path = directory / name
    path.unlink(missing_ok=True)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(str(path))
    s.listen(4)
    return s

def read_json(s):
    data = bytearray()
    while not data.endswith(b'\n'):
        part = s.recv(1)
        if not part:
            raise RuntimeError('EOF before control/registration response')
        data.extend(part)
        if len(data) > 1024 * 1024:
            raise RuntimeError('oversized response')
    return json.loads(data)

def send_json(s, value):
    s.sendall((json.dumps(value) + '\n').encode())

def register(claim):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(os.environ['CHANNEL_TEST_SOCKET'])
    params = {'session_generation': 'test-session-1'}
    if os.environ.get('CHANNEL_TEST_MODE') != 'old_extension':
        params['draft_guard'] = True
    if claim:
        params['pane_id'] = os.environ['HERDR_PANE_ID'] if claim == 'self' else claim
    send_json(s, {'id': 'real-process-register', 'method': 'agent.register_self', 'params': params})
    response = read_json(s)
    s.settimeout(None)
    return s, response

if role == 'probe':
    s, response = register(sys.argv[3])
    print(json.dumps(response), flush=True)
    s.close()
    sys.exit(0)

if role == 'launcher':
    tty.setraw(0)
    publish('launcher.json', {'pid': os.getpid(), 'pgid': os.getpgrp()})
    number = 0
    mode = os.environ.get('CHANNEL_TEST_MODE', 'accepted')
    claim = os.environ.get('CHANNEL_TEST_CLAIM', '')
    while True:
        number += 1
        if mode != 'no_channel':
            child = subprocess.Popen([sys.executable, script, 'agent', str(directory), str(number), mode, claim])
            result = child.wait()
            publish('exited-%s.json' % number, {'pid': child.pid, 'status': result})
        # Actual shell successor. It inherits raw, undrained stdin; no message
        # is evaluated as shell source, even when the guarded text is shell-like.
        command = 'exec "$1" "$2" recorder "$3" "$4"'
        child = subprocess.Popen(['/bin/sh', '-c', command, 'recorder-shell', sys.executable, script, str(directory), str(number)])
        result = child.wait()
        if result != 42:
            sys.exit(result)
        restart = json.loads((directory / 'restart.json').read_text())
        mode, claim = restart['mode'], 'self'

if role == 'recorder':
    number = int(sys.argv[3])
    control = listen('recorder.sock')
    log = (directory / 'shell.bytes').open('ab', buffering=0)
    def drain():
        while select.select([0], [], [], 0)[0]:
            data = os.read(0, 65536)
            if not data:
                raise RuntimeError('pane stdin closed')
            log.write(data)
    publish('recorder-%s.json' % number, {'pid': os.getpid()})
    while True:
        ready, _, _ = select.select([0, control], [], [])
        if 0 in ready:
            drain()
        if control in ready:
            peer, _ = control.accept()
            with peer:
                command = read_json(peer)
                drain()
                send_json(peer, {'bytes': (directory / 'shell.bytes').read_bytes().hex()})
                if command['op'] == 'restart':
                    publish('restart.json', {'mode': command['mode']})
                    control.close()
                    sys.exit(42)
                if command['op'] == 'stop':
                    sys.exit(0)

if role != 'agent':
    raise RuntimeError('unknown role')

number, mode, claim = int(sys.argv[3]), sys.argv[4], sys.argv[5]
control = listen('agent.sock')
channel, response = register(claim)
publish('registration-%s.json' % number, dict(response, peer_pid=os.getpid(), pgid=os.getpgrp()))
if 'error' in response:
    channel.close()
    sys.exit(0)
epoch = response['result']['registration_epoch']
buffer = bytearray()
paused = False

while True:
    watched = [control]
    if channel is not None and not paused:
        watched.append(channel)
    ready, _, _ = select.select(watched, [], [])
    if control in ready:
        peer, _ = control.accept()
        with peer:
            command = read_json(peer)
            op = command['op']
            result = {'ok': True}
            if op == 'mode':
                mode = command['mode']
            elif op in ('disconnect', 'reconnect'):
                if channel is not None:
                    channel.close()
                channel = None
                buffer.clear()
                paused = False
                if op == 'reconnect':
                    channel, result = register('')
                    if 'error' not in result:
                        epoch = result['result']['registration_epoch']
            elif op == 'reregister':
                # Keep the old connection alive until the same-peer replacement
                # is installed; this differs from disconnect/reconnect.
                old = channel
                channel, result = register('')
                if 'error' not in result:
                    epoch = result['result']['registration_epoch']
                if old is not None:
                    old.close()
                buffer.clear()
                paused = False
            elif op == 'query':
                # The real CLI as a child of this registered agent: actual kernel
                # caller attribution, never a supplied pane ID. A thread keeps this
                # loop serving our own channel so a same-pane query can complete.
                def run_query(argv, name):
                    env = dict(os.environ, HERDR_SOCKET_PATH=os.environ['CHANNEL_TEST_SOCKET'])
                    done = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=10)
                    publish(name, {'status': done.returncode, 'stdout': done.stdout, 'stderr': done.stderr})
                threading.Thread(target=run_query, args=(command['argv'], command['name']), daemon=True).start()
            elif op == 'peer':
                # A second real process, still in this pane's foreground group,
                # must not replace the live registrant (even after disconnect).
                output = subprocess.check_output([sys.executable, script, 'probe', str(directory), 'self'])
                result = json.loads(output)
            elif op != 'stop':
                raise RuntimeError('unknown agent control')
            send_json(peer, result)
            if op == 'stop':
                if channel is not None:
                    channel.close()
                sys.exit(0)
        # Reconnect/stop can change the descriptor selected above.
        continue
    if channel in ready:
        data = channel.recv(1 if mode == 'prefix' else 65536)
        if not data:
            channel.close()
            channel = None
            publish('disconnected-%s.json' % number, True)
            continue
        buffer.extend(data)
        if mode == 'prefix':
            # Hold an incomplete receiver frame. We do NOT claim this proves a
            # partial kernel write; that finer boundary needs a core test hook.
            publish('prefix-%s.json' % number, {'bytes': len(buffer)})
            paused = True
            continue
        while b'\n' in buffer:
            line, _, rest = buffer.partition(b'\n')
            buffer = bytearray(rest)
            frame = json.loads(line)
            assert frame['registration_epoch'] == epoch, frame
            assert frame['session_generation'] == 'test-session-1', frame
            if frame['type'] == 'draft_state':
                # Deliberately read-only: no admission log, editor data or prompt
                # ingress. The fake peer exercises real server projection/parser.
                append('queries.jsonl', frame)
                reply = {key: frame[key] for key in ('registration_epoch', 'request_id', 'session_generation')}
                reply.update(type='draft_state', empty=True, hold=None)
                if mode == 'query_timeout':
                    continue
                if mode == 'query_unknown':
                    reply = {key: reply[key] for key in ('type', 'registration_epoch', 'request_id', 'session_generation')}
                    reply['unknown'] = True
                elif mode == 'query_nonempty':
                    reply.update(empty=False)
                elif mode in ('query_dialog', 'query_custom', 'query_editor'):
                    reply['hold'] = mode.removeprefix('query_')
                elif mode == 'query_text':
                    reply['text'] = 'private draft must never escape API'
                elif mode == 'query_wrong_epoch':
                    reply['registration_epoch'] = 'wrong'
                elif mode == 'query_wrong_session':
                    reply['session_generation'] = 'wrong'
                elif mode == 'query_wrong_id':
                    reply['request_id'] = 'wrong'
                elif mode == 'query_count':
                    # A legacy/foreign draft size must fail closed, never be projected.
                    reply['chars'] = 7
                send_json(channel, reply)
                continue
            assert frame['type'] == 'deliver', frame
            append('frames.jsonl', frame)
            status = 'queued' if mode == 'queued' else 'accepted'
            reason = None
            if mode == 'rejected':
                status, reason = 'rejected', 'admission_refused'
            if frame.get('if_draft_empty'):
                assert isinstance(frame.get('deadline_ms'), int), frame
                if time.time() * 1000 >= frame['deadline_ms']:
                    status, reason = 'rejected', 'expired'
                elif mode in ('draft_present', 'ui_hold', 'unknown'):
                    status, reason = 'rejected', mode
            if status != 'rejected' and mode != 'complete_before_admission':
                # No receiver dedup to conceal a server redispatch bug. Count
                # every ingress attempt independently of the caller's result.
                append('queue.jsonl' if status == 'queued' else 'admitted.jsonl', frame)
            publish('received-%s.json' % frame['request_id'], frame)
            if mode in ('lost_ack', 'complete_before_admission'):
                continue
            ack = {key: frame[key] for key in ('registration_epoch', 'request_id', 'session_generation')}
            ack.update(type='ack', status=status)
            if status == 'rejected':
                ack['reason'] = reason
            if mode == 'wrong_ack':
                ack['request_id'] = 'uncorrelated-request'
            send_json(channel, ack)
"#;

struct ChannelServer {
    base: PathBuf,
    socket: PathBuf,
    server: Option<std::process::Child>,
}

impl ChannelServer {
    fn new() -> Self {
        let base = unique_test_dir();
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let process = base.join("process.py");
        fs::write(&process, PROCESS).unwrap();
        let shell = base.join("pane-shell");
        fs::write(
            &shell,
            format!(
                "#!/bin/sh\ntest -n \"$CHANNEL_TEST_DIR\" || exec /bin/sh\nexec python3 '{}' launcher \"$CHANNEL_TEST_DIR\"\n",
                process.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
        let config = base.join("config").join(app_dir_name());
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("config.toml"),
            format!(
                "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
                shell.to_str().unwrap()
            ),
        )
        .unwrap();
        let socket = named_session_socket(&base.join("config"), SESSION);
        let mut fixture = Self {
            base,
            socket,
            server: None,
        };
        register_runtime_dir(&fixture.base.join("runtime"));
        // The named-server helper fixes config and cannot set HOME/default
        // shell. Reuse the shared sanitized command/cleanup facilities instead
        // of mutating that read-only harness or the invoking process environment.
        let mut command = fixture.command();
        command
            .arg("server")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(fixture.base.join("server.stderr")).unwrap());
        let child = command.spawn().unwrap();
        register_spawned_herdr_pid(Some(child.id()));
        fixture.server = Some(child);
        wait_for_socket(&fixture.socket, DEADLINE);
        fixture
    }

    fn command(&self) -> Command {
        let mut command = crate::test_command::herdr_command();
        for (key, directory) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = self.base.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            command.env(key, path);
        }
        command.args(["--session", SESSION]).env("SHELL", "/bin/sh");
        command
    }

    fn request(&self, method: &str, params: Value) -> Value {
        request(&self.socket, method, params)
    }

    fn pane(&self, label: &str, mode: &str, claim: &str) -> TestPane {
        let directory = self.base.join(label);
        fs::create_dir_all(&directory).unwrap();
        let created = self.request(
            "workspace.create",
            json!({"cwd": self.base, "focus": true, "env": {
                "CHANNEL_TEST_DIR": directory,
                "CHANNEL_TEST_SOCKET": self.socket,
                "CHANNEL_TEST_MODE": mode,
                "CHANNEL_TEST_CLAIM": claim,
            }}),
        );
        assert_result(&created);
        let pane = TestPane {
            directory,
            socket: self.socket.clone(),
            approved_stdin: RefCell::new(String::new()),
            id: created["result"]["root_pane"]["pane_id"]
                .as_str()
                .unwrap()
                .to_owned(),
            terminal: created["result"]["root_pane"]["terminal_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        pane.file("launcher.json");
        pane
    }

    fn info(&self, pane: &TestPane) -> Value {
        let response = self.request("agent.channel_info", json!({"target": pane.id}));
        assert_result(&response);
        assert_eq!(
            response["result"]["terminal_id"], pane.terminal,
            "{response}"
        );
        response["result"].clone()
    }

    fn prompt_params(&self, pane: &TestPane, registration: &Value, id: &str, text: &str) -> Value {
        json!({
            "target": pane.id,
            "text": text,
            "expected_terminal": pane.terminal,
            "expected_registration_epoch": registration["registration_epoch"],
            "request_id": id,
            "timeout_ms": 1500,
            // Caller policy is covered elsewhere; this isolates channel receiver
            // authority even when the test runner inherits an outer Herdr pane.
            "allow_cross_pane": true,
        })
    }

    /// Receiver-focused draft query. As with prompt_params, caller policy is covered
    /// separately (with sanitized real CLI callers), so an inherited outer Herdr pane
    /// marker on the test runner cannot change these results.
    fn draft_state(&self, pane: &TestPane) -> Value {
        self.request(
            "agent.draft_state",
            json!({"target": pane.id, "allow_cross_pane": true}),
        )
    }

    /// Real `herdr agent draft-state` from outside every pane, sanitized env.
    fn cli_draft_state(&self, target: &str, allow: bool, env: &[(&str, &str)]) -> Value {
        let mut command = self.command();
        command.args(["agent", "draft-state", target]);
        if allow {
            command.arg("--allow-cross-pane");
        }
        command.envs(env.iter().copied());
        cli_reply(command.output().unwrap())
    }

    fn prompt(&self, pane: &TestPane, registration: &Value, id: &str, text: &str) -> Value {
        self.request(
            "agent.prompt_guarded",
            self.prompt_params(pane, registration, id, text),
        )
    }

    fn pending(
        &self,
        pane: &TestPane,
        registration: &Value,
        id: &str,
        text: &str,
    ) -> thread::JoinHandle<Value> {
        let params = self.prompt_params(pane, registration, id, text);
        let socket = self.socket.clone();
        thread::spawn(move || request(&socket, "agent.prompt_guarded", params))
    }
}

impl Drop for ChannelServer {
    fn drop(&mut self) {
        // Only our private named endpoint and exact owned child; no main/default
        // session, PID guessing or process-name cleanup. Avoid API on unwind.
        if let Some(mut child) = self.server.take() {
            let pid = child.id();
            // SIGTERM is Herdr's graceful stop path, including on assertion
            // unwind. The unreaped owned Child prevents PID reuse here.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            let exited = wait_until(Duration::from_secs(3), Duration::from_millis(10), || {
                child.try_wait().ok().flatten().is_some()
            });
            if !exited {
                let _ = child.kill();
            }
            let _ = child.wait();
            unregister_spawned_herdr_pid(Some(pid));
        }
        cleanup_test_base(&self.base);
    }
}

struct TestPane {
    directory: PathBuf,
    socket: PathBuf,
    approved_stdin: RefCell<String>,
    id: String,
    terminal: String,
}

impl TestPane {
    fn file(&self, name: &str) -> Value {
        let path = self.directory.join(name);
        let mut parsed = None;
        assert!(
            wait_until(DEADLINE, Duration::from_millis(10), || {
                parsed = fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                parsed.is_some()
            }),
            "process did not publish {}",
            path.display()
        );
        parsed.unwrap()
    }

    fn registration(&self, number: u32) -> Value {
        let response = self.file(&format!("registration-{number}.json"));
        assert_result(&response);
        let registration = &response["result"];
        assert_eq!(registration["ready"], true, "{response}");
        assert_eq!(registration["terminal_id"], self.terminal, "{response}");
        assert_eq!(registration["session_generation"], GENERATION, "{response}");
        assert!(registration["registration_epoch"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
        assert_ne!(response["peer_pid"], self.file("launcher.json")["pid"]);
        assert_eq!(response["pgid"], self.file("launcher.json")["pgid"]);
        self.wait_ready(registration);
        registration.clone()
    }

    fn wait_ready(&self, registration: &Value) {
        assert!(
            wait_until(DEADLINE, Duration::from_millis(10), || {
                let info = request(
                    &self.socket,
                    "agent.channel_info",
                    json!({"target": self.id}),
                );
                assert_result(&info);
                info["result"]["ready"] == true
                    && info["result"]["registration_epoch"] == registration["registration_epoch"]
            }),
            "registration never became discoverably ready: {registration}"
        );
    }

    fn control(&self, command: Value) -> Value {
        request_control(&self.directory.join("agent.sock"), command)
    }

    fn exit(&self, number: u32) {
        assert_eq!(self.control(json!({"op": "stop"}))["ok"], true);
        self.file(&format!("exited-{number}.json"));
        self.file(&format!("recorder-{number}.json"));
        let launcher = self.file("launcher.json")["pid"].as_u64().unwrap() as u32;
        assert!(process_exists(launcher), "surviving launcher exited");
    }

    fn shell_bytes(&self, number: u32) -> String {
        self.file(&format!("recorder-{number}.json"));
        let result = request_control(
            &self.directory.join("recorder.sock"),
            json!({"op": "barrier"}),
        );
        result["bytes"].as_str().unwrap().to_owned()
    }

    fn assert_no_shell_input(&self, number: u32) {
        self.file(&format!("recorder-{number}.json"));
        // A control-socket snapshot alone could run ahead of a delayed PTY
        // Enter. Queue a harmless raw marker through the actual PTY writer and
        // wait until the shell successor consumes it. All earlier user-input
        // writes, including delayed Enter, must precede this FIFO barrier.
        // Compare the entire byte stream against ONLY our explicit calibration
        // markers; no guarded byte (not even a lone Enter) is whitelisted.
        let marker = format!(
            "\x1fguarded-channel-barrier-{}\x1f",
            self.approved_stdin.borrow().len()
        );
        let marker_hex: String = marker.bytes().map(|byte| format!("{byte:02x}")).collect();
        let response = request(
            &self.socket,
            "pane.send_input",
            json!({
                "pane_id": self.id, "text": marker, "allow_cross_pane": true,
            }),
        );
        assert_result(&response);
        self.approved_stdin.borrow_mut().push_str(&marker_hex);
        let mut actual = String::new();
        assert!(
            wait_until(DEADLINE, Duration::from_millis(10), || {
                actual = self.shell_bytes(number);
                actual.contains(&marker_hex)
            }),
            "PTY recorder never consumed its FIFO barrier: {actual}"
        );
        assert_eq!(
            actual.as_str(),
            self.approved_stdin.borrow().as_str(),
            "guarded text/Enter/preparatory bytes reached shell stdin"
        );
    }

    fn restart(&self, number: u32, mode: &str) -> Value {
        self.assert_no_shell_input(number - 1);
        request_control(
            &self.directory.join("recorder.sock"),
            json!({"op": "restart", "mode": mode}),
        );
        self.registration(number)
    }

    /// `herdr agent draft-state` run by this pane's registered agent process.
    fn agent_cli_draft_state(&self, name: &str, target: &str, allow: bool) -> Value {
        let mut argv = vec![env!("CARGO_BIN_EXE_herdr"), "agent", "draft-state", target];
        if allow {
            argv.push("--allow-cross-pane");
        }
        let started = self.control(json!({"op": "query", "argv": argv, "name": name}));
        assert_eq!(started["ok"], true, "{started}");
        let done = self.file(name);
        let stream = if done["status"] == 0 {
            "stdout"
        } else {
            "stderr"
        };
        let reply: Value = serde_json::from_str(done[stream].as_str().unwrap())
            .unwrap_or_else(|error| panic!("{error}: {done}"));
        assert_eq!(done["status"] == 0, reply.get("result").is_some(), "{done}");
        reply
    }

    fn entries(&self, name: &str) -> Vec<Value> {
        fs::read_to_string(self.directory.join(name))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// CLI success prints the response on stdout; refusals print it on stderr with exit 1.
fn cli_reply(output: std::process::Output) -> Value {
    let code = output.status.code();
    let stream = if code == Some(0) {
        &output.stdout
    } else {
        &output.stderr
    };
    let reply: Value = serde_json::from_slice(stream).unwrap_or_else(|error| {
        panic!(
            "{error}: status {code:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(code == Some(0), reply.get("result").is_some(), "{reply}");
    reply
}

fn request_control(socket: &Path, command: Value) -> Value {
    exchange(socket, &command)
}

fn request(socket: &Path, method: &str, params: Value) -> Value {
    exchange(
        socket,
        &json!({"id": "guarded-channel-test", "method": method, "params": params}),
    )
}

fn exchange(socket: &Path, value: &Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(DEADLINE)).unwrap();
    stream.set_write_timeout(Some(DEADLINE)).unwrap();
    writeln!(stream, "{value}").unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
}

fn assert_result(response: &Value) {
    assert!(response.get("result").is_some(), "{response}");
    assert!(response.get("error").is_none(), "{response}");
}

fn assert_refusal(response: &Value) {
    assert!(response.get("error").is_some(), "{response}");
    assert!(response.get("result").is_none(), "{response}");
    assert_ne!(
        response["error"]["code"], "unknown_method",
        "not an implemented refusal: {response}"
    );
    assert_ne!(
        response["error"]["code"], "delivery_unknown",
        "known pre-dispatch refusal became uncertain: {response}"
    );
}

fn assert_receipt(response: &Value, registration: &Value, id: &str, status: &str) {
    assert_result(response);
    for key in ["terminal_id", "registration_epoch", "session_generation"] {
        assert_eq!(response["result"][key], registration[key], "{response}");
    }
    assert_eq!(response["result"]["request_id"], id, "{response}");
    assert_eq!(response["result"]["status"], status, "{response}");
}

fn literal_text(server: &ChannelServer) -> String {
    format!("printf COMPROMISED > '{}'; $(printf injected)\n--expected-terminal --allow-cross-pane /new {{template}}", server.base.join("must-not-execute").display())
}

#[test]
fn guarded_channel_normal_duplicate_and_payload_conflict_admit_literal_once() {
    let server = ChannelServer::new();
    let pane = server.pane("normal", "accepted", "self");
    let registration = pane.registration(1);
    let info = server.info(&pane);
    assert_eq!(info["ready"], true);
    assert_eq!(
        info["registration_epoch"],
        registration["registration_epoch"]
    );
    let text = literal_text(&server);
    let response = server.prompt(&pane, &registration, "first", &text);
    assert_receipt(&response, &registration, "first", "accepted");
    let duplicate = server.prompt(&pane, &registration, "first", &text);
    assert_receipt(&duplicate, &registration, "first", "accepted");
    assert_eq!(duplicate["result"]["duplicate"], true, "{duplicate}");
    assert_refusal(&server.prompt(&pane, &registration, "first", "changed payload"));
    pane.exit(1);
    let admitted = pane.entries("admitted.jsonl");
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0]["text"], text);
    assert_eq!(
        pane.entries("frames.jsonl").len(),
        1,
        "duplicate redispatched over channel"
    );
    pane.assert_no_shell_input(1);
    assert!(!server.base.join("must-not-execute").exists());
}

#[test]
fn guarded_channel_cli_delivers_literal_text_and_global_option_shaped_data() {
    let server = ChannelServer::new();
    let pane = server.pane("cli-literals", "accepted", "");
    let registration = pane.registration(1);
    let info = parse_cli_json_output(
        &["agent", "channel-info"],
        server
            .command()
            .args(["agent", "channel-info", &pane.id])
            .output()
            .unwrap(),
    );
    assert_eq!(info["result"]["ready"], true);
    assert_eq!(
        info["result"]["registration_epoch"],
        registration["registration_epoch"]
    );
    let shell_like = literal_text(&server);
    for (id, text) in [
        ("shell-literal", shell_like.as_str()),
        (
            "global-literal",
            "--session=must-not-select-another-session",
        ),
    ] {
        let output = server
            .command()
            .args([
                "agent",
                "prompt-guarded",
                "--expected-terminal",
                &pane.terminal,
                "--expected-registration-epoch",
                registration["registration_epoch"].as_str().unwrap(),
                "--request-id",
                id,
                "--timeout-ms",
                "1500",
                "--allow-cross-pane",
                "--",
                &pane.id,
                text,
            ])
            .output()
            .unwrap();
        let response = parse_cli_json_output(&["agent", "prompt-guarded"], output);
        assert_receipt(&response, &registration, id, "accepted");
    }
    pane.exit(1);
    let admitted = pane.entries("admitted.jsonl");
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0]["text"], shell_like);
    assert_eq!(
        admitted[1]["text"],
        "--session=must-not-select-another-session"
    );
    pane.assert_no_shell_input(1);
    assert!(!server.base.join("must-not-execute").exists());
}

#[test]
fn guarded_channel_pending_duplicate_shares_uncertain_outcome_without_second_frame() {
    let server = ChannelServer::new();
    let pane = server.pane("pending", "lost_ack", "");
    let registration = pane.registration(1);
    let first = server.pending(&pane, &registration, "pending", "one pending input");
    pane.file("received-pending.json");
    // The first request is provably dispatched but unacknowledged. The same ID
    // must attach to its reserved outcome, not send a second delivery.
    let second = server.pending(&pane, &registration, "pending", "one pending input");
    assert_refusal(&server.prompt(&pane, &registration, "pending", "conflict while pending"));
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_eq!(first["error"]["code"], "delivery_unknown", "{first}");
    assert_eq!(second["error"]["code"], "delivery_unknown", "{second}");
    let retry = server.prompt(&pane, &registration, "pending", "one pending input");
    assert_eq!(retry["error"]["code"], "delivery_unknown", "{retry}");
    assert_eq!(retry["error"]["message"], first["error"]["message"]);
    assert_eq!(retry["error"]["duplicate"], true);
    assert_eq!(pane.entries("frames.jsonl").len(), 1);
    let reconnected = pane.control(json!({"op": "reregister"}));
    assert_result(&reconnected);
    pane.wait_ready(&reconnected["result"]);
    let historical = server.prompt(&pane, &registration, "pending", "one pending input");
    assert_eq!(historical["error"]["code"], "delivery_unknown");
    assert_eq!(historical["error"]["message"], first["error"]["message"]);
    assert_eq!(historical["error"]["duplicate"], true);
    assert_eq!(pane.entries("frames.jsonl").len(), 1);
    // Re-registration must not itself replay an uncertain old delivery. A new
    // explicit ID below is a new request, not an automatically retargeted retry.
    pane.control(json!({"op": "mode", "mode": "accepted"}));
    let fresh = server.prompt(&pane, &reconnected["result"], "new-explicit", "new request");
    assert_receipt(&fresh, &reconnected["result"], "new-explicit", "accepted");
    pane.exit(1);
    let frames = pane.entries("frames.jsonl");
    assert_eq!(frames.len(), 2);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["request_id"] == "pending")
            .count(),
        1
    );
    assert_eq!(pane.entries("admitted.jsonl").len(), 2);
    pane.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_busy_queued_and_rejected_receipts_are_not_socket_success() {
    let server = ChannelServer::new();
    let pane = server.pane("receipts", "queued", "");
    let registration = pane.registration(1);
    let queued = server.prompt(&pane, &registration, "busy", "follow-up, not a new turn");
    assert_receipt(&queued, &registration, "busy", "queued");
    assert!(pane.entries("admitted.jsonl").is_empty());
    assert_eq!(pane.entries("queue.jsonl").len(), 1);
    pane.control(json!({"op": "mode", "mode": "rejected"}));
    let rejected = server.prompt(
        &pane,
        &registration,
        "consumed",
        "consumed input is not admitted",
    );
    assert_eq!(
        rejected["error"]["code"], "agent_prompt_rejected",
        "{rejected}"
    );
    // Socket delivery happened in both cases. A rejected receipt may never be
    // advertised as accepted/queued; actual Pi queue/hook behavior is separate.
    pane.exit(1);
    assert!(pane.entries("admitted.jsonl").is_empty());
    assert_eq!(pane.entries("queue.jsonl").len(), 1);
    assert_eq!(pane.entries("frames.jsonl").len(), 2);
    pane.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_unmatched_ack_is_unknown_and_never_replayed() {
    let server = ChannelServer::new();
    let pane = server.pane("wrong-ack", "wrong_ack", "");
    let registration = pane.registration(1);
    let response = server.prompt(&pane, &registration, "correlated", "exact ACK correlation");
    assert_eq!(response["error"]["code"], "delivery_unknown", "{response}");
    let retry = server.prompt(&pane, &registration, "correlated", "exact ACK correlation");
    // Invalid frames may revoke the channel. Either the retained uncertain
    // outcome or a now-unavailable channel is safe, never success or redispatch.
    assert!(retry.get("error").is_some(), "{retry}");
    assert_ne!(retry["error"]["code"], "unknown_method", "{retry}");
    pane.exit(1);
    assert_eq!(pane.entries("frames.jsonl").len(), 1);
    assert_eq!(pane.entries("admitted.jsonl").len(), 1);
    pane.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_outside_peer_and_a_claiming_b_refuse_b_self_allows() {
    let server = ChannelServer::new();
    let b = server.pane("pane-b", "no_channel", "");
    b.file("recorder-1.json");
    let mut outside = Command::new("python3");
    crate::test_command::sanitize_command_env(&mut outside);
    let outside = outside
        .arg(server.base.join("process.py"))
        .arg("probe")
        .arg(server.base.join("outside-peer"))
        .arg(&b.id)
        .env("CHANNEL_TEST_SOCKET", &server.socket)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        outside.status.success(),
        "outside helper failed: {}",
        String::from_utf8_lossy(&outside.stderr)
    );
    let outside: Value = serde_json::from_slice(&outside.stdout).unwrap();
    assert_refusal(&outside);
    assert_eq!(server.info(&b)["ready"], false);
    let a = server.pane("pane-a", "accepted", &b.id);
    let spoof = a.file("registration-1.json");
    assert_refusal(&spoof);
    a.file("exited-1.json");
    assert_eq!(server.info(&a)["ready"], false);
    assert_eq!(server.info(&b)["ready"], false);
    let own = b.restart(2, "accepted");
    assert_eq!(server.info(&b)["ready"], true);
    let response = server.prompt(&b, &own, "b-self", "B is a legitimate receiver");
    assert_receipt(&response, &own, "b-self", "accepted");
    b.exit(2);
    assert_eq!(b.entries("admitted.jsonl").len(), 1);
    a.assert_no_shell_input(1);
    b.assert_no_shell_input(2);
}

#[test]
fn guarded_channel_live_owner_conflict_disconnect_reservation_and_reconnect_epoch() {
    let server = ChannelServer::new();
    let pane = server.pane("reconnect", "accepted", "");
    let original = pane.registration(1);
    assert_refusal(&pane.control(json!({"op": "peer"})));
    pane.control(json!({"op": "disconnect"}));
    assert!(wait_until(DEADLINE, Duration::from_millis(10), || server
        .info(&pane)["ready"]
        == false));
    assert_refusal(&pane.control(json!({"op": "peer"})));
    assert_refusal(&server.prompt(
        &pane,
        &original,
        "disconnected",
        "no channel is not shell authority",
    ));
    let reconnected = pane.control(json!({"op": "reconnect"}));
    assert_result(&reconnected);
    let current = &reconnected["result"];
    pane.wait_ready(current);
    assert_ne!(
        current["registration_epoch"],
        original["registration_epoch"]
    );
    assert_eq!(current["terminal_id"], original["terminal_id"]);
    assert_refusal(&server.prompt(&pane, &original, "old-epoch", "must not retarget"));
    let superseded = pane.control(json!({"op": "reregister"}));
    assert_result(&superseded);
    let newest = &superseded["result"];
    pane.wait_ready(newest);
    assert_ne!(newest["registration_epoch"], current["registration_epoch"]);
    assert_refusal(&server.prompt(
        &pane,
        current,
        "superseded-epoch",
        "only one active connection",
    ));
    let response = server.prompt(&pane, newest, "fresh", "new epoch works");
    assert_receipt(&response, newest, "fresh", "accepted");
    pane.exit(1);
    let replacement = pane.restart(2, "accepted");
    assert_ne!(
        replacement["registration_epoch"],
        newest["registration_epoch"]
    );
    let old_pid = pane.file("registration-1.json")["peer_pid"].clone();
    let new_pid = pane.file("registration-2.json")["peer_pid"].clone();
    assert_ne!(
        old_pid, new_pid,
        "test requires a new real process, not a reconnect"
    );
    assert_refusal(&server.prompt(
        &pane,
        newest,
        "dead-generation",
        "dead registrant must not retarget",
    ));
    let response = server.prompt(
        &pane,
        &replacement,
        "replacement",
        "confirmed death releases ownership",
    );
    assert_receipt(&response, &replacement, "replacement", "accepted");
    pane.exit(2);
    assert_eq!(pane.entries("admitted.jsonl").len(), 2);
    assert_eq!(pane.entries("frames.jsonl").len(), 2);
    pane.assert_no_shell_input(2);
}

#[test]
fn guarded_channel_surviving_launcher_and_no_channel_refuse_without_shell_input() {
    let server = ChannelServer::new();
    let pane = server.pane("surviving-launcher", "accepted", "");
    let registration = pane.registration(1);
    pane.exit(1);
    // Process death is reaped and the shell recorder is ready BEFORE dispatch.
    // The root launcher is still alive, so authorizing it would reproduce #167.
    let text = literal_text(&server);
    assert_eq!(server.info(&pane)["ready"], false);
    assert_refusal(&server.prompt(&pane, &registration, "after-exit", &text));
    pane.assert_no_shell_input(1);
    assert!(pane.entries("frames.jsonl").is_empty());
    assert!(pane.entries("admitted.jsonl").is_empty());
    let unregistered = server.pane("unregistered", "no_channel", "");
    assert_eq!(server.info(&unregistered)["ready"], false);
    let absent = json!({"registration_epoch": "never-registered"});
    assert_refusal(&server.prompt(&unregistered, &absent, "no-channel", &text));
    unregistered.assert_no_shell_input(1);
    assert!(!server.base.join("must-not-execute").exists());
}

#[test]
fn guarded_channel_exit_with_incomplete_receiver_frame_never_reaches_shell() {
    let server = ChannelServer::new();
    let pane = server.pane("prefix-exit", "prefix", "");
    let registration = pane.registration(1);
    let pending = server.pending(&pane, &registration, "partial", &literal_text(&server));
    assert_eq!(pane.file("prefix-1.json")["bytes"], 1);
    pane.exit(1);
    let response = pending.join().unwrap();
    // A prefix was observed, so dispatch happened. Admission remains unknown to
    // the server, but this receiver never parsed/submitted an incomplete frame.
    assert_eq!(response["error"]["code"], "delivery_unknown", "{response}");
    assert!(pane.entries("frames.jsonl").is_empty());
    assert!(pane.entries("admitted.jsonl").is_empty());
    pane.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_exit_after_complete_frame_before_admission_or_ack_is_unknown() {
    for (label, mode, count) in [
        ("before-admission", "complete_before_admission", 0),
        ("after-admission", "lost_ack", 1),
    ] {
        let server = ChannelServer::new();
        let pane = server.pane(label, mode, "");
        let registration = pane.registration(1);
        let pending = server.pending(&pane, &registration, "complete", &literal_text(&server));
        pane.file("received-complete.json");
        pane.exit(1);
        let response = pending.join().unwrap();
        assert_eq!(response["error"]["code"], "delivery_unknown", "{response}");
        assert_eq!(pane.entries("frames.jsonl").len(), 1);
        assert_eq!(pane.entries("admitted.jsonl").len(), count);
        pane.assert_no_shell_input(1);
        let replacement = pane.restart(2, "accepted");
        assert_ne!(
            replacement["registration_epoch"],
            registration["registration_epoch"]
        );
        assert_refusal(&server.prompt(&pane, &registration, "complete", &literal_text(&server)));
        // Fresh registration must not replay the possibly admitted old request.
        pane.exit(2);
        assert_eq!(pane.entries("frames.jsonl").len(), 1);
        assert_eq!(pane.entries("admitted.jsonl").len(), count);
        pane.assert_no_shell_input(2);
    }
}

#[test]
fn guarded_channel_stale_terminal_and_epoch_have_no_delivery_effects() {
    let server = ChannelServer::new();
    let pane = server.pane("identities", "accepted", "");
    let registration = pane.registration(1);
    let other = server.pane("other-terminal", "no_channel", "");
    for terminal in [other.terminal.as_str(), "term_nonexistent"] {
        let mut params = server.prompt_params(
            &pane,
            &registration,
            "stale-terminal",
            &literal_text(&server),
        );
        params["expected_terminal"] = json!(terminal);
        let response = server.request("agent.prompt_guarded", params);
        assert_eq!(
            response["error"]["code"], "terminal_identity_mismatch",
            "{response}"
        );
    }
    let stale = json!({"registration_epoch": "never-this-epoch"});
    assert_refusal(&server.prompt(&pane, &stale, "stale-epoch", &literal_text(&server)));
    // Positive control ensures refusals are not caused by a permanently broken
    // channel or an unsupported target resolver.
    let response = server.prompt(&pane, &registration, "right", "exact identity succeeds");
    assert_receipt(&response, &registration, "right", "accepted");
    pane.exit(1);
    assert_eq!(pane.entries("frames.jsonl").len(), 1);
    assert_eq!(pane.entries("admitted.jsonl").len(), 1);
    pane.assert_no_shell_input(1);
    other.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_if_draft_empty_accepts_and_retains_flag_identity_across_epochs() {
    let server = ChannelServer::new();
    let pane = server.pane("draft-guard-accepted", "accepted", "self");
    let registration = pane.registration(1);
    assert_eq!(server.info(&pane)["draft_guard"], true);
    let text = literal_text(&server);
    let mut params = server.prompt_params(&pane, &registration, "guarded-first", &text);
    params["if_draft_empty"] = true.into();
    let accepted = server.request("agent.prompt_guarded", params.clone());
    assert_receipt(&accepted, &registration, "guarded-first", "accepted");
    let duplicate = server.request("agent.prompt_guarded", params.clone());
    assert_eq!(duplicate["result"]["duplicate"], true);
    let mut mismatch = params.clone();
    mismatch["if_draft_empty"] = false.into();
    assert_eq!(
        server.request("agent.prompt_guarded", mismatch.clone())["error"]["code"],
        "payload_mismatch"
    );
    let current = pane.control(json!({"op":"reregister"}));
    assert_result(&current);
    pane.wait_ready(&current["result"]);
    let retained = server.request("agent.prompt_guarded", params);
    assert_receipt(&retained, &registration, "guarded-first", "accepted");
    assert_eq!(retained["result"]["duplicate"], true);
    assert_eq!(
        server.request("agent.prompt_guarded", mismatch)["error"]["code"],
        "payload_mismatch"
    );
    let frames = pane.entries("frames.jsonl");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["if_draft_empty"], true);
    assert!(frames[0]["deadline_ms"].as_u64().is_some());
    pane.exit(1);
    pane.assert_no_shell_input(1);
    assert!(!server.base.join("must-not-execute").exists());
}

#[test]
fn guarded_channel_if_draft_empty_typed_refusals_send_no_input_and_do_not_replay() {
    let server = ChannelServer::new();
    let pane = server.pane("draft-guard-refusals", "accepted", "");
    let registration = pane.registration(1);
    for reason in ["draft_present", "ui_hold", "unknown"] {
        pane.control(json!({"op":"mode", "mode":reason}));
        let mut params = server.prompt_params(&pane, &registration, reason, &literal_text(&server));
        params["if_draft_empty"] = true.into();
        let rejected = server.request("agent.prompt_guarded", params.clone());
        assert_refusal(&rejected);
        assert_eq!(rejected["error"]["code"], "agent_prompt_rejected");
        assert_eq!(rejected["error"]["reason"], reason);
        // Even after the UI becomes observable/empty, this key retains its refusal.
        pane.control(json!({"op":"mode", "mode":"accepted"}));
        let retained = server.request("agent.prompt_guarded", params);
        assert_eq!(retained["error"]["reason"], reason);
        assert_eq!(retained["error"]["duplicate"], true);
    }
    pane.exit(1);
    assert_eq!(pane.entries("frames.jsonl").len(), 3);
    assert!(pane.entries("admitted.jsonl").is_empty());
    assert!(pane.entries("queue.jsonl").is_empty());
    pane.assert_no_shell_input(1);
    assert!(!server.base.join("must-not-execute").exists());
}

#[test]
fn guarded_channel_draft_query_known_unknown_timeout_is_read_only_and_not_authority() {
    let server = ChannelServer::new();
    let pane = server.pane("draft-query", "accepted", "");
    let registration = pane.registration(1);
    for (mode, expected) in [
        (
            "accepted",
            json!({"status":"known", "empty":true, "hold":null}),
        ),
        (
            "query_nonempty",
            json!({"status":"known", "empty":false, "hold":null}),
        ),
        (
            "query_dialog",
            json!({"status":"known", "empty":true, "hold":"dialog"}),
        ),
        (
            "query_custom",
            json!({"status":"known", "empty":true, "hold":"custom"}),
        ),
        (
            "query_editor",
            json!({"status":"known", "empty":true, "hold":"editor"}),
        ),
        (
            "query_unknown",
            json!({"status":"unknown", "reason":"unknown"}),
        ),
        (
            "query_timeout",
            json!({"status":"unknown", "reason":"timeout"}),
        ),
    ] {
        pane.control(json!({"op":"mode", "mode":mode}));
        let start = std::time::Instant::now();
        let response = server.draft_state(&pane);
        assert_result(&response);
        assert_eq!(response["result"], expected, "mode {mode}: {response}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "query exceeded bounded default"
        );
    }
    assert!(pane.entries("frames.jsonl").is_empty());
    assert!(pane.entries("admitted.jsonl").is_empty());
    // A previous empty observation cannot authorize a later prompt after draft changes.
    pane.control(json!({"op":"mode", "mode":"draft_present"}));
    let mut params = server.prompt_params(&pane, &registration, "after-query", "literal");
    params["if_draft_empty"] = true.into();
    assert_eq!(
        server.request("agent.prompt_guarded", params)["error"]["reason"],
        "draft_present"
    );
    assert_eq!(pane.entries("queries.jsonl").len(), 7);
    for query in pane.entries("queries.jsonl") {
        assert_eq!(query.as_object().unwrap().len(), 4);
        assert!(query.get("text").is_none());
    }
    pane.exit(1);
    pane.assert_no_shell_input(1);
}

#[test]
fn guarded_channel_draft_query_unregistered_and_old_extension_send_no_frames() {
    let server = ChannelServer::new();
    let absent = server.pane("draft-unregistered", "no_channel", "");
    absent.file("recorder-1.json");
    let response = server.draft_state(&absent);
    assert_eq!(
        response["result"],
        json!({"status":"unknown", "reason":"unregistered"})
    );
    let mut params = server.prompt_params(
        &absent,
        &json!({"registration_epoch":"absent"}),
        "none",
        "literal",
    );
    params["if_draft_empty"] = true.into();
    let rejected = server.request("agent.prompt_guarded", params);
    assert_eq!(rejected["error"]["code"], "agent_prompt_rejected");
    assert_eq!(rejected["error"]["reason"], "unregistered");
    absent.assert_no_shell_input(1);
    let old = server.pane("draft-old-extension", "old_extension", "");
    let registration = old.registration(1);
    assert_eq!(server.info(&old)["draft_guard"], false);
    let response = server.draft_state(&old);
    assert_eq!(
        response["result"],
        json!({"status":"unknown", "reason":"unsupported"})
    );
    let mut params = server.prompt_params(&old, &registration, "unsupported", "literal");
    params["if_draft_empty"] = true.into();
    let rejected = server.request("agent.prompt_guarded", params);
    assert_eq!(rejected["error"]["code"], "agent_prompt_rejected");
    assert_eq!(rejected["error"]["reason"], "unsupported");
    assert!(old.entries("frames.jsonl").is_empty());
    assert!(old.entries("queries.jsonl").is_empty());
    // Legacy explicit ingress remains available, but guarded failures never fall back.
    assert_receipt(
        &server.prompt(&old, &registration, "legacy", "literal"),
        &registration,
        "legacy",
        "accepted",
    );
    old.exit(1);
    old.assert_no_shell_input(1);
    assert_eq!(old.entries("frames.jsonl").len(), 1);
}

#[test]
fn guarded_channel_draft_query_bad_receipt_or_private_text_fails_closed_without_leak() {
    let server = ChannelServer::new();
    for mode in [
        "query_text",
        "query_wrong_epoch",
        "query_wrong_session",
        "query_wrong_id",
        "query_count",
    ] {
        let pane = server.pane(mode, mode, "");
        pane.registration(1);
        let response = server.draft_state(&pane);
        assert_result(&response);
        assert_eq!(
            response["result"],
            json!({"status":"unknown", "reason":"unknown"})
        );
        assert!(!response.to_string().contains("private draft"));
        assert!(response["result"].get("chars").is_none());
        assert!(pane.entries("admitted.jsonl").is_empty());
        pane.exit(1);
        pane.assert_no_shell_input(1);
    }
}

/// agent.draft_state uses agent.prompt's caller policy, decided before any ledger
/// reservation or receiver frame. Callers are real `herdr` CLI processes: one run by
/// pane A's registered agent (kernel-attributed), others outside every pane.
#[test]
fn guarded_channel_draft_query_authorizes_caller_before_reservation_or_frame() {
    let server = ChannelServer::new();
    let source = server.pane("draft-auth-source", "accepted", "");
    source.registration(1);
    let target = server.pane("draft-auth-target", "accepted", "");
    target.registration(1);
    let known = json!({"status":"known", "empty":true, "hold":null});

    // Agent -> another pane without opt-in: refused; the receiver sees no frame.
    let denied = source.agent_cli_draft_state("cross-denied.json", &target.id, false);
    assert_refusal(&denied);
    assert_eq!(denied["error"]["code"], "cross_pane_input_denied");
    assert!(!denied.to_string().contains("empty"), "{denied}");
    // An unattributable caller is refused the same way unless it opts in.
    let unknown = server.cli_draft_state(&target.id, false, &[("HERDR_ENV", "0")]);
    assert_refusal(&unknown);
    assert_eq!(unknown["error"]["code"], "input_origin_unknown");
    assert!(target.entries("queries.jsonl").is_empty());
    assert!(source.entries("queries.jsonl").is_empty());

    // Same pane by default, and another pane only with the explicit flag.
    let own = source.agent_cli_draft_state("own.json", &source.id, false);
    assert_eq!(own["result"], known, "{own}");
    assert_eq!(source.entries("queries.jsonl").len(), 1);
    let allowed = source.agent_cli_draft_state("cross-allowed.json", &target.id, true);
    assert_eq!(allowed["result"], known, "{allowed}");
    assert_eq!(target.entries("queries.jsonl").len(), 1);
    let unknown = server.cli_draft_state(&target.id, true, &[("HERDR_ENV", "0")]);
    assert_eq!(unknown["result"], known, "{unknown}");
    // An ordinary external operator keeps agent.prompt's ordinary policy.
    let ordinary = server.cli_draft_state(&target.id, false, &[]);
    assert_eq!(ordinary["result"], known, "{ordinary}");
    assert_eq!(target.entries("queries.jsonl").len(), 3);
    assert_eq!(source.entries("queries.jsonl").len(), 1);

    for pane in [&source, &target] {
        assert!(pane.entries("frames.jsonl").is_empty());
        assert!(pane.entries("admitted.jsonl").is_empty());
        pane.exit(1);
        pane.assert_no_shell_input(1);
    }
}

#[test]
fn guarded_channel_shell_recorder_positive_control_captures_partial_text_and_enter() {
    let server = ChannelServer::new();
    let pane = server.pane("recorder-control", "no_channel", "");
    pane.file("recorder-1.json");
    assert_eq!(pane.shell_bytes(1), "");
    // Deliberately unguarded input solely to calibrate the real PTY recorder.
    // Text without Enter must be visible, so a canonical line recorder cannot
    // accidentally make every negative assertion pass.
    let sent = server.request(
        "pane.send_input",
        json!({"pane_id": pane.id, "text": "raw-recorder-control", "allow_cross_pane": true}),
    );
    assert_result(&sent);
    assert!(wait_until(DEADLINE, Duration::from_millis(10), || pane
        .shell_bytes(1)
        == "7261772d7265636f726465722d636f6e74726f6c"));
    let sent = server.request(
        "pane.send_input",
        json!({"pane_id": pane.id, "keys": ["Enter"], "allow_cross_pane": true}),
    );
    assert_result(&sent);
    assert!(wait_until(DEADLINE, Duration::from_millis(10), || {
        let bytes = pane.shell_bytes(1);
        bytes == "7261772d7265636f726465722d636f6e74726f6c0d"
            || bytes == "7261772d7265636f726465722d636f6e74726f6c0a"
    }));
}
