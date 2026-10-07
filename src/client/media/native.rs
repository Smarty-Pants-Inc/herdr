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
use std::time::{Duration, Instant};

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
use crate::protocol::media::{close_code, MediaPeerState};

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
const ICE_GATHER_TIMEOUT: Duration = Duration::from_secs(3);
const PEER_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const THREAD_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The OpenAI Realtime peer expects a data channel with this label.
const EVENTS_CHANNEL_LABEL: &str = "oai-events";

/// Takes captured microphone samples.
pub(crate) type CaptureFn = Box<dyn FnMut(&[f32]) + Send>;
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
    /// Call immediately after acquiring each stream/guard, including partial opens that
    /// later return Err. This marker must not be deferred until `open` succeeds.
    pub(crate) acquired: Arc<dyn Fn() + Send + Sync>,
}

/// Microphone and speaker access.
pub(crate) trait AudioBackend: Send {
    /// Start capture and playback. Dropping the returned guard stops both streams and
    /// releases the devices. It is dropped on the peer thread.
    fn open(self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String>;
}

enum Command {
    Answer(String),
    Mute(bool),
    Close,
}

/// Events from WebRTC and audio callbacks to the peer's control loop.
enum Internal {
    PeerState(RTCPeerConnectionState),
    ChannelOpen,
    /// The required events channel closed; the remote side ended the call.
    ChannelClosed,
    Fatal(String),
}

/// How a session ended when it was not cancelled by the controller.
type SessionEnd = Result<Option<String>, String>;

type Playback = Arc<Mutex<VecDeque<f32>>>;

/// A running native media peer. Dropping it closes the session.
pub(crate) struct NativePeer {
    commands: mpsc::UnboundedSender<Command>,
    /// Cancels setup and negotiation at their next await, so close never waits for them.
    cancel: watch::Sender<bool>,
    closed: Arc<AtomicBool>,
    /// Set by the peer thread once it has released the devices and stopped.
    // Only tests read it; the reaper thread waits on `done` instead.
    #[cfg_attr(not(test), allow(dead_code))]
    stopped: Arc<AtomicBool>,
    /// Admission and close share this lock; no opener can start after the seal.
    acquisition: Arc<Mutex<Acquisition>>,
    /// Wake the already-started reaper without blocking the client loop.
    reap: Option<std_mpsc::Sender<Reap>>,
}

/// Why the reaper was woken. Only `Teardown`, a dropped sender or an expired `Opening`
/// deadline starts the one-shot join.
enum Reap {
    Teardown,
    /// The backend marked (partial) acquisition while `open` still runs. Starts the join
    /// deadline now: a failing/unwinding backend may block in its own guard drop before
    /// any outer notifier can drop.
    Opening,
    /// `open` returned its guard; cancel the `Opening` deadline.
    Opened,
}

#[derive(Default)]
struct Acquisition {
    sealed: bool,
    opener_started: bool,
    acquired: bool,
}

impl Acquisition {
    fn may_have_acquired(&self) -> bool {
        // Only a locked, skipped opener proves never-acquired. An admitted backend
        // returning Err without a marker is not sufficient evidence for acquired:false.
        self.acquired || self.opener_started
    }
}

/// Wake natural teardown even when the worker unwinds before sending its done result.
struct CompletionNotify(std_mpsc::Sender<Reap>);

impl Drop for CompletionNotify {
    fn drop(&mut self) {
        let _ = self.0.send(Reap::Teardown);
    }
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
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (cancel, cancel_rx) = watch::channel(false);
    let (done_tx, done) = std_mpsc::channel();
    let (reap, reap_rx) = std_mpsc::channel();
    let completion = CompletionNotify(reap.clone());
    let acquisition = Arc::new(Mutex::new(Acquisition::default()));
    let thread_acquisition = Arc::clone(&acquisition);
    let teardown_ok = Arc::new(AtomicBool::new(true));
    let thread_teardown_ok = Arc::clone(&teardown_ok);
    let closed = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = Arc::clone(&stopped);
    let emitter = Emitter {
        session_id: session_id.clone(),
        sink: Arc::clone(&sink),
        closed: Arc::clone(&closed),
        reap: reap.clone(),
    };
    let thread = std::thread::Builder::new()
        .name("herdr-media-peer".to_owned())
        .spawn(move || {
            let _completion = completion;
            let result = runtime.block_on(session(
                &emitter,
                audio,
                command_rx,
                cancel_rx,
                thread_acquisition,
                &thread_teardown_ok,
            ));
            // Fully release the runtime, rather than detaching unfinished tasks. The
            // reaper's deadline covers a stuck runtime as well as a stuck device guard.
            drop(runtime);
            match result {
                Ok(None) => {}
                Ok(Some(message)) => emitter.emit(PeerEvent::Closed {
                    session_id: emitter.session_id.clone(),
                    code: close_code::CLOSED,
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
            let _ = done_tx.send(teardown_ok.load(Ordering::SeqCst));
        })
        .map_err(|error| format!("could not start the media thread: {error}"))?;
    let reaper_acquisition = Arc::clone(&acquisition);
    let reaper_sink = Arc::clone(&sink);
    let reaper_session = session_id.clone();
    let reaper = std::thread::Builder::new()
        .name("herdr-media-reaper".to_owned())
        .spawn(move || {
            // Close wakes us promptly; completion/drop wakes us for natural end/panic.
            // One reaper owns the handle, so a timeout can never produce a late receipt.
            // ponytail: a backend open that is still running THREAD_JOIN_TIMEOUT after its
            // first acquisition marker is reported stuck (diagnostic, never a receipt);
            // backends cannot signal failure before their partial guards drop.
            let mut deadline: Option<Instant> = None;
            let mut opened = false;
            loop {
                let wake = match deadline {
                    Some(at) => reap_rx
                        .recv_timeout(at.saturating_duration_since(Instant::now()))
                        .ok(),
                    None => reap_rx.recv().ok(),
                };
                match wake {
                    Some(Reap::Opening) if !opened => {
                        deadline.get_or_insert_with(|| Instant::now() + THREAD_JOIN_TIMEOUT);
                    }
                    Some(Reap::Opening) => {}
                    Some(Reap::Opened) => {
                        opened = true;
                        deadline = None;
                    }
                    Some(Reap::Teardown) | None => break,
                }
            }
            let deadline = deadline.unwrap_or_else(|| Instant::now() + THREAD_JOIN_TIMEOUT);
            let done_ok = matches!(
                done.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Ok(true)
            );
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let joined = thread.is_finished() && thread.join().is_ok();
            let success = done_ok && joined;
            let acquired = reaper_acquisition
                .lock()
                .map(|state| state.may_have_acquired())
                .unwrap_or(true);
            if !success {
                tracing::warn!(session_id = %reaper_session, "media teardown failed or did not stop within 2 s");
            }
            // Teardown bypasses Emitter.closed: cancel suppresses normal events, not
            // the post-join result needed to distinguish completion from admission.
            reaper_sink(PeerEvent::Teardown {
                session_id: reaper_session,
                acquired,
                success,
            });
        });
    if let Err(error) = reaper {
        tracing::warn!(%error, session_id = %session_id, "could not start the media reaper thread");
        if let Ok(mut state) = acquisition.lock() {
            state.sealed = true;
        }
        closed.store(true, Ordering::SeqCst);
        let _ = cancel.send(true);
        let acquired = acquisition
            .lock()
            .map(|state| state.may_have_acquired())
            .unwrap_or(true);
        sink(PeerEvent::Teardown {
            session_id,
            acquired,
            success: false,
        });
    }
    Ok(NativePeer {
        commands,
        cancel,
        closed,
        stopped,
        acquisition,
        reap: Some(reap),
    })
}

impl MediaPeer for NativePeer {
    fn apply_answer(&mut self, sdp: String) {
        let _ = self.commands.send(Command::Answer(sdp));
    }

    fn set_muted(&mut self, muted: bool) {
        let _ = self.commands.send(Command::Mute(muted));
    }

    fn close(&mut self) {
        let Some(reap) = self.reap.take() else {
            return;
        };
        // Never hold the admission lock across device work. Close seals it promptly;
        // an already admitted opener remains owned by the worker until it returns.
        if let Ok(mut state) = self.acquisition.lock() {
            state.sealed = true;
            self.closed.store(true, Ordering::SeqCst);
            let _ = self.cancel.send(true);
        } else {
            self.closed.store(true, Ordering::SeqCst);
            let _ = self.cancel.send(true);
        }
        let _ = self.commands.send(Command::Close);
        let _ = reap.send(Reap::Teardown);
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
    reap: std_mpsc::Sender<Reap>,
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
/// `Ok(Some(message))` when the remote side ended the call. Audio is released and the peer
/// connection closed before it returns.
async fn session(
    emitter: &Emitter,
    audio: Box<dyn AudioBackend>,
    commands: mpsc::UnboundedReceiver<Command>,
    mut cancel: watch::Receiver<bool>,
    acquisition: Arc<Mutex<Acquisition>>,
    teardown_ok: &AtomicBool,
) -> SessionEnd {
    // The same lock that seals close guards admission, not just an atomic check
    // followed by a raceable open. No backend is invoked after a skipped admission.
    {
        let mut state = acquisition
            .lock()
            .map_err(|_| "media acquisition lock poisoned".to_owned())?;
        if state.sealed {
            return Ok(None);
        }
        state.opener_started = true;
    }
    let (internal_tx, internal) = mpsc::unbounded_channel();
    let playback: Playback = Arc::new(Mutex::new(VecDeque::with_capacity(PLAYBACK_CAPACITY)));
    let (frames_tx, frames) = mpsc::channel(CAPTURE_QUEUE_FRAMES);
    let muted = Arc::new(AtomicBool::new(false));

    let marker_state = Arc::clone(&acquisition);
    let marker_reap = emitter.reap.clone();
    let acquired: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        if let Ok(mut state) = marker_state.lock() {
            state.acquired = true;
        }
        // Wake before the backend can drop (and block in) a partial guard on Err/unwind.
        let _ = marker_reap.send(Reap::Opening);
    });
    let audio_guard = audio.open(audio_io(
        frames_tx,
        Arc::clone(&playback),
        &internal_tx,
        acquired,
    ))?;
    let _ = emitter.reap.send(Reap::Opened);
    // Declared after the guard, so early-return/unwind wakes teardown before
    // implicit guard drop. The normal path wakes it explicitly before dropping.
    let _teardown = CompletionNotify(emitter.reap.clone());
    if let Ok(mut state) = acquisition.lock() {
        state.acquired = true;
    }
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
    if let Ok(mut state) = acquisition.lock() {
        state.acquired = true; // The WebRTC peer itself also counts as acquisition.
    }
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
                muted,
            },
        ) => result,
    };
    let _ = emitter.reap.send(Reap::Teardown);
    drop(audio_guard);
    match tokio::time::timeout(PEER_CLOSE_TIMEOUT, peer.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            teardown_ok.store(false, Ordering::SeqCst);
            tracing::warn!(%error, "media peer close failed");
        }
        Err(_) => {
            teardown_ok.store(false, Ordering::SeqCst);
            tracing::warn!("media peer close timed out");
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
    commands: mpsc::UnboundedReceiver<Command>,
    internal: mpsc::UnboundedReceiver<Internal>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    frames: mpsc::Receiver<Vec<f32>>,
    playback: Playback,
    muted: Arc<AtomicBool>,
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
async fn drive(emitter: &Emitter, peer: &Arc<RTCPeerConnection>, channels: Channels) -> SessionEnd {
    let Channels {
        mut commands,
        mut internal,
        internal_tx,
        frames,
        playback,
        muted,
    } = channels;
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
        Arc::clone(&muted),
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
    peer.on_track(Box::new(move |remote, _, _| {
        // The handler future runs under a lock; keep it short and play in a task.
        tokio::spawn(play_remote(remote, Arc::clone(&playback), track_tx.clone()));
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
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Answer(sdp)) => {
                    let answer = RTCSessionDescription::answer(sdp)
                        .map_err(|error| webrtc_error("invalid answer", error))?;
                    peer.set_remote_description(answer)
                        .await
                        .map_err(|error| webrtc_error("could not apply the answer", error))?;
                    if !reported_connected {
                        emitter.state(MediaPeerState::Connecting, muted.load(Ordering::SeqCst));
                    }
                }
                Some(Command::Mute(mute)) => {
                    muted.store(mute, Ordering::SeqCst);
                    let state = if reported_connected {
                        MediaPeerState::Connected
                    } else {
                        MediaPeerState::Connecting
                    };
                    emitter.state(state, mute);
                }
                Some(Command::Close) | None => return Ok(None),
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
                    return Ok(Some("the remote side closed the events channel".to_owned()));
                }
                Some(Internal::Fatal(message)) => return Err(message),
                None => return Ok(None),
            },
        }
        if connected && channel_open && !reported_connected {
            reported_connected = true;
            emitter.state(MediaPeerState::Connected, muted.load(Ordering::SeqCst));
        }
    }
}

/// Encode captured 20 ms frames and write them to the local track. Sends silence while
/// muted so the remote side keeps a steady stream.
async fn encode_microphone(
    mut frames: mpsc::Receiver<Vec<f32>>,
    track: Arc<TrackLocalStaticSample>,
    muted: Arc<AtomicBool>,
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
    let silence = [0.0_f32; FRAME_SAMPLES];
    let mut packet = [0_u8; 1500];
    while let Some(frame) = frames.recv().await {
        let input = if muted.load(Ordering::SeqCst) {
            &silence[..]
        } else {
            &frame[..]
        };
        let size = match encoder.encode_float(input, &mut packet) {
            Ok(size) => size,
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
    }
}

/// Decode the remote audio track into the playback buffer.
async fn play_remote(
    remote: Arc<TrackRemote>,
    playback: Playback,
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
        match decoder.decode_float(&packet.payload, &mut pcm, false) {
            Ok(count) => push_playback(&playback, &pcm[..count.min(pcm.len())]),
            Err(error) => tracing::debug!(%error, "could not decode a remote audio packet"),
        }
    }
}

fn push_playback(playback: &Playback, samples: &[f32]) {
    let Ok(mut buffer) = playback.lock() else {
        return;
    };
    let samples = &samples[samples.len().saturating_sub(PLAYBACK_CAPACITY)..];
    let overflow = (buffer.len() + samples.len()).saturating_sub(PLAYBACK_CAPACITY);
    let dropped = overflow.min(buffer.len());
    buffer.drain(..dropped);
    buffer.extend(samples.iter().copied());
}

/// Callbacks that connect an audio backend to the encoder and the playback buffer.
fn audio_io(
    frames: mpsc::Sender<Vec<f32>>,
    playback: Playback,
    internal: &mpsc::UnboundedSender<Internal>,
    acquired: Arc<dyn Fn() + Send + Sync>,
) -> AudioIo {
    let mut pending = Vec::with_capacity(FRAME_SAMPLES * 2);
    let error_tx = internal.clone();
    AudioIo {
        acquired,
        // ponytail: allocates one Vec per 20 ms frame on the device thread; a lock-free
        // frame pool is only worth it if glitches show up.
        capture: Box::new(move |samples| {
            pending.extend_from_slice(samples);
            while pending.len() >= FRAME_SAMPLES {
                let frame: Vec<f32> = pending.drain(..FRAME_SAMPLES).collect();
                // A full queue drops the frame rather than blocking the device thread.
                let _ = frames.try_send(frame);
            }
        }),
        playback: Box::new(move |out| {
            // Never block the device thread: play silence when the buffer is busy.
            let Ok(mut buffer) = playback.try_lock() else {
                out.fill(0.0);
                return;
            };
            for sample in out.iter_mut() {
                *sample = buffer.pop_front().unwrap_or(0.0);
            }
        }),
        error: Arc::new(move |message| {
            let _ = error_tx.send(Internal::Fatal(message));
        }),
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
        AudioBackend, AudioIo, CaptureFn, PlaybackFn, Resampler, FRAME_SAMPLES, SAMPLE_RATE,
    };

    /// The default CoreAudio microphone and speaker.
    pub(super) struct CpalAudio;

    impl AudioBackend for CpalAudio {
        fn open(self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
            let host = cpal::default_host();
            let input = host
                .default_input_device()
                .ok_or_else(|| "no default microphone".to_owned())?;
            let output = host
                .default_output_device()
                .ok_or_else(|| "no default speaker".to_owned())?;
            let input_config = input
                .default_input_config()
                .map_err(|error| format!("microphone: {error}"))?;
            let output_config = output
                .default_output_config()
                .map_err(|error| format!("speaker: {error}"))?;

            let capture = match input_config.sample_format() {
                SampleFormat::F32 => {
                    input_stream::<f32>(&input, input_config.config(), io.capture, &io.error)
                }
                SampleFormat::I16 => {
                    input_stream::<i16>(&input, input_config.config(), io.capture, &io.error)
                }
                other => Err(format!("unsupported microphone sample format {other}")),
            }?;
            (io.acquired)();
            let playback = match output_config.sample_format() {
                SampleFormat::F32 => {
                    output_stream::<f32>(&output, output_config.config(), io.playback, &io.error)
                }
                SampleFormat::I16 => {
                    output_stream::<i16>(&output, output_config.config(), io.playback, &io.error)
                }
                other => Err(format!("unsupported speaker sample format {other}")),
            }?;
            (io.acquired)();
            capture
                .play()
                .map_err(|error| format!("microphone: {error}"))?;
            playback
                .play()
                .map_err(|error| format!("speaker: {error}"))?;
            Ok(Box::new((capture, playback)))
        }
    }

    fn input_stream<T>(
        device: &cpal::Device,
        config: StreamConfig,
        mut capture: CaptureFn,
        error: &Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let channels = usize::from(config.channels).max(1);
        let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
        let mut mono = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut resampled = Vec::with_capacity(FRAME_SAMPLES * 2);
        let error = Arc::clone(error);
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    mono.extend(data.chunks(channels).map(|frame| {
                        frame.iter().map(|&s| f32::from_sample_(s)).sum::<f32>()
                            / frame.len() as f32
                    }));
                    resampled.clear();
                    resampler.process(&mono, &mut resampled);
                    capture(&resampled);
                },
                move |stream_error| error(format!("microphone: {stream_error}")),
                None,
            )
            .map_err(|error| format!("microphone: {error}"))
    }

    fn output_stream<T>(
        device: &cpal::Device,
        config: StreamConfig,
        mut playback: PlaybackFn,
        error: &Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<cpal::Stream, String>
    where
        T: SizedSample + FromSample<f32>,
    {
        let channels = usize::from(config.channels).max(1);
        let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
        let mut chunk = vec![0.0_f32; FRAME_SAMPLES / 2];
        let mut pending = Vec::with_capacity(FRAME_SAMPLES * 2);
        let error = Arc::clone(error);
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    let frames = data.len() / channels;
                    while pending.len() < frames {
                        playback(&mut chunk);
                        resampler.process(&chunk, &mut pending);
                    }
                    for (frame, value) in data.chunks_mut(channels).zip(pending.drain(..frames)) {
                        frame.fill(T::from_sample_(value));
                    }
                },
                move |stream_error| error(format!("speaker: {stream_error}")),
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
    }

    struct FakeAudio(Arc<FakeDevices>);

    struct FakeGuard(Arc<FakeDevices>);

    impl Drop for FakeGuard {
        fn drop(&mut self) {
            self.0.released.store(true, Ordering::SeqCst);
        }
    }

    impl AudioBackend for FakeAudio {
        fn open(self: Box<Self>, mut io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
            // Exercise both paths once: 40 ms of tone in, 10 ms out.
            (io.capture)(&vec![0.1; FRAME_SAMPLES * 2]);
            (io.playback)(&mut [1.0; 480]);
            self.0.opened.store(true, Ordering::SeqCst);
            (io.acquired)();
            Ok(Box::new(FakeGuard(Arc::clone(&self.0))))
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

    // #370: hold actual AudioBackend open/guard-drop work, not a mock screen or
    // an invented receipt path. Every gate has a finite fallback and releases on panic.
    struct ReceiptRelease(Option<std_mpsc::Sender<()>>);

    impl ReceiptRelease {
        fn release(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    impl Drop for ReceiptRelease {
        fn drop(&mut self) {
            self.release();
        }
    }

    type DeviceErrorCallback = Arc<dyn Fn(String) + Send + Sync>;

    struct ReceiptAudio {
        devices: Arc<FakeDevices>,
        open_hold: Option<(std_mpsc::Sender<()>, std_mpsc::Receiver<()>)>,
        drop_hold: Option<(std_mpsc::Sender<()>, std_mpsc::Receiver<()>)>,
        fail_after_acquisition: bool,
        error: Arc<Mutex<Option<DeviceErrorCallback>>>,
    }

    struct ReceiptGuard {
        devices: Arc<FakeDevices>,
        hold: Option<(std_mpsc::Sender<()>, std_mpsc::Receiver<()>)>,
    }

    impl Drop for ReceiptGuard {
        fn drop(&mut self) {
            if let Some((entered, release)) = self.hold.take() {
                let _ = entered.send(());
                let _ = release.recv_timeout(Duration::from_secs(10));
            }
            self.devices.released.store(true, Ordering::SeqCst);
        }
    }

    impl AudioBackend for ReceiptAudio {
        fn open(mut self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
            *self.error.lock().unwrap() = Some(io.error);
            if let Some((entered, release)) = self.open_hold.take() {
                let _ = entered.send(());
                release
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release held opener");
            }
            self.devices.opened.store(true, Ordering::SeqCst);
            (io.acquired)();
            let guard = ReceiptGuard {
                devices: self.devices.clone(),
                hold: self.drop_hold.take(),
            };
            if self.fail_after_acquisition {
                // Mirrors capture stream acquired, speaker setup fails in cpal_audio::open.
                // The guard is real evidence of acquisition even though open returns Err.
                drop(guard);
                Err("speaker setup failed after capture acquisition".into())
            } else {
                Ok(Box::new(guard))
            }
        }
    }

    struct ReceiptHarness {
        peer: NativePeer,
        devices: Arc<FakeDevices>,
        events: std_mpsc::Receiver<PeerEvent>,
        released_at_teardown: Arc<Mutex<Vec<bool>>>,
        open_entered: std_mpsc::Receiver<()>,
        open_release: ReceiptRelease,
        drop_entered: std_mpsc::Receiver<()>,
        drop_release: ReceiptRelease,
        error: Arc<Mutex<Option<DeviceErrorCallback>>>,
    }

    fn receipt_native(open_held: bool, drop_held: bool, fail: bool) -> ReceiptHarness {
        let devices = Arc::new(FakeDevices::default());
        let (events_tx, events) = std_mpsc::channel();
        let released_at_teardown = Arc::new(Mutex::new(Vec::new()));
        let observations = released_at_teardown.clone();
        let observed_devices = devices.clone();
        let sink: PeerEventSink = Arc::new(move |event| {
            if matches!(&event, PeerEvent::Teardown { .. }) {
                observations
                    .lock()
                    .unwrap()
                    .push(observed_devices.released.load(Ordering::SeqCst));
            }
            let _ = events_tx.send(event);
        });
        let (open_tx, open_entered) = std_mpsc::channel();
        let (open_release, open_rx) = std_mpsc::channel();
        let (drop_tx, drop_entered) = std_mpsc::channel();
        let (drop_release, drop_rx) = std_mpsc::channel();
        let error = Arc::new(Mutex::new(None));
        let peer = start_with_audio(
            "media_test".into(),
            sink,
            Box::new(ReceiptAudio {
                devices: devices.clone(),
                open_hold: open_held.then_some((open_tx, open_rx)),
                drop_hold: drop_held.then_some((drop_tx, drop_rx)),
                fail_after_acquisition: fail,
                error: error.clone(),
            }),
        )
        .expect("peer starts");
        ReceiptHarness {
            peer,
            devices,
            events,
            released_at_teardown,
            open_entered,
            open_release: ReceiptRelease(Some(open_release)),
            drop_entered,
            drop_release: ReceiptRelease(Some(drop_release)),
            error,
        }
    }

    fn receive_teardown(events: &std_mpsc::Receiver<PeerEvent>) -> PeerEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(event @ PeerEvent::Teardown { .. }) => return event,
                // Natural failure can report Closed first; Closed alone is not completion.
                Ok(PeerEvent::Closed { .. }) => {}
                other => panic!("expected post-join Teardown, got {other:?}"),
            }
        }
    }

    fn assert_successful_teardown(h: &mut ReceiptHarness) {
        assert_eq!(
            receive_teardown(&h.events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: true,
            }
        );
        assert!(
            h.peer.stopped.load(Ordering::SeqCst),
            "thread must stop before completion"
        );
        assert_eq!(
            *h.released_at_teardown.lock().unwrap(),
            vec![true],
            "guard must be released at event emission, not eventually afterwards"
        );
        h.peer.close();
        assert!(
            h.events.recv_timeout(Duration::from_millis(100)).is_err(),
            "idempotent close cannot emit a second teardown"
        );
    }

    #[test]
    fn receipt_370_slow_device_open_has_no_completion_until_opener_returns_and_guard_drops() {
        let mut h = receipt_native(true, false, false);
        h.open_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("opener is held");
        let started = std::time::Instant::now();
        h.peer.close();
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "close stays nonblocking"
        );
        assert!(
            h.events.recv_timeout(Duration::from_millis(100)).is_err(),
            "no receipt while an opener can still acquire"
        );
        assert!(!h.devices.opened.load(Ordering::SeqCst));
        assert!(!h.peer.stopped.load(Ordering::SeqCst));
        h.open_release.release();
        assert_successful_teardown(&mut h);
        assert!(
            h.devices.opened.load(Ordering::SeqCst),
            "late opener really acquired"
        );
    }

    #[test]
    fn receipt_370_opener_err_after_partial_acquisition_cannot_claim_never_acquired() {
        let mut h = receipt_native(true, false, true);
        h.open_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("opener is held");
        h.peer.close();
        assert!(h.events.recv_timeout(Duration::from_millis(100)).is_err());
        h.open_release.release();
        assert_successful_teardown(&mut h);
        assert!(h.devices.opened.load(Ordering::SeqCst));
        assert!(h.devices.released.load(Ordering::SeqCst));
    }

    #[test]
    fn receipt_370_connected_peer_waits_for_held_device_guard_drop_before_teardown() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let mut h = receipt_native(false, true, false);
        let offer = wait_offer(&h.events);
        runtime.block_on(async {
            let (remote, _channel) = connect_remote(&mut h.peer, offer, &h.events).await;
            h.peer.close();
            h.drop_entered
                .recv_timeout(Duration::from_secs(5))
                .expect("guard drop held");
            assert!(!h.devices.released.load(Ordering::SeqCst));
            assert!(
                h.events.recv_timeout(Duration::from_millis(100)).is_err(),
                "connected peer close is not a receipt while a device guard is held"
            );
            h.drop_release.release();
            assert_successful_teardown(&mut h);
            let _ = remote.close().await;
        });
    }

    #[test]
    fn receipt_370_natural_device_failure_uses_post_join_completion_without_explicit_close() {
        let mut h = receipt_native(false, true, false);
        wait_offer(&h.events);
        let error = h
            .error
            .lock()
            .unwrap()
            .clone()
            .expect("audio error callback");
        error("device lost".into());
        h.drop_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("natural guard drop held");
        assert!(!h.devices.released.load(Ordering::SeqCst));
        assert!(h.events.recv_timeout(Duration::from_millis(100)).is_err());
        h.drop_release.release();
        // No caller close drives this join: natural endings must use the same reaper.
        assert_successful_teardown(&mut h);
    }

    #[test]
    fn receipt_370_join_timeout_emits_failed_teardown_only_and_never_a_late_success() {
        let mut h = receipt_native(false, true, false);
        wait_offer(&h.events);
        h.peer.close();
        h.drop_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("guard drop held");
        assert_eq!(
            receive_teardown(&h.events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: false,
            },
            "join timeout is diagnostic, never completed teardown"
        );
        assert_eq!(*h.released_at_teardown.lock().unwrap(), vec![false]);
        assert!(!h.peer.stopped.load(Ordering::SeqCst));
        h.drop_release.release();
        assert!(wait_until(&h.peer.stopped, Duration::from_secs(5)));
        h.peer.close();
        assert!(
            h.events.recv_timeout(Duration::from_millis(200)).is_err(),
            "timed-out reaper cannot invent a receipt when the detached thread later exits"
        );
    }

    #[test]
    fn receipt_370_natural_guard_drop_timeout_is_failed_once_without_caller_close() {
        let mut h = receipt_native(false, true, false);
        wait_offer(&h.events);
        h.error.lock().unwrap().as_ref().unwrap()("device lost".into());
        h.drop_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("natural guard drop held");
        assert_eq!(
            receive_teardown(&h.events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: false,
            }
        );
        assert_eq!(*h.released_at_teardown.lock().unwrap(), vec![false]);
        h.drop_release.release();
        assert!(wait_until(&h.peer.stopped, Duration::from_secs(5)));
        // Closed may arrive late, but never a second/successful teardown.
        assert!(h
            .events
            .try_iter()
            .all(|event| matches!(event, PeerEvent::Closed { .. })));
        h.peer.close();
        assert!(h.events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn receipt_370_worker_panic_wakes_natural_reaper_and_is_not_success() {
        struct PanicAudio(Arc<FakeDevices>);
        impl AudioBackend for PanicAudio {
            fn open(self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
                self.0.opened.store(true, Ordering::SeqCst);
                let _guard = FakeGuard(self.0.clone());
                (io.acquired)();
                panic!("backend panicked after capture acquisition");
            }
        }
        let devices = Arc::new(FakeDevices::default());
        let (tx, events) = std_mpsc::channel();
        let sink: PeerEventSink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut peer = start_with_audio(
            "media_test".into(),
            sink,
            Box::new(PanicAudio(devices.clone())),
        )
        .expect("peer starts");
        assert_eq!(
            receive_teardown(&events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: false,
            }
        );
        assert!(devices.released.load(Ordering::SeqCst));
        peer.close();
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    /// Release a held guard, wait for the worker to stop, then collect what follows.
    fn release_and_drain(
        release: &mut ReceiptRelease,
        finished: &AtomicBool,
        peer: &mut NativePeer,
        events: &std_mpsc::Receiver<PeerEvent>,
    ) -> Vec<PeerEvent> {
        release.release();
        assert!(wait_until(finished, Duration::from_secs(5)));
        peer.close();
        let mut late = Vec::new();
        while let Ok(event) = events.recv_timeout(Duration::from_millis(300)) {
            late.push(event);
        }
        late
    }

    // SEC-370-03: a backend that marked partial acquisition and then fails/unwinds while
    // its own guard drop blocks must still get the stuck diagnostic within the deadline.
    #[test]
    fn receipt_370_partial_open_failure_wakes_reaper_before_held_drop() {
        let mut h = receipt_native(false, true, true);
        h.drop_entered
            .recv_timeout(Duration::from_secs(5))
            .expect("partial guard drop held");
        assert!(h.devices.opened.load(Ordering::SeqCst));
        assert!(!h.devices.released.load(Ordering::SeqCst));
        let before_release = h
            .events
            .recv_timeout(THREAD_JOIN_TIMEOUT + Duration::from_secs(1));
        // Clean up and join before asserting, even when RED.
        let late = release_and_drain(
            &mut h.drop_release,
            &h.peer.stopped.clone(),
            &mut h.peer,
            &h.events,
        );
        assert!(h.devices.released.load(Ordering::SeqCst));
        assert_eq!(
            before_release,
            Ok(PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: false,
            }),
            "partial-open error blocked in guard drop must wake the reaper"
        );
        assert_eq!(*h.released_at_teardown.lock().unwrap(), vec![false]);
        assert!(
            late.iter()
                .all(|event| matches!(event, PeerEvent::Closed { .. })),
            "a timed-out reaper never emits a late teardown: {late:?}"
        );
    }

    #[test]
    fn receipt_370_backend_unwind_wakes_reaper_before_held_drop() {
        struct HeldPanicAudio {
            devices: Arc<FakeDevices>,
            hold: (std_mpsc::Sender<()>, std_mpsc::Receiver<()>),
        }
        impl AudioBackend for HeldPanicAudio {
            fn open(self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
                self.devices.opened.store(true, Ordering::SeqCst);
                (io.acquired)();
                let _guard = ReceiptGuard {
                    devices: self.devices.clone(),
                    hold: Some(self.hold),
                };
                panic!("test: partial backend unwind with held guard");
            }
        }
        let devices = Arc::new(FakeDevices::default());
        let (drop_tx, entered) = std_mpsc::channel();
        let (release_tx, release) = std_mpsc::channel();
        let mut release_guard = ReceiptRelease(Some(release_tx));
        let (tx, events) = std_mpsc::channel();
        let sink: PeerEventSink = Arc::new(move |event| {
            let _ = tx.send(event);
        });
        let mut peer = start_with_audio(
            "media_test".into(),
            sink,
            Box::new(HeldPanicAudio {
                devices: devices.clone(),
                hold: (drop_tx, release),
            }),
        )
        .expect("peer starts");
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("unwind held in guard drop");
        let before_release = events.recv_timeout(THREAD_JOIN_TIMEOUT + Duration::from_secs(1));
        let late = release_and_drain(&mut release_guard, &devices.released, &mut peer, &events);
        assert!(devices.released.load(Ordering::SeqCst));
        assert_eq!(
            before_release,
            Ok(PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: false,
            }),
            "backend unwind blocked in guard drop must wake the reaper"
        );
        assert!(late.is_empty(), "no late teardown after timeout: {late:?}");
    }

    #[test]
    fn receipt_370_locked_seal_skips_backend_admission() {
        struct ForbiddenAudio;
        impl AudioBackend for ForbiddenAudio {
            fn open(self: Box<Self>, _io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
                panic!("sealed opener must not run");
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let acquisition = Arc::new(Mutex::new(Acquisition {
            sealed: true,
            ..Default::default()
        }));
        let (reap, _reap_rx) = std_mpsc::channel();
        let emitter = Emitter {
            session_id: "media_test".into(),
            sink: Arc::new(|_| panic!("sealed opener emits no normal event")),
            closed: Arc::new(AtomicBool::new(true)),
            reap,
        };
        let (_commands, command_rx) = mpsc::unbounded_channel();
        let (_cancel, cancel_rx) = watch::channel(true);
        let teardown_ok = AtomicBool::new(true);
        assert_eq!(
            runtime.block_on(session(
                &emitter,
                Box::new(ForbiddenAudio),
                command_rx,
                cancel_rx,
                acquisition.clone(),
                &teardown_ok,
            )),
            Ok(None)
        );
        assert!(!acquisition.lock().unwrap().may_have_acquired());
        assert!(teardown_ok.load(Ordering::SeqCst));
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
        // Close suppresses late negotiation/Closed, but the joined peer must report completion.
        assert_eq!(
            receive_teardown(&events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: true,
            }
        );
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn close_during_a_slow_device_open_returns_at_once_and_starts_no_media() {
        struct SlowAudio(Arc<FakeDevices>);
        impl AudioBackend for SlowAudio {
            fn open(self: Box<Self>, io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
                std::thread::sleep(Duration::from_millis(300));
                self.0.opened.store(true, Ordering::SeqCst);
                (io.acquired)();
                Ok(Box::new(FakeGuard(Arc::clone(&self.0))))
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
        assert_eq!(
            receive_teardown(&events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: true,
                success: true,
            }
        );
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
        assert_eq!(
            receive_teardown(&events),
            PeerEvent::Teardown {
                session_id: "media_test".into(),
                acquired: devices.opened.load(Ordering::SeqCst),
                success: true,
            }
        );
        assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
    }

    /// Answer the peer's offer with a real in-process WebRTC peer, as the Realtime server
    /// would, and return that remote connection and its events channel once both sides
    /// report connected.
    async fn connect_remote(
        peer: &mut NativePeer,
        offer: String,
        events: &std_mpsc::Receiver<PeerEvent>,
    ) -> (
        Arc<RTCPeerConnection>,
        Arc<webrtc::data_channel::RTCDataChannel>,
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
            let _ = channel_tx.send(channel);
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
        let channel = tokio::time::timeout(Duration::from_secs(5), channel_rx.recv())
            .await
            .expect("remote events channel")
            .expect("remote events channel");
        assert_eq!(channel.label(), EVENTS_CHANNEL_LABEL);
        (remote, channel)
    }

    #[test]
    fn remote_closing_the_events_channel_ends_the_call_and_releases_the_devices() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let (mut peer, devices, events) = start_fake();
        let offer = wait_offer(&events);
        runtime.block_on(async {
            let (remote, channel) = connect_remote(&mut peer, offer, &events).await;
            assert!(!devices.released.load(Ordering::SeqCst));

            // Only the channel goes away; ICE and DTLS stay up on the remote side.
            channel.close().await.expect("close the events channel");
            match events.recv_timeout(Duration::from_secs(10)) {
                Ok(PeerEvent::Closed { code, .. }) => assert_eq!(code, close_code::CLOSED),
                other => panic!("expected Closed after the channel closed, got {other:?}"),
            }
            assert!(wait_until(&devices.released, Duration::from_secs(2)));
            assert!(wait_until(&peer.stopped, Duration::from_secs(3)));
            assert_eq!(
                receive_teardown(&events),
                PeerEvent::Teardown {
                    session_id: "media_test".into(),
                    acquired: true,
                    success: true,
                }
            );
            let _ = remote.close().await;
        });
        peer.close();
    }

    #[test]
    fn native_peer_confirms_mute_before_connect() {
        let (mut peer, _devices, events) = start_fake();
        wait_offer(&events);
        peer.set_muted(true);
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
            fn open(self: Box<Self>, _io: AudioIo) -> Result<Box<dyn std::any::Any>, String> {
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
