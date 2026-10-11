//! Real pane consumer fixture. The Python program is test code, not a fake actor:
//! it reads the pane's slave, calls the real JSON socket with its own SO_PEERCRED,
//! and hashes the actual bytes received. No production API symbols are assumed.
use crate::{support, test_command};
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CONSUMER: &str = r#"
import os, sys, socket, json, time, termios, tty, select, hashlib
root, api, pane = sys.argv[1:]
def save(name, value):
    p = root + '/' + name
    with open(p+'.tmp','w') as f: json.dump(value,f,default=lambda b: b.hex() if isinstance(b,bytes) else str(b))
    os.replace(p+'.tmp',p)
def rpc(method, params):
    s=socket.socket(socket.AF_UNIX); s.settimeout(3); s.connect(api)
    s.sendall((json.dumps(dict(id='consumer',method=method,params=params))+'\n').encode())
    b=b''
    while b'\n' not in b: b+=s.recv(65536)
    s.close(); return json.loads(b.split(b'\n')[0])
tty.setraw(0)
st=os.fstat(0)
save('ready',dict(pid=os.getpid(),pgid=os.getpgrp(),sid=os.getsid(0),foreground=os.tcgetpgrp(0),tty=os.ttyname(0),tty_dev=str(st.st_dev),tty_ino=str(st.st_ino),termios=termios.tcgetattr(0)))
raw=b''; epoch_bytes=b''; pre=b''; active=False; epoch=None; previous=0; seq=0; index=0
end=time.monotonic()+100
while time.monotonic()<end:
    if select.select([0],[],[],0.01)[0]:
        b=os.read(0,65536); raw+=b
        if active: epoch_bytes+=b
        elif epoch:
            marker=b'\x1b_herdr-epoch;'+epoch['nonce'].encode()+b'\x1b\\'
            pos=raw.find(marker)
            if pos>=0:
                pre=raw[:pos]; epoch_bytes=raw[pos+len(marker):]; active=True
        save('bytes',dict(raw=raw.hex(),epoch=epoch_bytes.hex(),pre=pre.hex(),active=active))
    cmdpath=root+'/cmd-'+str(index)
    if not os.path.exists(cmdpath): continue
    with open(cmdpath) as f: c=json.load(f)
    op=c['op']; out={}
    if op=='enroll':
        if 'flag' in c:
            a=termios.tcgetattr(0); field=3 if c['flag'] in ['ICANON','ECHO','IEXTEN'] else 0
            a[field]|=getattr(termios,c['flag']); termios.tcsetattr(0,termios.TCSANOW,a)
        challenge=c.get('challenge',os.urandom(32).hex())
        out=rpc('pane.input_consumer.enroll',dict(pane_id=pane,challenge=challenge)); out['_challenge']=challenge
        if 'result' in out and all(k in out['result'] for k in ['epoch','epoch_key','nonce']): epoch=out['result']
    elif op=='cut':
        seq=c.get('seq',seq+1); cut=c.get('cut',len(epoch_bytes))
        params=dict(epoch=epoch['epoch'],epoch_key=epoch['epoch_key'],seq=seq,token=c.get('token',('%032x'%seq)),cut=cut,digest=c.get('digest',hashlib.sha256(epoch_bytes[previous:cut]).hexdigest()),kind=c.get('kind','submit'))
        out=rpc('pane.input_consumer.cut',params); out['_request']=params
        if not c.get('no_advance'): previous=cut
    elif op=='rpc': out=rpc(c['method'],c['params']); out['_request']=c['params']
    elif op=='termios':
        a=termios.tcgetattr(0)
        if c['flag']=='speed': a[4]=termios.B9600; a[5]=termios.B9600
        else:
            field=3 if c['flag'] in ['ICANON','ECHO','IEXTEN'] else 0
            a[field]|=getattr(termios,c['flag'])
        termios.tcsetattr(0,termios.TCSANOW,a); out=dict(changed=termios.tcgetattr(0))
    elif op=='child':
        pid=os.fork()
        if pid==0:
            if c.get('different'): os.setpgid(0,0)
            save('child-result',dict(pid=os.getpid(),pgid=os.getpgrp(),response=rpc('pane.input_consumer.enroll',dict(pane_id=pane,challenge=os.urandom(32).hex()))))
            os._exit(0)
        os.waitpid(pid,0)
        with open(root+'/child-result') as f: out=json.load(f)
    elif op=='query': os.write(1,bytes.fromhex(c['hex'])); out=dict(query=c['hex'])
    elif op=='steal':
        # A second slave reader physically removes bytes from the input queue.
        pid=os.fork()
        if pid==0:
            b=b''
            if select.select([0],[],[],2)[0]: b=os.read(0,1)
            save('stolen',dict(bytes=b.hex())); os._exit(0)
        save('steal-ready',dict(pid=pid)); os.waitpid(pid,0)
        with open(root+'/stolen') as f: out=json.load(f)
    elif op=='stop': save('out-'+str(index),dict(stopped=True)); break
    save('out-'+str(index),out); index+=1
"#;

pub struct Fixture {
    pub base: PathBuf,
    pub api: PathBuf,
    pub pane: String,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    index: usize,
}

pub fn request(api: &Path, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(api).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    writeln!(
        stream,
        "{}",
        json!({"id":"cut-test","method":method,"params":params})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn fields(list: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in list {
        out.extend((f.len() as u32).to_be_bytes());
        out.extend(f.as_bytes());
    }
    out
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

impl Fixture {
    pub fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("c{}-{stamp:x}", std::process::id()));
        let config = base.join("config");
        let runtime = base.join("runtime");
        let state = base.join("state");
        fs::create_dir_all(config.join("herdr")).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(config.join("herdr/config.toml"), "onboarding = false\n").unwrap();
        fs::write(base.join("consumer.py"), CONSUMER).unwrap();
        support::register_runtime_dir(&runtime);
        let api = runtime.join("herdr.sock");
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = test_command::herdr_pty_command();
        cmd.arg("server");
        cmd.env("HOME", &base);
        cmd.env("XDG_CONFIG_HOME", &config);
        // Keep the private consumer audit log inside this disposable fixture; do not
        // inherit the harness user's state directory or share writes between tests.
        cmd.env("XDG_STATE_HOME", &state);
        cmd.env("XDG_RUNTIME_DIR", &runtime);
        cmd.env("HERDR_SOCKET_PATH", &api);
        cmd.env("SHELL", "/bin/sh");
        let child = pair.slave.spawn_command(cmd).unwrap();
        support::register_spawned_herdr_pid(child.process_id());
        drop(pair.slave);
        // Drain the server's real outer PTY so diagnostics cannot block it.
        let mut reader = pair.master.try_clone_reader().unwrap();
        thread::spawn(move || {
            let mut b = [0; 8192];
            while let Ok(n) = reader.read(&mut b) {
                if n == 0 {
                    break;
                }
            }
        });
        let mut f = Self {
            base,
            api,
            pane: String::new(),
            master: pair.master,
            child,
            index: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        while !f.api.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(f.api.exists(), "private real server failed to start");
        let r = request(&f.api, "workspace.create", json!({"label":"input-cut"}));
        f.pane = r
            .pointer("/result/root_pane/pane_id")
            .and_then(Value::as_str)
            .expect("real pane")
            .into();
        let command = format!(
            "exec python3 {} {} {} {}",
            f.base.join("consumer.py").display(),
            f.base.display(),
            f.api.display(),
            f.pane
        );
        // Shell initialization is asynchronous: require a real round-trip.
        let ack = f.base.join("shell-ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ack.exists() && Instant::now() < deadline {
            f.api_input(
                "pane.send_input",
                json!({"text":format!("printf ready > {}",ack.display()),"keys":["Enter"]}),
            );
            thread::sleep(Duration::from_millis(100));
        }
        assert!(ack.exists(), "real shell did not acknowledge bootstrap");
        f.api_input("pane.send_input", json!({"text":command,"keys":["Enter"]}));
        let evidence = f.file("ready");
        assert_eq!(
            evidence["pgid"], evidence["foreground"],
            "consumer must be foreground"
        );
        assert!(evidence["tty"].as_str().unwrap().starts_with("/dev/pts/"));
        eprintln!(
            "REAL_PTY {} pane={} socket={}",
            evidence,
            f.pane,
            f.api.display()
        );
        f
    }
    pub fn file(&self, name: &str) -> Value {
        let path = self.base.join(name);
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if let Ok(bytes) = fs::read(&path) {
                return serde_json::from_slice(&bytes).unwrap();
            }
            thread::sleep(Duration::from_millis(10));
        }
        let screen = request(
            &self.api,
            "pane.read",
            json!({"pane_id": self.pane, "source": "recent", "lines": 80}),
        );
        panic!("real consumer timed out: {} pane={screen}", path.display());
    }
    pub fn start(&mut self, cmd: Value) {
        let path = self.base.join(format!("cmd-{}", self.index));
        fs::write(
            path.with_extension("tmp"),
            serde_json::to_vec(&cmd).unwrap(),
        )
        .unwrap();
        fs::rename(path.with_extension("tmp"), path).unwrap();
    }
    pub fn finish(&mut self) -> Value {
        let r = self.file(&format!("out-{}", self.index));
        self.index += 1;
        r
    }
    pub fn command(&mut self, cmd: Value) -> Value {
        self.start(cmd);
        self.finish()
    }
    pub fn enroll(&mut self) -> Value {
        let r = self.command(json!({"op":"enroll"}));
        assert!(
            r.get("error").is_none(),
            "pane.input_consumer.enroll is required on real PTY: {r}"
        );
        for key in ["epoch", "epoch_key", "nonce"] {
            assert!(r["result"][key].is_string(), "enroll contract {key}: {r}");
        }
        // No server authentication in this build (smarty-dev#6690): never a signature.
        assert!(r["result"].get("sig").is_none(), "unsigned enroll: {r}");
        self.verify_enroll(&r);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if self.file("bytes")["active"] == true {
                return r["result"].clone();
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("enroll did not write matching one-operation nonce marker");
    }
    /// The answer names this consumer's own stdin tty.
    pub fn verify_enroll(&self, r: &Value) {
        let res = &r["result"];
        let ready = self.file("ready");
        assert_eq!(
            res["tty"]["dev"], ready["tty_dev"],
            "tty is the consumer's: {r}"
        );
        assert_eq!(
            res["tty"]["ino"], ready["tty_ino"],
            "tty is the consumer's: {r}"
        );
    }
    pub fn api_input(&self, method: &str, mut params: Value) -> Value {
        params["pane_id"] = json!(self.pane);
        let r = request(&self.api, method, params);
        assert!(r.get("error").is_none(), "{method}: {r}");
        r
    }
    pub fn client(&self) -> UnixStream {
        let mut s = UnixStream::connect(self.api.with_file_name("herdr-client.sock")).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(4))).unwrap();
        let (generation, error) = support::client_shell_handshake(&mut s, 1, 80, 24).unwrap();
        assert_eq!(generation, 1);
        assert!(error.is_none(), "{error:?}");
        support::wait_for_client_shell_bootstrap(&mut s, Duration::from_secs(4)).unwrap();
        // Drain presentation frames, retaining the connection that owns input.
        let mut reader = s.try_clone().unwrap();
        thread::spawn(move || {
            let mut b = [0; 8192];
            while let Ok(n) = reader.read(&mut b) {
                if n == 0 {
                    break;
                }
            }
        });
        s
    }
    pub fn consumer_audit_path(&self) -> PathBuf {
        let app_dir = if cfg!(debug_assertions) {
            "herdr-dev"
        } else {
            "herdr"
        };
        self.base
            .join("state")
            .join(app_dir)
            .join("input-consumer.jsonl")
    }
    pub fn consumer_audit_records(&self) -> (String, Vec<Value>) {
        let raw =
            fs::read_to_string(self.consumer_audit_path()).expect("private consumer audit log");
        let records = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("consumer audit JSONL"))
            .collect();
        (raw, records)
    }
    pub fn bytes(&self) -> Vec<u8> {
        let s = self.file("bytes")["epoch"].as_str().unwrap().to_owned();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    pub fn wait_len(&self, len: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let b = self.bytes();
            if b.len() >= len {
                return b;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("slave did not receive {len} bytes: {:?}", self.bytes());
    }
    pub fn cut(&mut self, extra: Value) -> Value {
        let mut cmd = json!({"op":"cut"});
        for (k, v) in extra.as_object().unwrap() {
            cmd[k] = v.clone()
        }
        self.command(cmd)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = &self.master;
        let pid = self.child.process_id();
        let _ = self.child.kill();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        support::unregister_spawned_herdr_pid(pid);
        support::cleanup_test_base(&self.base);
        eprintln!(
            "OWNED_REAL_PTY_CLEANED pid={pid:?} runtime={}",
            self.base.display()
        );
    }
}

// Frozen generation-1 ClientShellPaneInput framing, copied from the existing
// shift-enter pattern; no unpublished production Rust APIs are introduced.
pub fn client_text(s: &mut UnixStream, pane: &str, text: &str, paste: bool) {
    fn varint(v: u32) -> Vec<u8> {
        if v < 251 {
            vec![v as u8]
        } else if v < 65536 {
            let mut b = vec![251];
            b.extend((v as u16).to_le_bytes());
            b
        } else {
            let mut b = vec![252];
            b.extend(v.to_le_bytes());
            b
        }
    }
    let mut payload = varint(13);
    payload.extend(varint(pane.len() as u32));
    payload.extend(pane.as_bytes());
    payload.extend(varint(1));
    payload.extend(varint(if paste { 3 } else { 1 }));
    payload.extend(varint(text.len() as u32));
    payload.extend(text.as_bytes());
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend(payload);
    s.write_all(&frame).unwrap();
    s.flush().unwrap();
}

/// Pi's cut answer (`result`, `reason?`, `principal?`, `mac`); the MAC must verify.
pub fn classification(r: &Value) -> &str {
    assert!(r.get("error").is_none(), "cut failed: {r}");
    let res = &r["result"];
    let kind = res["result"].as_str().expect("cut answer needs result");
    let req = &r["_request"];
    let num = |v: &Value| v.as_u64().map(|n| n.to_string()).unwrap();
    let message = fields(&[
        "herdr-cut-v1",
        req["epoch"].as_str().unwrap(),
        &num(&req["seq"]),
        req["token"].as_str().unwrap(),
        &num(&req["cut"]),
        req["digest"].as_str().unwrap(),
        req["kind"].as_str().unwrap(),
        kind,
        res["reason"].as_str().unwrap_or(""),
        res["principal"]["smarty_id"].as_str().unwrap_or(""),
        res["principal"]["display_name"].as_str().unwrap_or(""),
    ]);
    let key = ring::hmac::Key::new(
        ring::hmac::HMAC_SHA256,
        &unhex(req["epoch_key"].as_str().unwrap()),
    );
    ring::hmac::verify(&key, &message, &unhex(res["mac"].as_str().expect("mac")))
        .expect("cut answer MAC verifies under the epoch key");
    kind
}
pub fn unknown(r: &Value) {
    assert_eq!(classification(r), "unknown", "{r}");
    assert!(
        r["result"]["reason"].is_string(),
        "unknown must explain: {r}"
    );
}
