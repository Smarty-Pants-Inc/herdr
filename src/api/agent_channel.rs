//! Server-owned registered agent channels. Guarded input is socket data only.
//! No PTY writer, focus event, terminal preparation or Enter exists in this module.
#[cfg(test)]
#[path = "agent_channel_tests.rs"]
mod tests;

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Condvar, Mutex,
};
use std::time::{Duration, Instant};

use interprocess::TryClone as _;

use crate::api::schema::{AdmissionAck, AdmissionStatus, AgentChannelInfoParams, Method, Request};
use crate::ipc::{
    poll_local_stream_read_count, set_local_stream_polling, LocalStream, LocalStreamReadCount,
};
use crate::platform::ProcessIdentity;

pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LEDGER_KEYS: usize = 256;
pub(crate) const MAX_LEDGER_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_IN_FLIGHT: usize = 32;
pub(crate) const MAX_ID_BYTES: usize = 256;
pub(crate) const MAX_RECEIPT_WAITERS: usize = 128;
pub(crate) const MAX_WAITERS_PER_REQUEST: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Outcome {
    Receipt(serde_json::Value),
    Failure { code: &'static str, reason: String },
}
impl Outcome {
    pub(crate) fn failure(code: &'static str, reason: impl Into<String>) -> Self {
        Self::Failure {
            code,
            reason: reason.into(),
        }
    }
    pub(crate) fn response(&self, id: String, duplicate: bool) -> String {
        let value = match self {
            Self::Receipt(receipt) => {
                let mut result = receipt.clone();
                if duplicate {
                    result["duplicate"] = true.into();
                }
                serde_json::json!({"id":id,"result":result})
            }
            Self::Failure { code, reason } => {
                let mut value = serde_json::json!({"id":id,"error":{"code":code,"message":reason}});
                if *code == "agent_prompt_rejected" {
                    value["error"]["reason"] = reason.clone().into();
                }
                value
            }
        };
        value.to_string()
    }
}

#[derive(Debug)]
struct RequestState {
    possible_dispatch: bool,
    complete_dispatch: bool,
    outcome: Option<Outcome>,
}
#[derive(Debug)]
pub(crate) struct Delivery {
    text: String,
    frame: Vec<u8>,
    deadline: Instant,
    state: Mutex<RequestState>,
    completed: Condvar,
    waiters: AtomicUsize,
    channel_waiters: Arc<AtomicUsize>,
}
pub(crate) struct ReceiptWaiter(Arc<Delivery>);
impl Drop for ReceiptWaiter {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::AcqRel);
        self.0.channel_waiters.fetch_sub(1, Ordering::AcqRel);
    }
}
impl Delivery {
    pub(crate) fn waiter(self: &Arc<Self>) -> Result<ReceiptWaiter, Outcome> {
        let increment = |value: &AtomicUsize, max| {
            value.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < max).then_some(count + 1)
            })
        };
        if increment(&self.channel_waiters, MAX_RECEIPT_WAITERS).is_err() {
            return Err(Outcome::failure(
                "agent_channel_capacity",
                "channel receipt waiter capacity exhausted",
            ));
        }
        if increment(&self.waiters, MAX_WAITERS_PER_REQUEST).is_err() {
            self.channel_waiters.fetch_sub(1, Ordering::AcqRel);
            return Err(Outcome::failure(
                "agent_channel_capacity",
                "request receipt waiter capacity exhausted",
            ));
        }
        Ok(ReceiptWaiter(self.clone()))
    }
    fn finish(&self, outcome: Outcome) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.outcome.is_none() {
            state.outcome = Some(outcome);
            self.completed.notify_all();
        }
    }
    fn cancel(&self) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.outcome.is_none() {
            state.outcome = Some(if state.possible_dispatch {
                Outcome::failure(
                    "delivery_unknown",
                    "channel lost after possible admission; do not replay",
                )
            } else {
                Outcome::failure(
                    "agent_channel_unavailable",
                    "channel revoked before dispatch",
                )
            });
            self.completed.notify_all();
        }
    }
    pub(crate) fn wait(&self) -> Outcome {
        let Ok(mut state) = self.state.lock() else {
            return Outcome::failure("delivery_unknown", "request state unavailable");
        };
        loop {
            if let Some(outcome) = &state.outcome {
                return outcome.clone();
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let outcome = if state.possible_dispatch {
                    Outcome::failure(
                        "delivery_unknown",
                        "receipt timed out after possible admission; do not replay",
                    )
                } else {
                    Outcome::failure(
                        "agent_channel_unavailable",
                        "deadline expired before dispatch",
                    )
                };
                state.outcome = Some(outcome.clone());
                self.completed.notify_all();
                return outcome;
            }
            match self.completed.wait_timeout(state, remaining) {
                Ok((next, _)) => state = next,
                Err(_) => return Outcome::failure("delivery_unknown", "request state unavailable"),
            }
        }
    }
    fn pending(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.outcome.is_none())
    }
}

#[derive(Debug, Default)]
struct Ledger {
    requests: HashMap<String, Arc<Delivery>>,
    bytes: usize,
}
#[cfg(test)]
type ReaderGate = Arc<(Mutex<bool>, Condvar)>;

#[derive(Debug)]
pub(crate) struct Channel {
    pub(crate) terminal_id: String,
    pub(crate) epoch: String,
    pub(crate) session_generation: String,
    pub(crate) peer: ProcessIdentity,
    pub(crate) root: ProcessIdentity,
    pub(crate) workspace_id: String,
    pub(crate) pane_id: crate::layout::PaneId,
    active: AtomicBool,
    ready: AtomicBool,
    /// Serializes revocation with each actual socket write, never with App access.
    effect: Mutex<()>,
    waiters: Arc<AtomicUsize>,
    ledger: Mutex<Ledger>,
    outbound: std::sync::mpsc::SyncSender<Arc<Delivery>>,
    // Test-only scheduling fence reproduces a reader first scheduled after writer expiry.
    #[cfg(test)]
    reader_gate: Mutex<Option<ReaderGate>>,
    #[cfg(test)]
    writer_stopped: AtomicBool,
}
impl Channel {
    pub(crate) fn new(
        terminal_id: String,
        epoch: String,
        session_generation: String,
        peer: ProcessIdentity,
        root: ProcessIdentity,
        workspace_id: String,
        pane_id: crate::layout::PaneId,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<Arc<Delivery>>) {
        let (outbound, receiver) = std::sync::mpsc::sync_channel(MAX_IN_FLIGHT);
        (
            Arc::new(Self {
                terminal_id,
                epoch,
                session_generation,
                peer,
                root,
                workspace_id,
                pane_id,
                active: AtomicBool::new(true),
                ready: AtomicBool::new(false),
                effect: Mutex::new(()),
                waiters: Arc::new(AtomicUsize::new(0)),
                ledger: Mutex::new(Ledger::default()),
                outbound,
                #[cfg(test)]
                reader_gate: Mutex::new(None),
                #[cfg(test)]
                writer_stopped: AtomicBool::new(false),
            }),
            receiver,
        )
    }
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
    pub(crate) fn is_ready(&self) -> bool {
        self.is_active() && self.ready.load(Ordering::Acquire)
    }
    pub(crate) fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }
    pub(crate) fn revoke(&self) {
        // A structural mutation starts only after any preceding socket write boundary ends.
        let _effect = self.effect.lock();
        self.active.store(false, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        if let Ok(ledger) = self.ledger.lock() {
            for delivery in ledger.requests.values() {
                delivery.cancel();
            }
        }
    }
    pub(crate) fn reserve(
        &self,
        request_id: String,
        text: String,
        timeout: Duration,
    ) -> Result<(Arc<Delivery>, bool, ReceiptWaiter), Outcome> {
        if !self.is_ready() {
            return Err(Outcome::failure(
                "agent_channel_unavailable",
                "no ready channel",
            ));
        }
        let mut ledger = self
            .ledger
            .lock()
            .map_err(|_| Outcome::failure("agent_channel_unavailable", "ledger unavailable"))?;
        if let Some(existing) = ledger.requests.get(&request_id) {
            return if existing.text == text {
                Ok((existing.clone(), true, existing.waiter()?))
            } else {
                Err(Outcome::failure(
                    "payload_mismatch",
                    "request ID already reserved with different text",
                ))
            };
        }
        let frame = serde_json::json!({"type":"deliver","registration_epoch":self.epoch,
            "request_id":request_id,"session_generation":self.session_generation,"text":text})
        .to_string();
        let mut frame = frame.into_bytes();
        frame.push(b'\n');
        let retained_bytes = frame.len() + text.len();
        if frame.len() > MAX_FRAME_BYTES {
            return Err(Outcome::failure(
                "agent_channel_capacity",
                "delivery frame exceeds capacity",
            ));
        }
        if ledger.requests.len() >= MAX_LEDGER_KEYS
            || ledger.bytes + retained_bytes > MAX_LEDGER_BYTES
            || ledger
                .requests
                .values()
                .filter(|delivery| delivery.pending())
                .count()
                >= MAX_IN_FLIGHT
        {
            return Err(Outcome::failure(
                "agent_channel_capacity",
                "epoch ledger or in-flight capacity exhausted",
            ));
        }
        let delivery = Arc::new(Delivery {
            text,
            frame,
            deadline: Instant::now() + timeout,
            state: Mutex::new(RequestState {
                possible_dispatch: false,
                complete_dispatch: false,
                outcome: None,
            }),
            completed: Condvar::new(),
            waiters: AtomicUsize::new(0),
            channel_waiters: self.waiters.clone(),
        });
        let waiter = delivery.waiter()?; // Capacity refusal is before the dispatch queue effect.
        ledger.bytes += retained_bytes;
        ledger.requests.insert(request_id, delivery.clone()); // Reservation precedes enqueue/effect.
        if self.outbound.try_send(delivery.clone()).is_err() {
            delivery.finish(Outcome::failure(
                "agent_channel_capacity",
                "dispatch queue unavailable",
            ));
        }
        Ok((delivery, false, waiter))
    }
    fn ack(&self, ack: AdmissionAck) -> io::Result<()> {
        if !self.is_active()
            || ack.kind != "ack"
            || ack.registration_epoch != self.epoch
            || ack.session_generation != self.session_generation
        {
            return Err(io::Error::other("uncorrelated agent ACK"));
        }
        let ledger = self
            .ledger
            .lock()
            .map_err(|_| io::Error::other("ledger unavailable"))?;
        let Some(delivery) = ledger.requests.get(&ack.request_id) else {
            return Err(io::Error::other("ACK for unreserved request"));
        };
        // A peer may only acknowledge a frame we started dispatching.
        if !delivery
            .state
            .lock()
            .is_ok_and(|state| state.complete_dispatch)
        {
            return Err(io::Error::other("ACK before complete dispatch"));
        }
        let outcome = if ack.status == AdmissionStatus::Rejected {
            let reason = ack
                .reason
                .ok_or_else(|| io::Error::other("rejected ACK requires typed reason"))?;
            Outcome::failure(
                "agent_prompt_rejected",
                serde_json::to_value(reason)
                    .map_err(io::Error::other)?
                    .as_str()
                    .unwrap_or("admission_refused"),
            )
        } else {
            if ack.reason.is_some() {
                return Err(io::Error::other(
                    "success ACK cannot carry rejection reason",
                ));
            }
            let mut result = serde_json::json!({"terminal_id":self.terminal_id,"registration_epoch":self.epoch,
                "request_id":ack.request_id,"status":ack.status,"session_generation":ack.session_generation});
            if ack.duplicate {
                result["duplicate"] = true.into();
            }
            Outcome::Receipt(result)
        };
        delivery.finish(outcome);
        Ok(())
    }
}

/// Single-use connection-local install slot. Caller JSON cannot supply this authority.
#[derive(Debug, Default)]
pub(crate) struct RegistrationInstall {
    pub(crate) closed: bool,
    pub(crate) channel: Option<(Arc<Channel>, std::sync::mpsc::Receiver<Arc<Delivery>>)>,
}
#[derive(Debug, Clone)]
pub(crate) struct RegistrationTransport(pub(crate) Arc<Mutex<RegistrationInstall>>);
impl Default for RegistrationTransport {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(RegistrationInstall::default())))
    }
}
impl RegistrationTransport {
    pub(crate) fn take(&self) -> Option<(Arc<Channel>, std::sync::mpsc::Receiver<Arc<Delivery>>)> {
        let mut slot = self.0.lock().ok()?;
        slot.closed = true; // Cancels queued late installation even after an App timeout.
        slot.channel.take()
    }
}
impl PartialEq for RegistrationTransport {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for RegistrationTransport {}

/// Attachment/ancestry checks run through an ordered App request, not an App lock
/// in the writer. Native liveness/foreground is rechecked after that round trip.
fn boundary_valid(channel: &Channel, api_tx: &crate::api::ApiRequestSender) -> bool {
    if !channel.is_active() {
        return false;
    }
    let response = super::server::dispatch_to_app_with_timeout(
        Request {
            id: "channel:boundary".into(),
            method: Method::AgentChannelInfo(AgentChannelInfoParams {
                target: channel.terminal_id.clone(),
            }),
        },
        api_tx,
        Some(Duration::from_secs(2)),
    );
    let valid = serde_json::from_str::<serde_json::Value>(&response)
        .ok()
        .is_some_and(|value| {
            value["result"]["ready"] == true
                && value["result"]["registration_epoch"] == channel.epoch
        });
    valid
        && channel.is_active()
        && crate::platform::registered_process_is_foreground(channel.root, channel.peer)
}

pub(crate) fn serve(
    mut stream: LocalStream,
    channel: Arc<Channel>,
    receiver: std::sync::mpsc::Receiver<Arc<Delivery>>,
    api_tx: &crate::api::ApiRequestSender,
    running: &AtomicBool,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    let result = serve_inner(&mut stream, &channel, receiver, api_tx, running, stop);
    channel.revoke(); // Disconnect reserves owner in App, but never keeps a ready channel.
    result
}
fn serve_inner(
    stream: &mut LocalStream,
    channel: &Channel,
    receiver: std::sync::mpsc::Receiver<Arc<Delivery>>,
    api_tx: &crate::api::ApiRequestSender,
    running: &AtomicBool,
    stop: Option<&AtomicBool>,
) -> io::Result<()> {
    set_local_stream_polling(stream, true)?;
    let mut reader_stream = stream.try_clone()?;
    let reader_stop = AtomicBool::new(false);
    // Stop even if the writer unwinds; scope joins before either stream can escape.
    struct StopReader<'a>(&'a AtomicBool);
    impl Drop for StopReader<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    std::thread::scope(|scope| {
        let stop_reader = StopReader(&reader_stop);
        let reader = std::thread::Builder::new()
            .name("agent-channel-ack".into())
            .spawn_scoped(scope, || {
                let result = read_acks(&mut reader_stream, channel, running, stop, &reader_stop);
                channel.revoke();
                result
            })?;
        channel.mark_ready();
        let written = (|| {
            while running.load(Ordering::Acquire)
                && !stop.is_some_and(|flag| flag.load(Ordering::Acquire))
                && channel.is_active()
            {
                let delivery = match receiver.recv_timeout(Duration::from_millis(5)) {
                    Ok(delivery) => delivery,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                write_delivery(
                    stream,
                    channel,
                    &delivery,
                    || boundary_valid(channel, api_tx),
                    || {
                        running.load(Ordering::Acquire)
                            && !stop.is_some_and(|flag| flag.load(Ordering::Acquire))
                            && crate::platform::registered_process_is_foreground(
                                channel.root,
                                channel.peer,
                            )
                    },
                )?;
            }
            Ok(())
        })();
        drop(stop_reader);
        #[cfg(test)]
        channel.writer_stopped.store(true, Ordering::Release);
        let read = reader
            .join()
            .map_err(|_| io::Error::other("agent ACK reader panicked"))?;
        written.and(read)
    })
}

/// One bounded parser per connection, independent of App round trips and write retries.
/// ACK handling takes ledger -> request state, never the effect gate or the App queue.
fn read_acks(
    stream: &mut LocalStream,
    channel: &Channel,
    running: &AtomicBool,
    stop: Option<&AtomicBool>,
    reader_stop: &AtomicBool,
) -> io::Result<()> {
    #[cfg(test)]
    if let Some(gate) = channel
        .reader_gate
        .lock()
        .ok()
        .and_then(|gate| gate.clone())
    {
        let (open, changed) = &*gate;
        let mut open = open
            .lock()
            .map_err(|_| io::Error::other("reader test gate"))?;
        while !*open {
            open = changed
                .wait(open)
                .map_err(|_| io::Error::other("reader test gate"))?;
        }
    }
    let mut input = Vec::new();
    let mut bytes = [0u8; 4096];
    let mut drain_remaining = None;
    while running.load(Ordering::Acquire)
        && !stop.is_some_and(|flag| flag.load(Ordering::Acquire))
        && channel.is_active()
    {
        if reader_stop.load(Ordering::Acquire) && drain_remaining.is_none() {
            // Preserve buffered receipts when a partial outbound frame expires before the
            // reader is scheduled. Never wait for more bytes, and cap even a flooding peer.
            drain_remaining = Some(MAX_IN_FLIGHT * (MAX_FRAME_BYTES + 1));
        }
        match poll_local_stream_read_count(stream, &mut bytes)? {
            LocalStreamReadCount::Data(count) => {
                if let Some(remaining) = &mut drain_remaining {
                    if count > *remaining {
                        return Err(io::Error::other("agent ACK stop drain exceeded capacity"));
                    }
                    *remaining -= count;
                }
                for byte in &bytes[..count] {
                    if *byte == b'\n' {
                        let ack = serde_json::from_slice::<AdmissionAck>(&input)
                            .map_err(io::Error::other)?;
                        input.clear();
                        channel.ack(ack)?;
                    } else {
                        if input.len() >= MAX_FRAME_BYTES {
                            return Err(io::Error::other("agent ACK frame exceeded capacity"));
                        }
                        input.push(*byte);
                    }
                }
            }
            LocalStreamReadCount::Pending => {
                if reader_stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            LocalStreamReadCount::Closed => break,
        }
    }
    Ok(())
}

/// Complete newline frames only. Partial cancellation retires the connection.
/// The two checks intentionally straddle the App validation and the socket gate.
fn write_delivery(
    writer: &mut impl Write,
    channel: &Channel,
    delivery: &Delivery,
    mut attachment_valid: impl FnMut() -> bool,
    mut native_valid: impl FnMut() -> bool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < delivery.frame.len() {
        if !delivery.pending() {
            return if offset == 0 {
                Ok(())
            } else {
                Err(io::Error::other("partial frame cancelled"))
            };
        }
        if Instant::now() >= delivery.deadline || !attachment_valid() {
            delivery.cancel();
            return if offset == 0 {
                Ok(())
            } else {
                Err(io::Error::other("partial frame revoked"))
            };
        }
        // Never call App or wait on its queue while this narrow effect gate is held.
        let effect = channel
            .effect
            .lock()
            .map_err(|_| io::Error::other("channel gate unavailable"))?;
        if !channel.is_active() || Instant::now() >= delivery.deadline || !native_valid() {
            drop(effect);
            delivery.cancel();
            return if offset == 0 {
                Ok(())
            } else {
                Err(io::Error::other("partial frame lost eligibility"))
            };
        }
        let mut state = delivery
            .state
            .lock()
            .map_err(|_| io::Error::other("request state unavailable"))?;
        if state.outcome.is_some() {
            return if offset == 0 {
                Ok(())
            } else {
                Err(io::Error::other("partial frame cancelled"))
            };
        }
        state.possible_dispatch = true;
        let write = writer.write(&delivery.frame[offset..]);
        if write
            .as_ref()
            .is_ok_and(|count| offset + count == delivery.frame.len())
        {
            state.complete_dispatch = true;
        }
        drop(state);
        drop(effect);
        match write {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) =>
            {
                std::thread::sleep(Duration::from_millis(5)); // Recheck every retry.
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn json_success(id: String, result: serde_json::Value) -> String {
    serde_json::json!({"id":id,"result":result}).to_string()
}
