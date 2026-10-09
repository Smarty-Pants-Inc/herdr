use std::{
    collections::VecDeque,
    io::{Read, Write},
    os::fd::{AsRawFd, OwnedFd, RawFd},
    sync::{mpsc as std_mpsc, Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::mpsc::{self, error::TryRecvError as DataTryRecvError};
use tracing::{debug, warn};

use crate::pty::fd;
use crate::pty::input_consumer::{
    classify_replies, AuditSink, ConsumerOperation, ConsumerResponse, InputSource, Ledger,
    Sanitizer,
};

// Actor handle methods must call wake_actor() after queuing work. The idle
// timeout is only a fallback for missed wakes; PTY and wake readiness drive
// normal responsiveness.
const ACTOR_IDLE_POLL_MS: i32 = 1000;
const ACTOR_COMMAND_BUFFER: usize = 1024;
const HANDOFF_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorState {
    Running,
    Quiesced,
    Released,
}

pub(crate) struct PtyReadResult {
    pub terminal_responses: Vec<Bytes>,
}

impl PtyReadResult {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            terminal_responses: Vec::new(),
        }
    }
}

type ReadCallback = Box<dyn FnMut(&[u8]) -> PtyReadResult + Send + 'static>;
type ReaderExitCallback = Box<dyn FnOnce() + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PtyResize {
    rows: u16,
    cols: u16,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PtyResizeRequest {
    resize: PtyResize,
    terminal_responses: Vec<Bytes>,
}

#[derive(Default)]
struct SharedPtyControls {
    resize: Option<PtyResizeRequest>,
    nudge: Option<PtyResize>,
    terminal_responses: Vec<Bytes>,
}

pub(crate) struct PtyIoActorConfig {
    pub pane_id: u32,
    pub master_fd: OwnedFd,
    pub initially_quiesced: bool,
    pub on_read: ReadCallback,
    pub on_reader_exit: Option<ReaderExitCallback>,
}

enum PtyIoDataCommand {
    WriteUserInput(Bytes, InputSource),
    Consumer {
        operation: ConsumerOperation,
        audit: Option<AuditSink>,
        reply: std_mpsc::Sender<ConsumerResponse>,
    },
    SubmitUserInput {
        source: InputSource,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        reply: std_mpsc::Sender<std::io::Result<()>>,
    },
}

enum PtyIoControlCommand {
    BeginHandoff(std_mpsc::Sender<std::io::Result<()>>),
    DuplicateForHandoff(std_mpsc::Sender<std::io::Result<RawFd>>),
    ForegroundProcessGroup(std_mpsc::Sender<Option<u32>>),
    RollbackHandoff(std_mpsc::Sender<std::io::Result<()>>),
    ReleaseAfterCommit(std_mpsc::Sender<std::io::Result<()>>),
    Shutdown,
}

#[derive(Clone)]
pub(crate) struct PtyIoActorHandle {
    data_tx: mpsc::Sender<PtyIoDataCommand>,
    control_tx: std_mpsc::Sender<PtyIoControlCommand>,
    wake: fd::WakeWriter,
    user_writes: Arc<Mutex<UserWriteGate>>,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    foreground_fd: PtyForegroundObserver,
    consumer_epoch: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct UserWriteGate {
    accepting: bool,
}

impl PtyIoActorHandle {
    pub(crate) fn queue_input_consumer_operation(
        &self,
        operation: ConsumerOperation,
        audit: Option<AuditSink>,
    ) -> std::io::Result<std_mpsc::Receiver<crate::pty::input_consumer::ConsumerResponse>> {
        let (tx, rx) = std_mpsc::channel();
        self.data_tx
            .try_send(PtyIoDataCommand::Consumer {
                operation,
                audit,
                reply: tx,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "consumer queue full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "consumer queue closed")
                }
            })?;
        self.wake_actor();
        Ok(rx)
    }

    pub(crate) fn try_write_user_input_with_source(
        &self,
        bytes: Bytes,
        source: InputSource,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(mpsc::error::TrySendError::Closed(bytes));
        }
        match self
            .data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(bytes, source.user()))
        {
            Ok(()) => {
                self.wake_actor();
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(command)) => {
                let PtyIoDataCommand::WriteUserInput(bytes, _) = command else {
                    unreachable!("queued write returned another command")
                };
                Err(mpsc::error::TrySendError::Full(bytes))
            }
            Err(mpsc::error::TrySendError::Closed(command)) => {
                let PtyIoDataCommand::WriteUserInput(bytes, _) = command else {
                    unreachable!("queued write returned another command")
                };
                Err(mpsc::error::TrySendError::Closed(bytes))
            }
        }
    }

    pub(crate) fn input_consumer_epoch_matches(&self, epoch: &str) -> bool {
        self.consumer_epoch
            .lock()
            .ok()
            .is_some_and(|e| e.as_deref() == Some(epoch))
    }
    #[cfg(test)]
    pub(crate) fn try_write_user_input(
        &self,
        bytes: Bytes,
    ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        self.try_write_user_input_with_source(bytes, InputSource::Api)
    }
    #[cfg(test)]
    pub(crate) fn queue_user_input_submission(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        self.queue_user_input_submission_with_source(text, enter, delay, InputSource::Api)
    }
    pub(crate) fn queue_user_input_submission_with_source(
        &self,
        text: Bytes,
        enter: Bytes,
        delay: Duration,
        source: InputSource,
    ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
        let user_writes = self
            .user_writes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !user_writes.accepting {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty actor closed",
            ));
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.data_tx
            .try_send(PtyIoDataCommand::SubmitUserInput {
                source: source.user(),
                text,
                enter,
                delay,
                reply: reply_tx,
            })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => {
                    std::io::Error::new(std::io::ErrorKind::WouldBlock, "pty input queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                }
            })?;
        self.wake_actor();
        Ok(reply_rx)
    }

    pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
        let _order = self
            .response_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(bytes) = response() else {
            return;
        };
        if !bytes.is_empty() {
            self.controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .terminal_responses
                .push(bytes);
            self.wake_actor();
        }
    }

    pub(crate) fn resize(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        terminal_responses: Vec<Bytes>,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.resize = Some(PtyResizeRequest {
                resize: PtyResize {
                    rows,
                    cols,
                    cell_width_px,
                    cell_height_px,
                },
                terminal_responses,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn nudge_child_redraw_after_handoff(
        &self,
        rows: u16,
        cols: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) {
        {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            controls.nudge = Some(PtyResize {
                rows,
                cols,
                cell_width_px,
                cell_height_px,
            });
        }
        self.wake_actor();
    }

    pub(crate) fn begin_handoff(&self, timeout: Duration) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !user_writes.accepting {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "PTY handoff is already in progress",
                ));
            }
            user_writes.accepting = false;
            if self
                .control_tx
                .send(PtyIoControlCommand::BeginHandoff(reply_tx))
                .is_err()
            {
                user_writes.accepting = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            self.wake_actor();
        }
        match reply_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => {
                let _ = self.rollback_handoff();
                Err(err)
            }
            Err(_) => {
                let _ = self.rollback_handoff();
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for PTY actor to quiesce",
                ))
            }
        }
    }

    pub(crate) fn duplicate_for_handoff(&self) -> std::io::Result<RawFd> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::DuplicateForHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff duplicate",
            )
        })?
    }

    /// Scalar-only observation must hold this close-exclusion gate for the
    /// ioctl. The gate never owns or duplicates the original master fd.
    pub(crate) fn foreground_observer(&self) -> PtyForegroundObserver {
        self.foreground_fd.clone()
    }

    pub(crate) fn foreground_process_group_id(&self) -> Option<u32> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ForegroundProcessGroup(reply_tx))
            .ok()?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).ok()?
    }

    pub(crate) fn rollback_handoff(&self) -> std::io::Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::RollbackHandoff(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        let result = reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY handoff rollback",
            )
        })?;
        if result.is_ok() {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = true;
        }
        result
    }

    pub(crate) fn release_after_commit(&self) -> std::io::Result<()> {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.control_tx
            .send(PtyIoControlCommand::ReleaseAfterCommit(reply_tx))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed"))?;
        self.wake_actor();
        reply_rx.recv_timeout(Duration::from_secs(1)).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for PTY actor release",
            )
        })?
    }

    pub(crate) fn shutdown(&self) {
        {
            let mut user_writes = self
                .user_writes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            user_writes.accepting = false;
        }
        if self.control_tx.send(PtyIoControlCommand::Shutdown).is_ok() {
            self.wake_actor();
        }
    }

    fn wake_actor(&self) {
        if let Err(err) = self.wake.wake() {
            debug!(err = %err, "failed to wake PTY actor");
        }
    }
}

pub(crate) struct PtyIoActor;

impl PtyIoActor {
    pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, None)
    }

    fn spawn_inner(
        config: PtyIoActorConfig,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> std::io::Result<PtyIoActorHandle> {
        fd::set_cloexec(config.master_fd.as_raw_fd())?;
        fd::set_nonblocking(config.master_fd.as_raw_fd())?;

        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe()?;
        let user_writes = Arc::new(Mutex::new(UserWriteGate {
            accepting: !config.initially_quiesced,
        }));
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let file = ActorPtyFile::new(std::fs::File::from(config.master_fd));
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            foreground_fd: PtyForegroundObserver(Arc::downgrade(&file.foreground)),
            consumer_epoch: Arc::new(Mutex::new(None)),
        };

        let mut runner = PtyIoActorRunner {
            pane_id: config.pane_id,
            consumer_epoch: Arc::clone(&handle.consumer_epoch),
            consumer: None,
            enrolled_groups: Vec::new(),
            pending_consumer: None,
            marker_reply: None,
            sanitizer: Sanitizer::default(),
            file,
            data_rx,
            control_rx,
            state: if config.initially_quiesced {
                ActorState::Quiesced
            } else {
                ActorState::Running
            },
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls,
            response_order,
            on_read: config.on_read,
            on_reader_exit: config.on_reader_exit,
            poll_observer,
        };
        std::thread::Builder::new()
            .name(format!("herdr-pty-{}", config.pane_id))
            .spawn(move || runner.run())
            .map_err(|err| std::io::Error::other(err.to_string()))?;

        Ok(handle)
    }

    #[cfg(test)]
    fn spawn_with_poll_observer(
        config: PtyIoActorConfig,
        poll_observer: std_mpsc::Sender<()>,
    ) -> std::io::Result<PtyIoActorHandle> {
        Self::spawn_inner(config, Some(poll_observer))
    }
}

/// Outer None means closed; inner None means a live fd with no foreground
/// observation. Never let callers retain the fd or escape the close gate.
#[derive(Clone, Default)]
pub(crate) struct PtyForegroundObserver(Weak<Mutex<Option<RawFd>>>);

impl PtyForegroundObserver {
    pub(crate) fn observe(&self) -> Option<Option<u32>> {
        let slot = self.0.upgrade()?;
        let fd = slot.lock().ok()?;
        Some(crate::platform::foreground_process_group_id_for_tty_fd(
            (*fd)?,
        ))
    }
}

/// Owns exactly one master fd. The observer's gate excludes close only during
/// its bounded ioctl; actor poll/read/callback paths never hold that mutex.
struct ActorPtyFile {
    file: Option<std::fs::File>,
    foreground: Arc<Mutex<Option<RawFd>>>,
}

impl ActorPtyFile {
    fn new(file: std::fs::File) -> Self {
        let foreground = Arc::new(Mutex::new(Some(file.as_raw_fd())));
        Self {
            file: Some(file),
            foreground,
        }
    }

    fn close(&mut self) {
        let mut foreground = self.foreground.lock().unwrap_or_else(|p| p.into_inner());
        *foreground = None;
        // Drop the fd under the gate, before observers can see its number reused.
        drop(self.file.take());
    }
}

impl Drop for ActorPtyFile {
    fn drop(&mut self) {
        self.close();
    }
}

impl AsRawFd for ActorPtyFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }
}

impl Read for ActorPtyFile {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "PTY actor closed"))?
            .read(bytes)
    }
}

impl Write for ActorPtyFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "PTY actor closed"))?
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "PTY actor closed"))?
            .flush()
    }
}

struct PtyIoActorRunner {
    pane_id: u32,
    consumer_epoch: Arc<Mutex<Option<String>>>,
    consumer: Option<(crate::platform::InputConsumerSnapshot, Ledger)>,
    /// (group leader, original consumer) per foreground-group incarnation.
    /// Only the original consumer may re-enroll, after its epoch ends.
    enrolled_groups: Vec<(
        crate::platform::ProcessIdentity,
        crate::platform::ProcessIdentity,
    )>,
    pending_consumer: Option<(
        ConsumerOperation,
        Option<AuditSink>,
        std_mpsc::Sender<ConsumerResponse>,
    )>,
    marker_reply: Option<(std_mpsc::Sender<ConsumerResponse>, ConsumerResponse)>,
    sanitizer: Sanitizer,
    file: ActorPtyFile,
    data_rx: mpsc::Receiver<PtyIoDataCommand>,
    control_rx: std_mpsc::Receiver<PtyIoControlCommand>,
    state: ActorState,
    pending_writes: VecDeque<PendingWrite>,
    current_write_offset: usize,
    active_submission: Option<ActiveSubmission>,
    pending_handoff: Option<std_mpsc::Sender<std::io::Result<()>>>,
    wake_read_fd: OwnedFd,
    controls: Arc<Mutex<SharedPtyControls>>,
    response_order: Arc<Mutex<()>>,
    on_read: ReadCallback,
    on_reader_exit: Option<ReaderExitCallback>,
    poll_observer: Option<std_mpsc::Sender<()>>,
}

struct ActiveSubmission {
    source: InputSource,
    enter: Bytes,
    delay: Duration,
    phase: SubmissionPhase,
    reply: std_mpsc::Sender<std::io::Result<()>>,
}

#[derive(Debug, PartialEq, Eq)]
struct PendingWrite {
    source: Option<InputSource>,
    bytes: Bytes,
    boundary: Option<SubmissionBoundary>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionBoundary {
    Text,
    Enter,
    Marker,
}

enum SubmissionPhase {
    WritingText,
    WaitingUntil(Instant),
    WritingEnter,
}

impl PtyIoActorRunner {
    fn end_consumer(&mut self) {
        self.consumer = None;
        *self
            .consumer_epoch
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        if let Some((reply, _)) = self.marker_reply.take() {
            let _ = reply.send(ConsumerResponse::Refused {
                reason: "consumer_ended".into(),
            });
        }
    }
    /// First enroller owns its foreground-group incarnation for life. Only
    /// that same (pid, start time) may enroll again, and only once its epoch
    /// ended (release, foreground change, Ctrl+Z) or was poisoned.
    fn enroll_admission(
        &self,
        leader: crate::platform::ProcessIdentity,
        peer: crate::platform::ProcessIdentity,
    ) -> std::io::Result<()> {
        let refused = || {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "already_enrolled",
            ))
        };
        match self.enrolled_groups.iter().find(|(l, _)| *l == leader) {
            None => Ok(()),
            Some((_, original)) if *original != peer => refused(),
            Some(_) => match &self.consumer {
                Some((s, ledger)) if s.leader == leader && !ledger.poisoned() => refused(),
                _ => Ok(()),
            },
        }
    }
    fn check_consumer(&mut self) {
        if self.consumer.as_ref().is_some_and(|(snapshot, _)| {
            !crate::platform::input_consumer_alive(self.file.as_raw_fd(), snapshot)
        }) {
            self.end_consumer();
        }
        if let Some((_, ledger)) = &mut self.consumer {
            ledger.expire(Instant::now());
        }
    }
    fn execute_pending_consumer(&mut self) {
        let Some((operation, audit, reply)) = self.pending_consumer.take() else {
            return;
        };
        self.check_consumer();
        let response = match operation {
            ConsumerOperation::Enroll { peer } => {
                let admission =
                    crate::platform::input_consumer_incarnation(self.file.as_raw_fd(), peer)
                        .and_then(|leader| {
                            self.enroll_admission(leader, peer)?;
                            crate::platform::input_consumer_snapshot(self.file.as_raw_fd(), peer)
                        });
                match admission {
                    Err(err) => ConsumerResponse::Refused {
                        reason: err.to_string(),
                    },
                    Ok(snapshot) => {
                        let known = self
                            .enrolled_groups
                            .iter()
                            .any(|(leader, _)| *leader == snapshot.leader);
                        if let Err(err) = self.enroll_admission(snapshot.leader, peer) {
                            ConsumerResponse::Refused {
                                reason: err.to_string(),
                            }
                        } else if !known && self.enrolled_groups.len() >= 1024 {
                            ConsumerResponse::Refused {
                                reason: "incarnation_overflow".into(),
                            }
                        } else {
                            let mut entropy = [0u8; 80];
                            let tty = crate::platform::pane_tty_identity(self.file.as_raw_fd());
                            match (crate::platform::input_consumer_random(&mut entropy), tty) {
                                (Err(_), _) => ConsumerResponse::Refused {
                                    reason: "entropy_unavailable".into(),
                                },
                                (_, Err(_)) => ConsumerResponse::Refused {
                                    reason: "tty_unavailable".into(),
                                },
                                (Ok(()), Ok(tty)) => {
                                    fn hex(bytes: &[u8]) -> String {
                                        bytes.iter().map(|b| format!("{b:02x}")).collect()
                                    }
                                    let epoch = hex(&entropy[..16]);
                                    let epoch_key = hex(&entropy[16..48]);
                                    let nonce = hex(&entropy[48..]);
                                    if known {
                                        // Original consumer after its epoch
                                        // ended or was poisoned: fresh epoch.
                                        self.end_consumer();
                                    } else {
                                        self.enrolled_groups.push((snapshot.leader, peer));
                                    }
                                    self.consumer = Some((
                                        snapshot,
                                        Ledger::new(epoch.clone(), epoch_key.clone()),
                                    ));
                                    self.sanitizer.reset();
                                    self.pending_writes.push_back(PendingWrite {
                                        bytes: Bytes::from(format!(
                                            "\x1b_herdr-epoch;{nonce}\x1b\\"
                                        )),
                                        source: None,
                                        boundary: Some(SubmissionBoundary::Marker),
                                    });
                                    self.marker_reply = Some((
                                        reply,
                                        ConsumerResponse::Enrolled {
                                            epoch,
                                            epoch_key,
                                            nonce,
                                            tty,
                                        },
                                    ));
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            ConsumerOperation::Cut(request) => match &mut self.consumer {
                Some((snapshot, ledger)) => {
                    if !ledger.authentic(&request.epoch, &request.epoch_key) {
                        ConsumerResponse::Refused {
                            reason: "invalid_epoch".into(),
                        }
                    } else {
                        if !crate::platform::input_consumer_unchanged(
                            self.file.as_raw_fd(),
                            snapshot,
                        ) {
                            ledger.poison("termios_changed");
                        }
                        ledger.cut(request, audit.as_ref(), Instant::now())
                    }
                }
                None => ConsumerResponse::Refused {
                    reason: "invalid_epoch".into(),
                },
            },
            ConsumerOperation::Release { epoch, epoch_key } => {
                if self
                    .consumer
                    .as_ref()
                    .is_some_and(|(_, l)| l.authentic(&epoch, &epoch_key))
                {
                    self.end_consumer();
                    ConsumerResponse::Released
                } else {
                    ConsumerResponse::Refused {
                        reason: "invalid_epoch".into(),
                    }
                }
            }
        };
        let _ = reply.send(response);
    }
    #[cfg(test)]
    fn enqueue_write(&mut self, bytes: Bytes) {
        self.enqueue_sourced_write(bytes, InputSource::Api, None);
    }
    fn enqueue_sourced_write(
        &mut self,
        bytes: Bytes,
        source: InputSource,
        boundary: Option<SubmissionBoundary>,
    ) {
        if !bytes.is_empty() {
            let bytes = self.sanitizer.sanitize(bytes);
            self.pending_writes.push_back(PendingWrite {
                bytes,
                source: Some(source),
                boundary,
            });
        }
    }
    fn enqueue_submission_write(&mut self, bytes: Bytes, boundary: SubmissionBoundary) {
        let source = self
            .active_submission
            .as_ref()
            .map(|s| s.source.clone())
            .unwrap_or(InputSource::Api);
        self.enqueue_sourced_write(bytes, source, Some(boundary));
    }

    fn run(&mut self) {
        let mut should_exit = false;
        while !should_exit {
            self.check_consumer();
            should_exit = self.drain_commands();
            if should_exit || self.state == ActorState::Released {
                break;
            }

            self.apply_pending_controls();

            if !self.pending_writes.is_empty() {
                match self.flush_pending_writes_once() {
                    Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                    Ok(None) => {}
                    Err(err) => {
                        self.fail_active_submission(err);
                        break;
                    }
                }
            }
            self.schedule_submission_enter();
            if self.pending_writes.is_empty()
                && self.active_submission.is_none()
                && self.pending_consumer.is_some()
            {
                self.execute_pending_consumer();
                continue;
            }
            if self.active_submission.is_none() && self.pending_handoff.is_some() {
                continue;
            }

            if let Some(poll_observer) = &self.poll_observer {
                let _ = poll_observer.send(());
            }

            match fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                self.state == ActorState::Running,
                !self.pending_writes.is_empty(),
                self.poll_timeout_ms(),
            ) {
                Ok(readiness) => {
                    if readiness.wake_ready {
                        if let Err(err) = fd::drain_wake_fd(self.wake_read_fd.as_raw_fd()) {
                            debug!(pane = self.pane_id, err = %err, "PTY actor wake drain failed");
                            break;
                        }
                        continue;
                    }
                    if self.state == ActorState::Running
                        && readiness.pty_read_ready
                        && !self.read_once()
                    {
                        break;
                    }
                    if readiness.pty_write_ready && !self.pending_writes.is_empty() {
                        match self.flush_pending_writes_once() {
                            Ok(Some(boundary)) => self.complete_submission_boundary(boundary),
                            Ok(None) => {}
                            Err(err) => {
                                self.fail_active_submission(err);
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    debug!(pane = self.pane_id, err = %err, "PTY actor poll failed");
                    break;
                }
            }
        }

        self.end_consumer();
        self.file.close();
        self.close_input_queue();
        if let Some(on_reader_exit) = self.on_reader_exit.take() {
            on_reader_exit();
        }
        debug!(pane = self.pane_id, "PTY actor exiting");
    }

    fn drain_commands(&mut self) -> bool {
        if self.drain_control_commands() {
            return true;
        }
        if self.active_submission.is_some()
            || self.pending_consumer.is_some()
            || self.marker_reply.is_some()
        {
            return false;
        }
        if let Some(reply) = self.pending_handoff.take() {
            self.defer_or_begin_handoff(reply);
            return false;
        }
        self.drain_data_commands()
    }

    fn drain_control_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.control_rx.try_recv() {
                Ok(command) => {
                    if self.handle_control_command(command) {
                        should_exit = true;
                        break;
                    }
                }
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn drain_data_commands(&mut self) -> bool {
        let mut should_exit = false;
        loop {
            match self.data_rx.try_recv() {
                Ok(command) => {
                    if self.handle_data_command(command) {
                        should_exit = true;
                        break;
                    }
                    if self.active_submission.is_some()
                        || self.pending_consumer.is_some()
                        || self.marker_reply.is_some()
                    {
                        break;
                    }
                }
                Err(DataTryRecvError::Empty) => break,
                Err(DataTryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }
        should_exit
    }

    fn handle_data_command(&mut self, command: PtyIoDataCommand) -> bool {
        match command {
            PtyIoDataCommand::Consumer {
                operation,
                audit,
                reply,
            } => {
                if self.state == ActorState::Running {
                    self.pending_consumer = Some((operation, audit, reply));
                } else {
                    let _ = reply.send(ConsumerResponse::Refused {
                        reason: "runtime_unavailable".into(),
                    });
                }
            }
            PtyIoDataCommand::WriteUserInput(bytes, source) => {
                if self.state == ActorState::Running {
                    self.enqueue_sourced_write(bytes, source, None);
                }
            }
            PtyIoDataCommand::SubmitUserInput {
                source,
                text,
                enter,
                delay,
                reply,
            } => {
                if self.state == ActorState::Running {
                    let phase = if text.is_empty() {
                        SubmissionPhase::WaitingUntil(Instant::now() + delay)
                    } else {
                        self.enqueue_sourced_write(
                            text,
                            source.clone(),
                            Some(SubmissionBoundary::Text),
                        );
                        SubmissionPhase::WritingText
                    };
                    self.active_submission = Some(ActiveSubmission {
                        source,
                        enter,
                        delay,
                        phase,
                        reply,
                    });
                } else {
                    let _ = reply.send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "pty actor is not accepting input",
                    )));
                }
            }
        }
        false
    }

    fn handle_control_command(&mut self, command: PtyIoControlCommand) -> bool {
        match command {
            PtyIoControlCommand::BeginHandoff(reply) => {
                self.defer_or_begin_handoff(reply);
            }
            PtyIoControlCommand::DuplicateForHandoff(reply) => {
                let result = if self.state == ActorState::Quiesced {
                    fd::duplicate_cloexec_fd(self.file.as_raw_fd())
                } else {
                    Err(std::io::Error::other(
                        "PTY actor must be quiesced before handoff duplication",
                    ))
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ForegroundProcessGroup(reply) => {
                let result =
                    crate::platform::foreground_process_group_id_for_tty_fd(self.file.as_raw_fd());
                let _ = reply.send(result);
            }
            PtyIoControlCommand::RollbackHandoff(reply) => {
                self.pending_handoff.take();
                let result = if self.state == ActorState::Released {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "PTY actor was released before handoff rollback",
                    ))
                } else {
                    self.state = ActorState::Running;
                    Ok(())
                };
                let _ = reply.send(result);
            }
            PtyIoControlCommand::ReleaseAfterCommit(reply) => {
                self.end_consumer();
                self.state = ActorState::Released;
                self.pending_writes.clear();
                self.file.close();
                let _ = reply.send(Ok(()));
                return true;
            }
            PtyIoControlCommand::Shutdown => return true,
        }
        false
    }

    fn defer_or_begin_handoff(&mut self, reply: std_mpsc::Sender<std::io::Result<()>>) {
        if self.active_submission.is_none() {
            self.drain_pre_quiesce_commands();
        }
        if self.active_submission.is_some() {
            self.pending_handoff = Some(reply);
        } else {
            let result = self.begin_handoff();
            let _ = reply.send(result);
        }
    }

    fn begin_handoff(&mut self) -> std::io::Result<()> {
        self.drain_pre_quiesce_commands();
        if self.active_submission.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "PTY input submission is still in progress",
            ));
        }
        self.apply_pending_controls();
        if self.state == ActorState::Released {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "PTY actor was released before handoff quiesce",
            ));
        }
        let deadline = Instant::now() + HANDOFF_DRAIN_TIMEOUT;
        let _ = self.flush_pending_writes_once()?;
        while !self.pending_writes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out draining PTY writes before handoff",
                ));
            }
            let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as i32;
            let readiness = fd::poll_pty_and_wake(
                self.file.as_raw_fd(),
                self.wake_read_fd.as_raw_fd(),
                true,
                true,
                timeout_ms,
            )?;
            if readiness.wake_ready {
                fd::drain_wake_fd(self.wake_read_fd.as_raw_fd())?;
            }
            if readiness.pty_read_ready && !self.read_once() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "PTY closed while draining writes before handoff",
                ));
            }
            if readiness.pty_write_ready {
                let _ = self.flush_pending_writes_once()?;
            }
        }
        self.end_consumer();
        if let Some((_, _, reply)) = self.pending_consumer.take() {
            let _ = reply.send(ConsumerResponse::Refused {
                reason: "runtime_unavailable".into(),
            });
        }
        self.state = ActorState::Quiesced;
        Ok(())
    }

    fn drain_pre_quiesce_commands(&mut self) {
        // Lifecycle controls retain priority, but must still drain all accepted
        // ordinary input behind a refused consumer operation before handoff.
        if let Some((_, _, reply)) = self.pending_consumer.take() {
            let _ = reply.send(ConsumerResponse::Refused {
                reason: "runtime_unavailable".into(),
            });
        }
        if self.marker_reply.is_some() {
            self.end_consumer();
        }
        while let Ok(command) = self.data_rx.try_recv() {
            if let PtyIoDataCommand::Consumer { reply, .. } = command {
                let _ = reply.send(ConsumerResponse::Refused {
                    reason: "runtime_unavailable".into(),
                });
                continue;
            }
            if self.handle_data_command(command) {
                break;
            }
            if self.active_submission.is_some() {
                break;
            }
        }
    }

    fn apply_pending_controls(&mut self) {
        let (resize, nudge, terminal_responses) = {
            let mut controls = self
                .controls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                controls.resize.take(),
                controls.nudge.take(),
                std::mem::take(&mut controls.terminal_responses),
            )
        };
        if self.state == ActorState::Released {
            return;
        }
        if let Some(request) = resize {
            self.resize(request.resize);
            self.enqueue_terminal_responses(request.terminal_responses);
        }
        if let Some(nudge) = nudge {
            self.nudge(nudge);
        }
        self.enqueue_terminal_responses(terminal_responses);
    }

    fn read_once(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        match self.file.read(&mut buf) {
            Ok(0) => false,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
            Err(err) => {
                debug!(pane = self.pane_id, err = %err, "PTY actor read failed");
                false
            }
            Ok(n) => {
                let response_order = Arc::clone(&self.response_order);
                let _order = response_order
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let result = (self.on_read)(&buf[..n]);
                self.controls
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .terminal_responses
                    .extend(result.terminal_responses);
                drop(_order);
                let terminal_responses = std::mem::take(
                    &mut self
                        .controls
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .terminal_responses,
                );
                self.enqueue_terminal_responses(terminal_responses);
                true
            }
        }
    }

    fn enqueue_terminal_responses(&mut self, terminal_responses: Vec<Bytes>) {
        if self.state == ActorState::Released {
            return;
        }
        for bytes in terminal_responses {
            for (frame, source) in classify_replies(bytes) {
                self.enqueue_sourced_write(frame, source, None);
            }
        }
    }

    fn complete_submission_boundary(&mut self, boundary: SubmissionBoundary) {
        match boundary {
            SubmissionBoundary::Marker => {
                if let Some((reply, response)) = self.marker_reply.take() {
                    if let Some((_, ledger)) = &self.consumer {
                        *self
                            .consumer_epoch
                            .lock()
                            .unwrap_or_else(|p| p.into_inner()) = Some(ledger.epoch.clone());
                    }
                    let _ = reply.send(response);
                }
            }
            SubmissionBoundary::Text => {
                let Some(submission) = self.active_submission.as_mut() else {
                    return;
                };
                debug_assert!(matches!(submission.phase, SubmissionPhase::WritingText));
                submission.phase = SubmissionPhase::WaitingUntil(Instant::now() + submission.delay);
            }
            SubmissionBoundary::Enter => {
                let Some(submission) = self.active_submission.take() else {
                    return;
                };
                debug_assert!(matches!(submission.phase, SubmissionPhase::WritingEnter));
                let _ = submission.reply.send(Ok(()));
            }
        }
    }

    fn schedule_submission_enter(&mut self) {
        let Some(ActiveSubmission {
            enter,
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return;
        };
        if Instant::now() >= *deadline {
            let enter = enter.clone();
            if enter.is_empty() {
                let submission = self.active_submission.take().unwrap();
                let _ = submission.reply.send(Ok(()));
            } else {
                self.active_submission.as_mut().unwrap().phase = SubmissionPhase::WritingEnter;
                self.enqueue_submission_write(enter, SubmissionBoundary::Enter);
            }
        }
    }

    fn poll_timeout_ms(&self) -> i32 {
        let Some(ActiveSubmission {
            phase: SubmissionPhase::WaitingUntil(deadline),
            ..
        }) = self.active_submission.as_ref()
        else {
            return ACTOR_IDLE_POLL_MS;
        };
        deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .max(1)
            .min(ACTOR_IDLE_POLL_MS as u128) as i32
    }

    fn fail_active_submission(&mut self, err: std::io::Error) {
        if let Some(submission) = self.active_submission.take() {
            let _ = submission.reply.send(Err(err));
        }
    }

    fn close_input_queue(&mut self) {
        self.data_rx.close();
        self.fail_active_submission(input_submission_closed_error());
        if let Some((_, _, reply)) = self.pending_consumer.take() {
            let _ = reply.send(ConsumerResponse::Refused {
                reason: "runtime_unavailable".into(),
            });
        }
        while let Some(command) = self.data_rx.blocking_recv() {
            match command {
                PtyIoDataCommand::SubmitUserInput { reply, .. } => {
                    let _ = reply.send(Err(input_submission_closed_error()));
                }
                PtyIoDataCommand::Consumer { reply, .. } => {
                    let _ = reply.send(ConsumerResponse::Refused {
                        reason: "runtime_unavailable".into(),
                    });
                }
                _ => {}
            }
        }
    }

    fn flush_pending_writes_once(&mut self) -> std::io::Result<Option<SubmissionBoundary>> {
        self.check_consumer();
        while let Some(write) = self.pending_writes.front() {
            let chunk = &write.bytes[self.current_write_offset..];
            match self.file.write(chunk) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "PTY actor write returned zero bytes",
                    ));
                }
                Ok(written) => {
                    if let (Some(source), Some((_, ledger))) = (&write.source, &mut self.consumer) {
                        ledger.record(&chunk[..written], source, Instant::now());
                    }
                    self.current_write_offset += written;
                    if self.current_write_offset >= write.bytes.len() {
                        let completed = self.pending_writes.pop_front().unwrap();
                        self.current_write_offset = 0;
                        if let Some(boundary) = completed.boundary {
                            self.file.flush()?;
                            return Ok(Some(boundary));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => return Ok(None),
                Err(err) => {
                    warn!(pane = self.pane_id, err = %err, "PTY actor write failed");
                    self.pending_writes.clear();
                    self.current_write_offset = 0;
                    return Err(err);
                }
            }
        }
        self.file.flush()?;
        Ok(None)
    }

    fn resize(&self, resize: PtyResize) {
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn nudge(&mut self, resize: PtyResize) {
        if self.state == ActorState::Released {
            return;
        }
        let nudge = if resize.rows > 2 {
            (
                resize.rows - 1,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        } else {
            (
                resize.rows,
                resize.cols.saturating_sub(1).max(4),
                resize.cell_width_px,
                resize.cell_height_px,
            )
        };
        if nudge
            == (
                resize.rows,
                resize.cols,
                resize.cell_width_px,
                resize.cell_height_px,
            )
        {
            return;
        }
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            nudge.0,
            nudge.1,
            nudge.2,
            nudge.3,
        ));
        std::thread::sleep(Duration::from_millis(30));
        self.log_resize_result(fd::resize_pty_fd(
            self.file.as_raw_fd(),
            resize.rows,
            resize.cols,
            resize.cell_width_px,
            resize.cell_height_px,
        ));
    }

    fn log_resize_result(&self, result: std::io::Result<()>) {
        if let Err(err) = result {
            debug!(pane = self.pane_id, err = %err, "PTY resize failed");
        }
    }
}

fn input_submission_closed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "PTY actor closed during input submission",
    )
}

/// A live, no-foreground test channel using the real observer and close gate.
/// Keep the descriptor owned for the fixture's lifetime; observers remain weak.
#[cfg(test)]
pub(crate) struct TestPtyForeground(ActorPtyFile);

#[cfg(test)]
impl TestPtyForeground {
    pub(crate) fn new() -> std::io::Result<Self> {
        // A valid non-TTY makes tcgetpgrp return no foreground, not "closed".
        std::fs::File::open("/dev/null").map(|file| Self(ActorPtyFile::new(file)))
    }

    pub(crate) fn observer(&self) -> PtyForegroundObserver {
        PtyForegroundObserver(Arc::downgrade(&self.0.foreground))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, FromRawFd, IntoRawFd},
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn test_foreground_fixture_observes_live_no_foreground_until_drop() {
        let fixture = TestPtyForeground::new().expect("live no-foreground fixture");
        let observer = fixture.observer();
        assert_eq!(observer.observe(), Some(None));
        let cloned_observer = observer.clone();
        drop(fixture);
        assert_eq!(observer.observe(), None, "observer must not retain the fd");
        assert_eq!(cloned_observer.observe(), None);
    }

    #[test]
    fn default_foreground_observer_is_closed() {
        assert_eq!(PtyForegroundObserver::default().observe(), None);
    }

    fn test_wake_pair() -> (fd::WakeWriter, OwnedFd) {
        let pipe = fd::create_wake_pipe().expect("wake pipe");
        (pipe.writer, pipe.read_fd)
    }

    fn actor_with_socket_pair(
        initially_quiesced: bool,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        actor_with_socket_pair_and_poll_observer(initially_quiesced, None)
    }

    fn actor_with_socket_pair_and_poll_observer(
        initially_quiesced: bool,
        poll_observer: Option<std_mpsc::Sender<()>>,
    ) -> (PtyIoActorHandle, UnixStream, std_mpsc::Receiver<Bytes>) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        };
        let handle = if let Some(poll_observer) = poll_observer {
            PtyIoActor::spawn_with_poll_observer(config, poll_observer)
        } else {
            PtyIoActor::spawn(config)
        }
        .expect("actor spawn");
        (handle, peer, read_rx)
    }

    fn actor_runner_for_unit_test() -> (PtyIoActorRunner, UnixStream) {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (_data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let runner = PtyIoActorRunner {
            pane_id: 1,
            consumer_epoch: Arc::new(Mutex::new(None)),
            consumer: None,
            enrolled_groups: Vec::new(),
            pending_consumer: None,
            marker_reply: None,
            sanitizer: Sanitizer::default(),
            file: ActorPtyFile::new(std::fs::File::from(owned)),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };
        (runner, peer)
    }

    #[test]
    fn actor_ignores_empty_user_input_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();

        assert!(
            !runner.handle_data_command(PtyIoDataCommand::WriteUserInput(
                Bytes::new(),
                InputSource::Api
            ))
        );

        assert!(runner.pending_writes.is_empty());
    }

    #[test]
    fn submission_boundary_does_not_wait_for_following_protocol_write() {
        let (mut runner, _peer) = actor_runner_for_unit_test();
        runner.enqueue_submission_write(Bytes::from_static(b"prompt"), SubmissionBoundary::Text);
        runner.enqueue_write(Bytes::from_static(b"response"));

        assert_eq!(
            runner.flush_pending_writes_once().unwrap(),
            Some(SubmissionBoundary::Text)
        );
        assert_eq!(
            runner.pending_writes[0].bytes,
            Bytes::from_static(b"response")
        );
    }

    #[test]
    fn actor_writes_user_input_to_owned_fd() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);

        handle
            .try_write_user_input(Bytes::from_static(b"hello"))
            .expect("write command accepted");

        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives write");
        assert_eq!(&buf, b"hello");
        handle.shutdown();
    }

    #[test]
    fn actor_delays_enter_from_completed_prompt_write() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let text = Bytes::from(vec![b'x'; 4 * 1024 * 1024]);
        let text_len = text.len();
        let delay = Duration::from_millis(200);
        let reader = std::thread::spawn(move || {
            std::thread::sleep(delay);
            let mut received = vec![0; text_len];
            peer.read_exact(&mut received)
                .expect("peer receives prompt");
            let prompt_completed = Instant::now();
            let mut enter = [0; 1];
            peer.read_exact(&mut enter).expect("peer receives enter");
            let enter_received = Instant::now();
            let mut user = [0; 4];
            peer.read_exact(&mut user)
                .expect("peer receives queued input");
            // Close the endpoint even if parallel test forks inherited peer descriptors.
            peer.shutdown(std::net::Shutdown::Both)
                .expect("peer shutdown");
            (prompt_completed, enter_received, enter, user)
        });

        let completion = handle
            .queue_user_input_submission(text, Bytes::from_static(b"\r"), delay)
            .expect("submission queues");
        handle
            .try_write_user_input(Bytes::from_static(b"user"))
            .expect("ordinary input queues behind submission");
        completion
            .recv()
            .expect("actor reports submission")
            .expect("submission completes");
        let (prompt_completed, enter_received, enter, user) = reader.join().expect("reader joins");

        assert_eq!(enter, *b"\r");
        assert_eq!(user, *b"user");
        assert!(enter_received.duration_since(prompt_completed) >= delay / 2);

        let err = match handle.queue_user_input_submission(
            Bytes::from_static(b"prompt"),
            Bytes::from_static(b"\r"),
            Duration::ZERO,
        ) {
            Ok(completion) => completion
                .recv()
                .expect("actor reports submission")
                .expect_err("closed PTY rejects submission"),
            Err(err) => err,
        };

        assert!(matches!(
            err.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::WriteZero
        ));
    }

    #[test]
    fn actor_completes_empty_submission_parts() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let completion = handle
            .queue_user_input_submission(Bytes::new(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("empty prompt submission queues");
        let mut enter = [0; 1];
        peer.read_exact(&mut enter)
            .expect("peer receives enter for empty prompt");
        assert_eq!(enter, *b"\r");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty prompt submission")
            .expect("empty prompt submission completes");

        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::new(),
                Duration::from_millis(40),
            )
            .expect("empty enter submission queues");
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_millis(250)));
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt)
            .expect("peer receives prompt before empty enter");
        assert_eq!(&prompt, b"prompt");
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports empty enter submission")
            .expect("empty enter submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff resumes without an idle poll after submission");
        handle.shutdown();
    }

    #[test]
    fn actor_reports_peer_closure_during_submission_delay() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let completion = handle
            .queue_user_input_submission(
                Bytes::from_static(b"prompt"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("submission queues");
        let mut prompt = [0; 6];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        peer.shutdown(std::net::Shutdown::Both)
            .expect("peer shutdown");
        drop(peer);

        let err = completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports peer closure")
            .expect_err("peer closure fails the active submission");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_fails_buffered_submissions_on_exit() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let active = handle
            .queue_user_input_submission(
                Bytes::from_static(b"first"),
                Bytes::from_static(b"\r"),
                Duration::from_secs(1),
            )
            .expect("first submission queues");
        let mut prompt = [0; 5];
        peer.read_exact(&mut prompt).expect("peer receives prompt");
        let buffered = handle
            .queue_user_input_submission(
                Bytes::from_static(b"second"),
                Bytes::from_static(b"\r"),
                Duration::ZERO,
            )
            .expect("second submission queues");

        peer.shutdown(std::net::Shutdown::Both)
            .expect("peer shutdown");
        drop(peer);
        let active_err = active
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports active submission")
            .expect_err("peer closure fails active submission");
        let buffered_err = buffered
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports buffered submission")
            .expect_err("peer closure fails buffered submission");

        assert_eq!(active_err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(buffered_err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn actor_rejects_submission_after_io_loop_exits() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let handle_slot = Arc::new(Mutex::new(None::<PtyIoActorHandle>));
        let (attempt_tx, attempt_rx) = std_mpsc::channel();
        let (callback_continue_tx, callback_continue_rx) = std_mpsc::channel();
        let config = PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: Some(Box::new({
                let handle_slot = Arc::clone(&handle_slot);
                move || {
                    let handle = handle_slot
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                        .expect("actor handle installed")
                        .clone();
                    assert_eq!(
                        handle.foreground_observer().observe(),
                        None,
                        "original fd must be closed before reader-exit callbacks"
                    );
                    let attempt = handle.queue_user_input_submission(
                        Bytes::from_static(b"prompt"),
                        Bytes::from_static(b"\r"),
                        Duration::ZERO,
                    );
                    attempt_tx.send(attempt).expect("attempt receiver alive");
                    callback_continue_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("unblock reader-exit callback");
                }
            })),
        };
        let handle = PtyIoActor::spawn(config).expect("actor spawn");
        let observer = handle.foreground_observer();
        *handle_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);

        peer.shutdown(std::net::Shutdown::Both)
            .expect("peer shutdown");
        drop(peer);
        let err = match attempt_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("reader exit callback attempts submission")
        {
            Ok(completion) => completion
                .recv_timeout(Duration::from_secs(1))
                .expect("actor reports submission")
                .expect_err("closed actor rejects submission"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(
            observer.observe(),
            None,
            "closed fd cannot be retained by a blocked reader-exit callback"
        );
        callback_continue_tx
            .send(())
            .expect("allow actor callback to exit");
    }

    #[test]
    fn actor_wakes_idle_poll_for_user_input() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, mut peer, _read_rx) =
            actor_with_socket_pair_and_poll_observer(false, Some(poll_tx));
        peer.set_read_timeout(Some(Duration::from_millis(500)))
            .expect("peer timeout");
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        handle
            .try_write_user_input(Bytes::from_static(b"x"))
            .expect("write command accepted");

        let mut buf = [0u8; 1];
        peer.read_exact(&mut buf)
            .expect("peer receives write without waiting for actor poll timeout");
        assert_eq!(&buf, b"x");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "actor write should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn actor_reads_output_while_input_is_backpressured() {
        let (mut actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");

        let fill = [0xAA; 8192];
        let mut prefilled = 0;
        loop {
            match actor_socket.write(&fill) {
                Ok(written) => prefilled += written,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => panic!("failed to fill actor write buffer: {err}"),
            }
        }
        assert!(prefilled > 0, "actor write buffer should accept some bytes");

        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (read_tx, read_rx) = std_mpsc::channel();
        let handle = PtyIoActor::spawn(PtyIoActorConfig {
            pane_id: 1,
            master_fd: owned,
            initially_quiesced: false,
            on_read: Box::new(move |bytes| {
                read_tx
                    .send(Bytes::copy_from_slice(bytes))
                    .expect("read callback receiver alive");
                PtyReadResult::empty()
            }),
            on_reader_exit: None,
        })
        .expect("actor spawn");

        let marker = Bytes::from_static(b"queued-input");
        let completion = handle
            .queue_user_input_submission(marker.clone(), Bytes::from_static(b"\r"), Duration::ZERO)
            .expect("submission accepted");

        const OUTPUT_LEN: usize = 128 * 1024;
        let mut peer_writer = peer.try_clone().expect("clone peer writer");
        let output_writer = std::thread::spawn(move || {
            peer_writer
                .write_all(&vec![0xBB; OUTPUT_LEN])
                .expect("peer writes sustained output");
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut output_len = 0;
        while output_len < OUTPUT_LEN {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "actor did not keep reading blocked peer output"
            );
            let output = read_rx
                .recv_timeout(remaining)
                .expect("actor keeps reading while input remains blocked");
            assert!(output.iter().all(|byte| *byte == 0xBB));
            output_len += output.len();
        }
        assert_eq!(output_len, OUTPUT_LEN);
        output_writer.join().expect("output writer joins");

        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));

        let mut received_input = vec![0; prefilled + marker.len() + 1];
        peer.read_exact(&mut received_input)
            .expect("peer receives prefill and queued input");
        assert!(received_input[..prefilled].iter().all(|byte| *byte == 0xAA));
        assert_eq!(
            &received_input[prefilled..prefilled + marker.len()],
            marker.as_ref()
        );
        assert_eq!(received_input.last(), Some(&b'\r'));
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reports submission")
            .expect("submission completes");
        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff waits for submission");
        handle.shutdown();
    }

    #[test]
    fn actor_wakes_idle_poll_for_handoff_control() {
        let (poll_tx, poll_rx) = std_mpsc::channel();
        let (handle, _peer, _read_rx) =
            actor_with_socket_pair_and_poll_observer(false, Some(poll_tx));
        poll_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor entered idle poll");

        let start = Instant::now();
        let handoff_handle = handle.clone();
        let handoff =
            std::thread::spawn(move || handoff_handle.begin_handoff(Duration::from_secs(1)));

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff control should wake idle actor");
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "handoff control should be driven by wake fd, not the idle poll timeout"
        );
        handle.shutdown();
    }

    #[test]
    fn poll_ignores_pty_hup_without_pty_interest() {
        let (actor_socket, peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.shutdown(std::net::Shutdown::Both)
            .expect("peer shutdown");
        drop(peer);
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");

        let readiness = fd::poll_pty_and_wake(
            actor_socket.as_raw_fd(),
            wake_pipe.read_fd.as_raw_fd(),
            false,
            false,
            10,
        )
        .expect("poll succeeds");

        assert!(!readiness.pty_read_ready);
        assert!(!readiness.pty_write_ready);
        assert!(!readiness.wake_ready);
    }

    #[test]
    fn actor_delivers_fd_reads_to_callback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        peer.write_all(b"from-peer").expect("peer write");

        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor read callback");
        assert_eq!(read, Bytes::from_static(b"from-peer"));
        handle.shutdown();
    }

    #[test]
    fn begin_handoff_stops_reads_and_rejects_user_writes_until_rollback() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let err = handle
            .begin_handoff(Duration::from_secs(1))
            .expect_err("concurrent handoff rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        peer.write_all(b"held").expect("peer write during quiesce");
        assert!(
            read_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "actor must not read while quiesced"
        );

        handle.rollback_handoff().expect("rollback resumes actor");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor reads held bytes after rollback");
        assert_eq!(read, Bytes::from_static(b"held"));

        handle
            .try_write_user_input(Bytes::from_static(b"after"))
            .expect("write accepted after rollback");
        let mut buf = [0u8; 5];
        peer.read_exact(&mut buf).expect("peer receives after");
        assert_eq!(&buf, b"after");
        handle.shutdown();
    }

    #[test]
    fn duplicate_for_handoff_requires_quiesced_actor() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        assert!(handle.duplicate_for_handoff().is_err());
        handle
            .begin_handoff(Duration::from_secs(1))
            .expect("handoff quiesced");
        let duplicate = handle
            .duplicate_for_handoff()
            .expect("handoff duplicate created");
        assert!(duplicate >= 0);
        unsafe {
            libc::close(duplicate);
        }
        handle.rollback_handoff().expect("rollback resumes actor");

        peer.write_all(b"still-live").expect("peer write");
        let read = read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("actor still reads after duplicate closes");
        assert_eq!(read, Bytes::from_static(b"still-live"));
        handle.shutdown();
    }

    #[test]
    fn resize_and_nudge_keep_latest_request_when_command_queue_is_full() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, _control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(
                Bytes::from_static(b"fill"),
                InputSource::Api,
            ))
            .expect("fill command queue");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::clone(&controls),
            response_order: Arc::new(Mutex::new(())),
            foreground_fd: PtyForegroundObserver::default(),
            consumer_epoch: Arc::new(Mutex::new(None)),
        };

        handle.resize(20, 80, 8, 16, vec![Bytes::from_static(b"old")]);
        handle.resize(40, 120, 9, 18, vec![Bytes::from_static(b"new")]);
        handle.nudge_child_redraw_after_handoff(41, 121, 10, 20);
        handle.write_terminal_response(|| Some(Bytes::from_static(b"response")));

        let controls = controls.lock().expect("controls lock");
        assert_eq!(
            controls.resize,
            Some(PtyResizeRequest {
                resize: PtyResize {
                    rows: 40,
                    cols: 120,
                    cell_width_px: 9,
                    cell_height_px: 18,
                },
                terminal_responses: vec![Bytes::from_static(b"new")],
            })
        );
        assert_eq!(
            controls.nudge,
            Some(PtyResize {
                rows: 41,
                cols: 121,
                cell_width_px: 10,
                cell_height_px: 20,
            })
        );
        assert_eq!(
            controls.terminal_responses,
            vec![Bytes::from_static(b"response")]
        );
    }

    #[test]
    fn appearance_transition_report_precedes_query_of_new_scheme() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        let owned = unsafe { OwnedFd::from_raw_fd(actor_socket.into_raw_fd()) };
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (control_tx, control_rx) = std_mpsc::channel();
        let wake_pipe = fd::create_wake_pipe().expect("wake pipe");
        let controls = Arc::new(Mutex::new(SharedPtyControls::default()));
        let response_order = Arc::new(Mutex::new(()));
        let light = Arc::new(AtomicBool::new(false));
        let query_light = Arc::clone(&light);
        let runner = PtyIoActorRunner {
            pane_id: 1,
            consumer_epoch: Arc::new(Mutex::new(None)),
            consumer: None,
            enrolled_groups: Vec::new(),
            pending_consumer: None,
            marker_reply: None,
            sanitizer: Sanitizer::default(),
            file: ActorPtyFile::new(std::fs::File::from(owned)),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: wake_pipe.read_fd,
            controls: Arc::clone(&controls),
            response_order: Arc::clone(&response_order),
            on_read: Box::new(move |_| PtyReadResult {
                terminal_responses: vec![if query_light.load(Ordering::Acquire) {
                    Bytes::from_static(b"query-light")
                } else {
                    Bytes::from_static(b"query-dark")
                }],
            }),
            on_reader_exit: None,
            poll_observer: None,
        };
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake: wake_pipe.writer,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls,
            response_order,
            foreground_fd: PtyForegroundObserver(Arc::downgrade(&runner.file.foreground)),
            consumer_epoch: Arc::clone(&runner.consumer_epoch),
        };
        let (changed_tx, changed_rx) = std_mpsc::channel();
        let (continue_tx, continue_rx) = std_mpsc::channel();

        let appearance = std::thread::spawn(move || {
            handle.write_terminal_response(|| {
                light.store(true, Ordering::Release);
                changed_tx.send(()).expect("notify appearance change");
                continue_rx.recv().expect("continue appearance report");
                Some(Bytes::from_static(b"live-light"))
            });
        });
        changed_rx.recv().expect("appearance changed");
        peer.write_all(b"query").expect("write query");
        let reader = std::thread::spawn(move || {
            let mut runner = runner;
            assert!(runner.read_once());
            runner
        });
        continue_tx.send(()).expect("release appearance report");
        appearance.join().expect("appearance thread joins");
        let runner = reader.join().expect("reader thread joins");

        assert_eq!(
            runner.pending_writes,
            VecDeque::from([
                PendingWrite {
                    source: Some(InputSource::Unknown),
                    bytes: Bytes::from_static(b"live-light"),
                    boundary: None,
                },
                PendingWrite {
                    source: Some(InputSource::Unknown),
                    bytes: Bytes::from_static(b"query-light"),
                    boundary: None,
                },
            ])
        );
    }

    #[test]
    fn resize_writes_terminal_responses_after_applying_resize() {
        let (handle, mut peer, _read_rx) = actor_with_socket_pair(false);
        let response = Bytes::from_static(b"\x1B[48;40;100;720;900t");

        handle.resize(40, 100, 9, 18, vec![response.clone()]);

        let mut buf = vec![0; response.len()];
        peer.read_exact(&mut buf)
            .expect("peer receives resize response");
        assert_eq!(Bytes::from(buf), response);
        handle.shutdown();
    }

    #[test]
    fn handoff_control_is_not_blocked_by_full_data_queue() {
        let (data_tx, _data_rx) = mpsc::channel(1);
        let (control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(
                Bytes::from_static(b"fill"),
                InputSource::Api,
            ))
            .expect("fill data queue");
        let (wake, _wake_read_fd) = test_wake_pair();
        let handle = PtyIoActorHandle {
            data_tx,
            control_tx,
            wake,
            user_writes: Arc::new(Mutex::new(UserWriteGate { accepting: true })),
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            foreground_fd: PtyForegroundObserver::default(),
            consumer_epoch: Arc::new(Mutex::new(None)),
        };

        let handoff = std::thread::spawn(move || handle.begin_handoff(Duration::from_secs(1)));
        match control_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("handoff control command")
        {
            PtyIoControlCommand::BeginHandoff(reply) => {
                reply.send(Ok(())).expect("handoff waiter alive");
            }
            _ => panic!("expected begin handoff command"),
        }

        handoff
            .join()
            .expect("handoff thread joins")
            .expect("handoff succeeds despite full data queue");
    }

    #[test]
    fn begin_handoff_drains_user_writes_already_in_command_queue() {
        let (actor_socket, mut peer) = UnixStream::pair().expect("socket pair");
        actor_socket
            .set_nonblocking(true)
            .expect("actor socket nonblocking");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer timeout");
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        let (_control_tx, control_rx) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(
                Bytes::from_static(b"queued-before-ack"),
                InputSource::Api,
            ))
            .expect("queued write");
        let mut runner = PtyIoActorRunner {
            pane_id: 1,
            consumer_epoch: Arc::new(Mutex::new(None)),
            consumer: None,
            enrolled_groups: Vec::new(),
            pending_consumer: None,
            marker_reply: None,
            sanitizer: Sanitizer::default(),
            file: ActorPtyFile::new(std::fs::File::from(unsafe {
                OwnedFd::from_raw_fd(actor_socket.into_raw_fd())
            })),
            data_rx,
            control_rx,
            state: ActorState::Running,
            pending_writes: VecDeque::new(),
            current_write_offset: 0,
            active_submission: None,
            pending_handoff: None,
            wake_read_fd: fd::create_wake_pipe().expect("wake pipe").read_fd,
            controls: Arc::new(Mutex::new(SharedPtyControls::default())),
            response_order: Arc::new(Mutex::new(())),
            on_read: Box::new(|_| PtyReadResult::empty()),
            on_reader_exit: None,
            poll_observer: None,
        };

        runner.begin_handoff().expect("handoff drains queued write");

        let mut buf = [0u8; 17];
        peer.read_exact(&mut buf)
            .expect("queued write reaches peer before quiesce ack");
        assert_eq!(&buf, b"queued-before-ack");
        assert_eq!(runner.state, ActorState::Quiesced);
    }

    #[test]
    fn input_consumer_handoff_refuses_operation_without_losing_accepted_later_input() {
        let (mut runner, mut peer) = actor_runner_for_unit_test();
        let (data_tx, data_rx) = mpsc::channel(ACTOR_COMMAND_BUFFER);
        runner.data_rx = data_rx;
        let (reply, receipt) = std_mpsc::channel();
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(
                Bytes::from_static(b"before"),
                InputSource::Api,
            ))
            .expect("before");
        data_tx
            .try_send(PtyIoDataCommand::Consumer {
                operation: ConsumerOperation::Enroll {
                    peer: crate::platform::ProcessIdentity {
                        pid: 1,
                        start_time: 0,
                    },
                },
                audit: None,
                reply,
            })
            .expect("consumer");
        data_tx
            .try_send(PtyIoDataCommand::WriteUserInput(
                Bytes::from_static(b"after"),
                InputSource::Api,
            ))
            .expect("after");
        runner
            .begin_handoff()
            .expect("handoff drains accepted ordinary writes");
        let mut bytes = [0; 11];
        peer.read_exact(&mut bytes).expect("all input drained");
        assert_eq!(&bytes, b"beforeafter");
        assert!(
            matches!(receipt.recv_timeout(Duration::from_secs(1)),Ok(ConsumerResponse::Refused { reason }) if reason=="runtime_unavailable")
        );
    }

    #[test]
    fn release_after_commit_prevents_further_io() {
        let (handle, mut peer, read_rx) = actor_with_socket_pair(false);

        let observer = handle.foreground_observer();
        assert_eq!(observer.observe(), Some(None), "live non-TTY is not closed");
        let slot = observer.0.upgrade().expect("live observer gate");
        let sample = slot.lock().expect("simulate in-flight scalar observation");
        let release_handle = handle.clone();
        let (release_tx, release_rx) = std_mpsc::channel();
        let release_thread = std::thread::spawn(move || {
            release_tx
                .send(release_handle.release_after_commit())
                .expect("release receiver");
        });
        assert!(
            release_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "release ACK must wait for an in-flight observer"
        );
        drop(sample);
        release_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("release ACK")
            .expect("actor released");
        release_thread.join().expect("release thread");
        assert_eq!(observer.observe(), None, "release ACK is a close barrier");
        assert!(handle
            .try_write_user_input(Bytes::from_static(b"blocked"))
            .is_err());

        let _ = peer.write_all(b"ignored");
        assert!(read_rx.recv_timeout(Duration::from_millis(150)).is_err());
    }
}
