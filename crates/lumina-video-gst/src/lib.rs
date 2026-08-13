//! Linux GStreamer media-session adapter.
//!
//! [`GstMediaSession`] keeps all GStreamer work on one worker.  The public
//! seam is a bounded, nonblocking command/event mailbox and owned CPU frame
//! leases; no GStreamer object, decoder, or second presentation clock crosses
//! it.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{self, Receiver, Sender, TryRecvError, TrySendError};
use lumina_video_core::audio::AudioHandle;
pub use lumina_video_core::session::AudioObservation;
use lumina_video_core::session::{
    CapabilityTier, MediaSession, SessionCommand, SessionError, SessionEvent, SessionMetadata,
    SessionSnapshot, SessionState,
};
pub use lumina_video_native_frame::linux_video_gst::GstAudioSinkMode;
use lumina_video_native_frame::linux_video_gst::{GStreamerDecoder, DEFAULT_LIFECYCLE_TIMEOUT};
use lumina_video_native_frame::video::{
    CpuFrame, DecodedFrame, VideoDecoderBackend, VideoError, VideoFrame,
};
use lumina_video_native_frame::{
    into_cpu_planes, AcquireSync, CpuMemory, FrameExtent, NativeFrameDescriptor, NativeFrameLease,
    NativeMemory,
};
use parking_lot::RwLock;
use url::Url;

/// The session's decode-to-presentation mailbox holds only the newest frame.
pub const FRAME_QUEUE_CAPACITY: usize = 1;
const CONTROL_QUEUE_CAPACITY: usize = 32;
const COMMAND_QUEUE_CAPACITY: usize = 32;

pub type Frame = NativeFrameLease;
type Event = SessionEvent<Frame>;
pub type SessionEventFrame = SessionEvent<Frame>;

#[derive(Debug)]
struct SequencedEvent {
    sequence: u64,
    event: Event,
}

/// The presentation result a GPUI tick can make from one session poll.
#[derive(Debug)]
pub enum PresentationDecision {
    /// A newer owned frame is ready for upload.
    Advanced(Frame),
    /// No newer frame was available; retain the existing texture.
    Hold,
    /// The session has not produced a frame yet.
    Empty,
}

impl PresentationDecision {
    /// Turns one event poll into the explicit presentation decision used by
    /// the GPUI adapter. `has_presented` distinguishes initial emptiness from
    /// a later hold, while non-frame events never replace the current texture.
    pub fn from_event(event: Option<SessionEventFrame>, has_presented: bool) -> Self {
        match event {
            Some(SessionEvent::Frame { frame, .. }) => Self::Advanced(frame),
            Some(_) | None if has_presented => Self::Hold,
            Some(_) | None => Self::Empty,
        }
    }
}

#[derive(Debug)]
struct SnapshotState {
    snapshot: RwLock<SessionSnapshot>,
    position_us: AtomicU64,
    audio_connected: AtomicBool,
    audio_buffers_seen: AtomicU64,
}

impl SnapshotState {
    fn new() -> Self {
        Self {
            snapshot: RwLock::new(SessionSnapshot::new(CapabilityTier::SystemMemoryUpload)),
            position_us: AtomicU64::new(0),
            audio_connected: AtomicBool::new(false),
            audio_buffers_seen: AtomicU64::new(0),
        }
    }
}

fn state_position(state: &SessionState) -> Option<Duration> {
    match state {
        SessionState::Playing { position }
        | SessionState::Paused { position }
        | SessionState::Buffering { position } => Some(*position),
        _ => None,
    }
}

fn update_audio_observation(
    state: &Arc<SnapshotState>,
    audio_handle: &AudioHandle,
    decoder: &GStreamerDecoder,
    position: Duration,
) {
    let native_audio = decoder.audio_handle();
    let connected = native_audio.has_audio();
    let buffers_seen = native_audio.audio_buffers_seen();
    state.audio_connected.store(connected, Ordering::Relaxed);
    state
        .audio_buffers_seen
        .store(buffers_seen, Ordering::Relaxed);
    audio_handle.set_available(connected);
    audio_handle.set_native_position(position);
}

fn sync_audio_controls(
    audio_handle: &AudioHandle,
    decoder: &mut GStreamerDecoder,
    last_applied: &mut Option<(bool, u32)>,
) -> Result<(), VideoError> {
    let muted = audio_handle.is_muted();
    let volume = audio_handle.volume();
    let previous = *last_applied;
    if previous.map(|values| values.0) != Some(muted) {
        decoder.set_muted(muted)?;
    }
    if previous.map(|values| values.1) != Some(volume) {
        decoder.set_volume(volume as f32 / 100.0)?;
    }
    *last_applied = Some((muted, volume));
    Ok(())
}

fn local_source_url(source: &str) -> Result<String, SessionError> {
    if source.contains("://") {
        return Ok(source.to_string());
    }

    let path = std::fs::canonicalize(source)
        .map_err(|error| SessionError::Open(format!("{source}: {error}")))?;
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| SessionError::Open("source path cannot be represented as a file URL".into()))
}

fn session_metadata(metadata: &lumina_video_native_frame::VideoMetadata) -> SessionMetadata {
    SessionMetadata {
        width: metadata.width,
        height: metadata.height,
        duration: metadata.duration,
        frame_rate: metadata.frame_rate,
        codec: metadata.codec.clone(),
        pixel_aspect_ratio: metadata.pixel_aspect_ratio,
        start_time: metadata.start_time,
    }
}

fn session_error(error: VideoError) -> SessionError {
    match error {
        VideoError::OpenFailed(message) | VideoError::DecoderInit(message) => {
            SessionError::Open(message)
        }
        VideoError::DecodeFailed(message) => SessionError::Decode(message),
        VideoError::SeekFailed(message) => SessionError::Seek(message),
        VideoError::Network(message) => SessionError::Network(message),
        VideoError::UnsupportedFormat(message) => SessionError::Unsupported(message),
        VideoError::Generic(message) => SessionError::Fatal(message),
    }
}

fn owned_cpu_lease(
    frame: VideoFrame,
    frame_id: u64,
    stream_generation: u64,
    duration: Option<Duration>,
) -> Result<Frame, SessionError> {
    let VideoFrame { pts, frame } = frame;
    let DecodedFrame::Cpu(CpuFrame {
        format,
        width,
        height,
        planes,
    }) = frame
    else {
        return Err(SessionError::Unsupported(
            "GStreamer session only accepts owned system-memory frames".into(),
        ));
    };

    let descriptor = NativeFrameDescriptor {
        frame_id,
        stream_generation,
        pts,
        duration,
        extent: FrameExtent::new(width, height),
        format,
    };
    NativeFrameLease::new(
        descriptor,
        NativeMemory::Cpu(CpuMemory::new(into_cpu_planes(planes))),
        AcquireSync::None,
    )
    .map_err(|error| SessionError::Decode(error.to_string()))
}

fn send_control(sender: &Sender<SequencedEvent>, event: SequencedEvent) -> bool {
    sender.send(event).is_ok()
}

fn send_frame(
    sender: &Sender<SequencedEvent>,
    drop_receiver: &Receiver<SequencedEvent>,
    event: SequencedEvent,
    dropped_frames: &AtomicU64,
) -> bool {
    let mut event = event;
    loop {
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(TrySendError::Full(next)) => {
                event = next;
                match drop_receiver.try_recv() {
                    Ok(old) if matches!(old.event, SessionEvent::Frame { .. }) => {
                        dropped_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) => return false,
                    Err(TryRecvError::Empty) => thread::yield_now(),
                    Err(TryRecvError::Disconnected) => return false,
                }
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn publish_state(
    state: &Arc<SnapshotState>,
    control_sender: &Sender<SequencedEvent>,
    sequence: &mut u64,
    next_state: SessionState,
) -> bool {
    if let Some(position) = state_position(&next_state) {
        state
            .position_us
            .store(position.as_micros() as u64, Ordering::Relaxed);
    }
    state.snapshot.write().state = next_state.clone();
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::StateChanged { state: next_state },
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

fn publish_error(
    state: &Arc<SnapshotState>,
    control_sender: &Sender<SequencedEvent>,
    sequence: &mut u64,
    error: SessionError,
) -> bool {
    let error_event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::Error(error.clone()),
    };
    *sequence = sequence.saturating_add(1);
    if !send_control(control_sender, error_event) {
        return false;
    }
    publish_state(state, control_sender, sequence, SessionState::Error(error))
}

struct PlaybackState {
    playing: bool,
    position: Duration,
    stream_generation: u64,
}

fn mark_eos(playback: &mut PlaybackState) {
    playback.playing = false;
}

fn process_command(
    command: SessionCommand,
    decoder: &mut GStreamerDecoder,
    playback: &mut PlaybackState,
    audio_handle: &AudioHandle,
    state: &Arc<SnapshotState>,
    control_sender: &Sender<SequencedEvent>,
    sequence: &mut u64,
) -> bool {
    match command {
        SessionCommand::Play => {
            if decoder.is_eof() {
                if let Err(error) = decoder.seek(Duration::ZERO) {
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    return false;
                }
                playback.position = Duration::ZERO;
                playback.stream_generation = playback.stream_generation.saturating_add(1);
            }
            match decoder.resume() {
                Ok(()) => {
                    playback.playing = true;
                    audio_handle.start_playback_epoch();
                    publish_state(
                        state,
                        control_sender,
                        sequence,
                        SessionState::Playing {
                            position: playback.position,
                        },
                    )
                }
                Err(error) => {
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    false
                }
            }
        }
        SessionCommand::Pause => match decoder.pause() {
            Ok(()) => {
                playback.playing = false;
                publish_state(
                    state,
                    control_sender,
                    sequence,
                    SessionState::Paused {
                        position: playback.position,
                    },
                )
            }
            Err(error) => {
                let _ = publish_error(state, control_sender, sequence, session_error(error));
                false
            }
        },
        SessionCommand::Stop => {
            let _ = decoder.pause();
            let published = publish_state(state, control_sender, sequence, SessionState::Ended);
            if published {
                let _ = send_control(
                    control_sender,
                    SequencedEvent {
                        sequence: *sequence,
                        event: SessionEvent::Ended,
                    },
                );
            }
            decoder.shutdown();
            false
        }
        SessionCommand::Seek { position: target } => match decoder.seek(target) {
            Ok(()) => {
                playback.stream_generation = playback.stream_generation.saturating_add(1);
                playback.position = target;
                audio_handle.set_native_position(target);
                let next_state = if playback.playing {
                    SessionState::Playing {
                        position: playback.position,
                    }
                } else {
                    SessionState::Paused {
                        position: playback.position,
                    }
                };
                publish_state(state, control_sender, sequence, next_state)
            }
            Err(error) => {
                let _ = publish_error(state, control_sender, sequence, session_error(error));
                false
            }
        },
        SessionCommand::SetMuted { muted } => {
            audio_handle.set_muted(muted);
            true
        }
        SessionCommand::SetVolume { volume } => {
            let volume = volume.clamp(0.0, 1.0);
            audio_handle.set_volume((volume * 100.0) as u32);
            true
        }
        SessionCommand::Renegotiate { .. } => {
            let _ = publish_error(
                state,
                control_sender,
                sequence,
                SessionError::Unsupported(
                    "GStreamer session is fixed to SystemMemoryUpload".into(),
                ),
            );
            false
        }
    }
}

struct WorkerIo {
    commands: Receiver<SessionCommand>,
    control_sender: Sender<SequencedEvent>,
    frame_sender: Sender<SequencedEvent>,
    frame_drop_receiver: Receiver<SequencedEvent>,
    state: Arc<SnapshotState>,
    dropped_frames: Arc<AtomicU64>,
    audio_handle: AudioHandle,
    audio_sink: GstAudioSinkMode,
}

fn run_worker(
    source: String,
    autoplay: bool,
    lifecycle_timeout: Duration,
    initial_stream_generation: u64,
    io: WorkerIo,
) {
    let WorkerIo {
        commands,
        control_sender,
        frame_sender,
        frame_drop_receiver,
        state,
        dropped_frames,
        audio_handle,
        audio_sink,
    } = io;
    let mut sequence = 0_u64;
    let source = match local_source_url(&source) {
        Ok(source) => source,
        Err(error) => {
            let _ = publish_error(&state, &control_sender, &mut sequence, error);
            return;
        }
    };
    let mut decoder = match GStreamerDecoder::new_system_memory_with_audio_sink_and_timeout(
        &source,
        audio_sink,
        lifecycle_timeout,
    ) {
        Ok(decoder) => decoder,
        Err(error) => {
            let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
            return;
        }
    };

    let mut last_applied_audio = None;
    if let Err(error) = sync_audio_controls(&audio_handle, &mut decoder, &mut last_applied_audio) {
        let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
        return;
    }
    update_audio_observation(&state, &audio_handle, &decoder, Duration::ZERO);

    let metadata = session_metadata(decoder.metadata());
    state.snapshot.write().metadata = Some(metadata.clone());
    if !send_control(
        &control_sender,
        SequencedEvent {
            sequence,
            event: SessionEvent::Metadata { metadata },
        },
    ) {
        return;
    }
    sequence = sequence.saturating_add(1);
    if !publish_state(&state, &control_sender, &mut sequence, SessionState::Ready) {
        return;
    }

    let mut playback = PlaybackState {
        playing: false,
        position: Duration::ZERO,
        stream_generation: initial_stream_generation,
    };
    let mut frame_id = 0_u64;

    if autoplay {
        match decoder.resume() {
            Ok(()) => {
                playback.playing = true;
                audio_handle.start_playback_epoch();
                if !publish_state(
                    &state,
                    &control_sender,
                    &mut sequence,
                    SessionState::Playing {
                        position: playback.position,
                    },
                ) {
                    return;
                }
            }
            Err(error) => {
                let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
                return;
            }
        }
    }

    loop {
        while let Ok(command) = commands.try_recv() {
            if !process_command(
                command,
                &mut decoder,
                &mut playback,
                &audio_handle,
                &state,
                &control_sender,
                &mut sequence,
            ) {
                return;
            }
        }

        if let Err(error) =
            sync_audio_controls(&audio_handle, &mut decoder, &mut last_applied_audio)
        {
            let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
            return;
        }
        update_audio_observation(&state, &audio_handle, &decoder, playback.position);

        if !playback.playing {
            match commands.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => {
                    if !process_command(
                        command,
                        &mut decoder,
                        &mut playback,
                        &audio_handle,
                        &state,
                        &control_sender,
                        &mut sequence,
                    ) {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            }
            continue;
        }

        match decoder.decode_next() {
            Ok(Some(frame)) => {
                playback.position = frame.pts;
                state
                    .position_us
                    .store(playback.position.as_micros() as u64, Ordering::Relaxed);
                update_audio_observation(&state, &audio_handle, &decoder, playback.position);
                let lease = match owned_cpu_lease(
                    frame,
                    frame_id,
                    playback.stream_generation,
                    Some(decoder.metadata().frame_duration()),
                ) {
                    Ok(lease) => lease,
                    Err(error) => {
                        let _ = publish_error(&state, &control_sender, &mut sequence, error);
                        return;
                    }
                };
                frame_id = frame_id.saturating_add(1);
                if !send_frame(
                    &frame_sender,
                    &frame_drop_receiver,
                    SequencedEvent {
                        sequence,
                        event: SessionEvent::Frame {
                            pts: lease.descriptor.pts,
                            frame: lease,
                        },
                    },
                    &dropped_frames,
                ) {
                    return;
                }
                sequence = sequence.saturating_add(1);
            }
            Ok(None) if decoder.is_eof() => {
                mark_eos(&mut playback);
                if !publish_state(&state, &control_sender, &mut sequence, SessionState::Ended) {
                    return;
                }
                if !send_control(
                    &control_sender,
                    SequencedEvent {
                        sequence,
                        event: SessionEvent::Ended,
                    },
                ) {
                    return;
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
                return;
            }
        }
    }
}

/// A Linux GStreamer media session with bounded, nonblocking UI interaction.
pub struct GstMediaSession {
    commands: Sender<SessionCommand>,
    control_receiver: Receiver<SequencedEvent>,
    frame_receiver: Receiver<SequencedEvent>,
    state: Arc<SnapshotState>,
    audio_handle: AudioHandle,
    dropped_frames: Arc<AtomicU64>,
    pending_control: Option<SequencedEvent>,
    pending_frame: Option<SequencedEvent>,
    has_presented_frame: bool,
    worker: Option<JoinHandle<()>>,
    worker_disconnected: bool,
    lifecycle_timeout: Duration,
    stream_generation: u64,
    replay_pending: bool,
}

impl GstMediaSession {
    /// Starts opening `source` on a background worker. Playback starts paused.
    pub fn new(source: impl Into<String>) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            false,
            GstAudioSinkMode::Auto,
            DEFAULT_LIFECYCLE_TIMEOUT,
            0,
        )
    }

    /// Starts opening `source` on a background worker and optionally autoplays
    /// after GStreamer reaches its preroll-ready state.
    pub fn new_with_autoplay(source: impl Into<String>, autoplay: bool) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            autoplay,
            GstAudioSinkMode::Auto,
            DEFAULT_LIFECYCLE_TIMEOUT,
            0,
        )
    }

    /// Starts a session with an explicit lifecycle timeout.
    pub fn new_with_lifecycle_timeout(
        source: impl Into<String>,
        lifecycle_timeout: Duration,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            false,
            GstAudioSinkMode::Auto,
            lifecycle_timeout,
            0,
        )
    }

    /// Alias for [`Self::new_with_lifecycle_timeout`].
    pub fn new_with_timeout(source: impl Into<String>, lifecycle_timeout: Duration) -> Self {
        Self::new_with_lifecycle_timeout(source, lifecycle_timeout)
    }

    /// Starts a session with autoplay and an explicit lifecycle timeout.
    pub fn new_with_autoplay_and_lifecycle_timeout(
        source: impl Into<String>,
        autoplay: bool,
        lifecycle_timeout: Duration,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            autoplay,
            GstAudioSinkMode::Auto,
            lifecycle_timeout,
            0,
        )
    }

    /// Starts a session with an explicit GStreamer audio sink policy.
    pub fn new_with_autoplay_and_audio_sink(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            autoplay,
            audio_sink,
            DEFAULT_LIFECYCLE_TIMEOUT,
            0,
        )
    }

    /// Starts a session with explicit autoplay, sink, and lifecycle timeout.
    pub fn new_with_autoplay_and_audio_sink_and_timeout(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            0,
        )
    }

    /// Starts a session with explicit autoplay, sink, lifecycle, and stream
    /// generation policies.
    pub fn new_with_autoplay_and_audio_sink_and_timeout_and_generation(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        stream_generation: u64,
    ) -> Self {
        let source = source.into();
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = crossbeam_channel::bounded(CONTROL_QUEUE_CAPACITY);
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let state = Arc::new(SnapshotState::new());
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let audio_handle = AudioHandle::new();
        let worker_state = Arc::clone(&state);
        let worker_dropped_frames = Arc::clone(&dropped_frames);
        let worker_audio_handle = audio_handle.clone();
        let worker = thread::Builder::new()
            .name("lumina-gst-session".into())
            .spawn(move || {
                run_worker(
                    source,
                    autoplay,
                    lifecycle_timeout,
                    stream_generation,
                    WorkerIo {
                        commands: command_receiver,
                        control_sender,
                        frame_sender,
                        frame_drop_receiver,
                        state: worker_state,
                        dropped_frames: worker_dropped_frames,
                        audio_handle: worker_audio_handle,
                        audio_sink,
                    },
                )
            })
            .ok();
        let worker_disconnected = worker.is_none();

        Self {
            commands,
            control_receiver,
            frame_receiver,
            state,
            audio_handle,
            dropped_frames,
            pending_control: None,
            pending_frame: None,
            has_presented_frame: false,
            worker,
            worker_disconnected,
            lifecycle_timeout,
            stream_generation,
            replay_pending: false,
        }
    }

    /// Number of decoded frames discarded because the bounded frame mailbox
    /// already held a newer frame.
    pub fn dropped_frame_count(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// GStreamer owns the decoded audio sink and its presentation timing.
    pub const fn handles_audio_internally() -> bool {
        true
    }

    /// Returns the shared core audio control and position proxy.
    pub fn audio_handle(&self) -> &AudioHandle {
        &self.audio_handle
    }

    /// Returns the deadline used by worker-side seek and teardown work.
    pub fn lifecycle_timeout(&self) -> Duration {
        self.lifecycle_timeout
    }

    /// Returns the current frame stream generation.
    pub fn stream_generation(&self) -> u64 {
        self.stream_generation
    }

    /// Returns framework-neutral observations from the GStreamer audio branch.
    pub fn audio_observation(&self) -> AudioObservation {
        AudioObservation {
            connected: self.state.audio_connected.load(Ordering::Relaxed),
            buffers_seen: self.state.audio_buffers_seen.load(Ordering::Relaxed),
        }
    }

    /// Polls one event and maps it to the GPUI presentation decision.
    pub fn try_next_presentation(&mut self) -> Result<PresentationDecision, SessionError> {
        let decision =
            PresentationDecision::from_event(self.try_next_event()?, self.has_presented_frame);
        if matches!(decision, PresentationDecision::Advanced(_)) {
            self.has_presented_frame = true;
        }
        Ok(decision)
    }

    fn fill_pending(&mut self) {
        if self.pending_control.is_none() {
            match self.control_receiver.try_recv() {
                Ok(event) => self.pending_control = Some(event),
                Err(TryRecvError::Disconnected) => self.worker_disconnected = true,
                Err(TryRecvError::Empty) => {}
            }
        }
        loop {
            match self.frame_receiver.try_recv() {
                Ok(event) => {
                    let generation = match &event.event {
                        SessionEvent::Frame { frame, .. } => frame.descriptor.stream_generation,
                        _ => self.stream_generation,
                    };
                    if generation < self.stream_generation {
                        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if generation > self.stream_generation {
                        self.stream_generation = generation;
                    }
                    self.replay_pending = false;
                    if self.pending_frame.replace(event).is_some() {
                        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.worker_disconnected = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if self.pending_frame.as_ref().is_some_and(|event| {
            matches!(
                &event.event,
                SessionEvent::Frame { frame, .. }
                    if frame.descriptor.stream_generation < self.stream_generation
            )
        }) {
            self.pending_frame = None;
        }
    }
}

impl MediaSession for GstMediaSession {
    type Frame = Frame;

    fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = self.state.snapshot.read().clone();
        let position = Duration::from_micros(self.state.position_us.load(Ordering::Relaxed));
        snapshot.state = match snapshot.state {
            SessionState::Playing { .. } => SessionState::Playing { position },
            SessionState::Paused { .. } => SessionState::Paused { position },
            SessionState::Buffering { .. } => SessionState::Buffering { position },
            state => state,
        };
        snapshot
    }

    fn command(&mut self, command: SessionCommand) -> Result<(), SessionError> {
        if let SessionCommand::Renegotiate { tier } = command {
            if tier != CapabilityTier::SystemMemoryUpload {
                return Err(SessionError::Unsupported(
                    "Linux GStreamer session only supports SystemMemoryUpload".into(),
                ));
            }
            return Ok(());
        }
        let is_seek = matches!(&command, SessionCommand::Seek { .. });
        let is_play = matches!(&command, SessionCommand::Play);
        let was_ended = matches!(self.snapshot().state, SessionState::Ended);
        let is_replay = is_play && was_ended && !self.replay_pending;
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => SessionError::InvalidCommand("command queue full".into()),
                TrySendError::Disconnected(_) => {
                    SessionError::Fatal("session worker stopped".into())
                }
            })?;
        if is_seek {
            self.stream_generation = self.stream_generation.saturating_add(1);
            self.replay_pending = was_ended;
        } else if is_replay {
            self.stream_generation = self.stream_generation.saturating_add(1);
            self.replay_pending = true;
        } else if is_play {
            self.replay_pending = false;
        }
        Ok(())
    }

    fn try_next_event(&mut self) -> Result<Option<Event>, SessionError> {
        self.fill_pending();
        let next = match (&self.pending_control, &self.pending_frame) {
            (None, None) => None,
            (Some(_), None) => self.pending_control.take(),
            (None, Some(_)) => self.pending_frame.take(),
            (Some(control), Some(frame)) if control.sequence <= frame.sequence => {
                self.pending_control.take()
            }
            (Some(_), Some(_)) => self.pending_frame.take(),
        };
        if let Some(event) = next {
            return Ok(Some(event.event));
        }
        if self.worker_disconnected {
            return Ok(None);
        }
        Ok(None)
    }
}

impl Drop for GstMediaSession {
    fn drop(&mut self) {
        // The worker owns all potentially blocking GStreamer calls. A UI drop
        // only makes one nonblocking stop attempt and detaches the worker;
        // dropping the receivers lets it exit when its current call returns.
        let _ = self.commands.try_send(SessionCommand::Stop);
        self.worker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumina_video_native_frame::CpuPlane;

    fn test_frame_with_generation(frame_id: u64, stream_generation: u64) -> Frame {
        match NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id,
                stream_generation,
                pts: Duration::from_millis(frame_id),
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: lumina_video_native_frame::video::PixelFormat::Rgba,
            },
            NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(vec![0, 0, 0, 255], 4)])),
            AcquireSync::None,
        ) {
            Ok(frame) => frame,
            Err(error) => panic!("test frame must be valid: {error}"),
        }
    }

    fn test_frame(frame_id: u64) -> Frame {
        test_frame_with_generation(frame_id, 0)
    }

    #[test]
    fn presentation_decision_keeps_non_frames_on_hold() {
        let decision = PresentationDecision::from_event(Some(SessionEvent::Ended), true);
        assert!(matches!(decision, PresentationDecision::Hold));
        assert!(matches!(
            PresentationDecision::from_event(None, false),
            PresentationDecision::Empty
        ));
        assert!(matches!(
            PresentationDecision::from_event(None, true),
            PresentationDecision::Hold
        ));
    }

    #[test]
    fn frame_mailbox_drops_oldest_frame_only() {
        let (sender, receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let drop_receiver = receiver.clone();
        let dropped = AtomicU64::new(0);

        assert!(send_frame(
            &sender,
            &drop_receiver,
            SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::ZERO,
                    frame: test_frame(1),
                },
            },
            &dropped,
        ));
        assert!(send_frame(
            &sender,
            &drop_receiver,
            SequencedEvent {
                sequence: 2,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame(2),
                },
            },
            &dropped,
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let event = receiver.try_recv();
        assert!(matches!(
            event,
            Ok(SequencedEvent {
                sequence: 2,
                event: SessionEvent::Frame { .. }
            })
        ));
    }

    #[test]
    fn pending_frame_refresh_keeps_latest_after_control_event() {
        let (commands, _command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = crossbeam_channel::bounded(CONTROL_QUEUE_CAPACITY);
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let state = Arc::new(SnapshotState::new());
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let audio_handle = AudioHandle::new();
        let mut session = GstMediaSession {
            commands,
            control_receiver,
            frame_receiver,
            state,
            audio_handle,
            dropped_frames: Arc::clone(&dropped_frames),
            pending_control: None,
            pending_frame: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            stream_generation: 0,
            replay_pending: false,
        };

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame(1),
                },
            })
            .is_ok());
        session.fill_pending();

        assert!(control_sender
            .send(SequencedEvent {
                sequence: 0,
                event: SessionEvent::StateChanged {
                    state: SessionState::Ready,
                },
            })
            .is_ok());
        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 2,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(2),
                    frame: test_frame(2),
                },
            })
            .is_ok());

        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::StateChanged {
                state: SessionState::Ready
            }))
        ));
        let next = session.try_next_event();
        assert!(matches!(
            next,
            Ok(Some(SessionEvent::Frame { frame, .. })) if frame.descriptor.frame_id == 2
        ));
        assert_eq!(dropped_frames.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn eos_marks_playback_idle_for_restart() {
        let mut playback = PlaybackState {
            playing: true,
            position: Duration::from_secs(1),
            stream_generation: 0,
        };
        mark_eos(&mut playback);
        assert!(!playback.playing);
        assert_eq!(playback.position, Duration::from_secs(1));
    }

    #[test]
    fn stale_frames_are_dropped_after_generation_advance() {
        let (commands, _command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (_control_sender, control_receiver) =
            crossbeam_channel::bounded(CONTROL_QUEUE_CAPACITY);
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            control_receiver,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            pending_control: None,
            pending_frame: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            stream_generation: 1,
            replay_pending: false,
        };

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::ZERO,
                    frame: test_frame_with_generation(1, 0),
                },
            })
            .is_ok());
        assert!(matches!(session.try_next_event(), Ok(None)));
        assert_eq!(session.dropped_frame_count(), 1);

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 2,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame_with_generation(2, 1),
                },
            })
            .is_ok());
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Frame { frame, .. }))
                if frame.descriptor.stream_generation == 1
        ));
    }

    #[test]
    fn queued_seek_advances_generation_without_waiting_for_worker() {
        let (commands, _command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (_control_sender, control_receiver) =
            crossbeam_channel::bounded(CONTROL_QUEUE_CAPACITY);
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            control_receiver,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            pending_control: None,
            pending_frame: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: Duration::from_millis(50),
            stream_generation: 4,
            replay_pending: false,
        };

        assert!(session
            .command(SessionCommand::Seek {
                position: Duration::from_secs(1),
            })
            .is_ok());
        assert_eq!(session.stream_generation(), 5);
    }
}
