//! Native WebRTC media peer (feature `native-media`).
//!
//! Each peer runs on its own thread with its own tokio runtime, so WebRTC and audio work
//! never block the client event loop. The loop talks to the peer only through a command
//! channel and receives results as [`PeerEvent`]s.
//!
//! Audio: microphone → mono 48 kHz → 20 ms frames → Opus → local track; remote track →
//! Opus → bounded ring buffer → speaker. Devices sit behind [`AudioBackend`] so tests can
//! drive the peer without hardware.

// Off macOS `start` refuses, so only tests construct a peer there.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod controls;
mod speaker;

use controls::{enabled, AudioState, ControlOutcome, Controls};
#[cfg(any(target_os = "macos", test))]
use speaker::Speaker;

use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::interceptor::registry::Registry;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;

use super::peer::{
    MediaPeer, PeerCommandError, PeerEvent, PeerEventSink, PeerEventSinkError, PeerInitialState,
};
use crate::protocol::media::{
    close_code, MediaApplied, MediaControlFailed, MediaDeviceControl, MediaInfo, MediaLevels,
    MediaPeerState, MAX_MEDIA_TEXT_BYTES, MEDIA_ANSWER_CAPACITY, MEDIA_DEVICE_CONTROL_CAPACITY,
    MEDIA_LEGACY_MUTE_CAPACITY,
};

/// The Opus and pipeline sample rate.
pub(crate) const SAMPLE_RATE: u32 = 48_000;
/// One 20 ms mono frame at 48 kHz.
const FRAME_SAMPLES: usize = 960;
const FRAME_DURATION: Duration = Duration::from_millis(20);
/// Largest Opus frame (120 ms) at 48 kHz.
const MAX_DECODED_SAMPLES: usize = 5_760;
/// Playback buffer bound (500 ms). Older samples are dropped when the speaker lags.
const PLAYBACK_CAPACITY: usize = SAMPLE_RATE as usize / 2;
/// Captured frames waiting for the encoder (320 ms). Newer frames are dropped when full.
const CAPTURE_QUEUE_FRAMES: usize = 16;
/// Controls wait in this bounded FIFO while one command awaits the audio workers.
const COMMAND_CAPACITY: usize =
    MEDIA_DEVICE_CONTROL_CAPACITY + MEDIA_LEGACY_MUTE_CAPACITY + MEDIA_ANSWER_CAPACITY;
/// Bound waiting while audio is live; final notification retention is audio-off.
const CRITICAL_EVENT_DELIVERY_TIMEOUT: Duration = Duration::from_secs(1);
/// Observe atomic encoder/callback confirmations off the realtime threads.
const CONTROL_OBSERVE_INTERVAL: Duration = Duration::from_millis(10);
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(3);
const PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The OpenAI Realtime peer expects a data channel with this label.
const EVENTS_CHANNEL_LABEL: &str = "oai-events";

/// Takes mono microphone samples with the request token observed before conversion.
/// Keeping that origin token prevents a late callback from becoming fresh unmuted audio.
pub(crate) type CaptureFn = Box<dyn FnMut(u64, &[f32]) + Send>;
/// Fills a speaker buffer.
pub(crate) type PlaybackFn = Box<dyn FnMut(&mut [f32]) + Send>;

/// Callbacks the peer hands to an audio backend. All samples are mono 48 kHz `f32`.
pub(crate) struct AudioIo {
    /// Takes captured microphone samples. Any chunk size.
    pub(crate) capture: CaptureFn,
    /// Fills the whole buffer with speaker samples (silence when nothing is buffered).
    pub(crate) playback: PlaybackFn,
    /// Reports a device failure; the peer then closes with `device_error`.
    pub(crate) error: Arc<dyn Fn(String) + Send + Sync>,
    changed: Arc<dyn Fn() + Send + Sync>,
    audio: Arc<AudioState>,
}

/// Device ownership, bounded names, and an optional off-callback default-device check.
/// The guard is deliberately thread-local, as CPAL streams are not Send on every host.
pub(crate) struct OpenedAudio {
    _guard: Box<dyn std::any::Any>,
    input_device: String,
    output_device: String,
    default_changed: Option<Box<dyn FnMut() -> bool>>,
}

impl OpenedAudio {
    fn new(guard: Box<dyn std::any::Any>, input: String, output: String) -> Self {
        Self {
            _guard: guard,
            input_device: bounded_device_name(input),
            output_device: bounded_device_name(output),
            default_changed: None,
        }
    }

    fn info(&self, session_id: &str) -> MediaInfo {
        MediaInfo {
            session_id: session_id.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            commit: crate::build_info::commit().to_owned(),
            pid: std::process::id(),
            input_device: self.input_device.clone(),
            output_device: self.output_device.clone(),
        }
    }
}

fn bounded_device_name(mut name: String) -> String {
    let mut end = name.len().min(MAX_MEDIA_TEXT_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

struct CapturedFrame {
    samples: Vec<f32>,
    token: u64,
}

/// Microphone and speaker access.
pub(crate) trait AudioBackend: Send {
    /// Start capture and playback. Dropping the returned guard stops both streams and
    /// releases the devices. It is dropped on the peer thread.
    fn open(self: Box<Self>, io: AudioIo) -> Result<OpenedAudio, String>;
}

enum Command {
    Answer(String),
    Mute(bool),
    Control(MediaDeviceControl),
}

#[derive(Clone, Copy)]
enum CommandClass {
    Answer,
    LegacyMute,
    DeviceControl,
}

impl Command {
    fn class(&self) -> CommandClass {
        match self {
            Self::Answer(_) => CommandClass::Answer,
            Self::Mute(_) => CommandClass::LegacyMute,
            Self::Control(_) => CommandClass::DeviceControl,
        }
    }
}

#[derive(Default)]
struct CommandCredits {
    answer: AtomicUsize,
    legacy_mute: AtomicUsize,
    device_control: AtomicUsize,
}

impl CommandCredits {
    fn counter(&self, class: CommandClass) -> (&AtomicUsize, usize) {
        match class {
            CommandClass::Answer => (&self.answer, MEDIA_ANSWER_CAPACITY),
            CommandClass::LegacyMute => (&self.legacy_mute, MEDIA_LEGACY_MUTE_CAPACITY),
            CommandClass::DeviceControl => (&self.device_control, MEDIA_DEVICE_CONTROL_CAPACITY),
        }
    }

    fn reserve(self: &Arc<Self>, class: CommandClass) -> Result<CommandPermit, PeerCommandError> {
        let (counter, capacity) = self.counter(class);
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < capacity).then_some(used + 1)
            })
            .map_err(|_| PeerCommandError::Full)?;
        Ok(CommandPermit {
            credits: Arc::clone(self),
            class,
        })
    }
}

struct CommandPermit {
    credits: Arc<CommandCredits>,
    class: CommandClass,
}

impl Drop for CommandPermit {
    fn drop(&mut self) {
        self.credits
            .counter(self.class)
            .0
            .fetch_sub(1, Ordering::AcqRel);
    }
}

struct QueuedCommand {
    command: Command,
    permit: CommandPermit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDeliveryError {
    TimedOut,
    ConsumerClosed,
}

enum SessionError {
    Device(String),
    Delivery(EventDeliveryError),
}

impl From<String> for SessionError {
    fn from(message: String) -> Self {
        Self::Device(message)
    }
}

impl From<EventDeliveryError> for SessionError {
    fn from(error: EventDeliveryError) -> Self {
        Self::Delivery(error)
    }
}

/// Own the already-existing media tasks so notification-only retention cannot leave
/// an encoder, remote playback producer or RTCP reader running after audio teardown.
#[derive(Clone, Default)]
struct MediaTasks(Arc<Mutex<(bool, Vec<tokio::task::JoinHandle<()>>)>>);

impl MediaTasks {
    fn retain(&self, task: tokio::task::JoinHandle<()>) {
        match self.0.lock() {
            Ok(mut tasks) if !tasks.0 => tasks.1.push(task),
            Ok(_) => task.abort(),
            Err(_) => task.abort(),
        }
    }

    async fn stop(&self) {
        let tasks = {
            let mut tasks = match self.0.lock() {
                Ok(tasks) => tasks,
                Err(poisoned) => poisoned.into_inner(),
            };
            tasks.0 = true;
            std::mem::take(&mut tasks.1)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

/// Events from WebRTC and audio callbacks to the peer's control loop.
enum Internal {
    PeerState(RTCPeerConnectionState),
    ChannelOpen,
    /// The required events channel closed; the remote side ended the call.
    ChannelClosed,
    DeviceChanged,
    Fatal(String),
}

/// How a session ended when it was not cancelled by the controller.
type SessionEnd = Result<Option<(&'static str, String)>, SessionError>;

type Playback = Arc<Mutex<VecDeque<f32>>>;

/// A running native media peer. Dropping it closes the session.
pub(crate) struct NativePeer {
    commands: mpsc::Sender<QueuedCommand>,
    credits: Arc<CommandCredits>,
    /// Cancels setup and negotiation at their next await, so close never waits for them.
    cancel: watch::Sender<bool>,
    closed: Arc<AtomicBool>,
    /// Set by the peer thread once it has released the devices and stopped.
    // Only tests read it; the reaper thread waits on `done` instead.
    #[cfg_attr(not(test), allow(dead_code))]
    stopped: Arc<AtomicBool>,
    done: Option<std_mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

/// Start a peer with the default microphone and speaker.
#[cfg(target_os = "macos")]
pub(crate) fn start(
    session_id: String,
    initial: PeerInitialState,
    sink: PeerEventSink,
) -> Result<NativePeer, String> {
    start_with_audio(session_id, initial, sink, Box::new(cpal_audio::CpalAudio))
}

/// Start a peer with the default microphone and speaker.
#[cfg(not(target_os = "macos"))]
pub(crate) fn start(
    _session_id: String,
    _initial: PeerInitialState,
    _sink: PeerEventSink,
) -> Result<NativePeer, String> {
    Err("native media audio is only supported on macOS".to_owned())
}

/// Start a peer with the given audio backend. Returns at once; the offer arrives as
/// `PeerEvent::Offer`.
pub(crate) fn start_with_audio(
    session_id: String,
    initial: PeerInitialState,
    sink: PeerEventSink,
    audio: Box<dyn AudioBackend>,
) -> Result<NativePeer, String> {
    // ponytail: two workers keep WebRTC timers and the audio tasks responsive; one peer
    // exists per client at a time.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("herdr-media")
        .enable_all()
        .build()
        .map_err(|error| format!("could not start the media runtime: {error}"))?;
    let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let credits = Arc::new(CommandCredits::default());
    let (cancel, cancel_rx) = watch::channel(false);
    let (done_tx, done) = std_mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = Arc::clone(&stopped);
    let emitter = Emitter { session_id, sink };
    let thread = std::thread::Builder::new()
        .name("herdr-media-peer".to_owned())
        .spawn(move || {
            runtime.block_on(async {
                let result = session(&emitter, initial, audio, command_rx, cancel_rx).await;
                let terminal = match result {
                    Ok(None) => None, // Controller close/disconnect owns settlement.
                    Ok(Some(terminal)) => Some(terminal),
                    Err(SessionError::Device(message)) => {
                        tracing::warn!(session_id = %emitter.session_id, %message, "media peer failed");
                        Some((close_code::DEVICE_ERROR, message))
                    }
                    Err(SessionError::Delivery(error)) => {
                        let message = match error {
                            EventDeliveryError::TimedOut => "media event delivery timed out",
                            EventDeliveryError::ConsumerClosed => "media event consumer closed",
                        };
                        tracing::warn!(session_id = %emitter.session_id, ?error, "media event delivery failed; audio stopped");
                        Some((close_code::DISCONNECTED, message.to_owned()))
                    }
                };
                if let Some((code, message)) = terminal {
                    // Once selected, a later owner cancel cannot erase this terminal.
                    // session has already forced gates and released audio/media tasks.
                    if let Err(error) = emitter.emit_closed(code, message).await {
                        tracing::debug!(?error, "media terminal receiver gone; transport teardown owns remote settlement");
                    }
                }
            });
            runtime.shutdown_timeout(Duration::from_millis(500));
            thread_stopped.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        })
        .map_err(|error| format!("could not start the media thread: {error}"))?;
    Ok(NativePeer {
        commands,
        credits,
        cancel,
        closed,
        stopped,
        done: Some(done),
        thread: Some(thread),
    })
}

impl NativePeer {
    fn enqueue(&self, command: Command) -> Result<(), PeerCommandError> {
        if self.closed.load(Ordering::Acquire) || self.commands.is_closed() {
            return Err(PeerCommandError::Closed);
        }
        let permit = self.credits.reserve(command.class())?;
        self.commands
            .try_send(QueuedCommand { command, permit })
            .map_err(|error| match error {
                // Dropping either rejected envelope rolls its reserved credit back.
                mpsc::error::TrySendError::Full(_) => PeerCommandError::Full,
                mpsc::error::TrySendError::Closed(_) => PeerCommandError::Closed,
            })
    }
}

impl MediaPeer for NativePeer {
    fn apply_answer(&mut self, sdp: String) -> Result<(), PeerCommandError> {
        self.enqueue(Command::Answer(sdp))
    }

    fn set_muted(&mut self, muted: bool) -> Result<(), PeerCommandError> {
        self.enqueue(Command::Mute(muted))
    }

    fn apply_control(&mut self, control: MediaDeviceControl) -> Result<(), PeerCommandError> {
        self.enqueue(Command::Control(control))
    }

    fn close(&mut self) {
        let Some(done) = self.done.take() else {
            return;
        };
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.cancel.send(true);
        // Cancellation makes the peer thread drop the devices at its next await. Closing the
        // WebRTC connection can still take up to a second, so join off the client loop.
        let thread = self.thread.take();
        let reaper = std::thread::Builder::new()
            .name("herdr-media-reaper".to_owned())
            .spawn(move || match done.recv_timeout(THREAD_JOIN_TIMEOUT) {
                Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(thread) = thread {
                        let _ = thread.join();
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!("media peer did not stop within 2 s after close");
                }
            });
        if let Err(error) = reaper {
            tracing::warn!(%error, "could not start the media reaper thread");
        }
    }
}

impl Drop for NativePeer {
    fn drop(&mut self) {
        self.close();
    }
}

/// Critical queue admission runs only on the native worker. Owner cancellation
/// interrupts live delivery in session; selected final delivery is retained audio-off.
struct Emitter {
    session_id: String,
    sink: PeerEventSink,
}

impl Emitter {
    async fn emit(&self, event: PeerEvent) -> Result<(), EventDeliveryError> {
        match tokio::time::timeout(CRITICAL_EVENT_DELIVERY_TIMEOUT, (self.sink)(event)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(PeerEventSinkError::ConsumerClosed)) => Err(EventDeliveryError::ConsumerClosed),
            Err(_) => Err(EventDeliveryError::TimedOut),
        }
    }

    async fn state(&self, state: MediaPeerState, muted: bool) -> Result<(), EventDeliveryError> {
        self.emit(PeerEvent::State {
            session_id: self.session_id.clone(),
            state,
            muted,
            detail: None,
        })
        .await
    }

    async fn emit_closed(
        &self,
        code: &'static str,
        message: String,
    ) -> Result<(), PeerEventSinkError> {
        let delivery = (self.sink)(PeerEvent::Closed {
            session_id: self.session_id.clone(),
            code,
            message,
        });
        tokio::pin!(delivery);
        match tokio::time::timeout(CRITICAL_EVENT_DELIVERY_TIMEOUT, delivery.as_mut()).await {
            Ok(result) => result,
            Err(_) => {
                tracing::debug!(session_id = %self.session_id, "audio off; retaining final media notification");
                // Keep exactly this send future and its FIFO waiter position. No
                // retry, extra worker, live stream or late-cancel suppression.
                delivery.await
            }
        }
    }
}

/// One media session. Returns `Err` when the peer must report `device_error`, and
/// `Ok(Some((code, message)))` for remote close or a changed device. Audio is released and the peer
/// connection closed before it returns.
async fn session(
    emitter: &Emitter,
    initial: PeerInitialState,
    audio: Box<dyn AudioBackend>,
    mut commands: mpsc::Receiver<QueuedCommand>,
    mut cancel: watch::Receiver<bool>,
) -> SessionEnd {
    let audio_state = Arc::new(AudioState::with_initial(initial));
    let mut audio_guard = None;
    let mut peer = None;
    let tasks = MediaTasks::default();
    // Owned outside the cancelled drive future: cancellation cannot recycle its
    // active class credit until admission is closed and both gates are protective.
    let mut active_permit = None;
    let setup_cancel = cancel.clone();
    let result = tokio::select! {
        biased;
        () = cancelled(&mut cancel) => Ok(None),
        result = async {
            let (internal_tx, internal) = mpsc::unbounded_channel();
            let playback: Playback = Arc::new(Mutex::new(VecDeque::with_capacity(PLAYBACK_CAPACITY)));
            let (frames_tx, frames) = mpsc::channel(CAPTURE_QUEUE_FRAMES);
            audio_guard = Some(audio.open(audio_io(
                frames_tx,
                Arc::clone(&playback),
                Arc::clone(&audio_state),
                &internal_tx,
            ))?);
            // Synchronous OS open may finish after owner cancellation. Do not
            // negotiate or construct any more live media in that case.
            if *setup_cancel.borrow() {
                return Ok(None);
            }
            let connection = Arc::new(new_peer_connection().await
                .map_err(|error| format!("could not create the WebRTC peer: {error}"))?);
            peer = Some(Arc::clone(&connection));
            if let Some(devices) = audio_guard.as_mut() {
                drive(
                    emitter,
                    &connection,
                    Channels {
                        internal,
                        internal_tx,
                        frames,
                        playback,
                        audio: Arc::clone(&audio_state),
                    },
                    devices,
                    &mut commands,
                    &mut active_permit,
                    &tasks,
                ).await
            } else {
                Err(SessionError::Device("audio device ownership was lost".to_owned()))
            }
        } => result,
    };
    commands.close();
    audio_state.force_protective_gates();
    drop(audio_guard.take());
    tasks.stop().await;
    drop(active_permit.take());
    // Queued credits are released only after admission closes and teardown gates.
    while commands.try_recv().is_ok() {}
    if let Some(peer) = peer {
        match tokio::time::timeout(PEER_CLOSE_TIMEOUT, peer.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::debug!(%error, "media peer close failed"),
            Err(_) => tracing::debug!("media peer close timed out"),
        }
    }
    result
}

/// Resolves once the controller closes the peer (or drops it).
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

struct Channels {
    internal: mpsc::UnboundedReceiver<Internal>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    frames: mpsc::Receiver<CapturedFrame>,
    playback: Playback,
    audio: Arc<AudioState>,
}

/// The Opus parameters webrtc-rs registers by default.
fn opus_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        capability: RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

#[cfg(test)]
fn loopback_setting_engine() -> webrtc::api::setting_engine::SettingEngine {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    let mut settings = webrtc::api::setting_engine::SettingEngine::default();
    settings.set_include_loopback_candidate(true);
    settings.set_ip_filter(Box::new(|ip| {
        ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
    }));
    // Query-only mDNS opens wildcard:5353 and joins multicast interfaces outside this
    // fixture's loopback-only candidate filter.
    settings.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    settings
}

async fn new_peer_connection() -> webrtc::error::Result<RTCPeerConnection> {
    let mut media = MediaEngine::default();
    // Only Opus: capture encodes and playback decodes nothing else, so offering the other
    // default codecs (G722, PCMU, PCMA) would let an answer pick audio this peer cannot play.
    media.register_codec(opus_codec(), RTPCodecType::Audio)?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let builder = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry);
    #[cfg(test)]
    let builder = builder.with_setting_engine(loopback_setting_engine());
    let api = builder.build();
    // No ICE servers: host candidates reach the public ICE-lite remote peer.
    api.new_peer_connection(RTCConfiguration::default()).await
}

/// Negotiate, then run the control loop until close or failure.
async fn drive(
    emitter: &Emitter,
    peer: &Arc<RTCPeerConnection>,
    channels: Channels,
    devices: &mut OpenedAudio,
    commands: &mut mpsc::Receiver<QueuedCommand>,
    active_permit: &mut Option<CommandPermit>,
    tasks: &MediaTasks,
) -> SessionEnd {
    let Channels {
        mut internal,
        internal_tx,
        frames,
        playback,
        audio,
    } = channels;
    let mut controls = Controls::new(Arc::clone(&audio));
    let webrtc_error = |what: &str, error: webrtc::Error| format!("{what}: {error}");

    let track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: SAMPLE_RATE,
            channels: 2,
            ..Default::default()
        },
        "audio".to_owned(),
        "herdr".to_owned(),
    ));
    // add_track creates one sendrecv audio transceiver.
    let sender = peer
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
        .await
        .map_err(|error| webrtc_error("could not add the microphone track", error))?;
    // Interceptors (NACK, reports) need RTCP to be read.
    tasks.retain(tokio::spawn(async move {
        let mut buffer = vec![0_u8; 1500];
        while sender.read(&mut buffer).await.is_ok() {}
    }));
    tasks.retain(tokio::spawn(encode_microphone(
        frames,
        track,
        Arc::clone(&audio),
        internal_tx.clone(),
    )));

    let channel = peer
        .create_data_channel(EVENTS_CHANNEL_LABEL, None)
        .await
        .map_err(|error| webrtc_error("could not create the events channel", error))?;
    let open_tx = internal_tx.clone();
    channel.on_open(Box::new(move || {
        let _ = open_tx.send(Internal::ChannelOpen);
        Box::pin(async {})
    }));
    // A connected session needs the events channel: losing it ends the call even while
    // ICE and DTLS stay up, so the microphone never stays on without it.
    let close_tx = internal_tx.clone();
    channel.on_close(Box::new(move || {
        let _ = close_tx.send(Internal::ChannelClosed);
        Box::pin(async {})
    }));
    let error_tx = internal_tx.clone();
    channel.on_error(Box::new(move |error| {
        let _ = error_tx.send(Internal::Fatal(format!(
            "the events channel failed: {error}"
        )));
        Box::pin(async {})
    }));

    let state_tx = internal_tx.clone();
    peer.on_peer_connection_state_change(Box::new(move |state| {
        let _ = state_tx.send(Internal::PeerState(state));
        Box::pin(async {})
    }));

    let track_tx = internal_tx;
    let playback_audio = Arc::clone(&audio);
    let playback_tasks = tasks.clone();
    peer.on_track(Box::new(move |remote, _, _| {
        // The handler future runs under a lock; keep it short and play in a task.
        playback_tasks.retain(tokio::spawn(play_remote(
            remote,
            Arc::clone(&playback),
            Arc::clone(&playback_audio),
            track_tx.clone(),
        )));
        Box::pin(async {})
    }));

    let offer = peer
        .create_offer(None)
        .await
        .map_err(|error| webrtc_error("could not create the offer", error))?;
    let mut gathered = peer.gathering_complete_promise().await;
    peer.set_local_description(offer)
        .await
        .map_err(|error| webrtc_error("could not apply the offer", error))?;
    // ponytail: a timeout still sends the candidates gathered so far; host candidates
    // normally finish in milliseconds.
    let _ = tokio::time::timeout(ICE_GATHER_TIMEOUT, gathered.recv()).await;
    let sdp = peer
        .local_description()
        .await
        .ok_or_else(|| "the peer has no local description".to_owned())?
        .sdp;
    emitter
        .emit(PeerEvent::Offer {
            session_id: emitter.session_id.clone(),
            sdp,
        })
        .await?;

    let mut connected = false;
    let mut channel_open = false;
    let mut reported_connected = false;
    let mut observe = tokio::time::interval(CONTROL_OBSERVE_INTERVAL);
    observe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut device_check = tokio::time::interval(DEVICE_CHECK_INTERVAL);
    device_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv(), if controls.idle() && active_permit.is_none() => {
                let Some(QueuedCommand { command, permit }) = command else {
                    return Ok(None);
                };
                *active_permit = Some(permit);
                match command {
                    Command::Answer(sdp) => {
                        let answer = RTCSessionDescription::answer(sdp)
                            .map_err(|error| webrtc_error("invalid answer", error))?;
                        peer.set_remote_description(answer)
                            .await
                            .map_err(|error| webrtc_error("could not apply the answer", error))?;
                        if !reported_connected {
                            emitter.state(MediaPeerState::Connecting, controls.muted).await?;
                        }
                        drop(active_permit.take());
                    }
                    Command::Mute(mute) => {
                        controls.begin(None, Some(mute), None, None, Instant::now())?;
                    }
                    Command::Control(control) => {
                        controls.begin(
                            Some(control.control_id),
                            control.input_muted,
                            control.output_suppressed,
                            control.levels,
                            Instant::now(),
                        )?;
                    }
                }
            },
            _ = observe.tick(), if controls.observing() => {},
            _ = device_check.tick() => {
                if audio.device_changed.load(Ordering::Acquire)
                    || devices.default_changed.as_mut().is_some_and(|changed| changed())
                {
                    return Ok(Some((close_code::DEVICE_CHANGED, "the default audio device changed".to_owned())));
                }
            },
            event = internal.recv() => match event {
                Some(Internal::PeerState(RTCPeerConnectionState::Connected)) => connected = true,
                Some(Internal::PeerState(RTCPeerConnectionState::Failed)) => {
                    return Err(SessionError::Device("the WebRTC connection failed".to_owned()));
                }
                // Disconnected may recover; it turns into Failed when it does not.
                Some(Internal::PeerState(_)) => {}
                Some(Internal::ChannelOpen) => channel_open = true,
                Some(Internal::ChannelClosed) => {
                    return Ok(Some((close_code::CLOSED, "the remote side closed the events channel".to_owned())));
                }
                Some(Internal::DeviceChanged) => {
                    return Ok(Some((close_code::DEVICE_CHANGED, "the default audio device changed".to_owned())));
                }
                Some(Internal::Fatal(message)) => return Err(SessionError::Device(message)),
                None => return Ok(None),
            },
        }
        if let Some(outcome) = controls.poll(Instant::now()) {
            let legacy_state = match outcome {
                ControlOutcome::Applied(confirmation) => {
                    if let Some(control_id) = confirmation.control_id {
                        emitter
                            .emit(PeerEvent::Applied(MediaApplied {
                                session_id: emitter.session_id.clone(),
                                control_id,
                                input_muted: confirmation.muted,
                                output_suppressed: confirmation.suppressed,
                            }))
                            .await?;
                        false
                    } else {
                        true
                    }
                }
                ControlOutcome::Failed { control_id } => {
                    emitter
                        .emit(PeerEvent::ControlFailed(MediaControlFailed {
                            session_id: emitter.session_id.clone(),
                            control_id,
                            code: "control_timeout".to_owned(),
                            message: Some(
                                "the audio devices did not observe the control within five seconds"
                                    .to_owned(),
                            ),
                        }))
                        .await?;
                    control_id.is_none()
                }
            };
            if legacy_state {
                emitter
                    .state(
                        if reported_connected {
                            MediaPeerState::Connected
                        } else {
                            MediaPeerState::Connecting
                        },
                        controls.muted,
                    )
                    .await?;
            }
            // Includes the entire legacy failure + truthful State sequence.
            drop(active_permit.take());
        }
        if connected && channel_open && !reported_connected {
            reported_connected = true;
            emitter
                .state(MediaPeerState::Connected, controls.muted)
                .await?;
            emitter
                .emit(PeerEvent::Info(devices.info(&emitter.session_id)))
                .await?;
        }
        if let Some((input_peak, output_peak)) = controls.take_levels(Instant::now()) {
            emitter
                .emit(PeerEvent::Levels(MediaLevels {
                    session_id: emitter.session_id.clone(),
                    input_peak,
                    output_peak,
                }))
                .await?;
        }
    }
}

/// Encode captured 20 ms frames and write them to the local track. Sends silence while
/// muted so the remote side keeps a steady stream.
async fn encode_microphone(
    mut frames: mpsc::Receiver<CapturedFrame>,
    track: Arc<TrackLocalStaticSample>,
    audio: Arc<AudioState>,
    internal: mpsc::UnboundedSender<Internal>,
) {
    let mut encoder =
        match opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip) {
            Ok(encoder) => encoder,
            Err(error) => {
                let _ = internal.send(Internal::Fatal(format!("could not start Opus: {error}")));
                return;
            }
        };
    let mut packet = [0_u8; 1500];
    let mut encoded_token = 0;
    while let Some(frame) = frames.recv().await {
        let (size, token) = match encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet,
        ) {
            Ok(Some(encoded)) => encoded,
            Ok(None) => continue,
            Err(error) => {
                let _ = internal.send(Internal::Fatal(format!("Opus encoding failed: {error}")));
                return;
            }
        };
        let sample = Sample {
            data: Bytes::copy_from_slice(&packet[..size]),
            duration: FRAME_DURATION,
            ..Default::default()
        };
        if let Err(error) = track.write_sample(&sample).await {
            tracing::debug!(%error, "could not send a microphone frame");
        }
        // Only the real encoder confirms. Previous writes are finished before this
        // release, so an Applied cannot race an older in-flight microphone sample.
        audio.input_applied.store(token, Ordering::Release);
    }
}

fn encode_frame(
    encoder: &mut opus::Encoder,
    encoded_token: &mut u64,
    frame: &CapturedFrame,
    audio: &AudioState,
    packet: &mut [u8],
) -> Result<Option<(usize, u64)>, opus::Error> {
    let token = audio.input_token();
    // Reject queued and partial pre-control capture, including the first unmuted frame.
    if frame.token != token {
        return Ok(None);
    }
    if *encoded_token != token {
        // Do not carry pre-mute Opus prediction/lookahead into confirmed silence.
        encoder.reset_state()?;
        *encoded_token = token;
    }
    let silence = [0.0_f32; FRAME_SAMPLES];
    let samples = if enabled(token) {
        &silence[..]
    } else {
        &frame.samples[..]
    };
    // Meter exactly the post-mute samples sent to Opus, never the raw muted mic.
    audio.input_peak(samples);
    encoder
        .encode_float(samples, packet)
        .map(|size| Some((size, token)))
}

/// Decode the remote audio track into the playback buffer.
async fn play_remote(
    remote: Arc<TrackRemote>,
    playback: Playback,
    audio: Arc<AudioState>,
    internal: mpsc::UnboundedSender<Internal>,
) {
    let mut decoder = match opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono) {
        Ok(decoder) => decoder,
        Err(error) => {
            let _ = internal.send(Internal::Fatal(format!("could not start Opus: {error}")));
            return;
        }
    };
    let mut pcm = vec![0.0_f32; MAX_DECODED_SAMPLES];
    while let Ok((packet, _)) = remote.read_rtp().await {
        if packet.payload.is_empty() {
            continue;
        }
        let token = audio.output_token();
        match decoder.decode_float(&packet.payload, &mut pcm, false) {
            Ok(count) => push_playback(&playback, &audio, token, &pcm[..count.min(pcm.len())]),
            Err(error) => tracing::debug!(%error, "could not decode a remote audio packet"),
        }
    }
}

fn push_playback(playback: &Playback, audio: &AudioState, token: u64, samples: &[f32]) {
    if enabled(token) || audio.output_token() != token {
        return;
    }
    let Ok(mut buffer) = playback.lock() else {
        return;
    };
    // Check again under the producer lock: a packet decoded before suppression must
    // never refill the drained ring after the speaker confirms that suppression.
    if audio.output_token() != token {
        return;
    }
    let samples = &samples[samples.len().saturating_sub(PLAYBACK_CAPACITY)..];
    let overflow = (buffer.len() + samples.len()).saturating_sub(PLAYBACK_CAPACITY);
    let dropped = overflow.min(buffer.len());
    buffer.drain(..dropped);
    buffer.extend(samples.iter().copied());
}

/// Callbacks that connect an audio backend to the encoder and the playback buffer.
fn audio_io(
    frames: mpsc::Sender<CapturedFrame>,
    playback: Playback,
    audio: Arc<AudioState>,
    internal: &mpsc::UnboundedSender<Internal>,
) -> AudioIo {
    let mut pending = Vec::with_capacity(FRAME_SAMPLES * 2);
    let mut capture_token = 0;
    let capture_audio = Arc::clone(&audio);
    let playback_audio = Arc::clone(&audio);
    let changed_audio = Arc::clone(&audio);
    let error_tx = internal.clone();
    let changed_tx = internal.clone();
    AudioIo {
        // ponytail: allocates one Vec per 20 ms frame on the device thread; a lock-free
        // frame pool is only worth it if glitches show up.
        capture: Box::new(move |token, samples| {
            // Conversion may span a control change. Reject that old callback rather
            // than relabeling its samples (or interpolation history) as newly unmuted.
            if token != capture_audio.input_token() {
                return;
            }
            if capture_token != token {
                pending.clear();
                capture_token = token;
            }
            pending.extend_from_slice(samples);
            while pending.len() >= FRAME_SAMPLES {
                let samples: Vec<f32> = pending.drain(..FRAME_SAMPLES).collect();
                // A full queue drops the frame rather than blocking the device thread.
                let _ = frames.try_send(CapturedFrame { samples, token });
            }
        }),
        playback: Box::new(move |out| {
            let token = playback_audio.output_token();
            // Never block the device thread: play silence when the buffer is busy.
            let Ok(mut buffer) = playback.try_lock() else {
                out.fill(0.0);
                return;
            };
            if enabled(token) {
                buffer.clear();
                out.fill(0.0);
                playback_audio
                    .output_drained
                    .store(token, Ordering::Release);
            } else {
                for sample in out.iter_mut() {
                    *sample = buffer.pop_front().unwrap_or(0.0);
                }
            }
            // Deliberately no output_applied here: the outer CPAL resampler/pending
            // buffer has not necessarily written silence to the device yet.
        }),
        error: Arc::new(move |message| {
            let _ = error_tx.send(Internal::Fatal(message));
        }),
        changed: Arc::new(move || {
            changed_audio.device_changed.store(true, Ordering::Release);
            let _ = changed_tx.send(Internal::DeviceChanged);
        }),
        audio,
    }
}

/// Streaming linear resampler for mono audio.
// ponytail: linear interpolation is enough for speech; use a windowed-sinc resampler if
// aliasing becomes audible on 44.1 kHz devices.
pub(crate) struct Resampler {
    step: f64,
    position: f64,
    last: f32,
}

impl Resampler {
    pub(crate) fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: f64::from(from_rate) / f64::from(to_rate.max(1)),
            position: 0.0,
            last: 0.0,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.position = 0.0;
        self.last = 0.0;
    }

    /// Append the resampled `input` to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        let Some(&tail) = input.last() else {
            return;
        };
        // `position` indexes `input`; -1 refers to the last sample of the previous call.
        let end = (input.len() - 1) as f64;
        while self.position < end {
            let index = self.position.floor();
            let fraction = (self.position - index) as f32;
            let index = index as isize;
            let a = if index < 0 {
                self.last
            } else {
                input[index as usize]
            };
            let b = input[(index + 1) as usize];
            out.push(a + (b - a) * fraction);
            self.position += self.step;
        }
        self.position -= input.len() as f64;
        self.last = tail;
    }
}

#[cfg(target_os = "macos")]
mod cpal_audio {
    use std::sync::Arc;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

    use super::{
        AudioBackend, AudioIo, AudioState, CaptureFn, OpenedAudio, PlaybackFn, Resampler, Speaker,
        FRAME_SAMPLES, SAMPLE_RATE,
    };
    use crate::platform::macos_media;

    /// The default CoreAudio microphone and speaker.
    pub(super) struct CpalAudio;

    impl AudioBackend for CpalAudio {
        fn open(self: Box<Self>, io: AudioIo) -> Result<OpenedAudio, String> {
            let host = cpal::default_host();
            let input = host
                .default_input_device()
                .ok_or_else(|| "no default microphone".to_owned())?;
            let output = host
                .default_output_device()
                .ok_or_else(|| "no default speaker".to_owned())?;
            let input_name = macos_media::device_name(&input)?;
            let output_name = macos_media::device_name(&output)?;
            let input_check = macos_media::default_input_check(&input)?;
            let input_config = input
                .default_input_config()
                .map_err(|error| format!("microphone: {error}"))?;
            let output_config = output
                .default_output_config()
                .map_err(|error| format!("speaker: {error}"))?;

            let capture = match input_config.sample_format() {
                SampleFormat::F32 => input_stream::<f32>(
                    &input,
                    input_config.config(),
                    io.capture,
                    &io.error,
                    &io.changed,
                    Arc::clone(&io.audio),
                ),
                SampleFormat::I16 => input_stream::<i16>(
                    &input,
                    input_config.config(),
                    io.capture,
                    &io.error,
                    &io.changed,
                    Arc::clone(&io.audio),
                ),
                other => Err(format!("unsupported microphone sample format {other}")),
            }?;
            let playback = match output_config.sample_format() {
                SampleFormat::F32 => output_stream::<f32>(
                    &output,
                    output_config.config(),
                    io.playback,
                    &io.error,
                    &io.changed,
                    io.audio,
                ),
                SampleFormat::I16 => output_stream::<i16>(
                    &output,
                    output_config.config(),
                    io.playback,
                    &io.error,
                    &io.changed,
                    io.audio,
                ),
                other => Err(format!("unsupported speaker sample format {other}")),
            }?;
            capture
                .play()
                .map_err(|error| format!("microphone: {error}"))?;
            playback
                .play()
                .map_err(|error| format!("speaker: {error}"))?;
            let mut opened =
                OpenedAudio::new(Box::new((capture, playback)), input_name, output_name);
            opened.default_changed = Some(input_check);
            Ok(opened)
        }
    }

    fn input_stream<T>(
        device: &cpal::Device,
        config: StreamConfig,
        mut capture: CaptureFn,
        error: &Arc<dyn Fn(String) + Send + Sync>,
        changed: &Arc<dyn Fn() + Send + Sync>,
        audio: Arc<AudioState>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let channels = usize::from(config.channels).max(1);
        let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
        let mut mono = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut resampled = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut token = 0;
        let error = Arc::clone(error);
        let changed = Arc::clone(changed);
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    let requested = audio.input_token();
                    if token != requested {
                        resampler.reset();
                        token = requested;
                    }
                    mono.clear();
                    mono.extend(data.chunks(channels).map(|frame| {
                        frame.iter().map(|&s| f32::from_sample_(s)).sum::<f32>()
                            / frame.len() as f32
                    }));
                    resampled.clear();
                    resampler.process(&mono, &mut resampled);
                    capture(token, &resampled);
                },
                move |stream_error| {
                    if macos_media::stream_device_changed(&stream_error) {
                        changed();
                    } else {
                        error(format!("microphone: {stream_error}"));
                    }
                },
                None,
            )
            .map_err(|error| format!("microphone: {error}"))
    }

    fn output_stream<T>(
        device: &cpal::Device,
        config: StreamConfig,
        playback: PlaybackFn,
        error: &Arc<dyn Fn(String) + Send + Sync>,
        changed: &Arc<dyn Fn() + Send + Sync>,
        audio: Arc<AudioState>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample + FromSample<f32>,
        f32: FromSample<T>,
    {
        let channels = usize::from(config.channels).max(1);
        let mut speaker = Speaker::new(config.sample_rate, playback, audio);
        let error = Arc::clone(error);
        let changed = Arc::clone(changed);
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    speaker.render(data, channels, |value| {
                        let sample = T::from_sample_(value);
                        (sample, f32::from_sample_(sample))
                    });
                },
                move |stream_error| {
                    if macos_media::default_output_stream_device_changed(&stream_error) {
                        changed();
                    } else {
                        error(format!("speaker: {stream_error}"));
                    }
                },
                None,
            )
            .map_err(|error| format!("speaker: {error}"))
    }

    #[cfg(test)]
    mod tests {
        use super::macos_media;
        use cpal::{Error, ErrorKind};

        #[test]
        fn default_output_without_replacement_is_device_changed() {
            let error = Error::new(ErrorKind::DeviceNotAvailable);
            assert!(macos_media::default_output_stream_device_changed(&error));
        }

        #[test]
        fn unavailable_input_is_not_device_changed() {
            let error = Error::new(ErrorKind::DeviceNotAvailable);
            assert!(!macos_media::stream_device_changed(&error));
        }

        #[test]
        fn unrelated_output_errors_are_not_device_changed() {
            for kind in [
                ErrorKind::HostUnavailable,
                ErrorKind::PermissionDenied,
                ErrorKind::StreamInvalidated,
                ErrorKind::Xrun,
                ErrorKind::BackendError,
                ErrorKind::Other,
            ] {
                assert!(!macos_media::default_output_stream_device_changed(
                    &Error::new(kind)
                ));
            }
        }

        #[test]
        fn explicit_device_changed_is_recognized_for_input_and_output() {
            let error = Error::new(ErrorKind::DeviceChanged);
            assert!(macos_media::stream_device_changed(&error));
            assert!(macos_media::default_output_stream_device_changed(&error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeDevices {
        opened: AtomicBool,
        released: AtomicBool,
        default_changed: AtomicBool,
        callbacks: Mutex<Option<FakeCallbacks>>,
        audio: Mutex<Option<Arc<AudioState>>>,
        initial_tokens: Mutex<Option<(u64, u64)>>,
        protected_before_drop: AtomicBool,
    }

    struct FakeCallbacks {
        capture: CaptureFn,
        speaker: Speaker,
        changed: Arc<dyn Fn() + Send + Sync>,
    }

    impl FakeDevices {
        fn capture(&self, samples: &[f32]) {
            self.capture_with_token(self.state().input_token(), samples);
        }

        fn capture_with_token(&self, token: u64, samples: &[f32]) {
            (self.callbacks.lock().unwrap().as_mut().unwrap().capture)(token, samples);
        }

        fn render(&self) -> Vec<f32> {
            self.render_samples(127)
        }

        fn render_samples(&self, samples: usize) -> Vec<f32> {
            let mut out = vec![1.0; samples];
            self.callbacks
                .lock()
                .unwrap()
                .as_mut()
                .unwrap()
                .speaker
                .render(&mut out, 1, |sample| (sample, sample));
            out
        }

        fn state(&self) -> Arc<AudioState> {
            Arc::clone(self.audio.lock().unwrap().as_ref().unwrap())
        }

        fn wait_request(&self, input: bool, previous: u64) -> u64 {
            let audio = self.state();
            // Original false/false fixtures used zero requests. Production now
            // starts at generation one; preserve their exact bounded wait policy.
            let previous = if previous == 0 { 2 } else { previous };
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let token = if input {
                    audio.input_token()
                } else {
                    audio.output_token()
                };
                if token != previous {
                    return token;
                }
                assert!(
                    Instant::now() < deadline,
                    "the control did not reach the audio worker"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    struct FakeAudio(Arc<FakeDevices>);

    struct FakeGuard(Arc<FakeDevices>);

    impl Drop for FakeGuard {
        fn drop(&mut self) {
            if let Some(audio) = self.0.audio.lock().unwrap().as_ref() {
                self.0.protected_before_drop.store(
                    enabled(audio.input_token())
                        && enabled(audio.output_token())
                        && !audio.levels_enabled(),
                    Ordering::Release,
                );
            }
            self.0.callbacks.lock().unwrap().take();
            self.0.audio.lock().unwrap().take();
            self.0.released.store(true, Ordering::SeqCst);
        }
    }

    impl AudioBackend for FakeAudio {
        fn open(self: Box<Self>, mut io: AudioIo) -> Result<OpenedAudio, String> {
            // Exercise the real frame packer and outer renderer, then retain the
            // callbacks so tests can withhold actual capture/output confirmations.
            *self.0.initial_tokens.lock().unwrap() =
                Some((io.audio.input_token(), io.audio.output_token()));
            (io.capture)(io.audio.input_token(), &vec![0.1; FRAME_SAMPLES * 2]);
            let mut speaker = Speaker::new(44_100, io.playback, Arc::clone(&io.audio));
            speaker.render(&mut [1.0; 480], 1, |sample| (sample, sample));
            *self.0.audio.lock().unwrap() = Some(io.audio);
            *self.0.callbacks.lock().unwrap() = Some(FakeCallbacks {
                capture: io.capture,
                speaker,
                changed: io.changed,
            });
            self.0.opened.store(true, Ordering::SeqCst);
            let devices = Arc::clone(&self.0);
            let mut opened = OpenedAudio::new(
                Box::new(FakeGuard(Arc::clone(&self.0))),
                "test microphone".to_owned(),
                "test speaker".to_owned(),
            );
            opened.default_changed = Some(Box::new(move || {
                devices.default_changed.load(Ordering::Acquire)
            }));
            Ok(opened)
        }
    }

    fn start_fake() -> (NativePeer, Arc<FakeDevices>, std_mpsc::Receiver<PeerEvent>) {
        let devices = Arc::new(FakeDevices::default());
        let (events_tx, events) = std_mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let sink: PeerEventSink = Arc::new(move |event| {
            let result = events_tx
                .lock()
                .map_err(|_| PeerEventSinkError::ConsumerClosed)
                .and_then(|tx| {
                    tx.send(event)
                        .map_err(|_| PeerEventSinkError::ConsumerClosed)
                });
            Box::pin(async move { result })
        });
        let peer = start_with_audio(
            "media_test".to_owned(),
            PeerInitialState::default(),
            sink,
            Box::new(FakeAudio(Arc::clone(&devices))),
        )
        .expect("peer starts");
        (peer, devices, events)
    }

    fn wait_until(flag: &AtomicBool, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if flag.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        flag.load(Ordering::SeqCst)
    }

    fn wait_offer(events: &std_mpsc::Receiver<PeerEvent>) -> String {
        match events.recv_timeout(Duration::from_secs(10)) {
            Ok(PeerEvent::Offer { session_id, sdp }) => {
                assert_eq!(session_id, "media_test");
                sdp
            }
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    #[test]
    fn native_peer_offers_opus_audio_and_events_channel() {
        let (mut peer, devices, events) = start_fake();
        let sdp = wait_offer(&events);
        for needle in [
            "m=audio",
            "opus/48000",
            "a=fingerprint:",
            "a=setup:",
            "m=application",
        ] {
            assert!(sdp.contains(needle), "offer lacks {needle:?}:\n{sdp}");
        }
        // Only the codec this peer encodes and decodes is offered.
        for codec in ["G722/", "PCMU/", "PCMA/"] {
            assert!(!sdp.contains(codec), "offer advertises {codec:?}:\n{sdp}");
        }
        let audio_line = sdp
            .lines()
            .find(|line| line.starts_with("m=audio"))
            .expect("an audio m-line");
        assert!(
            audio_line.trim_end().ends_with("UDP/TLS/RTP/SAVPF 111"),
            "audio formats: {audio_line}"
        );
        assert!(devices.opened.load(Ordering::SeqCst));
        peer.close();
    }

    #[test]
    fn native_peer_close_releases_devices_and_is_idempotent() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        assert!(!devices.released.load(Ordering::SeqCst));
        peer.close();
        assert!(wait_until(&devices.released, Duration::from_secs(2)));
        assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
        peer.close();
        drop(peer);
        // A closed peer reports nothing more, not even Closed.
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn close_during_a_slow_device_open_returns_at_once_and_starts_no_media() {
        struct SlowAudio(Arc<FakeDevices>);
        impl AudioBackend for SlowAudio {
            fn open(self: Box<Self>, _io: AudioIo) -> Result<OpenedAudio, String> {
                std::thread::sleep(Duration::from_millis(300));
                self.0.opened.store(true, Ordering::SeqCst);
                Ok(OpenedAudio::new(
                    Box::new(FakeGuard(Arc::clone(&self.0))),
                    String::new(),
                    String::new(),
                ))
            }
        }
        let devices = Arc::new(FakeDevices::default());
        let (events_tx, events) = std_mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let sink: PeerEventSink = Arc::new(move |event| {
            let result = events_tx
                .lock()
                .map_err(|_| PeerEventSinkError::ConsumerClosed)
                .and_then(|tx| {
                    tx.send(event)
                        .map_err(|_| PeerEventSinkError::ConsumerClosed)
                });
            Box::pin(async move { result })
        });
        let mut peer = start_with_audio(
            "media_test".to_owned(),
            PeerInitialState::default(),
            sink,
            Box::new(SlowAudio(Arc::clone(&devices))),
        )
        .expect("peer starts");
        std::thread::sleep(Duration::from_millis(50));

        let started = std::time::Instant::now();
        peer.close();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "close must not block the client loop"
        );
        // The device finished opening after close; the peer releases it and stops without
        // negotiating.
        assert!(wait_until(&devices.opened, Duration::from_secs(2)));
        assert!(wait_until(&devices.released, Duration::from_secs(2)));
        assert!(wait_until(&peer.stopped, Duration::from_secs(1)));
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn close_before_the_offer_releases_the_devices() {
        let (mut peer, devices, events) = start_fake();
        peer.close();
        assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
        assert_eq!(
            devices.opened.load(Ordering::SeqCst),
            devices.released.load(Ordering::SeqCst),
            "an opened device is always released"
        );
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    fn assert_loopback_candidates(sdp: &str, side: &str) {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        use webrtc::ice::candidate::{candidate_base::unmarshal_candidate, Candidate};

        let mut addresses = std::collections::BTreeSet::new();
        for line in sdp.lines().filter(|line| line.starts_with("a=candidate:")) {
            let raw = line.strip_prefix("a=candidate:").expect("candidate prefix");
            let candidate = unmarshal_candidate(raw)
                .unwrap_or_else(|error| panic!("{side} candidate is invalid: {error}"));
            let address = candidate.address();
            let ip = address.parse::<IpAddr>().unwrap_or_else(|error| {
                panic!("{side} candidate address {address:?} is not an IP: {error}")
            });
            assert!(
                ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST),
                "{side} candidate address must be loopback, got {ip}"
            );
            addresses.insert(ip);
        }
        assert!(
            !addresses.is_empty(),
            "{side} SDP must contain at least one candidate"
        );
    }

    /// Answer with a real in-process WebRTC peer, then wait for client Connected/Info
    /// and a positive remote events-channel Open signal (discovery alone is too early).
    async fn connect_remote(
        peer: &mut NativePeer,
        offer: String,
        events: &std_mpsc::Receiver<PeerEvent>,
    ) -> (
        Arc<RTCPeerConnection>,
        Arc<webrtc::data_channel::RTCDataChannel>,
        MediaInfo,
    ) {
        let mut media = MediaEngine::default();
        media.register_default_codecs().expect("codecs");
        let registry =
            register_default_interceptors(Registry::new(), &mut media).expect("interceptors");
        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(loopback_setting_engine())
            .build();
        let remote = Arc::new(
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .expect("remote peer"),
        );
        let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
        remote.on_data_channel(Box::new(move |channel| {
            // webrtc 0.17.2 calls this before handle_open installs the transport.
            // Closing the discovered Connecting channel can return Ok without
            // closing anything. Do not publish it until its on_open actually fires.
            let channel_tx = channel_tx.clone();
            let opened = Arc::clone(&channel);
            channel.on_open(Box::new(move || {
                let _ = channel_tx.send(opened);
                Box::pin(async {})
            }));
            Box::pin(async {})
        }));
        assert_loopback_candidates(&offer, "native offer");
        remote
            .set_remote_description(RTCSessionDescription::offer(offer).expect("offer sdp"))
            .await
            .expect("apply offer");
        let answer = remote.create_answer(None).await.expect("answer");
        let mut gathered = remote.gathering_complete_promise().await;
        remote
            .set_local_description(answer)
            .await
            .expect("apply answer");
        let _ = tokio::time::timeout(ICE_GATHER_TIMEOUT, gathered.recv()).await;
        let answer = remote.local_description().await.expect("local answer").sdp;
        assert_loopback_candidates(&answer, "remote answer");
        peer.apply_answer(answer).expect("answer admitted");

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match events.recv_timeout(remaining) {
                Ok(PeerEvent::State {
                    state: MediaPeerState::Connected,
                    ..
                }) => break,
                Ok(PeerEvent::State { .. }) => {}
                other => panic!("expected the peer to connect, got {other:?}"),
            }
        }
        let info = match events.recv_timeout(Duration::from_secs(5)) {
            Ok(PeerEvent::Info(info)) => info,
            other => panic!("expected Info after Connected, got {other:?}"),
        };
        let channel = tokio::time::timeout(Duration::from_secs(5), channel_rx.recv())
            .await
            .expect("remote events channel")
            .expect("remote events channel");
        assert_eq!(channel.label(), EVENTS_CHANNEL_LABEL);
        assert_eq!(
            channel.ready_state(),
            webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
        );
        (remote, channel, info)
    }

    #[test]
    fn connected_peer_reports_info_and_remote_channel_close_releases_devices_with_or_without_payload(
    ) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        for send_payload in [false, true] {
            // Use a new peer for each case so a payload on one channel cannot mask
            // an empty-open-channel-close regression on the other.
            let (mut peer, devices, events) = start_fake();
            let offer = wait_offer(&events);
            runtime.block_on(async {
                let (remote, channel, info) = connect_remote(&mut peer, offer, &events).await;
                assert_eq!(info.session_id, "media_test");
                assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
                assert_eq!(info.commit, crate::build_info::commit());
                assert_eq!(info.pid, std::process::id());
                assert_eq!(info.input_device, "test microphone");
                assert_eq!(info.output_device, "test speaker");
                assert!(!devices.released.load(Ordering::SeqCst));
                if send_payload {
                    channel
                        .send_text("healthy events channel")
                        .await
                        .expect("an open channel can send");
                }

                // A positive round trip through both audio workers proves the healthy
                // open channel did not end the session or release its audio devices.
                peer.apply_control(device_control(1, Some(false), Some(false), None)).expect("control admitted");
                devices.wait_request(true, 0);
                devices.wait_request(false, 0);
                devices.capture(&[0.3; FRAME_SAMPLES]);
                devices.render();
                applied(&events, 1, false, false);
                assert!(!devices.released.load(Ordering::SeqCst));
                assert!(!peer.stopped.load(Ordering::SeqCst));
                assert_eq!(
                    channel.ready_state(),
                    webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                );
                assert!(matches!(
                    remote.ice_connection_state(),
                    webrtc::ice_transport::ice_connection_state::RTCIceConnectionState::Connected
                        | webrtc::ice_transport::ice_connection_state::RTCIceConnectionState::Completed
                ));
                assert_eq!(
                    remote.sctp().transport().state(),
                    webrtc::dtls_transport::dtls_transport_state::RTCDtlsTransportState::Connected
                );

                // Close only the open channel, not the live remote ICE/DTLS transports.
                // Client cleanup may subsequently tear them down in response.
                channel.close().await.expect("close the events channel");
                match events.recv_timeout(Duration::from_secs(10)) {
                    Ok(PeerEvent::Closed {
                        session_id,
                        code,
                        message,
                    }) => {
                        assert_eq!(session_id, "media_test");
                        assert_eq!(code, close_code::CLOSED);
                        assert_eq!(message, "the remote side closed the events channel");
                    }
                    other => panic!("expected Closed after the channel closed, got {other:?}"),
                }
                assert!(
                    devices.released.load(Ordering::SeqCst),
                    "Closed must follow device release"
                );
                assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
                let _ = remote.close().await;
            });
            peer.close();
        }
    }

    /// Run on macOS: `cargo test --locked --features native-media --bin herdr client::media::native::tests::manual_connected_cpal_input_device_change -- --ignored --exact --nocapture --test-threads=1`
    /// The positive marker is `HERDR_CPAL_CONNECTED_INFO <MediaInfo JSON>`; the human/harness
    /// changes the default input only after that marker and restores devices after the test.
    /// Capture and WebRTC stay local to this process.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires real CoreAudio devices and an external default-input switch"]
    fn manual_connected_cpal_input_device_change() {
        use cpal::traits::{DeviceTrait, HostTrait};
        use std::io::Write;

        struct TrackingGuard {
            inner: Option<Box<dyn std::any::Any>>,
            released: Arc<AtomicBool>,
        }

        impl Drop for TrackingGuard {
            fn drop(&mut self) {
                drop(self.inner.take());
                self.released.store(true, Ordering::Release);
            }
        }

        struct TrackingCpalAudio(Arc<AtomicBool>);

        impl AudioBackend for TrackingCpalAudio {
            fn open(self: Box<Self>, io: AudioIo) -> Result<OpenedAudio, String> {
                let mut opened = Box::new(super::cpal_audio::CpalAudio).open(io)?;
                let inner = std::mem::replace(&mut opened._guard, Box::new(()));
                opened._guard = Box::new(TrackingGuard {
                    inner: Some(inner),
                    released: Arc::clone(&self.0),
                });
                Ok(opened)
            }
        }

        fn default_device_ids() -> Result<(String, String), String> {
            let host = cpal::default_host();
            let input = host
                .default_input_device()
                .ok_or_else(|| "there is no default input device".to_owned())?
                .id()
                .map_err(|error| format!("default input identity: {error}"))?
                .id()
                .to_owned();
            let output = host
                .default_output_device()
                .ok_or_else(|| "there is no default output device".to_owned())?
                .id()
                .map_err(|error| format!("default output identity: {error}"))?
                .id()
                .to_owned();
            Ok((input, output))
        }

        let (input_before, output_before) =
            default_device_ids().expect("query initial CoreAudio default-device identities");
        let released = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = std_mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let sink: PeerEventSink = Arc::new(move |event| {
            let result = events_tx
                .lock()
                .map_err(|_| PeerEventSinkError::ConsumerClosed)
                .and_then(|tx| {
                    tx.send(event)
                        .map_err(|_| PeerEventSinkError::ConsumerClosed)
                });
            Box::pin(async move { result })
        });
        let mut peer = start_with_audio(
            "media_test".to_owned(),
            PeerInitialState::default(),
            sink,
            Box::new(TrackingCpalAudio(Arc::clone(&released))),
        )
        .expect("real CPAL peer starts");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let offer = match events.recv_timeout(Duration::from_secs(15)) {
            Ok(PeerEvent::Offer { session_id, sdp }) => {
                assert_eq!(session_id, "media_test");
                sdp
            }
            other => panic!("expected a real CPAL media offer, got {other:?}"),
        };
        let (remote, _channel, info) = runtime.block_on(connect_remote(&mut peer, offer, &events));
        assert_eq!(info.session_id, "media_test", "connected Info session");
        assert_eq!(
            default_device_ids().expect("query connected default-device identities"),
            (input_before.clone(), output_before.clone()),
            "default devices changed during setup; wait for the connected marker"
        );
        let info_json = serde_json::to_string(&info).expect("serialize connected media info");
        println!("HERDR_CPAL_CONNECTED_INFO {info_json}");
        std::io::stdout()
            .flush()
            .expect("flush connected media info marker");

        let (device_changed_closed, guard_released_before_closed) =
            match events.recv_timeout(Duration::from_secs(120)) {
                Ok(PeerEvent::Closed {
                    session_id, code, ..
                }) => (
                    session_id == "media_test" && code == close_code::DEVICE_CHANGED,
                    released.load(Ordering::Acquire),
                ),
                Ok(event) => {
                    eprintln!("expected device_changed Closed, got {event:?}");
                    (false, false)
                }
                Err(error) => {
                    eprintln!("timed out waiting for device_changed Closed: {error}");
                    (false, false)
                }
            };
        let (input_changed, output_unchanged) = match default_device_ids() {
            Ok((input_after, output_after)) => {
                (input_after != input_before, output_after == output_before)
            }
            Err(error) => {
                eprintln!("could not query default-device identities after close: {error}");
                (false, false)
            }
        };

        let _ = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), remote.close()).await });
        let worker_done = match peer.done.as_ref() {
            Some(done) => matches!(done.recv_timeout(Duration::from_secs(2)), Ok(())),
            None => false,
        };
        let worker_stopped = peer.stopped.load(Ordering::Acquire);
        peer.close();

        assert!(
            device_changed_closed,
            "the peer did not close with device_changed"
        );
        assert!(
            guard_released_before_closed,
            "the underlying CPAL streams were not released before Closed"
        );
        assert!(
            worker_done && worker_stopped,
            "the peer worker did not stop"
        );
        assert!(input_changed, "the default input identity did not change");
        assert!(
            output_unchanged,
            "the default output identity changed; this does not prove input-change handling"
        );
    }

    #[test]
    fn native_peer_confirms_mute_before_connect() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        peer.set_muted(true).expect("legacy mute admitted");
        devices.wait_request(true, 0);
        assert!(events.recv_timeout(Duration::from_millis(60)).is_err());
        devices.capture(&[0.2; FRAME_SAMPLES]);
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(PeerEvent::State { state, muted, .. }) => {
                assert!(muted);
                assert_eq!(state, MediaPeerState::Connecting);
            }
            other => panic!("expected a mute state, got {other:?}"),
        }
        peer.close();
    }

    #[test]
    fn native_peer_reports_device_failure_as_closed() {
        struct BrokenAudio;
        impl AudioBackend for BrokenAudio {
            fn open(self: Box<Self>, _io: AudioIo) -> Result<OpenedAudio, String> {
                Err("no microphone".to_owned())
            }
        }
        let (events_tx, events) = std_mpsc::channel();
        let events_tx = Mutex::new(events_tx);
        let sink: PeerEventSink = Arc::new(move |event| {
            let result = events_tx
                .lock()
                .map_err(|_| PeerEventSinkError::ConsumerClosed)
                .and_then(|tx| {
                    tx.send(event)
                        .map_err(|_| PeerEventSinkError::ConsumerClosed)
                });
            Box::pin(async move { result })
        });
        let _peer = start_with_audio(
            "media_test".to_owned(),
            PeerInitialState::default(),
            sink,
            Box::new(BrokenAudio),
        )
        .expect("peer starts");
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(PeerEvent::Closed { code, message, .. }) => {
                assert_eq!(code, close_code::DEVICE_ERROR);
                assert_eq!(message, "no microphone");
            }
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    fn device_control(
        id: u64,
        input: Option<bool>,
        output: Option<bool>,
        levels: Option<bool>,
    ) -> MediaDeviceControl {
        MediaDeviceControl {
            session_id: "media_test".to_owned(),
            control_id: id,
            input_muted: input,
            output_suppressed: output,
            levels,
        }
    }

    fn applied(events: &std_mpsc::Receiver<PeerEvent>, id: u64, muted: bool, suppressed: bool) {
        match events.recv_timeout(Duration::from_secs(3)) {
            Ok(PeerEvent::Applied(update)) => {
                assert_eq!(update.session_id, "media_test");
                assert_eq!(update.control_id, id);
                assert_eq!(update.input_muted, muted);
                assert_eq!(update.output_suppressed, suppressed);
            }
            other => panic!("expected Applied {id}, got {other:?}"),
        }
    }

    #[test]
    fn native_controls_wait_for_real_capture_encoder_and_outer_speaker_in_fifo_order() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        let audio = devices.state();
        peer.apply_control(device_control(10, Some(true), Some(true), None))
            .expect("first control admitted");
        peer.apply_control(device_control(11, Some(false), Some(false), None))
            .expect("second control admitted");
        let input = devices.wait_request(true, 0);
        let output = devices.wait_request(false, 0);
        assert!(enabled(input) && enabled(output));
        assert!(events.recv_timeout(Duration::from_millis(60)).is_err());
        assert_ne!(audio.input_applied.load(Ordering::Acquire), input);
        assert_ne!(audio.output_applied.load(Ordering::Acquire), output);

        // A partial frame is not a capture/encoder confirmation. Even actual speaker
        // silence cannot acknowledge the combined control until that frame completes.
        devices.capture(&[0.7; FRAME_SAMPLES - 1]);
        assert!(devices.render().iter().all(|&sample| sample == 0.0));
        assert!(events.recv_timeout(Duration::from_millis(60)).is_err());
        assert_eq!(
            audio.input_token(),
            input,
            "the second control must still be queued"
        );
        assert_eq!(audio.output_token(), output);
        devices.capture(&[0.7]);
        applied(&events, 10, true, true);

        let input = devices.wait_request(true, input);
        let output = devices.wait_request(false, output);
        assert!(!enabled(input) && !enabled(output));
        devices.capture(&[0.4; FRAME_SAMPLES]);
        assert!(
            events.recv_timeout(Duration::from_millis(60)).is_err(),
            "unmute does not bypass the pending outer output confirmation"
        );
        devices.render();
        applied(&events, 11, false, false);
        peer.close();
    }

    #[test]
    fn real_opus_encoder_rejects_old_capture_and_encodes_silence_only_while_muted() {
        use controls::LEVELS_INTERVAL;

        let now = Instant::now();
        let audio = Arc::new(AudioState::default());
        let mut controls = Controls::new(Arc::clone(&audio));
        let (frames_tx, mut frames) = mpsc::channel(CAPTURE_QUEUE_FRAMES);
        let (internal, _internal_rx) = mpsc::unbounded_channel();
        let playback: Playback = Arc::new(Mutex::new(VecDeque::new()));
        let mut io = audio_io(
            frames_tx,
            Arc::clone(&playback),
            Arc::clone(&audio),
            &internal,
        );
        let mut speaker = Speaker::new(SAMPLE_RATE, io.playback, Arc::clone(&audio));
        let mut encoder =
            opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
        let tone: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.7)
            .collect();
        let mut packet = [0_u8; 1500];
        let mut encoded_token = 0;
        controls
            .begin(Some(1), None, None, Some(true), now)
            .unwrap();
        assert!(controls.confirmed().is_some());
        (io.capture)(audio.input_token(), &tone);
        let frame = frames.try_recv().unwrap();
        encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet,
        )
        .unwrap()
        .unwrap();
        let (input, _) = controls.take_levels(now + LEVELS_INTERVAL).unwrap();
        assert!(input > 0.6, "real unmuted samples must reach the meter");
        // Accumulate another real peak without consuming it before mute.
        encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet,
        )
        .unwrap()
        .unwrap();
        controls
            .begin(Some(2), Some(true), None, Some(true), now + LEVELS_INTERVAL)
            .unwrap();
        assert!(encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet
        )
        .unwrap()
        .is_none());
        let muted_token = audio.input_token();
        (io.capture)(muted_token, &tone);
        let frame = frames.try_recv().unwrap();
        let (size, token) = encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet,
        )
        .unwrap()
        .unwrap();
        let mut decoded = [0.0_f32; FRAME_SAMPLES];
        decoder
            .decode_float(&packet[..size], &mut decoded, false)
            .unwrap();
        assert!(decoded.iter().all(|sample| sample.abs() < 0.001));
        // Input mute leaves real, unsuppressed speaker output and its meter intact.
        push_playback(
            &playback,
            &audio,
            audio.output_token(),
            &[0.4; FRAME_SAMPLES],
        );
        let mut out = [0.0; FRAME_SAMPLES];
        speaker.render(&mut out, 1, |sample| (sample, sample));
        assert!(out.iter().all(|&sample| sample == 0.4));
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL * 2),
            Some((0.0, 0.4)),
            "neither raw muted input nor an accumulated pre-mute peak may escape"
        );
        assert!(
            controls.confirmed().is_none(),
            "encoding alone is not the completed track-write confirmation"
        );
        audio.input_applied.store(token, Ordering::Release);
        assert!(controls.confirmed().unwrap().muted);

        // A callback starts under mute and carries old conversion/resampler output
        // across unmute. The production frame packer must preserve its origin token,
        // discard it, and also discard a partial frame left by the muted generation.
        (io.capture)(muted_token, &tone[..FRAME_SAMPLES - 1]);
        controls
            .begin(Some(3), Some(false), None, None, now + LEVELS_INTERVAL * 2)
            .unwrap();
        (io.capture)(muted_token, &tone);
        assert!(matches!(
            frames.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(controls.confirmed().is_none());
        assert_eq!(audio.input_applied.load(Ordering::Acquire), muted_token);
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL * 3),
            Some((0.0, 0.0)),
            "unmute cannot resurrect old input telemetry"
        );
        let unmuted_token = audio.input_token();
        (io.capture)(unmuted_token, &tone[..1]);
        assert!(matches!(
            frames.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(controls.confirmed().is_none());
        (io.capture)(unmuted_token, &tone[1..]);
        let mut first_fresh_frame = Some(frames.try_recv().unwrap());
        let mut decoded_peak = 0.0_f32;
        // Opus has standard startup fade/lookahead, so the first fresh frame can be
        // quiet even when the input is audible. Keep the >0.1 audibility bar, but
        // judge it over at most five (100 ms) frames containing only fresh tone.
        for fresh_frame_index in 0..5 {
            if fresh_frame_index > 0 {
                (io.capture)(unmuted_token, &tone);
            }
            let frame = first_fresh_frame
                .take()
                .unwrap_or_else(|| frames.try_recv().unwrap());
            assert_eq!(frame.token, unmuted_token);
            assert_eq!(frame.samples, tone);
            let (size, token) = encode_frame(
                &mut encoder,
                &mut encoded_token,
                &frame,
                &audio,
                &mut packet,
            )
            .unwrap()
            .unwrap();
            assert_eq!(token, unmuted_token);
            decoder
                .decode_float(&packet[..size], &mut decoded, false)
                .unwrap();
            decoded_peak = decoded_peak.max(
                decoded
                    .iter()
                    .map(|sample| sample.abs())
                    .fold(0.0_f32, f32::max),
            );
        }
        assert!(
            decoded_peak > 0.1,
            "fresh unmuted audio must exceed the audible threshold within five Opus frames; peak={decoded_peak}"
        );
        let (input, _) = controls.take_levels(now + LEVELS_INTERVAL * 4).unwrap();
        assert!(input > 0.6, "fresh unmuted input peaks must resume");
        audio.input_applied.store(unmuted_token, Ordering::Release);
        assert!(!controls.confirmed().unwrap().muted);

        // Legacy mute has the same silence and telemetry privacy boundary.
        controls
            .begin(None, Some(true), None, None, now + LEVELS_INTERVAL * 4)
            .unwrap();
        (io.capture)(audio.input_token(), &tone);
        let frame = frames.try_recv().unwrap();
        let (size, token) = encode_frame(
            &mut encoder,
            &mut encoded_token,
            &frame,
            &audio,
            &mut packet,
        )
        .unwrap()
        .unwrap();
        decoder.reset_state().unwrap();
        decoder
            .decode_float(&packet[..size], &mut decoded, false)
            .unwrap();
        assert!(decoded.iter().all(|sample| sample.abs() < 0.001));
        assert_eq!(
            controls.take_levels(now + LEVELS_INTERVAL * 5),
            Some((0.0, 0.0))
        );
        assert!(controls.confirmed().is_none());
        audio.input_applied.store(token, Ordering::Release);
        let confirmation = controls.confirmed().unwrap();
        assert_eq!(confirmation.control_id, None);
        assert!(confirmation.muted);
    }

    #[test]
    fn native_levels_start_only_on_opt_in_and_stop_when_disabled() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        devices.capture(&[0.9; FRAME_SAMPLES]);
        devices.render();
        assert!(events.recv_timeout(Duration::from_millis(120)).is_err());
        peer.apply_control(device_control(1, None, None, Some(true)))
            .expect("levels admitted");
        applied(&events, 1, false, false);
        devices.capture(&[0.75; FRAME_SAMPLES]);
        devices.render();
        match events.recv_timeout(Duration::from_secs(2)) {
            Ok(PeerEvent::Levels(levels)) => {
                assert_eq!(levels.session_id, "media_test");
                assert_eq!(levels.input_peak, 0.75);
                assert_eq!(levels.output_peak, 0.0);
            }
            other => panic!("expected levels after opt-in, got {other:?}"),
        }
        peer.apply_control(device_control(2, None, None, Some(false)))
            .expect("disable admitted");
        applied(&events, 2, false, false);
        devices.capture(&[1.0; FRAME_SAMPLES]);
        assert!(events.recv_timeout(Duration::from_millis(240)).is_err());
        peer.close();
    }

    #[test]
    fn native_muted_levels_are_zero_and_late_muted_capture_cannot_confirm_unmute() {
        fn confirmation_and_levels(
            events: &std_mpsc::Receiver<PeerEvent>,
            control_id: Option<u64>,
            muted: bool,
            expected_peak: f32,
        ) {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut confirmed = false;
            let mut metered = false;
            while !confirmed || !metered {
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(PeerEvent::Applied(update)) if control_id == Some(update.control_id) => {
                        assert_eq!(update.input_muted, muted);
                        assert!(!update.output_suppressed);
                        confirmed = true;
                    }
                    Ok(PeerEvent::State { muted: applied, .. }) if control_id.is_none() => {
                        assert_eq!(applied, muted);
                        confirmed = true;
                    }
                    Ok(PeerEvent::Levels(levels)) => {
                        assert_eq!(levels.session_id, "media_test");
                        assert_eq!(levels.output_peak, 0.0);
                        if muted {
                            assert_eq!(levels.input_peak, 0.0, "muted mic telemetry must be zero");
                            metered = true;
                        } else {
                            // A queued muted tick may precede the fresh unmuted peak.
                            assert!(levels.input_peak == 0.0 || levels.input_peak == expected_peak);
                            metered |= levels.input_peak == expected_peak;
                        }
                    }
                    other => panic!("expected confirmation and input levels, got {other:?}"),
                }
            }
        }

        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        let audio = devices.state();
        // Enable levels and mute together, with actual nonzero FakeAudio input.
        peer.apply_control(device_control(1, Some(true), None, Some(true)))
            .expect("mute admitted");
        let muted_token = devices.wait_request(true, 0);
        devices.capture(&[0.9; FRAME_SAMPLES]);
        confirmation_and_levels(&events, Some(1), true, 0.0);

        peer.apply_control(device_control(2, Some(false), None, None))
            .expect("unmute admitted");
        let unmuted_token = devices.wait_request(true, muted_token);
        // Model a CPAL callback whose conversion began under the old muted token.
        devices.capture_with_token(muted_token, &[0.9; FRAME_SAMPLES]);
        match events.recv_timeout(Duration::from_secs(2)) {
            Ok(PeerEvent::Levels(levels)) => assert_eq!(levels.input_peak, 0.0),
            other => panic!("late muted capture must not confirm unmute, got {other:?}"),
        }
        assert_eq!(audio.input_applied.load(Ordering::Acquire), muted_token);
        devices.capture(&[0.75; FRAME_SAMPLES]);
        confirmation_and_levels(&events, Some(2), false, 0.75);

        peer.set_muted(true).expect("legacy mute admitted");
        devices.wait_request(true, unmuted_token);
        devices.capture(&[0.8; FRAME_SAMPLES]);
        confirmation_and_levels(&events, None, true, 0.0);
        peer.close();
    }

    #[test]
    fn native_device_change_event_and_default_input_check_close_after_device_release() {
        for event_driven in [true, false] {
            let (mut peer, devices, events) = start_fake();
            wait_offer(&events);
            if event_driven {
                (devices.callbacks.lock().unwrap().as_ref().unwrap().changed)();
            } else {
                devices.default_changed.store(true, Ordering::Release);
            }
            match events.recv_timeout(Duration::from_secs(4)) {
                Ok(PeerEvent::Closed { code, .. }) => assert_eq!(code, close_code::DEVICE_CHANGED),
                other => panic!("expected device_changed, got {other:?}"),
            }
            assert!(devices.released.load(Ordering::SeqCst));
            assert!(wait_until(&peer.stopped, Duration::from_secs(2)));
            peer.close();
        }
    }

    #[test]
    fn native_command_class_capacity_includes_active_and_rolls_back_rejected_sends() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        let initial = devices.state().input_token();
        peer.apply_control(device_control(1, Some(true), None, None))
            .unwrap();
        devices.wait_request(true, initial);
        for id in 2..=MEDIA_DEVICE_CONTROL_CAPACITY as u64 {
            peer.apply_control(device_control(id, Some(false), None, None))
                .unwrap();
        }
        assert_eq!(
            peer.apply_control(device_control(33, None, None, None)),
            Err(PeerCommandError::Full)
        );
        for _ in 0..MEDIA_LEGACY_MUTE_CAPACITY {
            peer.set_muted(true).unwrap();
        }
        peer.apply_answer("queued, not interpreted before the pending control".to_owned())
            .unwrap();
        assert_eq!(peer.set_muted(false), Err(PeerCommandError::Full));
        assert_eq!(
            peer.apply_answer("overflow".to_owned()),
            Err(PeerCommandError::Full)
        );
        assert_eq!(COMMAND_CAPACITY, 65);
        assert_eq!(peer.credits.device_control.load(Ordering::Acquire), 32);
        assert_eq!(peer.credits.legacy_mute.load(Ordering::Acquire), 32);
        assert_eq!(peer.credits.answer.load(Ordering::Acquire), 1);
        assert!(!devices.released.load(Ordering::Acquire));
        let credits = Arc::clone(&peer.credits);
        peer.close();
        assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
        assert_eq!(credits.device_control.load(Ordering::Acquire), 0);
        assert_eq!(credits.legacy_mute.load(Ordering::Acquire), 0);
        assert_eq!(credits.answer.load(Ordering::Acquire), 0);
    }

    // Keep: #316 privacy, callback provenance and bounded terminal delivery cross
    // broker/controller/native trust boundaries. Only devices and event observation
    // are injected; control IDs, scoping, codecs, queue and acknowledgements are real.
    struct RoutedFixture {
        broker: crate::server::media::MediaBroker,
        media: super::super::ClientMedia,
        tx: mpsc::Sender<crate::client::ClientLoopEvent>,
        rx: Option<mpsc::Receiver<crate::client::ClientLoopEvent>>,
        devices: Arc<Mutex<Vec<Arc<FakeDevices>>>>,
        stopped: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
        credits: Arc<Mutex<Vec<Arc<CommandCredits>>>>,
        delivery: Arc<Mutex<Vec<(bool, PeerEvent)>>>,
        mode: Arc<Mutex<(bool, bool)>>, // eager callbacks, fail after eager callbacks
        controls: Vec<crate::protocol::media::MediaControl>,
        session: String,
    }

    struct RoutedAudio {
        devices: Arc<FakeDevices>,
        eager: bool,
        fail: bool,
    }

    impl AudioBackend for RoutedAudio {
        fn open(self: Box<Self>, mut io: AudioIo) -> Result<OpenedAudio, String> {
            *self.devices.initial_tokens.lock().unwrap() =
                Some((io.audio.input_token(), io.audio.output_token()));
            if self.fail {
                // A backend is allowed to call back before reporting open failure.
                // Constructor protection, not a later setter, must cover this case.
                (io.capture)(io.audio.input_token(), &[0.7; FRAME_SAMPLES]);
                let mut speaker = Speaker::new(44_100, io.playback, Arc::clone(&io.audio));
                let mut out = [1.0; 127];
                speaker.render(&mut out, 1, |sample| (sample, sample));
                assert!(out.iter().all(|sample| *sample == 0.0));
                self.devices.opened.store(true, Ordering::Release);
                self.devices.released.store(true, Ordering::Release);
                return Err("injected open failure after callbacks".to_owned());
            }
            if self.eager {
                return Box::new(FakeAudio(Arc::clone(&self.devices))).open(io);
            }
            let speaker = Speaker::new(44_100, io.playback, Arc::clone(&io.audio));
            *self.devices.audio.lock().unwrap() = Some(io.audio);
            *self.devices.callbacks.lock().unwrap() = Some(FakeCallbacks {
                capture: io.capture,
                speaker,
                changed: io.changed,
            });
            self.devices.opened.store(true, Ordering::Release);
            Ok(OpenedAudio::new(
                Box::new(FakeGuard(Arc::clone(&self.devices))),
                "test microphone".to_owned(),
                "test speaker".to_owned(),
            ))
        }
    }

    impl RoutedFixture {
        fn new(eager: bool) -> Self {
            use super::super::peer::PeerFactory;
            let (tx, rx) = mpsc::channel(256);
            let root = crate::client::media_peer_event_sink(tx.clone());
            let delivery = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&delivery);
            let sink: PeerEventSink = Arc::new(move |event| {
                log.lock().unwrap().push((false, event.clone()));
                let future = root(event.clone());
                let log = Arc::clone(&log);
                Box::pin(async move {
                    future.await?;
                    log.lock().unwrap().push((true, event));
                    Ok(())
                })
            });
            let devices = Arc::new(Mutex::new(Vec::new()));
            let stopped = Arc::new(Mutex::new(Vec::new()));
            let credits = Arc::new(Mutex::new(Vec::new()));
            let mode = Arc::new(Mutex::new((eager, false)));
            let (all_devices, all_stopped, all_credits, open_mode) = (
                Arc::clone(&devices),
                Arc::clone(&stopped),
                Arc::clone(&credits),
                Arc::clone(&mode),
            );
            let factory: PeerFactory = Box::new(move |session, initial, sink| {
                let devices = Arc::new(FakeDevices::default());
                let (eager, fail) = *open_mode.lock().unwrap();
                all_devices.lock().unwrap().push(Arc::clone(&devices));
                let peer = start_with_audio(
                    session,
                    initial,
                    sink,
                    Box::new(RoutedAudio {
                        devices,
                        eager,
                        fail,
                    }),
                )?;
                all_stopped.lock().unwrap().push(Arc::clone(&peer.stopped));
                all_credits.lock().unwrap().push(Arc::clone(&peer.credits));
                Ok(Box::new(peer))
            });
            let mut media =
                super::super::ClientMedia::new(crate::config::MediaMode::Auto, factory, sink);
            // Linux intentionally does not advertise native hardware. This fixture
            // supplies its own device backend, but uses the actual NativePeer.
            media.peer_available = true;
            let mut broker = crate::server::media::MediaBroker::new();
            broker.client_connected(7, true);
            Self {
                broker,
                media,
                tx,
                rx: Some(rx),
                devices,
                stopped,
                credits,
                delivery,
                mode,
                controls: Vec::new(),
                session: String::new(),
            }
        }

        fn latest_devices(&self) -> Arc<FakeDevices> {
            Arc::clone(self.devices.lock().unwrap().last().unwrap())
        }

        fn wire_server(&mut self, control: crate::protocol::media::MediaControl) {
            use crate::protocol::{media::MediaControl, ServerMessage};
            let json = serde_json::to_string(&control.server_message().unwrap()).unwrap();
            let ServerMessage::EndpointControl { kind, data } =
                serde_json::from_str(&json).unwrap()
            else {
                panic!("named endpoint envelope required");
            };
            let control = MediaControl::decode(&kind, &data).unwrap().unwrap();
            self.media.handle_server_control(
                &crate::client::endpoint::ClientEndpointId::Local,
                control,
                true,
                |_| Some("fixture pane".to_owned()),
                Instant::now(),
            );
            self.effects();
        }

        fn actions(&mut self, actions: Vec<crate::server::media::MediaAction>) {
            use crate::server::media::MediaAction;
            for action in actions {
                match action {
                    MediaAction::Send { client_id, control } => {
                        assert_eq!(client_id, 7, "actual broker target");
                        self.wire_server(control);
                    }
                    MediaAction::Respond {
                        respond_to,
                        response,
                        session_id: _,
                    } => {
                        let _ = respond_to.send(response);
                    }
                }
            }
        }

        fn effects(&mut self) {
            use crate::protocol::{media::MediaControl, ClientMessage};
            for effect in self.media.take_effects() {
                if let super::super::MediaEffect::Send(endpoint, control) = effect {
                    assert_eq!(endpoint, crate::client::endpoint::ClientEndpointId::Local);
                    let json = serde_json::to_string(&control.client_message().unwrap()).unwrap();
                    let ClientMessage::EndpointControl { kind, data } =
                        serde_json::from_str(&json).unwrap()
                    else {
                        panic!("named client envelope required");
                    };
                    let decoded = MediaControl::decode(&kind, &data).unwrap().unwrap();
                    self.controls.push(decoded.clone());
                    let actions = self.broker.client_control(7, decoded, Instant::now());
                    self.actions(actions);
                }
            }
        }

        fn open(&mut self, fresh: bool) -> String {
            let now = Instant::now();
            if fresh {
                self.broker.note_pane_input(
                    7,
                    crate::layout::PaneId::from_raw(316),
                    "fixture:p316",
                    now,
                );
                self.media.note_pane_input(
                    &crate::client::endpoint::ClientEndpointId::Local,
                    "fixture:p316",
                    now,
                );
            }
            let (tx, _rx) = std_mpsc::channel();
            let actions = self.broker.open(
                "fixture-open".to_owned(),
                tx,
                crate::layout::PaneId::from_raw(316),
                None,
                None,
                |_| true,
                now,
            );
            let session = actions
                .iter()
                .find_map(|action| match action {
                    crate::server::media::MediaAction::Send {
                        control: crate::protocol::media::MediaControl::OpenInitial(open),
                        ..
                    } => Some(open.session_id.clone()),
                    _ => None,
                })
                .expect("authorized broker open");
            self.session = session.clone();
            self.actions(actions);
            session
        }

        fn pump(&mut self, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            loop {
                let event = self.rx.as_mut().expect("live queue").try_recv();
                match event {
                    Ok(crate::client::ClientLoopEvent::Media(event)) => {
                        assert!(
                            matches!(&event, PeerEvent::Scoped { event, .. } if !matches!(event.as_ref(), PeerEvent::Scoped { .. }))
                        );
                        self.media.handle_peer_event(event);
                        self.effects();
                        return;
                    }
                    Ok(_) => return,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        panic!("client queue unexpectedly disconnected")
                    }
                    Err(mpsc::error::TryRecvError::Empty) => {
                        assert!(
                            Instant::now() < deadline,
                            "bounded client queue receipt timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        }

        fn until(&mut self, timeout: Duration, predicate: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + timeout;
            while !predicate(self) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "bounded real-path condition timed out"
                );
                self.pump(remaining);
            }
        }

        fn offer(&mut self, session: &str) -> String {
            self.until(Duration::from_secs(10), |fixture| fixture.controls.iter().any(|control| matches!(control, crate::protocol::media::MediaControl::Offer(offer) if offer.session_id == session)));
            self.controls
                .iter()
                .find_map(|control| match control {
                    crate::protocol::media::MediaControl::Offer(offer)
                        if offer.session_id == session =>
                    {
                        Some(offer.sdp.clone())
                    }
                    _ => None,
                })
                .unwrap()
        }

        fn dispatch(
            &mut self,
            input: Option<bool>,
            output: Option<bool>,
            levels: Option<bool>,
        ) -> u64 {
            let dispatch = self
                .broker
                .dispatch_control(&self.session, input, output, levels, Instant::now())
                .unwrap();
            assert_eq!(dispatch.session_ids, vec![self.session.clone()]);
            let id = dispatch.control_id;
            self.actions(dispatch.actions);
            id
        }

        fn fill_queue(&self) {
            assert_eq!(
                self.tx.capacity(),
                256,
                "start with the actual empty root queue"
            );
            for _ in 0..256 {
                assert!(self
                    .tx
                    .try_send(crate::client::ClientLoopEvent::Timer)
                    .is_ok());
            }
        }

        fn wait_delivery(&self, admitted: bool, predicate: impl Fn(&PeerEvent) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if self
                    .delivery
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(receipt, event)| {
                        *receipt == admitted
                            && match event {
                                PeerEvent::Scoped { event, .. } => predicate(event),
                                _ => false,
                            }
                    })
                {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "bounded actual sink receipt timed out"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn await_applied(&mut self, id: u64) {
            self.until(Duration::from_secs(3), |fixture| {
                fixture
                    .broker
                    .applied(&fixture.session)
                    .is_some_and(|update| update.control_id == id)
            });
        }

        fn replay_actual_applied(&mut self, id: u64) {
            use crate::protocol::{media::MediaControl, ClientMessage};
            let applied = self
                .delivery
                .lock()
                .unwrap()
                .iter()
                .find_map(|(_, event)| match event {
                    PeerEvent::Scoped { event, .. } => match event.as_ref() {
                        PeerEvent::Applied(update) if update.control_id == id => {
                            Some(update.clone())
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .expect("replay only a real encoder/outer-render outcome, never a fake ACK");
            let wire =
                serde_json::to_string(&MediaControl::Applied(applied).client_message().unwrap())
                    .unwrap();
            let ClientMessage::EndpointControl { kind, data } =
                serde_json::from_str(&wire).unwrap()
            else {
                panic!("named stale outcome");
            };
            let stale = MediaControl::decode(&kind, &data).unwrap().unwrap();
            let actions = self.broker.client_control(7, stale, Instant::now());
            self.actions(actions);
        }
    }

    impl Drop for RoutedFixture {
        fn drop(&mut self) {
            // Queue closure releases notification-only retention even on assertion
            // unwind. Never leave a native worker waiting for a dropped test driver.
            self.media.set_mode(crate::config::MediaMode::Off);
            self.rx.take();
            for stopped in self.stopped.lock().unwrap().iter() {
                let done = wait_until(stopped, Duration::from_secs(4));
                if !std::thread::panicking() {
                    assert!(done, "native test worker completed");
                }
            }
        }
    }

    struct RoutedRemote {
        peer: Arc<RTCPeerConnection>,
        channel: Arc<webrtc::data_channel::RTCDataChannel>,
        track: Arc<TrackLocalStaticSample>,
        decoded: mpsc::UnboundedReceiver<f32>,
    }

    impl RoutedRemote {
        async fn connect(fixture: &mut RoutedFixture) -> Self {
            let session = fixture.session.clone();
            let offer = fixture.offer(&session);
            assert_loopback_candidates(&offer, "routed native offer");
            let mut media = MediaEngine::default();
            media.register_default_codecs().unwrap();
            let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
            let api = APIBuilder::new()
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .with_setting_engine(loopback_setting_engine())
                .build();
            let peer = Arc::new(
                api.new_peer_connection(RTCConfiguration::default())
                    .await
                    .unwrap(),
            );
            let track = Arc::new(TrackLocalStaticSample::new(
                opus_codec().capability,
                "fixture-audio".to_owned(),
                "fixture".to_owned(),
            ));
            let sender = peer
                .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
                .await
                .unwrap();
            tokio::spawn(async move {
                let mut buffer = [0; 1500];
                while sender.read(&mut buffer).await.is_ok() {}
            });
            let (decoded_tx, decoded) = mpsc::unbounded_channel();
            peer.on_track(Box::new(move |remote, _, _| {
                let tx = decoded_tx.clone();
                tokio::spawn(async move {
                    let mut decoder =
                        opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
                    let mut pcm = [0.0; MAX_DECODED_SAMPLES];
                    while let Ok((packet, _)) = remote.read_rtp().await {
                        if packet.payload.is_empty() {
                            continue;
                        }
                        if let Ok(count) = decoder.decode_float(&packet.payload, &mut pcm, false) {
                            let peak = pcm[..count]
                                .iter()
                                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                            if tx.send(peak).is_err() {
                                break;
                            }
                        }
                    }
                });
                Box::pin(async {})
            }));
            let (channel_tx, mut channel_rx) = mpsc::unbounded_channel();
            peer.on_data_channel(Box::new(move |channel| {
                let tx = channel_tx.clone();
                let opened = Arc::clone(&channel);
                channel.on_open(Box::new(move || {
                    let _ = tx.send(opened);
                    Box::pin(async {})
                }));
                Box::pin(async {})
            }));
            peer.set_remote_description(RTCSessionDescription::offer(offer).unwrap())
                .await
                .unwrap();
            let answer = peer.create_answer(None).await.unwrap();
            let mut gathered = peer.gathering_complete_promise().await;
            peer.set_local_description(answer).await.unwrap();
            let _ = tokio::time::timeout(ICE_GATHER_TIMEOUT, gathered.recv()).await;
            let answer = peer.local_description().await.unwrap().sdp;
            assert_loopback_candidates(&answer, "routed remote answer");
            let actions = fixture
                .broker
                .answer(&session, answer, Instant::now())
                .unwrap();
            fixture.actions(actions);
            fixture.until(Duration::from_secs(20), |fixture| {
                fixture.broker.info(&session).is_some()
            });
            let channel = tokio::time::timeout(Duration::from_secs(5), channel_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(channel.label(), EVENTS_CHANNEL_LABEL);
            assert_eq!(
                channel.ready_state(),
                webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            );
            Self {
                peer,
                channel,
                track,
                decoded,
            }
        }

        async fn capture_peak(&mut self, devices: &FakeDevices) -> f32 {
            let tone: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|index| {
                    (index as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.7
                })
                .collect();
            // Drain any eager startup frame; subsequent packets originate in this
            // actual post-request callback, encoder, awaited write and RTP receiver.
            while self.decoded.try_recv().is_ok() {}
            let mut peak = 0.0_f32;
            for _ in 0..5 {
                devices.capture(&tone);
                peak = peak.max(
                    tokio::time::timeout(Duration::from_secs(3), self.decoded.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                );
            }
            peak
        }

        async fn output_peak(&self, devices: &FakeDevices) -> f32 {
            let mut encoder =
                opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
                    .unwrap();
            let tone: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|index| {
                    (index as f32 * 660.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.6
                })
                .collect();
            let mut peak = 0.0_f32;
            for _ in 0..5 {
                let mut packet = [0; 1500];
                let count = encoder.encode_float(&tone, &mut packet).unwrap();
                self.track
                    .write_sample(&Sample {
                        data: Bytes::copy_from_slice(&packet[..count]),
                        duration: FRAME_DURATION,
                        ..Default::default()
                    })
                    .await
                    .unwrap();
                // Existing device callback cadence, bounded to the five-frame Opus
                // positive control, not an ACK or injected playback ring.
                tokio::time::sleep(FRAME_DURATION).await;
                // Consume one actual 20 ms device interval at the fixture's 44.1 kHz.
                // A single 127-sample callback per interval would leave prefetched
                // startup silence ahead of the fresh Opus positive control.
                peak = devices
                    .render_samples(44_100 / 50)
                    .iter()
                    .fold(peak, |peak, sample| peak.max(sample.abs()));
            }
            peak
        }
    }

    impl Drop for RoutedRemote {
        fn drop(&mut self) {
            let peer = Arc::clone(&self.peer);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = peer.close().await;
                });
            }
        }
    }

    #[test]
    fn native_retained_intent_reopens_preprotected_and_explicit_false_is_audible() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for (muted, suppressed) in [(true, false), (false, true), (true, true)] {
                let mut fixture = RoutedFixture::new(true);
                fixture.open(true);
                let remote = RoutedRemote::connect(&mut fixture).await;
                assert_eq!(
                    remote.channel.ready_state(),
                    webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                );
                let devices = fixture.latest_devices();
                let audio = devices.state();
                let input = audio.input_token();
                let output = audio.output_token();
                let id = fixture.dispatch(Some(muted), Some(suppressed), None);
                devices.wait_request(true, input);
                devices.wait_request(false, output);
                devices.capture(&[0.7; FRAME_SAMPLES]);
                devices.render();
                fixture.await_applied(id);
                assert_eq!(
                    fixture
                        .broker
                        .intent(crate::layout::PaneId::from_raw(316))
                        .input_muted,
                    muted
                );
                (devices.callbacks.lock().unwrap().as_ref().unwrap().changed)();
                let old = fixture.session.clone();
                fixture.until(Duration::from_secs(4), |fixture| {
                    fixture.broker.state(&old).is_some_and(|state| {
                        state.code.as_deref() == Some(close_code::DEVICE_CHANGED)
                    })
                });
                assert!(devices.released.load(Ordering::Acquire));
                assert!(devices.protected_before_drop.load(Ordering::Acquire));
                let stale = Instant::now()
                    - crate::protocol::media::MEDIA_INPUT_WINDOW
                    - Duration::from_secs(1);
                fixture.broker.note_pane_input(
                    7,
                    crate::layout::PaneId::from_raw(316),
                    "fixture:p316",
                    stale,
                );
                fixture.media.note_pane_input(
                    &crate::client::endpoint::ClientEndpointId::Local,
                    "fixture:p316",
                    stale,
                );
                let (reply, replies) = std_mpsc::channel();
                let refused = fixture.broker.open(
                    "stale-input-negative".to_owned(),
                    reply,
                    crate::layout::PaneId::from_raw(316),
                    None,
                    None,
                    |_| true,
                    Instant::now(),
                );
                assert!(
                    !refused.iter().any(|action| matches!(
                        action,
                        crate::server::media::MediaAction::Send { .. }
                    )),
                    "retained intent is not renewed microphone authority"
                );
                fixture.actions(refused);
                let response: serde_json::Value =
                    serde_json::from_str(&replies.try_recv().unwrap()).unwrap();
                assert!(response.get("error").is_some());
                drop(remote);
                // A newly constructed client has no v1 safety memory. Retained
                // server policy itself must cross the serialized Initial envelope.
                let mut fresh_client = RoutedFixture::new(true);
                std::mem::swap(&mut fresh_client.broker, &mut fixture.broker);
                fixture = fresh_client;
                fixture.open(true);
                let mut remote = RoutedRemote::connect(&mut fixture).await;
                let devices = fixture.latest_devices();
                assert_eq!(
                    *devices.initial_tokens.lock().unwrap(),
                    Some((2 | u64::from(muted), 2 | u64::from(suppressed)))
                );
                let capture = remote.capture_peak(&devices).await;
                if muted {
                    assert!(
                        capture < 0.001,
                        "new protected peer must decode silence, peak={capture}"
                    );
                } else {
                    assert!(capture > 0.1, "input is independent of output suppression");
                }
                let output = remote.output_peak(&devices).await;
                if suppressed {
                    assert_eq!(output, 0.0);
                } else {
                    assert!(
                        output > 0.1,
                        "output is independent of input mute, peak={output}"
                    );
                }
                let audio = devices.state();
                let previous = audio.input_token();
                let id = fixture.dispatch(Some(false), Some(false), None);
                devices.wait_request(true, previous);
                devices.capture(&[0.7; FRAME_SAMPLES]);
                devices.render();
                fixture.await_applied(id);
                assert!(
                    remote.capture_peak(&devices).await > 0.1,
                    "explicit false must resume fresh audible microphone audio"
                );
                assert!(
                    remote.output_peak(&devices).await > 0.1,
                    "explicit false must resume real outer output"
                );
                assert!(
                    !fixture
                        .broker
                        .intent(crate::layout::PaneId::from_raw(316))
                        .input_muted
                );
                assert!(
                    !fixture
                        .broker
                        .intent(crate::layout::PaneId::from_raw(316))
                        .output_suppressed
                );
            }
        });
    }

    #[test]
    fn legacy_mute_input_only_state_without_outer_render() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut fixture = RoutedFixture::new(false);
            fixture.open(true);
            let _remote = RoutedRemote::connect(&mut fixture).await;
            let devices = fixture.latest_devices();
            let audio = devices.state();
            assert_eq!(audio.input_applied.load(Ordering::Acquire), 0);
            assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);
            assert!(!fixture.controls.iter().any(|control| matches!(control, crate::protocol::media::MediaControl::Applied(_))));
            let actions = fixture.broker.mute(&fixture.session, true).unwrap();
            fixture.actions(actions);
            devices.wait_request(true, 2);
            devices.capture(&[0.8; FRAME_SAMPLES]);
            fixture.until(Duration::from_secs(3), |fixture| fixture.controls.iter().any(|control| matches!(control, crate::protocol::media::MediaControl::State(state) if state.muted)));
            assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);
            let id = fixture.dispatch(None, None, Some(true));
            fixture.wait_delivery(false, |event| matches!(event, PeerEvent::ControlFailed(failure) if failure.control_id == Some(id)));
            fixture.until(Duration::from_secs(2), |fixture| fixture.broker.control_failed(&fixture.session).is_some_and(|failure| failure.control_id == Some(id)));
            let id2 = fixture.dispatch(None, None, Some(false));
            assert!(fixture.broker.applied(&fixture.session).is_none());
            devices.render();
            fixture.await_applied(id2);
            let applied = fixture.broker.applied(&fixture.session).unwrap();
            assert!(applied.input_muted);
            assert!(!applied.output_suppressed);
        });
    }

    #[test]
    fn paired_control_after_partial_timeout_waits_for_outer_silence() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut fixture = RoutedFixture::new(false);
            fixture.open(true);
            let _remote = RoutedRemote::connect(&mut fixture).await;
            let devices = fixture.latest_devices();
            let audio = devices.state();
            let id = fixture.dispatch(Some(true), Some(true), None);
            let input = devices.wait_request(true, 2);
            let output = devices.wait_request(false, 2);
            devices.capture(&[0.7; FRAME_SAMPLES]);
            fixture.until(Duration::from_secs(7), |fixture| {
                fixture
                    .broker
                    .control_failed(&fixture.session)
                    .is_some_and(|failure| failure.control_id == Some(id))
            });
            assert_eq!(audio.input_applied.load(Ordering::Acquire), input);
            assert_ne!(audio.output_applied.load(Ordering::Acquire), output);
            let id2 = fixture.dispatch(None, None, Some(true));
            // The omitted output is still the current unobserved true request;
            // neither legacy input nor a levels-only request manufactures a pair.
            assert!(fixture.broker.applied(&fixture.session).is_none());
            assert!(devices.render().iter().all(|sample| *sample == 0.0));
            fixture.await_applied(id2);
            let applied = fixture.broker.applied(&fixture.session).unwrap();
            assert!(applied.input_muted && applied.output_suppressed);
            assert_eq!(applied.control_id, id2);
            assert_eq!(audio.output_drained.load(Ordering::Acquire), output);
            assert_eq!(audio.output_applied.load(Ordering::Acquire), output);
        });
    }

    #[test]
    fn native_critical_outcome_backpressure_settles_broker() {
        for timeout in [false, true] {
            let mut fixture = RoutedFixture::new(true);
            let session = fixture.open(true);
            fixture.offer(&session);
            let devices = fixture.latest_devices();
            let old = devices.state().input_token();
            let id = fixture.dispatch(Some(true), None, None);
            let token = devices.wait_request(true, old);
            let id2 = fixture.dispatch(Some(false), None, None);
            fixture.fill_queue();
            if !timeout {
                devices.capture(&[0.7; FRAME_SAMPLES]);
            }
            fixture.wait_delivery(false, |event| match event {
                PeerEvent::Applied(update) => !timeout && update.control_id == id,
                PeerEvent::ControlFailed(failure) => timeout && failure.control_id == Some(id),
                _ => false,
            });
            assert_eq!(
                devices.state().input_token(),
                token,
                "ID2 cannot publish during terminal delivery"
            );
            assert_eq!(
                fixture.credits.lock().unwrap()[0]
                    .device_control
                    .load(Ordering::Acquire),
                2
            );
            assert!(fixture.broker.applied(&session).is_none());
            fixture.until(Duration::from_secs(2), |fixture| {
                if timeout {
                    fixture
                        .broker
                        .control_failed(&session)
                        .is_some_and(|failure| failure.control_id == Some(id))
                } else {
                    fixture
                        .broker
                        .applied(&session)
                        .is_some_and(|applied| applied.control_id == id)
                }
            });
            let fresh = devices.wait_request(true, token);
            devices.capture_with_token(token, &[0.8; FRAME_SAMPLES]);
            assert_ne!(devices.state().input_applied.load(Ordering::Acquire), fresh);
            devices.capture(&[0.7; FRAME_SAMPLES]);
            fixture.await_applied(id2);
            assert!(!fixture.broker.applied(&session).unwrap().input_muted);
            // Settlement freed the real per-target broker allowance, not just a
            // local permit/counter or fabricated Applied invocation.
            assert!(fixture
                .broker
                .dispatch_control(&session, None, None, None, Instant::now())
                .is_ok());
        }
    }

    #[test]
    fn native_delivery_timeout_gates_before_device_drop_and_retains_closed() {
        let mut fixture = RoutedFixture::new(true);
        let session = fixture.open(true);
        fixture.offer(&session);
        let devices = fixture.latest_devices();
        let audio = devices.state();
        let old = audio.input_token();
        let id = fixture.dispatch(Some(true), None, Some(true));
        let token = devices.wait_request(true, old);
        fixture.dispatch(Some(false), Some(false), None);
        fixture.fill_queue();
        devices.capture(&[0.7; FRAME_SAMPLES]);
        fixture.wait_delivery(
            false,
            |event| matches!(event, PeerEvent::Applied(update) if update.control_id == id),
        );
        assert!(wait_until(&devices.released, Duration::from_secs(3)));
        assert!(devices.protected_before_drop.load(Ordering::Acquire));
        assert!(enabled(audio.input_token()) && enabled(audio.output_token()));
        assert!(!audio.levels_enabled());
        assert_eq!(audio.input_token(), token, "ID2 never published");
        fixture.wait_delivery(false, |event| {
            matches!(
                event,
                PeerEvent::Closed {
                    code: close_code::DISCONNECTED,
                    ..
                }
            )
        });
        assert!(!wait_until(
            &fixture.stopped.lock().unwrap()[0],
            CRITICAL_EVENT_DELIVERY_TIMEOUT + CONTROL_OBSERVE_INTERVAL,
        ), "a full queue beyond the final delivery bound must retain the same waiter, not stop the runtime");
        assert!(
            fixture.broker.state(&session).unwrap().code.is_none(),
            "queue admission is not broker receipt"
        );
        // Owner cancellation after native terminal selection must not erase Closed.
        fixture.media.session.as_mut().unwrap().peer.close();
        fixture.until(Duration::from_secs(3), |fixture| {
            fixture
                .broker
                .state(&session)
                .is_some_and(|state| state.code.as_deref() == Some(close_code::DISCONNECTED))
        });
        fixture.wait_delivery(true, |event| matches!(event, PeerEvent::Closed { .. }));
        assert!(wait_until(
            &fixture.stopped.lock().unwrap()[0],
            Duration::from_secs(3)
        ));
        {
            let log = fixture.delivery.lock().unwrap();
            assert_eq!(log.iter().filter(|(admitted, event)| !admitted && matches!(event, PeerEvent::Scoped { event, .. } if matches!(event.as_ref(), PeerEvent::Closed { .. }))).count(), 1, "one final future, no retry/re-enqueue");
        }
        fixture.replay_actual_applied(id);
        assert!(fixture.broker.applied(&session).is_none());
        assert_eq!(
            fixture.broker.state(&session).unwrap().code.as_deref(),
            Some(close_code::DISCONNECTED)
        );
    }

    #[test]
    fn native_callback_failure_backpressure_timeout_is_gate_first_terminal() {
        let mut fixture = RoutedFixture::new(true);
        let session = fixture.open(true);
        fixture.offer(&session);
        let devices = fixture.latest_devices();
        let old = devices.state().input_token();
        let id = fixture.dispatch(Some(true), Some(true), None);
        let input = devices.wait_request(true, old);
        let audio = devices.state();
        fixture.dispatch(Some(false), Some(false), None);
        fixture.fill_queue();
        // Both device observations are withheld for the real five-second worker
        // deadline; the actual failure then waits on the saturated root queue.
        fixture.wait_delivery(false, |event| matches!(event, PeerEvent::ControlFailed(failure) if failure.control_id == Some(id)));
        assert!(wait_until(&devices.released, Duration::from_secs(3)));
        assert!(devices.protected_before_drop.load(Ordering::Acquire));
        fixture.wait_delivery(false, |event| matches!(event, PeerEvent::Closed { .. }));
        assert!(!fixture.stopped.lock().unwrap()[0].load(Ordering::Acquire));
        fixture.until(Duration::from_secs(3), |fixture| {
            fixture
                .broker
                .state(&session)
                .is_some_and(|state| state.code.as_deref() == Some(close_code::DISCONNECTED))
        });
        assert!(fixture.broker.applied(&session).is_none());
        assert_eq!(
            audio.input_token(),
            input,
            "ID2 was not published before terminal teardown"
        );
    }

    #[test]
    fn native_closed_consumer_fail_stops_and_disconnect_settles() {
        let mut fixture = RoutedFixture::new(true);
        let session = fixture.open(true);
        fixture.offer(&session);
        let devices = fixture.latest_devices();
        let old = devices.state().input_token();
        let id = fixture.dispatch(Some(true), None, None);
        devices.wait_request(true, old);
        fixture.fill_queue();
        devices.capture(&[0.8; FRAME_SAMPLES]);
        fixture.wait_delivery(
            false,
            |event| matches!(event, PeerEvent::Applied(update) if update.control_id == id),
        );
        fixture.rx.take();
        assert!(wait_until(&devices.released, Duration::from_secs(2)));
        assert!(devices.protected_before_drop.load(Ordering::Acquire));
        assert!(wait_until(
            &fixture.stopped.lock().unwrap()[0],
            Duration::from_secs(3)
        ));
        assert!(
            fixture.broker.state(&session).unwrap().code.is_none(),
            "receiver loss alone is not broker-observed disconnect"
        );
        let actions = fixture.broker.client_removed(7, Instant::now());
        fixture.actions(actions);
        assert_eq!(
            fixture.broker.state(&session).unwrap().code.as_deref(),
            Some(close_code::DISCONNECTED)
        );
        fixture.replay_actual_applied(id);
        assert!(fixture.broker.applied(&session).is_none());
    }

    #[test]
    fn native_cancel_while_backpressured_releases_audio() {
        let mut fixture = RoutedFixture::new(true);
        let session = fixture.open(true);
        fixture.offer(&session);
        let devices = fixture.latest_devices();
        let old = devices.state().input_token();
        let id = fixture.dispatch(Some(true), None, None);
        devices.wait_request(true, old);
        fixture.fill_queue();
        devices.capture(&[0.8; FRAME_SAMPLES]);
        fixture.wait_delivery(
            false,
            |event| matches!(event, PeerEvent::Applied(update) if update.control_id == id),
        );
        let cancelled = Instant::now();
        fixture.media.set_mode(crate::config::MediaMode::Off);
        fixture.effects();
        assert!(wait_until(&devices.released, Duration::from_millis(500)));
        assert!(cancelled.elapsed() < CRITICAL_EVENT_DELIVERY_TIMEOUT);
        assert!(devices.protected_before_drop.load(Ordering::Acquire));
        assert_eq!(
            fixture.broker.state(&session).unwrap().code.as_deref(),
            Some(close_code::DISABLED)
        );
        assert!(wait_until(
            &fixture.stopped.lock().unwrap()[0],
            Duration::from_secs(3)
        ));
        assert!(!fixture.delivery.lock().unwrap().iter().any(|(_, event)| matches!(event, PeerEvent::Scoped { event, .. } if matches!(event.as_ref(), PeerEvent::Closed { .. }))));
    }

    #[test]
    fn legacy_mute_timeout_reports_observed_state() {
        let mut fixture = RoutedFixture::new(false);
        let session = fixture.open(true);
        fixture.offer(&session);
        let devices = fixture.latest_devices();
        let actions = fixture.broker.mute(&session, true).unwrap();
        fixture.actions(actions);
        devices.wait_request(true, 2);
        fixture.until(Duration::from_secs(7), |fixture| fixture.controls.iter().any(|control| matches!(control, crate::protocol::media::MediaControl::ControlFailed(failure) if failure.control_id.is_none())));
        fixture.until(Duration::from_secs(2), |fixture| fixture.controls.iter().any(|control| matches!(control, crate::protocol::media::MediaControl::State(state) if state.session_id == session && !state.muted)));
        assert!(!fixture.broker.state(&session).unwrap().muted);
        assert!(
            fixture
                .broker
                .intent(crate::layout::PaneId::from_raw(316))
                .input_muted
        );
        assert!(enabled(devices.state().input_token()));
    }

    #[test]
    fn native_successor_open_failure_keeps_option_b_and_next_open_protection() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut fixture = RoutedFixture::new(true);
            let source = fixture.open(true);
            let _remote = RoutedRemote::connect(&mut fixture).await;
            let old_devices = fixture.latest_devices();
            let old = old_devices.state().input_token();
            let id = fixture.dispatch(Some(true), Some(true), None);
            old_devices.wait_request(true, old);
            old_devices.capture(&[0.6; FRAME_SAMPLES]);
            old_devices.render();
            fixture.await_applied(id);
            let stale = Instant::now()
                - crate::protocol::media::MEDIA_INPUT_WINDOW
                - Duration::from_secs(1);
            fixture.broker.note_pane_input(
                7,
                crate::layout::PaneId::from_raw(316),
                "fixture:p316",
                stale,
            );
            fixture.media.note_pane_input(
                &crate::client::endpoint::ClientEndpointId::Local,
                "fixture:p316",
                stale,
            );
            fixture.mode.lock().unwrap().1 = true;
            let failed = fixture.open(false); // Connected source authorizes idle renewal.
            fixture.until(Duration::from_secs(4), |fixture| {
                fixture
                    .broker
                    .state(&failed)
                    .is_some_and(|state| state.code.as_deref() == Some(close_code::DEVICE_ERROR))
            });
            assert!(
                wait_until(&old_devices.released, Duration::from_secs(2)),
                "Option B closes the physical source first"
            );
            let failing_devices = fixture.latest_devices();
            assert_eq!(
                *failing_devices.initial_tokens.lock().unwrap(),
                Some((3, 3))
            );
            assert!(failing_devices.released.load(Ordering::Acquire));
            assert!(
                fixture.media.session.is_none(),
                "no physical failed-handover preservation promise"
            );
            assert!(fixture.broker.state(&source).unwrap().code.is_some());
            fixture.mode.lock().unwrap().1 = false;
            let next = fixture.open(true);
            fixture.offer(&next);
            assert_eq!(
                *fixture.latest_devices().initial_tokens.lock().unwrap(),
                Some((3, 3))
            );
        });
    }

    #[test]
    fn native_broker_33rd_control_rejects_without_changing_intent_or_native_admission() {
        let mut fixture = RoutedFixture::new(true);
        let session = fixture.open(true);
        fixture.offer(&session);
        for _ in 0..32 {
            fixture.dispatch(Some(true), Some(true), None);
        }
        let devices = fixture.latest_devices();
        let input = devices.wait_request(true, 2);
        let before = fixture.broker.intent(crate::layout::PaneId::from_raw(316));
        assert!(fixture
            .broker
            .dispatch_control(
                &session,
                Some(false),
                Some(false),
                Some(true),
                Instant::now()
            )
            .is_err());
        assert_eq!(
            fixture.broker.intent(crate::layout::PaneId::from_raw(316)),
            before
        );
        assert_eq!(
            fixture.credits.lock().unwrap()[0]
                .device_control
                .load(Ordering::Acquire),
            32
        );
        assert_eq!(devices.state().input_token(), input);
        assert!(!devices.state().levels_enabled());
        let actions = fixture.broker.close(&session, Instant::now());
        fixture.actions(actions);
        assert!(wait_until(&devices.released, Duration::from_secs(2)));
        assert!(wait_until(
            &fixture.stopped.lock().unwrap()[0],
            Duration::from_secs(3)
        ));
        assert_eq!(
            fixture.credits.lock().unwrap()[0]
                .device_control
                .load(Ordering::Acquire),
            0
        );
        assert!(fixture.broker.applied(&session).is_none());
    }

    #[test]
    fn native_setup_reserves_all_65_fifo_credits_without_accepted_bound_overload() {
        struct GateAudio {
            devices: Arc<FakeDevices>,
            begun: Arc<AtomicBool>,
            gate: std_mpsc::Receiver<()>,
        }
        impl AudioBackend for GateAudio {
            fn open(self: Box<Self>, io: AudioIo) -> Result<OpenedAudio, String> {
                self.begun.store(true, Ordering::Release);
                self.gate
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| "setup gate not released".to_owned())?;
                Box::new(FakeAudio(Arc::clone(&self.devices))).open(io)
            }
        }
        let devices = Arc::new(FakeDevices::default());
        let begun = Arc::new(AtomicBool::new(false));
        let (release, gate) = std_mpsc::channel();
        let sink: PeerEventSink = Arc::new(|_| Box::pin(async { Ok(()) }));
        let mut peer = start_with_audio(
            "setup-budget".to_owned(),
            PeerInitialState::default(),
            sink,
            Box::new(GateAudio {
                devices: Arc::clone(&devices),
                begun: Arc::clone(&begun),
                gate,
            }),
        )
        .unwrap();
        assert!(wait_until(&begun, Duration::from_secs(2)));
        peer.apply_answer("queued until teardown".to_owned())
            .unwrap();
        for id in 1..=32 {
            peer.set_muted(id % 2 == 0).unwrap();
            peer.apply_control(device_control(id, Some(id % 2 == 0), None, None))
                .unwrap();
        }
        assert_eq!(peer.commands.capacity(), 0);
        assert_eq!(
            peer.apply_control(device_control(33, None, None, None)),
            Err(PeerCommandError::Full)
        );
        assert_eq!(peer.set_muted(false), Err(PeerCommandError::Full));
        assert_eq!(
            peer.apply_answer(String::new()),
            Err(PeerCommandError::Full)
        );
        assert!(!peer.closed.load(Ordering::Acquire));
        let credits = Arc::clone(&peer.credits);
        peer.close();
        release.send(()).unwrap();
        assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
        assert!(devices.protected_before_drop.load(Ordering::Acquire));
        assert_eq!(credits.answer.load(Ordering::Acquire), 0);
        assert_eq!(credits.legacy_mute.load(Ordering::Acquire), 0);
        assert_eq!(credits.device_control.load(Ordering::Acquire), 0);
    }

    #[test]
    fn native_enqueue_failed_send_rolls_back_permit_and_fifo_does_not_sort_ids() {
        let (commands, mut rx) = mpsc::channel(2);
        let credits = Arc::new(CommandCredits::default());
        let (cancel, _) = watch::channel(false);
        let peer = NativePeer {
            commands,
            credits: Arc::clone(&credits),
            cancel,
            closed: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            done: None,
            thread: None,
        };
        peer.enqueue(Command::Control(device_control(99, None, None, None)))
            .unwrap();
        peer.enqueue(Command::Mute(true)).unwrap();
        assert_eq!(
            peer.enqueue(Command::Answer("not admitted".to_owned())),
            Err(PeerCommandError::Full)
        );
        assert_eq!(
            credits.answer.load(Ordering::Acquire),
            0,
            "failed send releases its reserved credit"
        );
        let first = rx.try_recv().unwrap();
        assert!(matches!(
            first.command,
            Command::Control(MediaDeviceControl { control_id: 99, .. })
        ));
        assert_eq!(
            credits.device_control.load(Ordering::Acquire),
            1,
            "dequeue alone retains active credit"
        );
        let second = rx.try_recv().unwrap();
        assert!(matches!(second.command, Command::Mute(true)));
        drop(first.permit);
        drop(second.permit);
        assert_eq!(credits.device_control.load(Ordering::Acquire), 0);
        assert_eq!(credits.legacy_mute.load(Ordering::Acquire), 0);
        rx.close();
        assert_eq!(
            peer.enqueue(Command::Answer(String::new())),
            Err(PeerCommandError::Closed)
        );
        assert_eq!(credits.answer.load(Ordering::Acquire), 0);
    }

    #[test]
    fn device_info_bounds_names_by_utf8_bytes() {
        let info = OpenedAudio::new(
            Box::new(()),
            format!("{}é", "a".repeat(511)),
            "é".repeat(300),
        )
        .info("media_test");
        assert_eq!(info.input_device.len(), 511);
        assert_eq!(info.output_device.len(), 512);
        assert_eq!(info.output_device.chars().count(), 256);
    }

    #[test]
    fn resampler_keeps_rate_ratio_across_chunks() {
        let mut resampler = Resampler::new(44_100, SAMPLE_RATE);
        let mut out = Vec::new();
        for _ in 0..100 {
            resampler.process(&[0.5; 441], &mut out);
        }
        // 1 s of 44.1 kHz input gives about 1 s at 48 kHz.
        assert!((out.len() as i64 - 48_000).abs() <= 2, "got {}", out.len());
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
        let mut same = Resampler::new(SAMPLE_RATE, SAMPLE_RATE);
        let mut out = Vec::new();
        same.process(&[0.25; 7], &mut out);
        assert_eq!(out, vec![0.25; 7]);
    }
}
