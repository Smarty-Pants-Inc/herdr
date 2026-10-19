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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod controls;
mod speaker;

use controls::{enabled, AudioState, Controls};
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

use super::peer::{MediaPeer, PeerEvent, PeerEventSink};
use crate::protocol::media::{
    close_code, MediaApplied, MediaDeviceControl, MediaInfo, MediaLevels, MediaPeerState,
    MAX_MEDIA_TEXT_BYTES,
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
const COMMAND_CAPACITY: usize = 32;
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
    Close,
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
type SessionEnd = Result<Option<(&'static str, String)>, String>;

type Playback = Arc<Mutex<VecDeque<f32>>>;

/// A running native media peer. Dropping it closes the session.
pub(crate) struct NativePeer {
    commands: mpsc::Sender<Command>,
    overloaded: Arc<AtomicBool>,
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
pub(crate) fn start(session_id: String, sink: PeerEventSink) -> Result<NativePeer, String> {
    start_with_audio(session_id, sink, Box::new(cpal_audio::CpalAudio))
}

/// Start a peer with the default microphone and speaker.
#[cfg(not(target_os = "macos"))]
pub(crate) fn start(_session_id: String, _sink: PeerEventSink) -> Result<NativePeer, String> {
    Err("native media audio is only supported on macOS".to_owned())
}

/// Start a peer with the given audio backend. Returns at once; the offer arrives as
/// `PeerEvent::Offer`.
pub(crate) fn start_with_audio(
    session_id: String,
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
    let overloaded = Arc::new(AtomicBool::new(false));
    let thread_overloaded = Arc::clone(&overloaded);
    let (cancel, cancel_rx) = watch::channel(false);
    let (done_tx, done) = std_mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = Arc::clone(&stopped);
    let emitter = Emitter {
        session_id,
        sink,
        closed: Arc::clone(&closed),
    };
    let thread = std::thread::Builder::new()
        .name("herdr-media-peer".to_owned())
        .spawn(move || {
            let result = runtime.block_on(session(&emitter, audio, command_rx, cancel_rx));
            runtime.shutdown_timeout(Duration::from_millis(500));
            let result = if thread_overloaded.load(Ordering::Acquire) {
                Err("too many pending media commands".to_owned())
            } else {
                result
            };
            match result {
                Ok(None) => {}
                Ok(Some((code, message))) => emitter.emit(PeerEvent::Closed {
                    session_id: emitter.session_id.clone(),
                    code,
                    message,
                }),
                Err(message) => {
                    tracing::warn!(session_id = %emitter.session_id, %message, "media peer failed");
                    emitter.emit(PeerEvent::Closed {
                        session_id: emitter.session_id.clone(),
                        code: close_code::DEVICE_ERROR,
                        message,
                    });
                }
            }
            thread_stopped.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        })
        .map_err(|error| format!("could not start the media thread: {error}"))?;
    Ok(NativePeer {
        commands,
        overloaded,
        cancel,
        closed,
        stopped,
        done: Some(done),
        thread: Some(thread),
    })
}

impl NativePeer {
    fn enqueue(&self, command: Command) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.commands.try_send(command) {
            self.overloaded.store(true, Ordering::Release);
            // Cancellation releases devices before the thread reports the overload.
            let _ = self.cancel.send(true);
        }
    }
}

impl MediaPeer for NativePeer {
    fn apply_answer(&mut self, sdp: String) {
        self.enqueue(Command::Answer(sdp));
    }

    fn set_muted(&mut self, muted: bool) {
        self.enqueue(Command::Mute(muted));
    }

    fn apply_control(&mut self, control: MediaDeviceControl) {
        self.enqueue(Command::Control(control));
    }

    fn close(&mut self) {
        let Some(done) = self.done.take() else {
            return;
        };
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.cancel.send(true);
        let _ = self.commands.try_send(Command::Close);
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

/// Sends events to the controller until the controller closes the peer.
struct Emitter {
    session_id: String,
    sink: PeerEventSink,
    closed: Arc<AtomicBool>,
}

impl Emitter {
    fn emit(&self, event: PeerEvent) {
        if !self.closed.load(Ordering::SeqCst) {
            (self.sink)(event);
        }
    }

    fn state(&self, state: MediaPeerState, muted: bool) {
        self.emit(PeerEvent::State {
            session_id: self.session_id.clone(),
            state,
            muted,
            detail: None,
        });
    }
}

/// One media session. Returns `Err` when the peer must report `device_error`, and
/// `Ok(Some((code, message)))` for remote close or a changed device. Audio is released and the peer
/// connection closed before it returns.
async fn session(
    emitter: &Emitter,
    audio: Box<dyn AudioBackend>,
    commands: mpsc::Receiver<Command>,
    mut cancel: watch::Receiver<bool>,
) -> SessionEnd {
    if *cancel.borrow() {
        return Ok(None);
    }
    let (internal_tx, internal) = mpsc::unbounded_channel();
    let playback: Playback = Arc::new(Mutex::new(VecDeque::with_capacity(PLAYBACK_CAPACITY)));
    let (frames_tx, frames) = mpsc::channel(CAPTURE_QUEUE_FRAMES);
    let audio_state = Arc::new(AudioState::default());

    let mut audio_guard = audio.open(audio_io(
        frames_tx,
        Arc::clone(&playback),
        Arc::clone(&audio_state),
        &internal_tx,
    ))?;
    // A close that arrived while the device opened releases it here, before any WebRTC work.
    if *cancel.borrow() {
        return Ok(None);
    }
    let peer = tokio::select! {
        biased;
        () = cancelled(&mut cancel) => return Ok(None),
        peer = new_peer_connection() => match peer {
            Ok(peer) => Arc::new(peer),
            Err(error) => return Err(format!("could not create the WebRTC peer: {error}")),
        },
    };
    let result = tokio::select! {
        biased;
        () = cancelled(&mut cancel) => Ok(None),
        result = drive(
            emitter,
            &peer,
            Channels {
                commands,
                internal,
                internal_tx,
                frames,
                playback,
                audio: audio_state,
            },
            &mut audio_guard,
        ) => result,
    };
    drop(audio_guard);
    match tokio::time::timeout(PEER_CLOSE_TIMEOUT, peer.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "media peer close failed"),
        Err(_) => tracing::debug!("media peer close timed out"),
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
    commands: mpsc::Receiver<Command>,
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

async fn new_peer_connection() -> webrtc::error::Result<RTCPeerConnection> {
    let mut media = MediaEngine::default();
    // Only Opus: capture encodes and playback decodes nothing else, so offering the other
    // default codecs (G722, PCMU, PCMA) would let an answer pick audio this peer cannot play.
    media.register_codec(opus_codec(), RTPCodecType::Audio)?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build();
    // No ICE servers: host candidates reach the public ICE-lite remote peer.
    api.new_peer_connection(RTCConfiguration::default()).await
}

/// Negotiate, then run the control loop until close or failure.
async fn drive(
    emitter: &Emitter,
    peer: &Arc<RTCPeerConnection>,
    channels: Channels,
    devices: &mut OpenedAudio,
) -> SessionEnd {
    let Channels {
        mut commands,
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
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; 1500];
        while sender.read(&mut buffer).await.is_ok() {}
    });
    tokio::spawn(encode_microphone(
        frames,
        track,
        Arc::clone(&audio),
        internal_tx.clone(),
    ));

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
    peer.on_track(Box::new(move |remote, _, _| {
        // The handler future runs under a lock; keep it short and play in a task.
        tokio::spawn(play_remote(
            remote,
            Arc::clone(&playback),
            Arc::clone(&playback_audio),
            track_tx.clone(),
        ));
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
    emitter.emit(PeerEvent::Offer {
        session_id: emitter.session_id.clone(),
        sdp,
    });

    let mut connected = false;
    let mut channel_open = false;
    let mut reported_connected = false;
    let mut observe = tokio::time::interval(CONTROL_OBSERVE_INTERVAL);
    observe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut device_check = tokio::time::interval(DEVICE_CHECK_INTERVAL);
    device_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv(), if controls.idle() => match command {
                Some(Command::Answer(sdp)) => {
                    let answer = RTCSessionDescription::answer(sdp)
                        .map_err(|error| webrtc_error("invalid answer", error))?;
                    peer.set_remote_description(answer)
                        .await
                        .map_err(|error| webrtc_error("could not apply the answer", error))?;
                    if !reported_connected {
                        emitter.state(MediaPeerState::Connecting, controls.muted);
                    }
                }
                Some(Command::Mute(mute)) => {
                    controls.begin(None, Some(mute), None, None, Instant::now())?;
                }
                Some(Command::Control(control)) => {
                    controls.begin(
                        Some(control.control_id),
                        control.input_muted,
                        control.output_suppressed,
                        control.levels,
                        Instant::now(),
                    )?;
                }
                Some(Command::Close) | None => return Ok(None),
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
                    return Err("the WebRTC connection failed".to_owned());
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
                Some(Internal::Fatal(message)) => return Err(message),
                None => return Ok(None),
            },
        }
        if let Some(confirmation) = controls.confirmed() {
            if let Some(control_id) = confirmation.control_id {
                emitter.emit(PeerEvent::Applied(MediaApplied {
                    session_id: emitter.session_id.clone(),
                    control_id,
                    input_muted: confirmation.muted,
                    output_suppressed: confirmation.suppressed,
                }));
            } else {
                emitter.state(
                    if reported_connected {
                        MediaPeerState::Connected
                    } else {
                        MediaPeerState::Connecting
                    },
                    confirmation.muted,
                );
            }
        }
        if connected && channel_open && !reported_connected {
            reported_connected = true;
            emitter.state(MediaPeerState::Connected, controls.muted);
            emitter.emit(PeerEvent::Info(devices.info(&emitter.session_id)));
        }
        if let Some((input_peak, output_peak)) = controls.take_levels(Instant::now()) {
            emitter.emit(PeerEvent::Levels(MediaLevels {
                session_id: emitter.session_id.clone(),
                input_peak,
                output_peak,
            }));
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
                    if macos_media::stream_device_changed(&stream_error) {
                        changed();
                    } else {
                        error(format!("speaker: {stream_error}"));
                    }
                },
                None,
            )
            .map_err(|error| format!("speaker: {error}"))
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
            let mut out = vec![1.0; 127];
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
            self.0.callbacks.lock().unwrap().take();
            self.0.audio.lock().unwrap().take();
            self.0.released.store(true, Ordering::SeqCst);
        }
    }

    impl AudioBackend for FakeAudio {
        fn open(self: Box<Self>, mut io: AudioIo) -> Result<OpenedAudio, String> {
            // Exercise the real frame packer and outer renderer, then retain the
            // callbacks so tests can withhold actual capture/output confirmations.
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
            if let Ok(events_tx) = events_tx.lock() {
                let _ = events_tx.send(event);
            }
        });
        let peer = start_with_audio(
            "media_test".to_owned(),
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
            if let Ok(events_tx) = events_tx.lock() {
                let _ = events_tx.send(event);
            }
        });
        let mut peer = start_with_audio(
            "media_test".to_owned(),
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
        peer.apply_answer(answer);

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
                peer.apply_control(device_control(1, Some(false), Some(false), None));
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

    #[test]
    fn native_peer_confirms_mute_before_connect() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        peer.set_muted(true);
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
            if let Ok(events_tx) = events_tx.lock() {
                let _ = events_tx.send(event);
            }
        });
        let _peer = start_with_audio("media_test".to_owned(), sink, Box::new(BrokenAudio))
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
        peer.apply_control(device_control(10, Some(true), Some(true), None));
        peer.apply_control(device_control(11, Some(false), Some(false), None));
        let input = devices.wait_request(true, 0);
        let output = devices.wait_request(false, 0);
        assert!(enabled(input) && enabled(output));
        assert!(events.recv_timeout(Duration::from_millis(60)).is_err());
        assert_eq!(audio.input_applied.load(Ordering::Acquire), 0);
        assert_eq!(audio.output_applied.load(Ordering::Acquire), 0);

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
        peer.apply_control(device_control(1, None, None, Some(true)));
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
        peer.apply_control(device_control(2, None, None, Some(false)));
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
        peer.apply_control(device_control(1, Some(true), None, Some(true)));
        let muted_token = devices.wait_request(true, 0);
        devices.capture(&[0.9; FRAME_SAMPLES]);
        confirmation_and_levels(&events, Some(1), true, 0.0);

        peer.apply_control(device_control(2, Some(false), None, None));
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

        peer.set_muted(true);
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
    fn native_command_overload_closes_instead_of_growing_an_unbounded_control_queue() {
        let (mut peer, devices, events) = start_fake();
        wait_offer(&events);
        peer.apply_control(device_control(1, Some(true), None, None));
        devices.wait_request(true, 0);
        for id in 2..=(COMMAND_CAPACITY as u64 + 2) {
            peer.apply_control(device_control(id, Some(false), None, None));
        }
        match events.recv_timeout(Duration::from_secs(4)) {
            Ok(PeerEvent::Closed { code, message, .. }) => {
                assert_eq!(code, close_code::DEVICE_ERROR);
                assert!(message.contains("too many pending"));
            }
            other => panic!("expected an overload close, got {other:?}"),
        }
        assert!(devices.released.load(Ordering::SeqCst));
        peer.close();
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
