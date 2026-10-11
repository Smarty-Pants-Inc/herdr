use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

// Only process identity is used here; snapshot naming helpers belong to diagnostics.
#[allow(dead_code)]
#[path = "../../src/platform/diagnostic_owner.rs"]
mod diagnostic_owner;

// Re-registration transfers cleanup ownership to the latest registering thread.
static PID_REGISTRY: OnceLock<Mutex<HashMap<u32, thread::ThreadId>>> = OnceLock::new();
static RUNTIME_DIR_REGISTRY: OnceLock<Mutex<HashMap<PathBuf, thread::ThreadId>>> = OnceLock::new();
static INIT: Once = Once::new();
static CLEANUP_GUARD: OnceLock<CleanupGuard> = OnceLock::new();
const WATCHDOG_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const RUNTIME_OWNER_MARKER: &str = ".herdr-test-owner-pid";
pub const CURRENT_PROTOCOL: u32 = 22;
pub const CURRENT_ENDPOINT_PROTOCOL_GENERATION: u32 = 1;
pub const SERVER_MESSAGE_SERVER_SHUTDOWN: u32 = 3;
pub const SERVER_MESSAGE_ENDPOINT_CONTROL: u32 = 20;
pub const SERVER_MESSAGE_PANE_SURFACE: u32 = 13;
pub const SERVER_MESSAGE_SEMANTIC_NOTIFICATION: u32 = 14;
pub const SERVER_MESSAGE_PANE_SURFACE_PATCH: u32 = 19;
const CLIENT_MESSAGE_CLIENT_SHELL_PANE_INPUT: u32 = 13;
const CLIENT_MESSAGE_CLIENT_SHELL_FOCUS: u32 = 18;
const CLIENT_MESSAGE_ENDPOINT_CONTROL: u32 = 20;

pub fn isolate_herdr_test_process(command: &mut CommandBuilder) {
    command.env_remove("HERDR_STARTUP_CWD");
    command.env_remove("HERDR_SESSION");
}

pub fn register_spawned_herdr_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    ensure_cleanup_hooks();
    let mut registry = pid_registry_lock();
    registry.insert(pid, thread::current().id());
}

/// Asks a spawned herdr process to exit. portable-pty's `kill` sends SIGHUP,
/// which the server deliberately ignores.
pub fn stop_spawned_herdr(child: &mut (dyn portable_pty::Child + Send + Sync)) {
    match child.process_id() {
        Some(pid) => unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        },
        None => {
            let _ = child.kill();
        }
    }
}

pub fn unregister_spawned_herdr_pid(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    if let Some(registry) = PID_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(&pid);
    }
}

pub fn register_runtime_dir(path: &Path) {
    ensure_cleanup_hooks();

    let _ = fs::create_dir_all(path);
    // Every spawn registers its runtime dir again. Replace the marker atomically: the watchdog
    // reads it concurrently, and a truncated marker makes it SIGTERM this test's live server.
    let pid = std::process::id().to_string();
    let staged = path.join(format!("{RUNTIME_OWNER_MARKER}.{pid}.tmp"));
    let _ = fs::write(&staged, &pid)
        .and_then(|()| fs::rename(&staged, path.join(RUNTIME_OWNER_MARKER)));

    let mut runtime_dirs = runtime_dir_registry_lock();
    runtime_dirs.insert(path.to_path_buf(), thread::current().id());
}

pub fn unregister_runtime_dir(path: &Path) {
    if let Some(registry) = RUNTIME_DIR_REGISTRY.get() {
        let mut guard = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.remove(path);
    }
}

#[cfg(target_os = "linux")]
pub fn herdr_server_pids_for_runtime_dir(runtime_dir: &Path) -> std::io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for pid in iter_worktree_server_pids()? {
        let Some(process_runtime_dir) = process_runtime_dir(pid)? else {
            continue;
        };
        if process_runtime_dir == runtime_dir {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

pub fn cleanup_test_base(base: &Path) {
    let runtime_dir = base.join("runtime");
    let runtime_dirs = HashSet::from([runtime_dir.clone()]);

    terminate_servers_for_runtime_dirs(&runtime_dirs);
    unregister_runtime_dir(&runtime_dir);
    let _ = fs::remove_dir_all(base);
}

/// Test-scoped ownership of a server and every detached handoff importer. The
/// wrapper records its own PID/start identity before exec, including when the CLI
/// or an assertion fails before it can return. No process-name or /proc discovery
/// is needed on any Unix host. Paths are removed only after all owners terminate.
pub struct ScopedHandoffServer {
    base: PathBuf,
    socket: PathBuf,
    socket_identity: Option<(u64, u64)>,
    owners: Vec<(u32, String)>,
    importer_records: Vec<PathBuf>,
    diagnostics: Option<PathBuf>,
    importer_env: Vec<(String, String)>,
    importer_target: PathBuf,
    captures: Vec<StderrCapture>,
}

fn process_start_identity(pid: u32) -> std::io::Result<Option<String>> {
    diagnostic_owner::diagnostic_owner_identity(pid)
}

fn owned_process_running(pid: u32, identity: &str) -> std::io::Result<bool> {
    match process_start_identity(pid)? {
        None => Ok(false),
        Some(current) if current == identity => Ok(true),
        Some(_) => Err(std::io::Error::other(format!(
            "PID {pid} has a different kernel owner; preserving runtime"
        ))),
    }
}

// Run in the current integration executable, then keep the same PID across exec.
// Separate environment entries preserve even non-UTF8 arguments without letting
// libtest interpret Herdr's flags. They are removed from the final environment.
#[cfg(test)]
#[test]
#[ignore = "only invoked by the scoped handoff importer wrapper"]
fn handoff_importer_exec() {
    use std::os::unix::process::CommandExt;
    let record = PathBuf::from(std::env::var_os("H4609_IMPORTER_RECORD").expect("owner record"));
    let count: usize = std::env::var("H4609_IMPORTER_ARG_COUNT")
        .expect("argument count")
        .parse()
        .expect("numeric argument count");
    let target = std::env::var_os("H4609_IMPORTER_TARGET").expect("importer target");
    let mut command = std::process::Command::new(target);
    command.env_remove("H4609_IMPORTER_TARGET");
    command.env_remove("H4609_IMPORTER_RECORD");
    command.env_remove("H4609_IMPORTER_ARG_COUNT");
    for index in 0..count {
        let key = format!("H4609_IMPORTER_ARG_{index}");
        command.arg(std::env::var_os(&key).expect("importer argument"));
        command.env_remove(key);
    }
    let pid = std::process::id();
    let identity = process_start_identity(pid).unwrap().expect("live importer");
    let staged = record.with_extension("tmp");
    fs::write(&staged, format!("{pid}\n{identity}\n")).expect("stage importer identity");
    fs::rename(staged, record).expect("publish importer identity before exec");
    // Publish the native owner before opening stderr, so cleanup can stop this
    // same PID even if diagnostics setup or the final exec fails.
    if let Some(start) = std::env::var_os("H4609_IMPORTER_START") {
        let uptime = fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|text| text.split_whitespace().next().map(str::to_owned))
            .unwrap_or_else(|| "unavailable".into());
        fs::write(start, format!("pid={pid}\nuptime={uptime}\n")).expect("importer start receipt");
    }
    if let Some(fifo) = std::env::var_os("H4609_IMPORTER_STDERR") {
        use std::os::fd::AsRawFd;
        // Read-write never blocks, even if the drainer has not opened it yet.
        let capture = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(fifo)
            .expect("open importer stderr capture");
        assert_eq!(
            unsafe { libc::dup2(capture.as_raw_fd(), libc::STDERR_FILENO) },
            libc::STDERR_FILENO
        );
    }
    command.env_remove("H4609_IMPORTER_START");
    command.env_remove("H4609_IMPORTER_STDERR");
    panic!("exec importer failed: {}", command.exec());
}

pub fn test_process_running(pid: u32) -> bool {
    process_start_identity(pid)
        .expect("inspect exact test PID")
        .is_some()
}

impl ScopedHandoffServer {
    pub fn new(base: &Path) -> Self {
        Self {
            base: base.to_path_buf(),
            socket: base.join("runtime/herdr.sock"),
            socket_identity: None,
            owners: Vec::new(),
            importer_records: Vec::new(),
            diagnostics: None,
            importer_env: Vec::new(),
            importer_target: PathBuf::from(env!("CARGO_BIN_EXE_herdr")),
            captures: Vec::new(),
        }
    }

    /// Writes each later importer's stderr and start receipt into `dir`, which
    /// lives outside the fixture so it survives cleanup (see `HandoffFailureBundle`).
    pub fn set_diagnostics(&mut self, dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        self.diagnostics = Some(dir.to_path_buf());
    }

    /// Environment the wrapper exports before exec, for tests of failure capture.
    pub fn set_importer_env(&mut self, key: &str, value: &str) {
        self.importer_env.push((key.to_owned(), value.to_owned()));
    }

    /// Program the wrapper execs instead of Herdr, for tests of the capture itself.
    pub fn set_importer_target(&mut self, target: &Path) {
        self.importer_target = target.to_path_buf();
    }

    pub fn track_original(&mut self, pid: u32) {
        let identity = process_start_identity(pid)
            .unwrap()
            .expect("live original server");
        self.owners.push((pid, identity));
        self.refresh_socket_identity();
    }

    fn refresh_socket_identity(&mut self) {
        use std::os::unix::fs::MetadataExt;
        self.socket_identity = fs::metadata(&self.socket).ok().map(|m| (m.dev(), m.ino()));
    }

    pub fn importer_exe(&mut self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let index = self.importer_records.len();
        let record = self.base.join(format!("importer-{index}.owner"));
        let wrapper = self.base.join(format!("importer-{index}.sh"));
        fs::create_dir_all(&self.base).unwrap();
        // Resolve the runner before creating a capture that must be joined.
        let runner = std::env::current_exe().unwrap();
        let capture_count = self.captures.len();
        let quote =
            |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let mut diagnostics = String::new();
        if let Some(dir) = &self.diagnostics {
            // stderr (wrapper errors, panics, the importer's returned error) goes to a
            // per-importer file; exec keeps the importer PID equal to the spawned child.
            // A drainer thread in this test process keeps only the last
            // IMPORTER_STDERR_CAP_BYTES (at most twice that while it runs), so a
            // noisy importer neither fills the disk nor blocks. It is outside the importer's process
            // group, so a rollback SIGKILL cannot discard bytes already written.
            let start = dir.join(format!("importer-{index}.start"));
            let fifo = self.base.join(format!("importer-{index}.stderr.fifo"));
            // A wrapper without bounded capture would break the cap; fail loudly.
            let capture =
                spawn_capped_capture(&fifo, &dir.join(format!("importer-{index}.stderr")))
                    .expect("create bounded importer stderr capture");
            self.captures.push(capture);
            diagnostics = format!(
                "export H4609_IMPORTER_START={}\nexport H4609_IMPORTER_STDERR={}\n",
                quote(&start),
                quote(&fifo),
            );
        }
        for (key, value) in &self.importer_env {
            diagnostics.push_str(&format!("export {key}={}\n", quote(Path::new(value))));
        }
        // Register only after capture setup succeeds, but before the wrapper can
        // be spawned: failure cleanup also reads this record.
        self.importer_records.push(record.clone());
        let built = fs::write(&wrapper, format!(
            "#!/bin/sh\nset -eu\n{diagnostics}export H4609_IMPORTER_RECORD={}\nexport H4609_IMPORTER_TARGET={}\ni=0\nfor arg do\n  export \"H4609_IMPORTER_ARG_$i=$arg\"\n  i=$((i + 1))\ndone\nexport H4609_IMPORTER_ARG_COUNT=\"$i\"\nexec {} --exact support::handoff_importer_exec --ignored --nocapture\n",
            quote(&record), quote(&self.importer_target), quote(&runner)
        )).and_then(|()| fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)));
        if let Err(error) = built {
            // No wrapper was returned, so none of these newly created resources
            // can have a live importer. Roll them back in reverse order here;
            // normal cleanup must still fail closed on missing returned records.
            let _ = fs::remove_file(&wrapper);
            self.importer_records.pop();
            let _ = fs::remove_file(&record);
            for capture in self.captures.drain(capture_count..).rev() {
                capture.finish();
            }
            if let Some(dir) = &self.diagnostics {
                let _ = fs::remove_file(dir.join(format!("importer-{index}.stderr")));
                let _ = fs::remove_file(self.base.join(format!("importer-{index}.stderr.fifo")));
            }
            panic!("construct importer wrapper: {error}");
        }
        wrapper
    }

    /// Discards the latest wrapper only when the caller knows it was never
    /// spawned. Removing the executable prevents a later accidental launch;
    /// ordinary cleanup must still fail closed on absent owner records.
    pub fn discard_unspawned_importer(&mut self, wrapper: &Path) {
        let index = self
            .importer_records
            .len()
            .checked_sub(1)
            .expect("registered wrapper");
        assert_eq!(wrapper, self.base.join(format!("importer-{index}.sh")));
        assert!(
            !self.importer_records[index].exists(),
            "wrapper already published an owner"
        );
        fs::remove_file(wrapper).expect("remove unspawned wrapper");
        self.importer_records.pop();
    }

    fn read_importers(&mut self) -> std::io::Result<()> {
        for record in &self.importer_records {
            // A registered importer may be alive before publishing. Absence is
            // not evidence of termination; leave all owners and paths intact.
            let text = fs::read_to_string(record)?;
            let (pid, identity) = text
                .split_once('\n')
                .ok_or_else(|| std::io::Error::other("incomplete importer identity"))?;
            let pid: u32 = pid.parse().map_err(std::io::Error::other)?;
            if pid == 0
                || pid > i32::MAX as u32
                || pid == std::process::id()
                || identity.trim().is_empty()
                || identity.trim().contains(char::is_whitespace)
            {
                return Err(std::io::Error::other("invalid importer identity"));
            }
            let owner = (pid, identity.trim().to_owned());
            if !self.owners.contains(&owner) {
                self.owners.push(owner);
            }
        }
        Ok(())
    }

    pub fn track_importer(&mut self) -> u32 {
        self.read_importers().unwrap();
        self.refresh_socket_identity();
        let (pid, identity) = self.owners.last().expect("recorded importer");
        assert_eq!(
            process_start_identity(*pid).unwrap().as_ref(),
            Some(identity)
        );
        eprintln!(
            "owned handoff importer pid={pid} socket={} identity={identity}",
            self.socket.display()
        );
        *pid
    }

    pub fn stop_and_cleanup(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        self.read_importers()?;
        // Validate all owners before socket shutdown as well as before signals.
        // Unreadable or recycled PIDs cannot authorize removal of their paths.
        let mut live_owner = false;
        for (pid, identity) in &self.owners {
            live_owner |= owned_process_running(*pid, identity)?;
        }
        // Only address the exact private socket generation observed by this test.
        // On a pre-refresh failure, bounded PID/identity cleanup still works.
        let same_socket = fs::metadata(&self.socket)
            .ok()
            .is_some_and(|m| self.socket_identity == Some((m.dev(), m.ino())));
        if same_socket && live_owner {
            if let Ok(mut stream) = UnixStream::connect(&self.socket) {
                stream.set_write_timeout(Some(Duration::from_secs(2)))?;
                let _ = stream.write_all(
                    b"{\"id\":\"scoped-cleanup\",\"method\":\"server.stop\",\"params\":{}}\n",
                );
            }
        }
        // A wrapper may publish its record while earlier owners are stopped; rescan
        // until no new owner appears, so none is left running when paths are removed.
        let mut stopped = 0;
        loop {
            self.terminate_owners(stopped)?;
            stopped = self.owners.len();
            self.read_importers()?;
            if self.owners.len() == stopped {
                break;
            }
        }
        self.owners.clear();
        self.importer_records.clear();
        // Every owner is gone: complete each capture before its bundle is read.
        for capture in self.captures.drain(..) {
            capture.finish();
        }
        unregister_runtime_dir(&self.base.join("runtime"));
        if self.base.exists() {
            fs::remove_dir_all(&self.base)?;
        }
        Ok(())
    }

    /// Stops `owners[from..]`, each only while its recorded start identity holds.
    fn terminate_owners(&self, from: usize) -> std::io::Result<()> {
        for (pid, identity) in &self.owners[from..] {
            for signal in [libc::SIGTERM, libc::SIGKILL] {
                if !owned_process_running(*pid, identity)? {
                    break;
                }
                unsafe {
                    libc::kill(*pid as libc::pid_t, signal);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if !owned_process_running(*pid, identity)? {
                        break;
                    }
                    thread::sleep(Duration::from_millis(25));
                }
            }
            if owned_process_running(*pid, identity)? {
                return Err(std::io::Error::other(format!(
                    "owned server {pid} did not terminate; preserving runtime"
                )));
            }
            eprintln!("verified owned server pid={pid} terminated before runtime removal");
        }
        Ok(())
    }
}

impl Drop for ScopedHandoffServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_cleanup() {
            // Never double-panic on assertion failure, and never unlink a live owner's paths.
            eprintln!("scoped handoff cleanup failed: {error}");
        }
    }
}

struct StderrCapture {
    fifo: PathBuf,
    drainer: thread::JoinHandle<()>,
}

impl StderrCapture {
    /// Unblocks a drainer whose wrapper never ran, then joins it once every
    /// writer has closed. A descendant that still holds stderr is reported, not
    /// waited for indefinitely.
    fn finish(self) {
        use std::os::unix::fs::OpenOptionsExt;
        // Opening the write end releases a reader blocked in open(); it then reads
        // EOF. ENXIO means no reader is in open() yet, so retry until it finishes;
        // NotFound means it already opened the FIFO and removed the name.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.drainer.is_finished() && Instant::now() < deadline {
            drop(
                fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&self.fifo),
            );
            thread::sleep(Duration::from_millis(10));
        }
        if self.drainer.is_finished() {
            let _ = self.drainer.join();
        } else {
            eprintln!(
                "importer stderr capture {} still has a writer after cleanup",
                self.fifo.display()
            );
        }
    }
}

/// Streams a new FIFO at `fifo` into `out` until every writer closes, keeping
/// only the last `IMPORTER_STDERR_CAP_BYTES` (the rule for copied logs). The
/// file never exceeds twice the cap, and bytes are on disk as they arrive.
fn spawn_capped_capture(fifo: &Path, out: &Path) -> std::io::Result<StderrCapture> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(fifo.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Read access lets keep_tail rewrite the file in place.
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = fs::remove_file(fifo);
            return Err(error);
        }
    };
    let path = fifo.to_path_buf();
    let fifo = path.clone();
    let drainer = thread::Builder::new()
        .name("importer-stderr-capture".into())
        .spawn(move || {
            // Blocks until the wrapper opens it; then the name is no longer needed.
            let Ok(reader) = fs::File::open(&fifo) else {
                return;
            };
            let _ = fs::remove_file(&fifo);
            let mut buf = vec![0; 64 * 1024];
            let mut len = 0;
            loop {
                let n = match (&reader).read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                // On a write error keep draining: the importer must never block.
                if file.write_all(&buf[..n]).is_ok() {
                    len += n as u64;
                }
                if len >= 2 * IMPORTER_STDERR_CAP_BYTES {
                    len = keep_tail(&mut file, len).unwrap_or(len);
                }
            }
            let _ = keep_tail(&mut file, len);
        });
    let drainer = match drainer {
        Ok(drainer) => drainer,
        Err(error) => {
            let _ = fs::remove_file(out);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
    };
    Ok(StderrCapture {
        fifo: path,
        drainer,
    })
}

/// Rewrites `file` (`len` bytes, cursor at its end) to its last
/// `IMPORTER_STDERR_CAP_BYTES`; returns the new length.
fn keep_tail(file: &mut fs::File, len: u64) -> std::io::Result<u64> {
    use std::io::{Seek, SeekFrom};
    if len <= IMPORTER_STDERR_CAP_BYTES {
        return Ok(len);
    }
    let mut tail = Vec::new();
    file.seek(SeekFrom::Start(len - IMPORTER_STDERR_CAP_BYTES))?;
    file.read_to_end(&mut tail)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&tail)?;
    file.set_len(tail.len() as u64)?;
    Ok(tail.len() as u64)
}

/// Root for retained failure bundles: `HERDR_TEST_FAILURE_ARTIFACT_DIR`, else
/// `target/tmp/herdr-handoff-failures` (CI uploads it only when a job fails).
pub fn failure_artifact_root() -> PathBuf {
    std::env::var_os("HERDR_TEST_FAILURE_ARTIFACT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("herdr-handoff-failures"))
}

/// Failure-only evidence for a live-handoff fixture. Declare it after every
/// process it observes so it drops first: on a panic it snapshots socket files,
/// listener owners, related processes and logs before cleanup kills or removes
/// anything; on success it deletes its directory. It only reads process tables;
/// it never signals or unlinks anything outside its own directory.
pub struct HandoffFailureBundle {
    state: std::rc::Rc<std::cell::RefCell<BundleState>>,
}

struct BundleState {
    dir: PathBuf,
    fixtures: Vec<BundleFixture>,
    log_dirs: Vec<PathBuf>,
    snapshotted: bool,
}

thread_local! {
    // The panic hook kills this thread's registered servers before unwinding
    // reaches any Drop, so it snapshots the thread's bundles first.
    static ACTIVE_BUNDLES: std::cell::RefCell<Vec<std::rc::Rc<std::cell::RefCell<BundleState>>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn snapshot_active_bundles() {
    let _ = ACTIVE_BUNDLES.try_with(|bundles| {
        let Ok(bundles) = bundles.try_borrow() else {
            return;
        };
        for bundle in bundles.iter() {
            if let Ok(mut state) = bundle.try_borrow_mut() {
                state.snapshot();
            }
        }
    });
}

struct BundleFixture {
    label: String,
    base: PathBuf,
    runtime: PathBuf,
    original: Option<u32>,
}

const BUNDLE_LOG_TAIL_BYTES: u64 = 1 << 20;
/// Keeps the tail of importer stderr: the returned error and any panic come last.
pub const IMPORTER_STDERR_CAP_BYTES: u64 = 1 << 20;

impl HandoffFailureBundle {
    /// A fresh directory per process and nextest attempt; never reused.
    pub fn new(root: &Path, test: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let run = std::env::var("NEXTEST_RUN_ID").unwrap_or_else(|_| "local".into());
        let attempt = std::env::var("NEXTEST_ATTEMPT").unwrap_or_else(|_| "1".into());
        let dir = root.join(format!(
            "{test}-run-{run}-attempt-{attempt}-pid-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        ensure_cleanup_hooks();
        let state = std::rc::Rc::new(std::cell::RefCell::new(BundleState {
            dir,
            fixtures: Vec::new(),
            log_dirs: Vec::new(),
            snapshotted: false,
        }));
        ACTIVE_BUNDLES.with(|bundles| bundles.borrow_mut().push(state.clone()));
        let bundle = Self { state };
        bundle.note(&format!(
            "bundle created test={test} run={run} attempt={attempt}"
        ));
        bundle
    }

    pub fn dir(&self) -> PathBuf {
        self.state.borrow().dir.clone()
    }

    /// Observes a `ScopedHandoffServer` fixture and routes its importer stderr here.
    pub fn watch(&mut self, label: &str, server: &mut ScopedHandoffServer, original: Option<u32>) {
        let mut state = self.state.borrow_mut();
        server.set_diagnostics(&state.dir.join(label));
        state.fixtures.push(BundleFixture {
            label: label.to_owned(),
            base: server.base.clone(),
            runtime: server.base.join("runtime"),
            original,
        });
    }

    /// Copies the tail of every `*.log*` file in `dir` on failure.
    pub fn copy_logs_from(&mut self, dir: &Path) {
        self.state.borrow_mut().log_dirs.push(dir.to_path_buf());
    }

    /// Appends an ordered, timestamped phase receipt.
    pub fn note(&self, text: &str) {
        self.state.borrow().note(text);
    }
}

impl BundleState {
    fn note(&self, text: &str) {
        let line = format!("{} {text}\n", clock_stamp());
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("receipts.txt"))
            .and_then(|mut file| file.write_all(line.as_bytes()));
    }

    fn snapshot(&mut self) {
        if std::mem::replace(&mut self.snapshotted, true) {
            return;
        }
        let mut report = format!(
            "{} failure snapshot (before scoped cleanup)\n",
            clock_stamp()
        );
        let mut runtimes = Vec::new();
        for fixture in &self.fixtures {
            let importers = recorded_importers(&fixture.base);
            report.push_str(&format!(
                "\n[{}] runtime={} original={:?} intended_importers={importers:?}\n",
                fixture.label,
                fixture.runtime.display(),
                fixture
                    .original
                    .map(|pid| (pid, process_start_identity(pid).ok().flatten())),
            ));
            for (pid, identity) in &importers {
                let live = process_start_identity(*pid).ok().flatten();
                report.push_str(&format!(
                    "importer pid={pid} recorded_start={identity:?} now={}\n",
                    match live {
                        Some(now) if &now == identity => "alive (same start identity)".into(),
                        Some(now) => format!("pid reused by start={now:?}"),
                        None => "exited (exit status: see source server log 'reaped')".into(),
                    }
                ));
            }
            report.push_str(&runtime_socket_files(&fixture.runtime));
            runtimes.push(fixture.runtime.clone());
            let _ = copy_dir_files(&fixture.base, &self.dir.join(&fixture.label), |name| {
                name.starts_with("importer-") && name.ends_with(".owner")
            });
        }
        report.push_str(&listener_attribution(&runtimes, &self.fixtures));
        report.push_str(&related_processes(&runtimes));
        let _ = fs::write(self.dir.join("attribution.txt"), report);
        for (index, dir) in self.log_dirs.iter().enumerate() {
            let _ = copy_dir_files(dir, &self.dir.join(format!("logs-{index}")), |name| {
                name.contains(".log")
            });
            self.note(&format!("logs-{index} copied from {}", dir.display()));
        }
    }
}

impl Drop for HandoffFailureBundle {
    fn drop(&mut self) {
        let _ = ACTIVE_BUNDLES.try_with(|bundles| {
            bundles
                .borrow_mut()
                .retain(|bundle| !std::rc::Rc::ptr_eq(bundle, &self.state));
        });
        let mut state = self.state.borrow_mut();
        if thread::panicking() {
            state.snapshot();
            eprintln!("handoff failure bundle retained: {}", state.dir.display());
        } else {
            let _ = fs::remove_dir_all(&state.dir);
        }
    }
}

/// `uptime` shares the clock with the importer wrapper's `/proc/uptime` receipt.
fn clock_stamp() -> String {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let uptime = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unavailable".into());
    format!("epoch={epoch:.6} uptime={uptime}")
}

fn recorded_importers(base: &Path) -> Vec<(u32, String)> {
    let mut importers = Vec::new();
    for index in 0.. {
        let Ok(text) = fs::read_to_string(base.join(format!("importer-{index}.owner"))) else {
            if base.join(format!("importer-{index}.sh")).exists() {
                continue;
            }
            break;
        };
        if let Some((pid, identity)) = text.split_once('\n') {
            if let Ok(pid) = pid.parse() {
                importers.push((pid, identity.trim().to_owned()));
            }
        }
    }
    importers
}

fn runtime_socket_files(runtime: &Path) -> String {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut out = String::from("runtime entries (public, parked .handoff-*, .recover):\n");
    let Ok(entries) = fs::read_dir(runtime) else {
        out.push_str("  unavailable: runtime directory missing\n");
        return out;
    };
    let mut lines: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = fs::symlink_metadata(entry.path()).ok()?;
            let kind = if meta.file_type().is_socket() {
                "socket"
            } else if meta.is_dir() {
                "dir"
            } else {
                "file"
            };
            Some(format!(
                "  {} {kind} dev={} ino={}\n",
                entry.path().display(),
                meta.dev(),
                meta.ino()
            ))
        })
        .collect();
    lines.sort();
    out.extend(lines);
    out
}

#[cfg(target_os = "linux")]
fn listener_attribution(runtimes: &[PathBuf], fixtures: &[BundleFixture]) -> String {
    let mut out = String::from(
        "\nlisteners bound under the runtimes (/proc/net/unix; path is the bind-time name):\n",
    );
    let Ok(table) = fs::read_to_string("/proc/net/unix") else {
        out.push_str("  unavailable: cannot read /proc/net/unix\n");
        return out;
    };
    let mut sockets = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 || !runtimes.iter().any(|r| Path::new(fields[7]).starts_with(r)) {
            continue;
        }
        // Flags 00010000 is __SO_ACCEPTCON: a listening socket.
        let listening = u32::from_str_radix(fields[3], 16).is_ok_and(|f| f & 0x10000 != 0);
        sockets.push((fields[6].to_owned(), fields[7].to_owned(), listening));
    }
    let mut holders: HashMap<String, Vec<u32>> = HashMap::new();
    if let Ok(procs) = fs::read_dir("/proc") {
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            for fd in fds.flatten() {
                if let Ok(target) = fs::read_link(fd.path()) {
                    let target = target.to_string_lossy();
                    if let Some(inode) = target
                        .strip_prefix("socket:[")
                        .and_then(|t| t.strip_suffix(']'))
                    {
                        if sockets.iter().any(|(i, _, _)| i == inode) {
                            let pids = holders.entry(inode.to_owned()).or_default();
                            if !pids.contains(&pid) {
                                pids.push(pid);
                            }
                        }
                    }
                }
            }
        }
    }
    for (inode, path, listening) in sockets {
        let state = if listening { "LISTEN" } else { "conn" };
        let pids = holders.get(&inode).cloned().unwrap_or_default();
        if pids.is_empty() {
            out.push_str(&format!(
                "  {state} sock_inode={inode} {path} holder=unattributed (no readable fd)\n"
            ));
        }
        for pid in pids {
            let role = fixtures
                .iter()
                .find_map(|f| {
                    if f.original == Some(pid) {
                        return Some(format!("original source server ({})", f.label));
                    }
                    recorded_importers(&f.base)
                        .iter()
                        .any(|(p, id)| {
                            *p == pid
                                && process_start_identity(pid).ok().flatten().as_ref() == Some(id)
                        })
                        .then(|| format!("intended importer ({})", f.label))
                })
                .unwrap_or_else(|| "OTHER process (not an owned server/importer)".into());
            out.push_str(&format!(
                "  {state} sock_inode={inode} {path} pid={pid} role={role} {}\n",
                process_summary(pid)
            ));
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn listener_attribution(_runtimes: &[PathBuf], _fixtures: &[BundleFixture]) -> String {
    "\nlistener attribution unavailable: no /proc/net/unix on this platform\n".into()
}

#[cfg(target_os = "linux")]
fn process_summary(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // Fields after the parenthesized command: state ppid pgrp ...
    let rest: Vec<&str> = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default();
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            let mut args: Vec<String> = bytes
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect();
            // `--handoff-import <socket> <token>`: never record the token.
            if let Some(at) = args.iter().position(|arg| arg == "--handoff-import") {
                if let Some(token) = args.get_mut(at + 2) {
                    *token = "<token redacted>".into();
                }
            }
            args.join(" ")
        })
        .unwrap_or_else(|_| "unavailable".into());
    format!(
        "state={} ppid={} pgid={} start={:?} cmd={cmdline}",
        rest.first().unwrap_or(&"?"),
        rest.get(1).unwrap_or(&"?"),
        rest.get(2).unwrap_or(&"?"),
        process_start_identity(pid).ok().flatten()
    )
}

#[cfg(target_os = "linux")]
fn related_processes(runtimes: &[PathBuf]) -> String {
    let mut out = String::from("\nprocesses whose XDG_RUNTIME_DIR/HERDR_SOCKET_PATH is a watched runtime (servers, importers, bridges, auto-started daemons):\n");
    let Ok(procs) = fs::read_dir("/proc") else {
        out.push_str("  unavailable: cannot read /proc\n");
        return out;
    };
    let mut pids: Vec<u32> = procs
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .filter(|pid| {
            process_runtime_dir(*pid)
                .ok()
                .flatten()
                .is_some_and(|dir| runtimes.contains(&dir))
        })
        .collect();
    pids.sort_unstable();
    for pid in pids {
        out.push_str(&format!("  pid={pid} {}\n", process_summary(pid)));
    }
    out
}

#[cfg(not(target_os = "linux"))]
fn related_processes(_runtimes: &[PathBuf]) -> String {
    "\nrelated process listing unavailable: no /proc on this platform\n".into()
}

/// Copies the bounded tail of matching regular files; never follows into subdirectories.
fn copy_dir_files(from: &Path, to: &Path, keep: impl Fn(&str) -> bool) -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom};
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !keep(&name) || !entry.file_type()?.is_file() {
            continue;
        }
        let mut file = fs::File::open(entry.path())?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(BUNDLE_LOG_TAIL_BYTES)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        fs::write(to.join(name), bytes)?;
    }
    Ok(())
}

pub fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

pub fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("file did not appear at {}", path.display());
}

fn encode_varint_u32(v: u32) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else if v < 65536 {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&(v as u16).to_le_bytes());
        buf
    } else {
        let mut buf = vec![252u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

fn encode_varint_u16(v: u16) -> Vec<u8> {
    if v < 251 {
        vec![v as u8]
    } else {
        let mut buf = vec![251u8];
        buf.extend_from_slice(&v.to_le_bytes());
        buf
    }
}

fn frame_message(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut framed = len.to_le_bytes().to_vec();
    framed.extend_from_slice(payload);
    framed
}

fn decode_varint_u32(payload: &[u8], offset: usize) -> Result<(u32, usize), String> {
    if offset >= payload.len() {
        return Err("payload too short for varint".into());
    }
    let first_byte = payload[offset];
    match first_byte {
        0..=250 => Ok((first_byte as u32, 1)),
        251 => {
            if offset + 3 > payload.len() {
                return Err("payload too short for u16 varint".into());
            }
            let v = u16::from_le_bytes(
                payload[offset + 1..offset + 3]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v as u32, 3))
        }
        252 => {
            if offset + 5 > payload.len() {
                return Err("payload too short for u32 varint".into());
            }
            let v = u32::from_le_bytes(
                payload[offset + 1..offset + 5]
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
            );
            Ok((v, 5))
        }
        _ => Err(format!("unsupported varint tag: {first_byte}")),
    }
}

fn encode_varint_enum(variant_idx: u32, fields: &[&[u8]]) -> Vec<u8> {
    let mut buf = encode_varint_u32(variant_idx);
    for field in fields {
        buf.extend_from_slice(field);
    }
    buf
}

fn encode_string(value: &str) -> Vec<u8> {
    let mut encoded = encode_varint_u32(value.len() as u32);
    encoded.extend_from_slice(value.as_bytes());
    encoded
}

fn decode_string(payload: &[u8], offset: &mut usize) -> Result<String, String> {
    let (len, consumed) = decode_varint_u32(payload, *offset)?;
    *offset += consumed;
    let len = len as usize;
    if *offset + len > payload.len() {
        return Err("payload too short for string content".into());
    }
    let value = String::from_utf8(payload[*offset..*offset + len].to_vec())
        .map_err(|err| err.to_string())?;
    *offset += len;
    Ok(value)
}

fn decode_welcome(payload: &[u8]) -> Result<(u32, Option<String>), String> {
    let mut offset = 0;
    let (variant, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;
    if variant != 0 {
        return Err(format!(
            "expected Welcome (variant 0), got variant {variant}"
        ));
    }

    let (version, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    let (_encoding, consumed) = decode_varint_u32(payload, offset)?;
    offset += consumed;

    if offset >= payload.len() {
        return Err("payload too short for Option tag".into());
    }
    let option_tag = payload[offset];
    offset += 1;

    let error = if option_tag == 1 {
        let (str_len, consumed) = decode_varint_u32(payload, offset)?;
        offset += consumed;
        let str_len = str_len as usize;
        if offset + str_len > payload.len() {
            return Err("payload too short for string content".into());
        }
        Some(
            String::from_utf8(payload[offset..offset + str_len].to_vec())
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };

    Ok((version, error))
}

fn read_handshake_response(
    stream: &mut UnixStream,
    hello_payload: &[u8],
) -> Result<Vec<u8>, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&frame_message(hello_payload))
        .map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(format!("oversized response: {len}"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).map_err(|e| e.to_string())?;
    Ok(payload)
}

pub fn client_handshake(
    stream: &mut UnixStream,
    version: u32,
    cols: u16,
    rows: u16,
) -> Result<(u32, Option<String>), String> {
    let hello_payload = encode_varint_enum(
        0,
        &[
            &encode_varint_u32(version),
            &encode_varint_u16(cols),
            &encode_varint_u16(rows),
            &encode_varint_u32(8),  // cell_width_px
            &encode_varint_u32(16), // cell_height_px
            &[0],                   // pixel_mouse = false
        ],
    );
    let response = read_handshake_response(stream, &hello_payload)?;
    decode_welcome(&response)
}

pub fn client_shell_handshake(
    stream: &mut UnixStream,
    endpoint_generation: u32,
    surface_cols: u16,
    surface_rows: u16,
) -> Result<(u32, Option<String>), String> {
    let data = serde_json::json!({
        "generation": endpoint_generation,
        "cell_width_px": 8,
        "cell_height_px": 16,
        "surface_size": {"cols": surface_cols, "rows": surface_rows},
        "pixel_mouse": false,
        "direct_graphics": false,
        "endpoint_keybindings": false,
        "mouse_capture": false,
        "snapshot_codecs": ["shell.snapshot.v1"],
        "surface_codecs": ["shell.surface.v1"],
        "input_codecs": ["shell.input.semantic.v1"],
        "blob_codecs": ["shell.blob.v1"]
    })
    .to_string();
    let hello_payload = encode_varint_enum(
        CLIENT_MESSAGE_ENDPOINT_CONTROL,
        &[&encode_string("endpoint.hello.v1"), &encode_string(&data)],
    );
    let response = read_handshake_response(stream, &hello_payload)?;
    let mut offset = 0;
    let (variant, consumed) = decode_varint_u32(&response, offset)?;
    offset += consumed;
    if variant != SERVER_MESSAGE_ENDPOINT_CONTROL {
        return Err(format!(
            "expected EndpointControl (variant {SERVER_MESSAGE_ENDPOINT_CONTROL}), got variant {variant}"
        ));
    }
    let kind = decode_string(&response, &mut offset)?;
    if kind != "endpoint.welcome.v1" {
        return Err(format!("expected endpoint.welcome.v1, got {kind}"));
    }
    let data = decode_string(&response, &mut offset)?;
    let value: serde_json::Value = serde_json::from_str(&data).map_err(|err| err.to_string())?;
    let generation = value["generation"]
        .as_u64()
        .ok_or_else(|| "endpoint welcome omitted generation".to_owned())?
        as u32;
    let error = value["error"]
        .as_object()
        .and_then(|error| error.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok((generation, error))
}

pub fn read_server_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>), String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .map_err(|e| format!("read length prefix: {e}"))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 2 * 1024 * 1024 {
        return Err(format!("oversized frame: {len} bytes"));
    }
    if len == 0 {
        return Err("zero-length frame".into());
    }

    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .map_err(|e| format!("read payload: {e}"))?;

    let (variant, consumed) = decode_varint_u32(&payload, 0)?;
    Ok((variant, payload[consumed..].to_vec()))
}

pub fn send_client_shell_shift_enter(stream: &mut UnixStream, pane_id: &str) -> Result<(), String> {
    let mut payload = encode_varint_u32(CLIENT_MESSAGE_CLIENT_SHELL_PANE_INPUT);
    payload.extend_from_slice(&encode_varint_u32(pane_id.len() as u32));
    payload.extend_from_slice(pane_id.as_bytes());
    payload.extend_from_slice(&encode_varint_u32(1)); // one pane input event
    payload.extend_from_slice(&encode_varint_u32(0)); // Key
    payload.extend_from_slice(&encode_varint_u32(1)); // Enter
    payload.push(1); // Shift
    payload.extend_from_slice(&encode_varint_u32(0)); // Press
    payload.extend_from_slice(&encode_varint_u16(1));
    payload.push(0); // no shifted codepoint
    payload.push(0); // no generated text
    payload.push(0); // does not track release
    payload.push(0); // no physical key id
    payload.push(0); // no Windows key record

    stream
        .write_all(&frame_message(&payload))
        .map_err(|e| format!("write client shell key: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush client shell key: {e}"))
}

pub fn send_client_shell_focus(stream: &mut UnixStream, focused: bool) -> Result<(), String> {
    let mut payload = encode_varint_u32(CLIENT_MESSAGE_CLIENT_SHELL_FOCUS);
    payload.push(u8::from(focused));
    stream
        .write_all(&frame_message(&payload))
        .map_err(|e| format!("write client shell focus: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("flush client shell focus: {e}"))
}

pub fn send_detach(stream: &mut UnixStream) -> Result<(), String> {
    let detach_payload = encode_varint_u32(4);
    let framed = frame_message(&detach_payload);
    stream
        .write_all(&framed)
        .map_err(|e| format!("write detach: {e}"))?;
    stream.flush().map_err(|e| format!("flush detach: {e}"))?;
    Ok(())
}

pub fn drain_messages(stream: &mut UnixStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    while read_server_message(stream).is_ok() {}
    stream.set_read_timeout(None).unwrap();
}

pub fn wait_until<F>(timeout: Duration, interval: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(interval);
    }
    predicate()
}

pub fn wait_for_message_variant(
    stream: &mut UnixStream,
    timeout: Duration,
    variant: u32,
) -> Result<bool, String> {
    wait_for_message_variants(stream, timeout, &[variant])
}

pub fn wait_for_message_variants(
    stream: &mut UnixStream,
    timeout: Duration,
    variants: &[u32],
) -> Result<bool, String> {
    let read_timeout = Some(Duration::from_millis(200));
    // Darwin can reject resetting the timeout after peer closure with queued data.
    if stream.read_timeout().map_err(|e| e.to_string())? != read_timeout {
        stream
            .set_read_timeout(read_timeout)
            .map_err(|e| e.to_string())?;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((got, _)) if variants.contains(&got) => return Ok(true),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }
    Ok(false)
}

pub fn wait_for_client_shell_bootstrap(
    stream: &mut UnixStream,
    timeout: Duration,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut saw_snapshot = false;
    while Instant::now() < deadline {
        match read_server_message(stream) {
            Ok((SERVER_MESSAGE_ENDPOINT_CONTROL, payload)) => {
                let mut offset = 0;
                if decode_string(&payload, &mut offset).as_deref() == Ok("shell.snapshot.v1") {
                    saw_snapshot = true;
                }
            }
            Ok((SERVER_MESSAGE_PANE_SURFACE, _)) if saw_snapshot => return Ok(()),
            Ok((SERVER_MESSAGE_PANE_SURFACE, _)) => {
                return Err("client shell pane surface arrived before its snapshot".into());
            }
            Ok(_) | Err(_) => {}
        }
    }
    Err(format!(
        "timed out waiting for client shell {}",
        if saw_snapshot {
            "pane surface"
        } else {
            "snapshot"
        }
    ))
}

pub fn wait_for_disconnect(stream: &mut UnixStream, timeout: Duration) -> Result<bool, String> {
    stream.set_nonblocking(true).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut idle_since = None;
    let result = loop {
        match read_server_message(stream) {
            Ok(_) => idle_since = None,
            Err(err)
                if err.to_ascii_lowercase().contains("would block")
                    || err.contains("Resource temporarily unavailable") =>
            {
                let idle_started = *idle_since.get_or_insert_with(Instant::now);
                if idle_started.elapsed() >= Duration::from_millis(200) {
                    break Ok(true);
                }
            }
            Err(_) => break Ok(true),
        }
        if Instant::now() >= deadline {
            break Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    };
    let _ = stream.set_nonblocking(false);
    result
}

pub fn cleanup_registered_herdr_pids() {
    cleanup_registered_herdr_pids_for_thread(None);
    let _ = cleanup_servers_with_missing_runtime_dir();
}

fn cleanup_registered_herdr_pids_for_thread(owner: Option<thread::ThreadId>) {
    let pids: Vec<u32> = {
        let mut registry = pid_registry_lock();
        if let Some(owner) = owner {
            let mut pids = Vec::new();
            registry.retain(|pid, registered_owner| {
                if *registered_owner == owner {
                    pids.push(*pid);
                    false
                } else {
                    true
                }
            });
            pids
        } else {
            registry.drain().map(|(pid, _)| pid).collect()
        }
    };

    for pid in pids {
        terminate_pid(pid);
    }

    let runtime_dirs: HashSet<PathBuf> = {
        let mut runtime_dirs = runtime_dir_registry_lock();
        if let Some(owner) = owner {
            let mut owned_dirs = HashSet::new();
            runtime_dirs.retain(|path, registered_owner| {
                if *registered_owner == owner {
                    owned_dirs.insert(path.clone());
                    false
                } else {
                    true
                }
            });
            owned_dirs
        } else {
            runtime_dirs.drain().map(|(path, _)| path).collect()
        }
    };

    terminate_servers_for_runtime_dirs(&runtime_dirs);
}

fn ensure_cleanup_hooks() {
    INIT.call_once(|| {
        let _ = cleanup_servers_with_missing_runtime_dir();
        start_global_watchdog();

        let _ = CLEANUP_GUARD.set(CleanupGuard);

        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            // Even a caught panic runs this hook. Clean only this thread's registrations;
            // other tests may still own live servers. Process-exit hooks still drain all.
            // Failure bundles snapshot first, while the failing processes still exist.
            snapshot_active_bundles();
            cleanup_registered_herdr_pids_for_thread(Some(thread::current().id()));
            previous_hook(panic_info);
        }));

        let _ = ctrlc::set_handler(|| {
            cleanup_registered_herdr_pids();
            std::process::exit(130);
        });

        unsafe {
            libc::atexit(run_atexit_cleanup);
        }
    });
}

fn pid_registry_lock() -> std::sync::MutexGuard<'static, HashMap<u32, thread::ThreadId>> {
    PID_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn runtime_dir_registry_lock() -> std::sync::MutexGuard<'static, HashMap<PathBuf, thread::ThreadId>>
{
    RUNTIME_DIR_REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn registered_runtime_dirs_snapshot() -> HashSet<PathBuf> {
    if let Some(runtime_dirs) = RUNTIME_DIR_REGISTRY.get() {
        runtime_dirs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    } else {
        HashSet::new()
    }
}

fn should_terminate_runtime_dir(
    runtime_dir: &Path,
    registered_runtime_dirs: &HashSet<PathBuf>,
) -> bool {
    if !registered_runtime_dirs.contains(runtime_dir) {
        return false;
    }

    if !runtime_dir.exists() {
        return true;
    }

    !runtime_dir_owner_alive(runtime_dir)
}

fn start_global_watchdog() {
    thread::spawn(|| loop {
        thread::sleep(WATCHDOG_SCAN_INTERVAL);

        if let Err(err) = cleanup_servers_with_missing_runtime_dir() {
            eprintln!("herdr test cleanup watchdog error: {err}");
        }
    });
}

fn cleanup_servers_with_missing_runtime_dir() -> std::io::Result<()> {
    let registered_runtime_dirs = registered_runtime_dirs_snapshot();
    if registered_runtime_dirs.is_empty() {
        return Ok(());
    }

    for pid in iter_worktree_server_pids()? {
        let Some(runtime_dir) = process_runtime_dir(pid)? else {
            continue;
        };

        if should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs) {
            terminate_pid(pid);
        }
    }

    Ok(())
}

fn terminate_servers_for_runtime_dirs(runtime_dirs: &HashSet<PathBuf>) {
    if runtime_dirs.is_empty() {
        return;
    }

    let Ok(pids) = iter_worktree_server_pids() else {
        return;
    };

    for pid in pids {
        let Ok(runtime_dir) = process_runtime_dir(pid) else {
            continue;
        };

        let Some(runtime_dir) = runtime_dir else {
            continue;
        };

        if runtime_dirs.contains(&runtime_dir) {
            terminate_pid(pid);
        }
    }
}

fn iter_worktree_server_pids() -> std::io::Result<Vec<u32>> {
    let own_pid = std::process::id();
    let mut pids = Vec::new();

    let proc_entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    for entry in proc_entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };

        if pid == own_pid {
            continue;
        }

        if is_test_herdr_server_process(pid) {
            pids.push(pid);
        }
    }

    Ok(pids)
}

fn is_test_herdr_server_process(pid: u32) -> bool {
    let Some(exe_path) = proc_link_target(pid, "exe") else {
        return false;
    };

    if !is_test_herdr_binary(&exe_path) {
        return false;
    }

    let Ok(cmdline) = read_cmdline(pid) else {
        return false;
    };

    cmdline.iter().any(|arg| arg == "server")
}

fn proc_link_target(pid: u32, link: &str) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/{link}")).ok()
}

fn read_cmdline(pid: u32) -> std::io::Result<Vec<String>> {
    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))?;
    Ok(cmdline
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).to_string())
        .collect())
}

fn process_runtime_dir(pid: u32) -> std::io::Result<Option<PathBuf>> {
    let environ = fs::read(format!("/proc/{pid}/environ"))?;

    let mut socket_path: Option<PathBuf> = None;

    for entry in environ.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }

        let kv = String::from_utf8_lossy(entry);
        if let Some(value) = kv.strip_prefix("XDG_RUNTIME_DIR=") {
            return Ok(Some(PathBuf::from(value)));
        }

        if let Some(value) = kv.strip_prefix("HERDR_SOCKET_PATH=") {
            socket_path = Some(PathBuf::from(value));
        }
    }

    Ok(socket_path.and_then(|path| path.parent().map(Path::to_path_buf)))
}

fn runtime_dir_owner_alive(runtime_dir: &Path) -> bool {
    let marker = runtime_dir.join(RUNTIME_OWNER_MARKER);
    let Ok(contents) = fs::read_to_string(marker) else {
        return false;
    };

    let Ok(owner_pid) = contents.trim().parse::<libc::pid_t>() else {
        return false;
    };

    process_exists(owner_pid)
}

fn is_test_herdr_binary(path: &Path) -> bool {
    // /proc resolves executable symlinks. Match only this Cargo build, including
    // custom target directories; binary identity alone never grants ownership.
    static TEST_BINARY: OnceLock<Option<PathBuf>> = OnceLock::new();
    TEST_BINARY
        .get_or_init(|| fs::canonicalize(env!("CARGO_BIN_EXE_herdr")).ok())
        .as_deref()
        .is_some_and(|binary| path == binary)
}

extern "C" fn run_atexit_cleanup() {
    cleanup_registered_herdr_pids();
}

struct CleanupGuard;

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        cleanup_registered_herdr_pids();
    }
}

fn terminate_pid(pid: u32) {
    let pid_t = pid as libc::pid_t;

    if process_exists(pid_t) {
        unsafe {
            libc::kill(pid_t, libc::SIGTERM);
        }
    }

    if wait_for_pid_exit(pid_t, Duration::from_millis(400)) {
        return;
    }

    if process_exists(pid_t) {
        unsafe {
            libc::kill(pid_t, libc::SIGKILL);
        }
    }

    let _ = wait_for_pid_exit(pid_t, Duration::from_secs(2));
}

fn wait_for_pid_exit(pid: libc::pid_t, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if !process_exists(pid) {
            return true;
        }

        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return true;
        }

        if result == -1 {
            match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ECHILD) => {
                    // Not our child (or already reaped elsewhere). Poll /proc existence
                    // until the process is truly gone.
                    if !process_exists(pid) {
                        return true;
                    }
                }
                Some(libc::ESRCH) => return true,
                _ => {
                    if !process_exists(pid) {
                        return true;
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(20));
    }

    !process_exists(pid)
}

fn process_exists(pid: libc::pid_t) -> bool {
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(test)]
mod cleanup_tests;
#[cfg(test)]
mod importer_exec_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_missing_runtime_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "herdr-watchdog-scoping-{label}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn scoped_cleanup_preserves_pid_with_different_start_identity() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let runtime = unique_missing_runtime_dir("different-process");
        let child = ChildGuard(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let mut cleanup = ScopedHandoffServer::new(&runtime);
        cleanup
            .owners
            .push((pid, "not this process start identity".to_owned()));
        assert!(cleanup.stop_and_cleanup().is_err());
        assert!(
            test_process_running(pid),
            "cleanup must not signal a reused/unowned PID"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn importer_wrapper_write_failure_joins_capture_and_unregisters_owner() {
        let capture_threads = || {
            fs::read_dir("/proc/self/task")
                .unwrap()
                .flatten()
                .filter(|task| {
                    fs::read_to_string(task.path().join("comm"))
                        .is_ok_and(|comm| comm.trim() == "importer-stderr")
                })
                .count()
        };
        let before = capture_threads();
        let base = unique_missing_runtime_dir("wrapper-write-failure");
        let fixture = base.join("fixture");
        let artifacts = base.join("artifacts");
        let mut cleanup = ScopedHandoffServer::new(&fixture);
        cleanup.set_diagnostics(&artifacts);
        // A directory at the wrapper path makes the real write fail only after
        // capture setup and owner registration, before any wrapper is returned.
        fs::create_dir_all(fixture.join("importer-0.sh")).unwrap();
        let built =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cleanup.importer_exe()));
        assert!(built.is_err(), "wrapper construction must fail");
        assert!(
            cleanup.importer_records.is_empty(),
            "owner must be unregistered"
        );
        assert!(cleanup.captures.is_empty(), "capture must be consumed");
        assert_eq!(
            capture_threads(),
            before,
            "drainer must be joined, not detached"
        );
        assert!(!fixture.join("importer-0.owner").exists());
        assert!(!fixture.join("importer-0.stderr.fifo").exists());
        assert!(!artifacts.join("importer-0.stderr").exists());
        cleanup.stop_and_cleanup().unwrap();
        assert!(!fixture.exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn watchdog_scoping_does_not_terminate_missing_unregistered_runtime_dir() {
        let runtime_dir = unique_missing_runtime_dir("unregistered");
        let registered_runtime_dirs = HashSet::new();

        assert!(
            !should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs),
            "missing runtime dirs must not be killable until they are proven session-owned"
        );
    }

    #[test]
    fn watchdog_scoping_terminates_missing_registered_runtime_dir() {
        let runtime_dir = unique_missing_runtime_dir("registered");
        let mut registered_runtime_dirs = HashSet::new();
        registered_runtime_dirs.insert(runtime_dir.clone());

        assert!(
            should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs),
            "missing runtime dirs that are session-owned should be considered killable"
        );
    }

    #[test]
    fn watchdog_scoping_preserves_registered_live_owner() {
        let runtime_dir = unique_missing_runtime_dir("live-owner");
        fs::create_dir_all(&runtime_dir).unwrap();
        fs::write(
            runtime_dir.join(RUNTIME_OWNER_MARKER),
            std::process::id().to_string(),
        )
        .unwrap();
        let registered_runtime_dirs = HashSet::from([runtime_dir.clone()]);
        let should_terminate = should_terminate_runtime_dir(&runtime_dir, &registered_runtime_dirs);
        fs::remove_dir_all(runtime_dir).unwrap();
        assert!(!should_terminate, "a live test owner must remain protected");
    }

    #[test]
    fn test_binary_matcher_accepts_cargo_test_binary() {
        let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr"))
            .expect("Cargo-built binary must exist");
        assert!(
            is_test_herdr_binary(&binary),
            "Cargo-built binary should be considered test-owned regardless of target directory"
        );
    }

    #[test]
    fn test_binary_matcher_rejects_other_binaries() {
        let nested_build = Path::new(env!("CARGO_MANIFEST_DIR")).join("other/target/debug/herdr");
        let sibling_build = Path::new(env!("CARGO_BIN_EXE_herdr"))
            .parent()
            .unwrap()
            .join("other-build/herdr");
        for binary in [
            Path::new("/home/can/.local/bin/herdr"),
            Path::new("/tmp/other-checkout/target/debug/herdr"),
            nested_build.as_path(),
            sibling_build.as_path(),
        ] {
            assert!(
                !is_test_herdr_binary(binary),
                "other binaries must not be considered test-owned: {}",
                binary.display()
            );
        }
    }
}
