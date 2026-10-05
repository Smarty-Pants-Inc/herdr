mod submission_guard;

pub(crate) use submission_guard::SubmissionGuard;

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub(crate) use unix::*;

#[cfg(windows)]
mod windows {
    use super::SubmissionGuard;
    use std::io::{Read, Write};
    use std::sync::{mpsc as std_mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use portable_pty::{MasterPty, PtySize};
    use tokio::sync::mpsc;
    use tracing::{debug, warn};

    pub(crate) struct PtyReadResult {
        pub terminal_responses: Vec<Bytes>,
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

    struct PtyResizeRequest {
        resize: PtyResize,
        terminal_responses: Vec<Bytes>,
    }

    pub(crate) struct PtyIoActorConfig {
        pub pane_id: u32,
        pub master: Box<dyn MasterPty + Send>,
        pub initially_quiesced: bool,
        pub on_read: ReadCallback,
        pub on_reader_exit: Option<ReaderExitCallback>,
    }

    enum PtyIoDataCommand {
        WriteUserInput(Bytes),
        SubmitUserInput {
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            guard: Option<SubmissionGuard>,
            reply: std_mpsc::Sender<std::io::Result<()>>,
        },
    }

    enum PtyIoWriteCommand {
        Write(Bytes),
        SubmissionPart {
            bytes: Bytes,
            deadline: Option<Instant>,
            guard: Option<SubmissionGuard>,
            reply: std_mpsc::Sender<std::io::Result<()>>,
        },
    }

    enum PtyIoControlCommand {
        Resize(PtyResizeRequest),
        Shutdown,
    }

    #[derive(Clone)]
    pub(crate) struct PtyIoActorHandle {
        data_tx: mpsc::Sender<PtyIoDataCommand>,
        control_tx: std_mpsc::Sender<PtyIoControlCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        response_order: Arc<Mutex<()>>,
        accepting: Arc<Mutex<bool>>,
    }

    impl PtyIoActorHandle {
        pub(crate) fn try_write_user_input(
            &self,
            bytes: Bytes,
        ) -> Result<(), mpsc::error::TrySendError<Bytes>> {
            if !*self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return Err(mpsc::error::TrySendError::Closed(bytes));
            }
            self.data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(bytes))
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(command) => {
                        let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                            unreachable!("queued write returned another command")
                        };
                        mpsc::error::TrySendError::Full(bytes)
                    }
                    mpsc::error::TrySendError::Closed(command) => {
                        let PtyIoDataCommand::WriteUserInput(bytes) = command else {
                            unreachable!("queued write returned another command")
                        };
                        mpsc::error::TrySendError::Closed(bytes)
                    }
                })
        }

        pub(crate) fn queue_user_input_submission(
            &self,
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
        ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
            self.queue_submission(text, enter, delay, deadline, None)
        }

        pub(crate) fn queue_guarded_user_input_submission(
            &self,
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            guard: SubmissionGuard,
        ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
            self.queue_submission(text, enter, delay, deadline, Some(guard))
        }

        fn queue_submission(
            &self,
            text: Bytes,
            enter: Bytes,
            delay: Duration,
            deadline: Option<Instant>,
            guard: Option<SubmissionGuard>,
        ) -> std::io::Result<std_mpsc::Receiver<std::io::Result<()>>> {
            let accepting = self
                .accepting
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !*accepting {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pty actor closed",
                ));
            }
            let (reply_tx, reply_rx) = std_mpsc::channel();
            self.data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    deadline,
                    guard,
                    reply: reply_tx,
                })
                .map_err(|err| match err {
                    mpsc::error::TrySendError::Full(_) => std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "pty input queue is full",
                    ),
                    mpsc::error::TrySendError::Closed(_) => {
                        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
                    }
                })?;
            Ok(reply_rx)
        }

        pub(crate) fn write_terminal_response(&self, response: impl FnOnce() -> Option<Bytes>) {
            let _order = self
                .response_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(bytes) = response().filter(|bytes| !bytes.is_empty()) {
                let _ = self.write_tx.send(PtyIoWriteCommand::Write(bytes));
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
            let _ = self
                .control_tx
                .send(PtyIoControlCommand::Resize(PtyResizeRequest {
                    resize: PtyResize {
                        rows,
                        cols,
                        cell_width_px,
                        cell_height_px,
                    },
                    terminal_responses,
                }));
        }

        pub(crate) fn shutdown(&self) {
            if let Ok(mut accepting) = self.accepting.lock() {
                *accepting = false;
            }
            let _ = self.control_tx.send(PtyIoControlCommand::Shutdown);
        }
    }

    pub(crate) struct PtyIoActor;

    impl PtyIoActor {
        pub(crate) fn spawn(config: PtyIoActorConfig) -> std::io::Result<PtyIoActorHandle> {
            let PtyIoActorConfig {
                pane_id,
                master,
                initially_quiesced,
                mut on_read,
                on_reader_exit,
            } = config;

            let mut reader = master
                .try_clone_reader()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let mut writer = master
                .take_writer()
                .map_err(|err| std::io::Error::other(err.to_string()))?;
            let (data_tx, mut data_rx) = mpsc::channel::<PtyIoDataCommand>(1024);
            let (control_tx, control_rx) = std_mpsc::channel::<PtyIoControlCommand>();
            let (write_tx, write_rx) = std_mpsc::channel::<PtyIoWriteCommand>();
            let response_order = Arc::new(Mutex::new(()));
            let accepting = Arc::new(Mutex::new(!initially_quiesced));

            std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                debug!(pane_id, "windows pty writer thread exiting");
            });

            {
                let write_tx = write_tx.clone();
                let accepting = Arc::clone(&accepting);
                std::thread::spawn(move || {
                    run_input_forwarder(&mut data_rx, write_tx, accepting);
                    debug!(pane_id, "windows pty input thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                let response_order = Arc::clone(&response_order);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let _order = response_order
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                let result = on_read(&buf[..n]);
                                if result.terminal_responses.into_iter().any(|response| {
                                    write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                }) {
                                    break;
                                }
                            }
                            Err(err) => {
                                debug!(pane_id, err = %err, "windows pty reader failed");
                                break;
                            }
                        }
                    }
                    if let Some(on_reader_exit) = on_reader_exit {
                        on_reader_exit();
                    }
                    debug!(pane_id, "windows pty reader thread exiting");
                });
            }

            {
                let write_tx = write_tx.clone();
                std::thread::spawn(move || {
                    for command in control_rx {
                        match command {
                            PtyIoControlCommand::Resize(request) => {
                                let size = request.resize;
                                if let Err(err) = master.resize(PtySize {
                                    rows: size.rows,
                                    cols: size.cols,
                                    pixel_width: size.cell_width_px.min(u16::MAX as u32) as u16,
                                    pixel_height: size.cell_height_px.min(u16::MAX as u32) as u16,
                                }) {
                                    warn!(pane_id, err = %err, "windows pty resize failed");
                                }
                                if request.terminal_responses.into_iter().any(|response| {
                                    write_tx.send(PtyIoWriteCommand::Write(response)).is_err()
                                }) {
                                    break;
                                }
                            }
                            PtyIoControlCommand::Shutdown => break,
                        }
                    }
                    debug!(pane_id, "windows pty control thread exiting");
                });
            }

            Ok(PtyIoActorHandle {
                data_tx,
                control_tx,
                write_tx,
                response_order,
                accepting,
            })
        }
    }

    fn run_writer(writer: &mut impl Write, write_rx: std_mpsc::Receiver<PtyIoWriteCommand>) {
        for command in write_rx {
            let result = match command {
                PtyIoWriteCommand::Write(bytes) => write_and_flush(writer, &bytes),
                PtyIoWriteCommand::SubmissionPart {
                    bytes,
                    deadline,
                    guard,
                    reply,
                } => {
                    let (result, failed) =
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            (Err(input_submission_timed_out()), false)
                        } else if let Some(guard) = guard {
                            match write_guarded_and_flush(writer, &bytes, &guard) {
                                Ok(()) => (Ok(()), false),
                                Err(SubmissionWriteError::Guard(err)) => (Err(err), false),
                                Err(SubmissionWriteError::Io(err)) => (Err(err), true),
                            }
                        } else {
                            let result = write_and_flush(writer, &bytes);
                            let failed = result
                                .as_ref()
                                .is_err_and(|err| err.kind() != std::io::ErrorKind::TimedOut);
                            (result, failed)
                        };
                    let _ = reply.send(result);
                    if failed {
                        break;
                    }
                    continue;
                }
            };
            if result.is_err() {
                break;
            }
        }
    }

    fn run_input_forwarder(
        data_rx: &mut mpsc::Receiver<PtyIoDataCommand>,
        write_tx: std_mpsc::Sender<PtyIoWriteCommand>,
        accepting: Arc<Mutex<bool>>,
    ) {
        while let Some(command) = data_rx.blocking_recv() {
            match command {
                PtyIoDataCommand::WriteUserInput(bytes) => {
                    if write_tx.send(PtyIoWriteCommand::Write(bytes)).is_err() {
                        break;
                    }
                }
                PtyIoDataCommand::SubmitUserInput {
                    text,
                    enter,
                    delay,
                    deadline,
                    guard,
                    reply,
                } => {
                    let result = if deadline.is_some_and(|deadline| {
                        deadline.saturating_duration_since(Instant::now()) <= delay
                    }) {
                        Err(input_submission_timed_out())
                    } else {
                        let text_deadline =
                            deadline.and_then(|deadline| deadline.checked_sub(delay));
                        write_submission_part(&write_tx, text, text_deadline, guard.clone())
                            .and_then(|()| {
                                // Legacy submissions finish Enter even if the caller stops
                                // waiting. Guarded submissions still require authorization.
                                std::thread::sleep(delay);
                                let accepting = accepting
                                    .lock()
                                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                                if !*accepting {
                                    return Err(pty_actor_closed());
                                }
                                if let Some(guard) = &guard {
                                    guard.check()?;
                                }
                                write_submission_part(&write_tx, enter, None, guard.clone())
                            })
                    };
                    let failed = result.as_ref().is_err_and(|err| {
                        err.kind() != std::io::ErrorKind::TimedOut
                            && !(guard.is_some()
                                && err.kind() == std::io::ErrorKind::PermissionDenied)
                    });
                    let _ = reply.send(result);
                    if failed {
                        break;
                    }
                }
            }
        }
    }

    fn write_submission_part(
        write_tx: &std_mpsc::Sender<PtyIoWriteCommand>,
        bytes: Bytes,
        deadline: Option<Instant>,
        guard: Option<SubmissionGuard>,
    ) -> std::io::Result<()> {
        let (reply, completion) = std_mpsc::channel();
        write_tx
            .send(PtyIoWriteCommand::SubmissionPart {
                bytes,
                deadline,
                guard,
                reply,
            })
            .map_err(|_| pty_actor_closed())?;
        completion
            .recv()
            .unwrap_or_else(|_| Err(pty_actor_closed()))
    }

    fn pty_actor_closed() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty actor closed")
    }

    fn input_submission_timed_out() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "agent prompt timed out before input submission",
        )
    }

    enum SubmissionWriteError {
        Guard(std::io::Error),
        Io(std::io::Error),
    }

    fn write_guarded_and_flush(
        writer: &mut impl Write,
        mut bytes: &[u8],
        guard: &SubmissionGuard,
    ) -> Result<(), SubmissionWriteError> {
        if bytes.is_empty() {
            guard.check().map_err(SubmissionWriteError::Guard)?;
        }
        while !bytes.is_empty() {
            // write_all hides partial writes and Interrupted retries from the
            // guard. Keep each actual Write operation inside this boundary.
            guard.check().map_err(SubmissionWriteError::Guard)?;
            match writer.write(bytes) {
                Ok(0) => {
                    return Err(SubmissionWriteError::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "PTY actor write returned zero bytes",
                    )))
                }
                Ok(written) => bytes = &bytes[written..],
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => return Err(SubmissionWriteError::Io(err)),
            }
        }
        writer.flush().map_err(SubmissionWriteError::Io)
    }

    fn write_and_flush(writer: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
        writer.write_all(bytes)?;
        writer.flush()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct RecordingWriter {
            writes: Vec<(Vec<u8>, Instant)>,
            flushes: Vec<Instant>,
            fail_after: Option<usize>,
            flushed: std_mpsc::Sender<()>,
        }

        impl Write for RecordingWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.fail_after == Some(self.writes.len()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "writer closed",
                    ));
                }
                self.writes.push((bytes.to_vec(), Instant::now()));
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes.push(Instant::now());
                let _ = self.flushed.send(());
                Ok(())
            }
        }

        fn run_recorded_submission(
            fail_after: Option<usize>,
            delay: Duration,
            deadline: Option<Instant>,
            during_delay: impl FnOnce(&std_mpsc::Sender<PtyIoWriteCommand>, &Arc<Mutex<bool>>),
        ) -> (RecordingWriter, std::io::Result<()>) {
            let (flushed_tx, flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"prompt"),
                    enter: Bytes::from_static(b"\r"),
                    delay,
                    deadline,
                    guard: None,
                    reply: reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                    b"user",
                )))
                .unwrap();
            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_accepting = Arc::clone(&accepting);
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(&mut data_rx, input_write_tx, input_accepting)
            });
            flushed_rx.recv().expect("prompt was flushed");
            during_delay(&write_tx, &accepting);
            let result = reply_rx.recv().expect("writer reports submission");
            drop(data_tx);
            input_thread.join().expect("input thread joins");
            drop(write_tx);
            (writer_thread.join().expect("writer thread joins"), result)
        }

        fn refusal() -> std::io::Error {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "target changed")
        }

        #[test]
        fn guarded_writer_queue_invalidation_preserves_unrelated_and_legacy_writes() {
            use std::sync::atomic::{AtomicBool, Ordering};
            let valid = Arc::new(AtomicBool::new(true));
            let guard = SubmissionGuard::new({
                let valid = Arc::clone(&valid);
                move || {
                    if valid.load(Ordering::SeqCst) {
                        Ok(())
                    } else {
                        Err(refusal())
                    }
                }
            });
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let (legacy_tx, legacy_rx) = std_mpsc::channel();
            write_tx
                .send(PtyIoWriteCommand::SubmissionPart {
                    bytes: Bytes::from_static(b"refused"),
                    deadline: None,
                    guard: Some(guard),
                    reply: reply_tx,
                })
                .unwrap();
            write_tx
                .send(PtyIoWriteCommand::Write(Bytes::from_static(b"response")))
                .unwrap();
            write_tx
                .send(PtyIoWriteCommand::SubmissionPart {
                    bytes: Bytes::from_static(b"legacy"),
                    deadline: None,
                    guard: None,
                    reply: legacy_tx,
                })
                .unwrap();
            drop(write_tx);
            valid.store(false, Ordering::SeqCst);
            let mut writer = Vec::new();
            run_writer(&mut writer, write_rx);
            assert_eq!(
                reply_rx.recv().unwrap().unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            legacy_rx.recv().unwrap().unwrap();
            assert_eq!(writer, b"responselegacy");
        }

        fn run_guarded_sequence(guard: SubmissionGuard) -> (Vec<u8>, std::io::Result<()>) {
            let (data_tx, mut data_rx) = mpsc::channel(3);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (reply_tx, reply_rx) = std_mpsc::channel();
            let (legacy_tx, legacy_rx) = std_mpsc::channel();
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"prompt"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::from_millis(1),
                    deadline: None,
                    guard: Some(guard),
                    reply: reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"legacy"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::ZERO,
                    deadline: None,
                    guard: None,
                    reply: legacy_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::WriteUserInput(Bytes::from_static(
                    b"user",
                )))
                .unwrap();
            drop(data_tx);
            let writer_thread = std::thread::spawn(move || {
                let mut writer = Vec::new();
                run_writer(&mut writer, write_rx);
                writer
            });
            run_input_forwarder(&mut data_rx, write_tx, Arc::new(Mutex::new(true)));
            let result = reply_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(matches!(
                reply_rx.try_recv(),
                Err(std_mpsc::TryRecvError::Disconnected)
            ));
            legacy_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
            (writer_thread.join().unwrap(), result)
        }

        #[test]
        fn guarded_forwarder_refusal_preserves_subsequent_inputs() {
            let (bytes, result) = run_guarded_sequence(SubmissionGuard::new(|| Err(refusal())));
            assert_eq!(
                result.unwrap_err().kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert_eq!(bytes, b"legacy\ruser");
        }

        #[test]
        fn guarded_enter_rechecks_in_forwarder_and_actual_writer() {
            use std::sync::atomic::{AtomicUsize, Ordering};
            // Check 0 is text at the actual writer, 1 is delayed Enter at the
            // forwarder, 2 is Enter after waiting in the actual writer queue.
            for refuse_at in [1, 2] {
                let checks = AtomicUsize::new(0);
                let guard = SubmissionGuard::new(move || {
                    if checks.fetch_add(1, Ordering::SeqCst) == refuse_at {
                        Err(refusal())
                    } else {
                        Ok(())
                    }
                });
                let (bytes, result) = run_guarded_sequence(guard);
                assert_eq!(
                    result.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(bytes, b"promptlegacy\ruser");
            }
        }

        #[test]
        fn guarded_success_writes_once_and_completes_once() {
            let (bytes, result) = run_guarded_sequence(SubmissionGuard::new(|| Ok(())));
            result.unwrap();
            assert_eq!(bytes, b"prompt\rlegacy\ruser");
        }

        #[test]
        fn guarded_chunks_and_retries_recheck_before_each_write() {
            use std::sync::atomic::{AtomicBool, Ordering};
            struct InvalidateWriter {
                valid: Arc<AtomicBool>,
                first: Option<std::io::Result<usize>>,
                attempts: usize,
            }
            impl Write for InvalidateWriter {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                    self.attempts += 1;
                    self.valid.store(false, Ordering::SeqCst);
                    self.first.take().unwrap_or(Ok(bytes.len()))
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            for first in [
                Ok(1),
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
            ] {
                let valid = Arc::new(AtomicBool::new(true));
                let guard = SubmissionGuard::new({
                    let valid = Arc::clone(&valid);
                    move || {
                        if valid.load(Ordering::SeqCst) {
                            Ok(())
                        } else {
                            Err(refusal())
                        }
                    }
                });
                let mut writer = InvalidateWriter {
                    valid,
                    first: Some(first),
                    attempts: 0,
                };
                match write_guarded_and_flush(&mut writer, b"prompt", &guard) {
                    Err(SubmissionWriteError::Guard(err)) => {
                        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied)
                    }
                    _ => panic!("guard must refuse remaining chunk/retry"),
                }
                assert_eq!(writer.attempts, 1);
            }
        }

        #[test]
        fn submission_sequences_user_input_but_allows_terminal_responses() {
            let delay = Duration::from_millis(30);
            let (writer, result) = run_recorded_submission(None, delay, None, |write_tx, _| {
                write_tx
                    .send(PtyIoWriteCommand::Write(Bytes::from_static(b"response")))
                    .unwrap();
            });
            result.expect("submission succeeds");

            assert_eq!(writer.writes[0].0, b"prompt");
            assert_eq!(writer.writes[1].0, b"response");
            assert_eq!(writer.writes[2].0, b"\r");
            assert_eq!(writer.writes[3].0, b"user");
            assert!(writer.writes[2].1.duration_since(writer.flushes[0]) >= delay);
        }

        #[test]
        fn submission_returns_enter_write_failure() {
            let (_writer, result) =
                run_recorded_submission(Some(1), Duration::ZERO, None, |_, _| {});
            let err = result.expect_err("enter failure reaches caller");

            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        }

        #[test]
        fn shutdown_during_submission_delay_cancels_enter() {
            let (writer, result) =
                run_recorded_submission(None, Duration::from_millis(30), None, |_, accepting| {
                    *accepting.lock().unwrap() = false;
                });
            let err = result.expect_err("shutdown cancels enter");
            assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"prompt"]
            );
        }

        #[test]
        fn expired_queued_submission_is_not_written() {
            let (flushed_tx, _flushed_rx) = std_mpsc::channel();
            let mut writer = RecordingWriter {
                writes: Vec::new(),
                flushes: Vec::new(),
                fail_after: None,
                flushed: flushed_tx,
            };
            let (data_tx, mut data_rx) = mpsc::channel(2);
            let (write_tx, write_rx) = std_mpsc::channel();
            let (first_reply_tx, first_reply_rx) = std_mpsc::channel();
            let (expired_reply_tx, expired_reply_rx) = std_mpsc::channel();
            let accepting = Arc::new(Mutex::new(true));
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"first"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::from_millis(30),
                    deadline: None,
                    guard: None,
                    reply: first_reply_tx,
                })
                .unwrap();
            data_tx
                .try_send(PtyIoDataCommand::SubmitUserInput {
                    text: Bytes::from_static(b"expired"),
                    enter: Bytes::from_static(b"\r"),
                    delay: Duration::ZERO,
                    deadline: Some(Instant::now() + Duration::from_millis(10)),
                    guard: None,
                    reply: expired_reply_tx,
                })
                .unwrap();

            let writer_thread = std::thread::spawn(move || {
                run_writer(&mut writer, write_rx);
                writer
            });
            let input_write_tx = write_tx.clone();
            let input_thread = std::thread::spawn(move || {
                run_input_forwarder(&mut data_rx, input_write_tx, accepting)
            });
            first_reply_rx.recv().unwrap().unwrap();
            let err = expired_reply_rx.recv().unwrap().unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);

            drop(data_tx);
            input_thread.join().unwrap();
            drop(write_tx);
            let writer = writer_thread.join().unwrap();
            assert_eq!(
                writer
                    .writes
                    .iter()
                    .map(|write| write.0.as_slice())
                    .collect::<Vec<_>>(),
                vec![b"first".as_slice(), b"\r".as_slice()]
            );
        }
    }
}

#[cfg(windows)]
pub(crate) use windows::*;
