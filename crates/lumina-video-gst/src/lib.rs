//! Linux GStreamer media-session adapter.
//!
//! [`GstMediaSession`] keeps all GStreamer work on one worker.  The public
//! seam is a bounded, nonblocking command/event mailbox and owned CPU frame
//! leases; no GStreamer object, decoder, or second presentation clock crosses
//! it.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{self, Receiver, Sender, TryRecvError, TrySendError};
use lumina_video_core::audio::AudioHandle;
pub use lumina_video_core::session::{AudioObservation, AudioTrack};
use lumina_video_core::session::{
    CapabilityDowngradeReason, CapabilityTier, ConversionMode, DecodeMode, DecodeResidency,
    FrameRealization, ImportMode, MediaSession, RendererOutcome, SessionCommand, SessionError,
    SessionEvent, SessionMetadata, SessionSnapshot, SessionState, SynchronizationMode,
};
use lumina_video_native_frame::linux_video_gst::{
    AudioTrackSelectionResult, GStreamerDecoder, GstLifecycleControl, GstPipelineObservation,
    DEFAULT_LIFECYCLE_TIMEOUT,
};
pub use lumina_video_native_frame::linux_video_gst::{GstAudioSinkMode, DEFAULT_OPEN_TIMEOUT};
use lumina_video_native_frame::video::{PixelFormat, VideoDecoderBackend, VideoError};
use lumina_video_native_frame::{
    nv12_to_rgba_into, AcquireSync, ColorMetadata, ColorRenderDecision, CpuMemory, CpuPlane,
    FrameExtent, NativeFrameDescriptor, NativeFrameLease, NativeMemory,
};
use parking_lot::RwLock;
use url::Url;

/// The session's decode-to-presentation mailbox holds only the newest frame.
pub const FRAME_QUEUE_CAPACITY: usize = 1;
const COMMAND_QUEUE_CAPACITY: usize = 32;

pub type Frame = NativeFrameLease;
type Event = SessionEvent<Frame>;
pub type SessionEventFrame = SessionEvent<Frame>;

/// GStreamer-owned timing and live-edge facts observed by a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GstObservation {
    pub is_live: bool,
    pub is_live_known: bool,
    pub seekable: bool,
    pub seekability_known: bool,
    pub pipeline_latency: Option<Duration>,
    pub frame_mailbox_occupancy: usize,
    pub dropped_frames: u64,
    pub qos_events: u64,
}

#[derive(Debug)]
struct SequencedEvent {
    sequence: u64,
    event: Event,
}

#[derive(Debug)]
struct GenerationIntent {
    command: SessionCommand,
    target_generation: u64,
}

enum WorkerCommand {
    Ordinary(SessionCommand),
    Generation(GenerationIntent),
}

const CONTROL_LANE_CAPACITY: usize = 1;

/// A bounded latest-value lane for one class of worker-to-session events.
///
/// The sender-side receiver clone drops the old value when the lane is full,
/// so a worker never blocks and the newest event in each class survives an
/// unpolled burst.
#[derive(Clone)]
struct ControlLane {
    sender: Sender<SequencedEvent>,
    drop_receiver: Receiver<SequencedEvent>,
}

impl ControlLane {
    fn new() -> (Self, Receiver<SequencedEvent>) {
        let (sender, receiver) = crossbeam_channel::bounded(CONTROL_LANE_CAPACITY);
        (
            Self {
                sender,
                drop_receiver: receiver.clone(),
            },
            receiver,
        )
    }

    fn send(&self, mut event: SequencedEvent) -> bool {
        loop {
            match self.sender.try_send(event) {
                Ok(()) => return true,
                Err(TrySendError::Full(next)) => {
                    event = next;
                    match self.drop_receiver.try_recv() {
                        Ok(_) => {}
                        Err(TryRecvError::Empty) => thread::yield_now(),
                        Err(TryRecvError::Disconnected) => return false,
                    }
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
    }
}

#[derive(Clone)]
struct ControlSender {
    metadata: ControlLane,
    audio_tracks: ControlLane,
    audio_selection: ControlLane,
    state: ControlLane,
    error: ControlLane,
    terminal: ControlLane,
    transient: Sender<SequencedEvent>,
    transient_drop: Receiver<SequencedEvent>,
}

struct ControlReceiver {
    metadata: Receiver<SequencedEvent>,
    audio_tracks: Receiver<SequencedEvent>,
    audio_selection: Receiver<SequencedEvent>,
    state: Receiver<SequencedEvent>,
    error: Receiver<SequencedEvent>,
    terminal: Receiver<SequencedEvent>,
    transient: Receiver<SequencedEvent>,
}

fn control_channels() -> (ControlSender, ControlReceiver) {
    let (metadata, metadata_receiver) = ControlLane::new();
    let (audio_tracks, audio_tracks_receiver) = ControlLane::new();
    let (audio_selection, audio_selection_receiver) = ControlLane::new();
    let (state, state_receiver) = ControlLane::new();
    let (error, error_receiver) = ControlLane::new();
    let (terminal, terminal_receiver) = ControlLane::new();
    let (transient, transient_receiver) = crossbeam_channel::bounded(1);
    (
        ControlSender {
            metadata,
            audio_tracks,
            audio_selection,
            state,
            error,
            terminal,
            transient,
            transient_drop: transient_receiver.clone(),
        },
        ControlReceiver {
            metadata: metadata_receiver,
            audio_tracks: audio_tracks_receiver,
            audio_selection: audio_selection_receiver,
            state: state_receiver,
            error: error_receiver,
            terminal: terminal_receiver,
            transient: transient_receiver,
        },
    )
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
    capability: AtomicU8,
    frame_realization: AtomicU64,
    decode_mode: AtomicU8,
    latest_renderer_outcome: AtomicU8,
    latest_downgrade_reason: AtomicU8,
    position_us: AtomicU64,
    audio_connected: AtomicBool,
    audio_buffers_seen: AtomicU64,
    is_live: AtomicBool,
    is_live_known: AtomicBool,
    seekable: AtomicBool,
    seekability_known: AtomicBool,
    pipeline_latency_ns: AtomicU64,
    pipeline_latency_known: AtomicBool,
    frame_mailbox_occupancy: AtomicU64,
    qos_events: AtomicU64,
}

impl SnapshotState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_capability(CapabilityTier::SystemMemoryUpload)
    }

    fn with_capability(capability: CapabilityTier) -> Self {
        Self {
            snapshot: RwLock::new(SessionSnapshot::new(capability)),
            capability: AtomicU8::new(encode_capability(Some(capability))),
            frame_realization: AtomicU64::new(0),
            decode_mode: AtomicU8::new(0),
            latest_renderer_outcome: AtomicU8::new(0),
            latest_downgrade_reason: AtomicU8::new(0),
            position_us: AtomicU64::new(0),
            audio_connected: AtomicBool::new(false),
            audio_buffers_seen: AtomicU64::new(0),
            is_live: AtomicBool::new(false),
            is_live_known: AtomicBool::new(false),
            seekable: AtomicBool::new(false),
            seekability_known: AtomicBool::new(false),
            pipeline_latency_ns: AtomicU64::new(0),
            pipeline_latency_known: AtomicBool::new(false),
            frame_mailbox_occupancy: AtomicU64::new(0),
            qos_events: AtomicU64::new(0),
        }
    }
}

fn publish_capability_if_changed(
    state: &SnapshotState,
    published: &mut CapabilityTier,
    next: CapabilityTier,
) -> bool {
    if *published == next {
        return false;
    }
    state
        .capability
        .store(encode_capability(Some(next)), Ordering::Relaxed);
    *published = next;
    true
}

fn record_downgrade_reason(state: &SnapshotState, reason: CapabilityDowngradeReason) {
    state
        .latest_downgrade_reason
        .store(encode_downgrade_reason(Some(reason)), Ordering::Relaxed);
}

fn record_renderer_outcome(state: &SnapshotState, outcome: RendererOutcome) {
    state
        .latest_renderer_outcome
        .store(encode_renderer_outcome(Some(outcome)), Ordering::Relaxed);
}

fn record_realization(state: &SnapshotState, realization: FrameRealization) {
    state.frame_realization.store(
        encode_frame_realization(Some(realization)),
        Ordering::Relaxed,
    );
}

const fn encode_capability(capability: Option<CapabilityTier>) -> u8 {
    match capability {
        None => 0,
        Some(CapabilityTier::DirectAlias) => 1,
        Some(CapabilityTier::GpuConversion) => 2,
        Some(CapabilityTier::SystemMemoryUpload) => 3,
    }
}

const fn decode_capability(value: u8) -> Option<CapabilityTier> {
    match value {
        1 => Some(CapabilityTier::DirectAlias),
        2 => Some(CapabilityTier::GpuConversion),
        3 => Some(CapabilityTier::SystemMemoryUpload),
        _ => None,
    }
}

const fn encode_frame_realization(realization: Option<FrameRealization>) -> u64 {
    let Some(realization) = realization else {
        return 0;
    };
    let decode = match realization.decode {
        DecodeMode::Hardware => 1_u64,
        DecodeMode::Software => 2_u64,
    };
    let residency = match realization.residency {
        DecodeResidency::NativeGpu => 1_u64,
        DecodeResidency::SystemMemory => 2_u64,
    };
    let import = match realization.import {
        ImportMode::DirectAlias => 1_u64,
        ImportMode::GpuCopy => 2_u64,
        ImportMode::CpuUpload => 3_u64,
    };
    let conversion = match realization.conversion {
        ConversionMode::None => 1_u64,
        ConversionMode::YuvShader => 2_u64,
        ConversionMode::GpuBlit => 3_u64,
    };
    let synchronization = match realization.synchronization {
        SynchronizationMode::None => 1_u64,
        SynchronizationMode::Explicit => 2_u64,
        SynchronizationMode::VerifiedImplicit => 3_u64,
        SynchronizationMode::CpuWait => 4_u64,
    };
    decode | (residency << 3) | (import << 6) | (conversion << 9) | (synchronization << 12)
}

const fn decode_frame_realization(value: u64) -> Option<FrameRealization> {
    if value == 0 || value & !0x7fff != 0 {
        return None;
    }
    let decode = match value & 0x7 {
        1 => DecodeMode::Hardware,
        2 => DecodeMode::Software,
        _ => return None,
    };
    let residency = match (value >> 3) & 0x7 {
        1 => DecodeResidency::NativeGpu,
        2 => DecodeResidency::SystemMemory,
        _ => return None,
    };
    let import = match (value >> 6) & 0x7 {
        1 => ImportMode::DirectAlias,
        2 => ImportMode::GpuCopy,
        3 => ImportMode::CpuUpload,
        _ => return None,
    };
    let conversion = match (value >> 9) & 0x7 {
        1 => ConversionMode::None,
        2 => ConversionMode::YuvShader,
        3 => ConversionMode::GpuBlit,
        _ => return None,
    };
    let synchronization = match (value >> 12) & 0x7 {
        1 => SynchronizationMode::None,
        2 => SynchronizationMode::Explicit,
        3 => SynchronizationMode::VerifiedImplicit,
        4 => SynchronizationMode::CpuWait,
        _ => return None,
    };
    Some(FrameRealization {
        decode,
        residency,
        import,
        conversion,
        synchronization,
    })
}

const fn encode_decode_mode(mode: Option<DecodeMode>) -> u8 {
    match mode {
        None => 0,
        Some(DecodeMode::Hardware) => 1,
        Some(DecodeMode::Software) => 2,
    }
}

const fn decode_decode_mode(value: u8) -> Option<DecodeMode> {
    match value {
        1 => Some(DecodeMode::Hardware),
        2 => Some(DecodeMode::Software),
        _ => None,
    }
}

const fn encode_renderer_outcome(outcome: Option<RendererOutcome>) -> u8 {
    match outcome {
        None => 0,
        Some(RendererOutcome::Accepted) => 1,
        Some(RendererOutcome::Unsupported) => 2,
        Some(RendererOutcome::TransientFailure) => 3,
        Some(RendererOutcome::FatalFailure) => 4,
    }
}

const fn decode_renderer_outcome(value: u8) -> Option<RendererOutcome> {
    match value {
        1 => Some(RendererOutcome::Accepted),
        2 => Some(RendererOutcome::Unsupported),
        3 => Some(RendererOutcome::TransientFailure),
        4 => Some(RendererOutcome::FatalFailure),
        _ => None,
    }
}

const fn encode_downgrade_reason(reason: Option<CapabilityDowngradeReason>) -> u8 {
    match reason {
        None => 0,
        Some(CapabilityDowngradeReason::HardwareOpenFailure) => 1,
        Some(CapabilityDowngradeReason::HardwareDecodeFailure) => 2,
        Some(CapabilityDowngradeReason::HardwareUnavailable) => 3,
        Some(CapabilityDowngradeReason::RendererUnsupported) => 4,
        Some(CapabilityDowngradeReason::RendererTransientFailure) => 5,
        Some(CapabilityDowngradeReason::RendererFatalFailure) => 6,
        Some(CapabilityDowngradeReason::UnsafeSync) => 7,
        Some(CapabilityDowngradeReason::UnsupportedImport) => 8,
        Some(CapabilityDowngradeReason::TransientImport) => 9,
        Some(CapabilityDowngradeReason::UnsupportedColor) => 10,
        Some(CapabilityDowngradeReason::TransitionTimeout) => 11,
    }
}

const fn decode_downgrade_reason(value: u8) -> Option<CapabilityDowngradeReason> {
    match value {
        1 => Some(CapabilityDowngradeReason::HardwareOpenFailure),
        2 => Some(CapabilityDowngradeReason::HardwareDecodeFailure),
        3 => Some(CapabilityDowngradeReason::HardwareUnavailable),
        4 => Some(CapabilityDowngradeReason::RendererUnsupported),
        5 => Some(CapabilityDowngradeReason::RendererTransientFailure),
        6 => Some(CapabilityDowngradeReason::RendererFatalFailure),
        7 => Some(CapabilityDowngradeReason::UnsafeSync),
        8 => Some(CapabilityDowngradeReason::UnsupportedImport),
        9 => Some(CapabilityDowngradeReason::TransientImport),
        10 => Some(CapabilityDowngradeReason::UnsupportedColor),
        11 => Some(CapabilityDowngradeReason::TransitionTimeout),
        _ => None,
    }
}

impl SnapshotState {
    fn snapshot_with_atomics(&self) -> SessionSnapshot {
        let mut snapshot = self.snapshot.read().clone();
        let realization = decode_frame_realization(self.frame_realization.load(Ordering::Relaxed));
        if let Some(realization) = realization {
            snapshot.capability = realization.capability_tier();
        } else if let Some(capability) = decode_capability(self.capability.load(Ordering::Relaxed))
        {
            snapshot.capability = capability;
        }
        snapshot.frame_realization = realization;
        snapshot.latest_renderer_outcome =
            decode_renderer_outcome(self.latest_renderer_outcome.load(Ordering::Relaxed));
        snapshot.latest_downgrade_reason =
            decode_downgrade_reason(self.latest_downgrade_reason.load(Ordering::Relaxed));
        snapshot
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

fn update_gst_observation(
    state: &Arc<SnapshotState>,
    decoder: &GStreamerDecoder,
    frame_mailbox_occupancy: usize,
) {
    let observation: GstPipelineObservation = decoder.pipeline_observation();
    if observation.is_live_known {
        state.is_live.store(observation.is_live, Ordering::Relaxed);
        state.is_live_known.store(true, Ordering::Release);
    }
    if observation.seekable_known {
        state
            .seekable
            .store(observation.seekable, Ordering::Relaxed);
        state.seekability_known.store(true, Ordering::Release);
    }
    if let Some(latency) = observation.latency {
        state.pipeline_latency_ns.store(
            latency.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
    state
        .pipeline_latency_known
        .store(observation.latency_known, Ordering::Release);
    state
        .frame_mailbox_occupancy
        .store(frame_mailbox_occupancy as u64, Ordering::Relaxed);
    state
        .qos_events
        .store(decoder.qos_events(), Ordering::Relaxed);
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
        VideoError::Tls(message) => SessionError::Tls(message),
        VideoError::UnsupportedFormat(message) => SessionError::Unsupported(message),
        VideoError::Generic(message) => SessionError::Fatal(message),
    }
}

fn send_control(sender: &ControlSender, event: SequencedEvent) -> bool {
    match &event.event {
        SessionEvent::Metadata { .. } => sender.metadata.send(event),
        SessionEvent::AudioTracks { .. } => sender.audio_tracks.send(event),
        SessionEvent::Error(_) => sender.error.send(event),
        SessionEvent::Ended => sender.terminal.send(event),
        SessionEvent::StateChanged {
            state: SessionState::Ready | SessionState::Ended | SessionState::Error(_),
        } => sender.state.send(event),
        SessionEvent::AudioTrackSelected { .. }
        | SessionEvent::AudioTrackSelectionFailed { .. } => sender.audio_selection.send(event),
        SessionEvent::StateChanged { .. } | SessionEvent::Frame { .. } => {
            let mut event = event;
            loop {
                match sender.transient.try_send(event) {
                    Ok(()) => return true,
                    Err(TrySendError::Full(next)) => {
                        event = next;
                        match sender.transient_drop.try_recv() {
                            Ok(_) => {}
                            Err(TryRecvError::Empty) => thread::yield_now(),
                            Err(TryRecvError::Disconnected) => return false,
                        }
                    }
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }
        }
    }
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
    control_sender: &ControlSender,
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

fn publish_audio_tracks(
    state: &Arc<SnapshotState>,
    control_sender: &ControlSender,
    sequence: &mut u64,
    tracks: Vec<AudioTrack>,
    selected_id: Option<String>,
) -> bool {
    {
        let mut snapshot = state.snapshot.write();
        snapshot.audio_tracks = tracks.clone();
        snapshot.selected_audio_track_id = selected_id.clone();
    }
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::AudioTracks {
            tracks,
            selected_id,
        },
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

fn publish_audio_selected(
    state: &Arc<SnapshotState>,
    control_sender: &ControlSender,
    sequence: &mut u64,
    track: AudioTrack,
) -> bool {
    state.snapshot.write().selected_audio_track_id = Some(track.id.clone());
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::AudioTrackSelected { track },
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

fn publish_audio_selection_failed(
    state: &Arc<SnapshotState>,
    control_sender: &ControlSender,
    sequence: &mut u64,
    requested_id: String,
    prior_restored_id: Option<String>,
    reason: String,
) -> bool {
    state.snapshot.write().selected_audio_track_id = prior_restored_id.clone();
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::AudioTrackSelectionFailed {
            requested_id,
            prior_restored_id,
            reason,
        },
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

fn publish_error(
    state: &Arc<SnapshotState>,
    control_sender: &ControlSender,
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

fn publish_nonterminal_error(
    control_sender: &ControlSender,
    sequence: &mut u64,
    error: SessionError,
) -> bool {
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::Error(error),
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

struct PlaybackState {
    playing: bool,
    buffering: bool,
    position: Duration,
    stream_generation: u64,
}

#[derive(Debug, Default)]
struct LiveGapDeadline {
    live_media_seen: bool,
    deadline: Option<std::time::Instant>,
}

impl LiveGapDeadline {
    fn disarm(&mut self) {
        self.deadline = None;
    }

    fn observe_media_progress(&mut self, playing: bool, is_live: bool, progressed: bool) {
        if !playing || !is_live {
            self.disarm();
            return;
        }
        if progressed {
            self.live_media_seen = true;
            self.disarm();
        }
    }

    fn expired(
        &mut self,
        now: std::time::Instant,
        playing: bool,
        is_live: bool,
        lifecycle_timeout: Duration,
    ) -> bool {
        if !playing || !is_live || !self.live_media_seen {
            if !playing || !is_live {
                self.disarm();
            }
            return false;
        }
        let deadline = self
            .deadline
            .get_or_insert_with(|| now.checked_add(lifecycle_timeout).unwrap_or(now));
        now >= *deadline
    }
}

fn observe_live_media_progress(
    live_gap: &mut LiveGapDeadline,
    decoder: &GStreamerDecoder,
    playback: &PlaybackState,
    last_audio_buffers_seen: &mut u64,
    video_progressed: bool,
) {
    let audio_buffers_seen = decoder.audio_handle().audio_buffers_seen();
    let audio_progressed = audio_buffers_seen > *last_audio_buffers_seen;
    *last_audio_buffers_seen = audio_buffers_seen;
    let observation = decoder.pipeline_observation();
    live_gap.observe_media_progress(
        playback.playing,
        observation.is_live_known && observation.is_live,
        video_progressed || audio_progressed,
    );
}

fn mark_eos(playback: &mut PlaybackState) {
    playback.playing = false;
    playback.buffering = false;
}

fn playback_session_state(playback: &PlaybackState) -> SessionState {
    if !playback.playing {
        SessionState::Paused {
            position: playback.position,
        }
    } else if playback.buffering {
        SessionState::Buffering {
            position: playback.position,
        }
    } else {
        SessionState::Playing {
            position: playback.position,
        }
    }
}

fn apply_buffering_state(playback: &mut PlaybackState, buffering: bool) -> Option<SessionState> {
    if playback.buffering == buffering {
        return None;
    }
    playback.buffering = buffering;
    Some(playback_session_state(playback))
}

fn sync_buffering_state(
    playback: &mut PlaybackState,
    decoder: &GStreamerDecoder,
    state: &Arc<SnapshotState>,
    control_sender: &ControlSender,
    sequence: &mut u64,
) -> bool {
    let buffering = playback.playing && decoder.buffering_percent() < 100;
    let Some(next_state) = apply_buffering_state(playback, buffering) else {
        return true;
    };
    publish_state(state, control_sender, sequence, next_state)
}

#[allow(clippy::too_many_arguments)]
fn process_command(
    worker_command: WorkerCommand,
    decoder: &mut GStreamerDecoder,
    playback: &mut PlaybackState,
    audio_handle: &AudioHandle,
    state: &Arc<SnapshotState>,
    published_capability: &mut CapabilityTier,
    control_sender: &ControlSender,
    sequence: &mut u64,
) -> bool {
    let (command, target_generation) = match worker_command {
        WorkerCommand::Ordinary(command) => (command, None),
        WorkerCommand::Generation(intent) => (intent.command, Some(intent.target_generation)),
    };
    match command {
        SessionCommand::Play => {
            if decoder.is_eof() {
                let Some(target_generation) = target_generation else {
                    let _ = publish_error(
                        state,
                        control_sender,
                        sequence,
                        SessionError::Fatal("replay missing generation token".into()),
                    );
                    return false;
                };
                // Produce a paused preroll during replay, then explicitly
                // resume below. This keeps one seek deadline and avoids
                // duplicate replay intent advancing generations.
                decoder.set_paused_intent(true);
                if let Err(error) = decoder.seek(Duration::ZERO) {
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    return false;
                }
                playback.position = Duration::ZERO;
                state.position_us.store(0, Ordering::Relaxed);
                playback.stream_generation = target_generation;
            }
            match decoder.resume() {
                Ok(()) => {
                    playback.playing = true;
                    playback.buffering = decoder.buffering_percent() < 100;
                    audio_handle.start_playback_epoch();
                    publish_state(
                        state,
                        control_sender,
                        sequence,
                        playback_session_state(playback),
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
                playback.buffering = false;
                publish_state(
                    state,
                    control_sender,
                    sequence,
                    playback_session_state(playback),
                )
            }
            Err(error) => {
                let _ = publish_error(state, control_sender, sequence, session_error(error));
                false
            }
        },
        SessionCommand::Stop => {
            let deadline = decoder.begin_operation_deadline();
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
            decoder.shutdown_with_deadline(Some(deadline));
            false
        }
        SessionCommand::Seek { position: target } => {
            let Some(target_generation) = target_generation else {
                let _ = publish_error(
                    state,
                    control_sender,
                    sequence,
                    SessionError::Fatal("seek missing generation token".into()),
                );
                return false;
            };
            decoder.set_paused_intent(!playback.playing);
            match decoder.seek(target) {
                Ok(()) => {
                    playback.stream_generation = target_generation;
                    playback.position = target;
                    state
                        .position_us
                        .store(target.as_micros() as u64, Ordering::Relaxed);
                    playback.buffering = playback.playing && decoder.buffering_percent() < 100;
                    audio_handle.set_native_position(target);
                    publish_state(
                        state,
                        control_sender,
                        sequence,
                        playback_session_state(playback),
                    )
                }
                Err(error) => {
                    if matches!(error, VideoError::UnsupportedFormat(_)) {
                        return publish_nonterminal_error(
                            control_sender,
                            sequence,
                            session_error(error),
                        );
                    }
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    false
                }
            }
        }
        SessionCommand::SetMuted { muted } => {
            audio_handle.set_muted(muted);
            true
        }
        SessionCommand::SetVolume { volume } => {
            let volume = volume.clamp(0.0, 1.0);
            audio_handle.set_volume((volume * 100.0) as u32);
            true
        }
        SessionCommand::SelectAudioTrack { id } => {
            let selection = decoder.select_audio_track(&id);
            if let Some(tracks) = decoder.take_audio_tracks_update() {
                if !publish_audio_tracks(
                    state,
                    control_sender,
                    sequence,
                    tracks,
                    decoder.selected_audio_track_id().map(str::to_owned),
                ) {
                    return false;
                }
            }
            match selection {
                AudioTrackSelectionResult::Selected(track) => {
                    publish_audio_selected(state, control_sender, sequence, track)
                }
                AudioTrackSelectionResult::Failed {
                    requested_id,
                    prior_restored_id,
                    reason,
                } => publish_audio_selection_failed(
                    state,
                    control_sender,
                    sequence,
                    requested_id,
                    prior_restored_id,
                    reason,
                ),
            }
        }
        SessionCommand::Renegotiate { tier } => match decoder.renegotiate(tier) {
            Ok(()) => {
                if decoder.active_tier() != CapabilityTier::SystemMemoryUpload {
                    publish_capability_if_changed(
                        state,
                        published_capability,
                        decoder.active_tier(),
                    );
                }
                true
            }
            Err(error) => publish_nonterminal_error(control_sender, sequence, session_error(error)),
        },
    }
}

struct WorkerIo {
    commands: Receiver<SessionCommand>,
    generation_commands: Receiver<GenerationIntent>,
    control_sender: ControlSender,
    frame_sender: Sender<SequencedEvent>,
    frame_drop_receiver: Receiver<SequencedEvent>,
    state: Arc<SnapshotState>,
    dropped_frames: Arc<AtomicU64>,
    audio_handle: AudioHandle,
    audio_sink: GstAudioSinkMode,
    tls_ca_file: Option<String>,
    requested_tier: CapabilityTier,
    lifecycle_control: GstLifecycleControl,
}

#[derive(Debug)]
struct ColorGeneration {
    extent: FrameExtent,
    color: ColorMetadata,
    decision: ColorRenderDecision,
    rgba_pool: Option<RgbaPool>,
}

#[derive(Debug)]
struct RgbaPool {
    recycle: Sender<Vec<CpuPlane>>,
    available: Receiver<Vec<CpuPlane>>,
    stride: usize,
    bytes: usize,
}

impl RgbaPool {
    fn new(extent: FrameExtent) -> Option<Self> {
        let width = usize::try_from(extent.width).ok()?;
        let height = usize::try_from(extent.height).ok()?;
        let stride = width.checked_mul(4)?;
        let bytes = stride.checked_mul(height)?;
        let (recycle, available) = crossbeam_channel::bounded(2);
        for _ in 0..2 {
            let payload = vec![CpuPlane::new(vec![0; bytes], stride)];
            if recycle.try_send(payload).is_err() {
                return None;
            }
        }
        Some(Self {
            recycle,
            available,
            stride,
            bytes,
        })
    }

    #[cfg(test)]
    fn try_acquire(&self) -> Option<CpuMemory> {
        self.try_acquire_checked().ok().flatten()
    }

    fn try_acquire_checked(&self) -> Result<Option<CpuMemory>, &'static str> {
        match self.available.try_recv() {
            Ok(planes) => {
                let Some(plane) = planes.first() else {
                    return Err("RGBA recycle payload has no plane");
                };
                if planes.len() != 1
                    || plane.stride != self.stride
                    || plane.bytes.len() != self.bytes
                {
                    return Err("RGBA recycle payload shape changed");
                }
                Ok(Some(CpuMemory::new_recyclable(
                    planes,
                    self.recycle.clone(),
                )))
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Ok(None),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ColorGenerationError {
    Unsupported,
    UnsupportedSdrColor,
}

fn ensure_color_generation(
    generation: &mut Option<ColorGeneration>,
    extent: FrameExtent,
    color: ColorMetadata,
    decision: ColorRenderDecision,
) -> Result<&mut ColorGeneration, ColorGenerationError> {
    let matches_current = generation
        .as_ref()
        .is_some_and(|current| current.extent == extent && current.color == color);
    if !matches_current {
        let rgba_pool = match decision {
            ColorRenderDecision::CpuRgba(_) => RgbaPool::new(extent),
            ColorRenderDecision::Gpu(_)
            | ColorRenderDecision::Unsupported
            | ColorRenderDecision::UnsupportedSdrColor => None,
        };
        match decision {
            ColorRenderDecision::Unsupported => return Err(ColorGenerationError::Unsupported),
            ColorRenderDecision::UnsupportedSdrColor => {
                return Err(ColorGenerationError::UnsupportedSdrColor)
            }
            ColorRenderDecision::Gpu(_) => {}
            ColorRenderDecision::CpuRgba(_) if rgba_pool.is_none() => {
                return Err(ColorGenerationError::UnsupportedSdrColor)
            }
            ColorRenderDecision::CpuRgba(_) => {}
        }
        *generation = Some(ColorGeneration {
            extent,
            color,
            decision,
            rgba_pool,
        });
    }
    generation
        .as_mut()
        .ok_or(ColorGenerationError::UnsupportedSdrColor)
}

#[derive(Debug)]
enum FramePreparationError {
    Unsupported(ColorGenerationError),
    Decode(String),
}

fn prepare_frame_memory(
    format: PixelFormat,
    extent: FrameExtent,
    color: ColorMetadata,
    decision: ColorRenderDecision,
    memory: NativeMemory,
    generation: &mut Option<ColorGeneration>,
) -> Result<Option<(PixelFormat, NativeMemory)>, FramePreparationError> {
    if format != PixelFormat::Nv12 {
        return Ok(Some((format, memory)));
    }
    let decision = ensure_color_generation(generation, extent, color, decision)
        .map_err(FramePreparationError::Unsupported)?
        .decision;
    match decision {
        ColorRenderDecision::Gpu(_) => Ok(Some((PixelFormat::Nv12, memory))),
        ColorRenderDecision::Unsupported => Err(FramePreparationError::Unsupported(
            ColorGenerationError::Unsupported,
        )),
        ColorRenderDecision::UnsupportedSdrColor => Err(FramePreparationError::Unsupported(
            ColorGenerationError::UnsupportedSdrColor,
        )),
        ColorRenderDecision::CpuRgba(matrix) => {
            let Some(pool) = generation
                .as_ref()
                .and_then(|current| current.rgba_pool.as_ref())
            else {
                return Ok(None);
            };
            let mut output = match pool.try_acquire_checked() {
                Ok(Some(output)) => output,
                Ok(None) => return Ok(None),
                Err(error) => return Err(FramePreparationError::Decode(error.into())),
            };
            let NativeMemory::Cpu(input) = memory else {
                return Err(FramePreparationError::Unsupported(
                    ColorGenerationError::UnsupportedSdrColor,
                ));
            };
            let Some(output_plane) = output.planes.first_mut() else {
                return Err(FramePreparationError::Decode(
                    "RGBA pool returned no plane".into(),
                ));
            };
            nv12_to_rgba_into(
                &input.planes,
                extent,
                color,
                &matrix,
                &mut output_plane.bytes,
            )
            .map_err(|error| {
                FramePreparationError::Decode(format!("NV12 conversion: {error:?}"))
            })?;
            Ok(Some((PixelFormat::Rgba, NativeMemory::Cpu(output))))
        }
    }
}

fn color_generation_error(error: ColorGenerationError) -> SessionError {
    match error {
        ColorGenerationError::Unsupported => SessionError::Unsupported(
            "native video color metadata is unsupported (unknown matrix/range or HDR)".into(),
        ),
        ColorGenerationError::UnsupportedSdrColor => SessionError::Unsupported(
            "native video uses SDR color metadata outside the implemented renderer contract".into(),
        ),
    }
}

fn shutdown_worker(decoder: &mut GStreamerDecoder, lifecycle: &GstLifecycleControl) {
    let deadline = lifecycle
        .deadline()
        .or_else(|| decoder.active_operation_deadline());
    if deadline.is_some() {
        decoder.shutdown_with_deadline(deadline);
    } else {
        decoder.shutdown();
    }
}

fn publish_ended(state: &Arc<SnapshotState>, control_sender: &ControlSender, sequence: &mut u64) {
    let _ = publish_state(state, control_sender, sequence, SessionState::Ended);
    let _ = send_control(
        control_sender,
        SequencedEvent {
            sequence: *sequence,
            event: SessionEvent::Ended,
        },
    );
}

fn lifecycle_cancelled(lifecycle: &GstLifecycleControl) -> bool {
    lifecycle.is_cancelled()
}

fn seed_worker_spawn_failure(state: &Arc<SnapshotState>, control_sender: &ControlSender) {
    let mut sequence = 0_u64;
    let _ = publish_error(
        state,
        control_sender,
        &mut sequence,
        SessionError::Open("failed to spawn GStreamer worker".into()),
    );
}

fn publish_decode_mode(state: &Arc<SnapshotState>, decoder: &GStreamerDecoder) {
    state.decode_mode.store(
        encode_decode_mode(Some(decoder.decode_mode())),
        Ordering::Relaxed,
    );
}

fn is_decode_or_open_failure(error: &VideoError) -> bool {
    matches!(
        error,
        VideoError::OpenFailed(_) | VideoError::DecoderInit(_) | VideoError::DecodeFailed(_)
    )
}

fn hardware_downgrade_reason(error: &VideoError) -> CapabilityDowngradeReason {
    match error {
        VideoError::OpenFailed(_) | VideoError::DecoderInit(_) => {
            CapabilityDowngradeReason::HardwareOpenFailure
        }
        _ => CapabilityDowngradeReason::HardwareDecodeFailure,
    }
}

fn native_downgrade_reason(error: &VideoError) -> CapabilityDowngradeReason {
    match error {
        VideoError::UnsupportedFormat(message) if message.contains("fence export") => {
            CapabilityDowngradeReason::UnsafeSync
        }
        _ => CapabilityDowngradeReason::UnsupportedImport,
    }
}

#[allow(clippy::too_many_arguments)]
fn open_system_memory_decoder(
    source: &str,
    audio_sink: GstAudioSinkMode,
    lifecycle_timeout: Duration,
    open_timeout: Duration,
    lifecycle_control: GstLifecycleControl,
    tls_ca_file: &Option<String>,
) -> Result<GStreamerDecoder, VideoError> {
    GStreamerDecoder::new_with_requested_tier_and_audio_sink_and_timeouts_and_control_and_tls_ca_file(
        source,
        CapabilityTier::SystemMemoryUpload,
        audio_sink,
        lifecycle_timeout,
        open_timeout,
        lifecycle_control,
        tls_ca_file.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
fn rebuild_system_memory_decoder(
    decoder: &mut GStreamerDecoder,
    source: &str,
    audio_sink: GstAudioSinkMode,
    lifecycle_timeout: Duration,
    lifecycle_control: GstLifecycleControl,
    tls_ca_file: &Option<String>,
    position: Duration,
    playing: bool,
) -> Result<GStreamerDecoder, VideoError> {
    decoder.shutdown();
    let mut replacement = open_system_memory_decoder(
        source,
        audio_sink,
        lifecycle_timeout,
        lifecycle_timeout,
        lifecycle_control,
        tls_ca_file,
    )?;
    replacement.set_paused_intent(!playing);
    if !position.is_zero() {
        replacement.seek(position)?;
    }
    if playing {
        replacement.resume()?;
    }
    Ok(replacement)
}

fn run_worker(
    source: String,
    autoplay: bool,
    lifecycle_timeout: Duration,
    open_timeout: Duration,
    initial_stream_generation: u64,
    io: WorkerIo,
) {
    let WorkerIo {
        commands,
        generation_commands,
        control_sender,
        frame_sender,
        frame_drop_receiver,
        state,
        dropped_frames,
        audio_handle,
        audio_sink,
        tls_ca_file,
        requested_tier,
        lifecycle_control,
    } = io;
    let mut sequence = 0_u64;
    if lifecycle_cancelled(&lifecycle_control) {
        if lifecycle_control.is_stop_requested() {
            publish_ended(&state, &control_sender, &mut sequence);
        }
        return;
    }
    let source = match local_source_url(&source) {
        Ok(source) => source,
        Err(error) => {
            if lifecycle_control.is_stop_requested() {
                publish_ended(&state, &control_sender, &mut sequence);
            } else if !lifecycle_cancelled(&lifecycle_control) {
                let _ = publish_error(&state, &control_sender, &mut sequence, error);
            }
            return;
        }
    };
    let mut fallback_attempted = false;
    let mut automatic_downgrade_recorded = false;
    let mut unsupported_color_recorded = false;
    let mut decoder =
        match GStreamerDecoder::new_with_requested_tier_and_audio_sink_and_timeouts_and_control_and_tls_ca_file(
            &source,
            requested_tier,
            audio_sink,
            lifecycle_timeout,
            open_timeout,
            lifecycle_control.clone(),
            tls_ca_file.clone(),
        ) {
            Ok(decoder) => decoder,
            Err(error)
                if requested_tier == CapabilityTier::DirectAlias
                    && is_decode_or_open_failure(&error) =>
            {
                fallback_attempted = true;
                record_downgrade_reason(&state, hardware_downgrade_reason(&error));
                match open_system_memory_decoder(
                    &source,
                    audio_sink,
                    lifecycle_timeout,
                    lifecycle_timeout,
                    lifecycle_control.clone(),
                    &tls_ca_file,
                ) {
                    Ok(decoder) => decoder,
                    Err(fallback_error) => {
                        if lifecycle_control.is_stop_requested() {
                            publish_ended(&state, &control_sender, &mut sequence);
                        } else if !lifecycle_cancelled(&lifecycle_control) {
                            let _ = publish_error(
                                &state,
                                &control_sender,
                                &mut sequence,
                                SessionError::Fatal(format!(
                                    "hardware open failed: {error}; fallback: {fallback_error}"
                                )),
                            );
                        }
                        return;
                    }
                }
            }
            Err(error) => {
                if lifecycle_control.is_stop_requested() {
                    publish_ended(&state, &control_sender, &mut sequence);
                } else if !lifecycle_cancelled(&lifecycle_control) {
                    let _ =
                        publish_error(&state, &control_sender, &mut sequence, session_error(error));
                }
                return;
            }
        };
    publish_decode_mode(&state, &decoder);
    if requested_tier == CapabilityTier::DirectAlias
        && !fallback_attempted
        && decoder.decode_mode() == DecodeMode::Software
    {
        record_downgrade_reason(&state, CapabilityDowngradeReason::HardwareUnavailable);
    }
    let mut published_capability = requested_tier;
    if decoder.active_tier() != CapabilityTier::SystemMemoryUpload {
        publish_capability_if_changed(&state, &mut published_capability, decoder.active_tier());
    }
    if lifecycle_cancelled(&lifecycle_control) {
        if lifecycle_control.is_stop_requested() {
            publish_ended(&state, &control_sender, &mut sequence);
        }
        shutdown_worker(&mut decoder, &lifecycle_control);
        return;
    }

    let mut last_applied_audio = None;
    if let Err(error) = sync_audio_controls(&audio_handle, &mut decoder, &mut last_applied_audio) {
        if lifecycle_control.is_stop_requested() {
            publish_ended(&state, &control_sender, &mut sequence);
        } else if !lifecycle_cancelled(&lifecycle_control) {
            let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
        }
        shutdown_worker(&mut decoder, &lifecycle_control);
        return;
    }
    update_audio_observation(&state, &audio_handle, &decoder, Duration::ZERO);
    update_gst_observation(&state, &decoder, frame_sender.len());

    let metadata = session_metadata(decoder.metadata());
    state.snapshot.write().metadata = Some(metadata.clone());
    if !send_control(
        &control_sender,
        SequencedEvent {
            sequence,
            event: SessionEvent::Metadata { metadata },
        },
    ) {
        shutdown_worker(&mut decoder, &lifecycle_control);
        return;
    }
    sequence = sequence.saturating_add(1);
    let initial_audio_tracks = decoder.audio_tracks().to_vec();
    if !publish_audio_tracks(
        &state,
        &control_sender,
        &mut sequence,
        initial_audio_tracks.clone(),
        decoder.selected_audio_track_id().map(str::to_owned),
    ) {
        shutdown_worker(&mut decoder, &lifecycle_control);
        return;
    }
    if let Some(selected_id) = decoder.selected_audio_track_id() {
        if let Some(track) = initial_audio_tracks
            .iter()
            .find(|track| track.id == selected_id)
            .cloned()
        {
            if !publish_audio_selected(&state, &control_sender, &mut sequence, track) {
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
        }
    }
    if !publish_state(&state, &control_sender, &mut sequence, SessionState::Ready) {
        shutdown_worker(&mut decoder, &lifecycle_control);
        return;
    }

    let mut playback = PlaybackState {
        playing: false,
        buffering: false,
        position: Duration::ZERO,
        stream_generation: initial_stream_generation,
    };
    let mut frame_id = 0_u64;
    // One decision and (for CPU fallback) one exactly-two-payload pool per
    // negotiated extent/color generation. A caps or color change replaces
    // this value; old payload senders then disconnect when outstanding leases
    // return, so no generation can recycle into a newer pool.
    let mut color_generation = None;
    let mut live_gap = LiveGapDeadline::default();
    let mut last_audio_buffers_seen = decoder.audio_handle().audio_buffers_seen();

    if autoplay {
        match decoder.resume() {
            Ok(()) => {
                playback.playing = true;
                playback.buffering = decoder.buffering_percent() < 100;
                audio_handle.start_playback_epoch();
                if !publish_state(
                    &state,
                    &control_sender,
                    &mut sequence,
                    playback_session_state(&playback),
                ) {
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
            }
            Err(error) => {
                if lifecycle_control.is_stop_requested() {
                    publish_ended(&state, &control_sender, &mut sequence);
                } else if !lifecycle_cancelled(&lifecycle_control) {
                    let _ =
                        publish_error(&state, &control_sender, &mut sequence, session_error(error));
                }
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
        }
    }

    loop {
        if lifecycle_cancelled(&lifecycle_control) {
            if lifecycle_control.is_stop_requested() {
                publish_ended(&state, &control_sender, &mut sequence);
            }
            shutdown_worker(&mut decoder, &lifecycle_control);
            return;
        }
        let command = match generation_commands.try_recv() {
            Ok(intent) => Some(WorkerCommand::Generation(intent)),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                commands.try_recv().ok().map(WorkerCommand::Ordinary)
            }
        };
        if let Some(command) = command {
            if !process_command(
                command,
                &mut decoder,
                &mut playback,
                &audio_handle,
                &state,
                &mut published_capability,
                &control_sender,
                &mut sequence,
            ) {
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
        }
        if let Some(tracks) = decoder.take_audio_tracks_update() {
            if !publish_audio_tracks(
                &state,
                &control_sender,
                &mut sequence,
                tracks,
                decoder.selected_audio_track_id().map(str::to_owned),
            ) {
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
        }
        update_gst_observation(&state, &decoder, frame_sender.len());
        if !sync_buffering_state(
            &mut playback,
            &decoder,
            &state,
            &control_sender,
            &mut sequence,
        ) {
            shutdown_worker(&mut decoder, &lifecycle_control);
            return;
        }
        if lifecycle_cancelled(&lifecycle_control) {
            if lifecycle_control.is_stop_requested() {
                publish_ended(&state, &control_sender, &mut sequence);
            }
            shutdown_worker(&mut decoder, &lifecycle_control);
            return;
        }

        if let Err(error) =
            sync_audio_controls(&audio_handle, &mut decoder, &mut last_applied_audio)
        {
            if lifecycle_control.is_stop_requested() {
                publish_ended(&state, &control_sender, &mut sequence);
            } else if !lifecycle_cancelled(&lifecycle_control) {
                let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
            }
            shutdown_worker(&mut decoder, &lifecycle_control);
            return;
        }
        update_audio_observation(&state, &audio_handle, &decoder, playback.position);

        if !playback.playing {
            observe_live_media_progress(
                &mut live_gap,
                &decoder,
                &playback,
                &mut last_audio_buffers_seen,
                false,
            );
            live_gap.disarm();
        }
        if !playback.playing {
            match commands.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => {
                    if !process_command(
                        WorkerCommand::Ordinary(command),
                        &mut decoder,
                        &mut playback,
                        &audio_handle,
                        &state,
                        &mut published_capability,
                        &control_sender,
                        &mut sequence,
                    ) {
                        shutdown_worker(&mut decoder, &lifecycle_control);
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            }
            continue;
        }

        match decoder.decode_next_native() {
            Ok(Some(frame)) => {
                if lifecycle_cancelled(&lifecycle_control) {
                    if lifecycle_control.is_stop_requested() {
                        publish_ended(&state, &control_sender, &mut sequence);
                    }
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                playback.position = frame.pts;
                observe_live_media_progress(
                    &mut live_gap,
                    &decoder,
                    &playback,
                    &mut last_audio_buffers_seen,
                    true,
                );
                state
                    .position_us
                    .store(playback.position.as_micros() as u64, Ordering::Relaxed);
                update_audio_observation(&state, &audio_handle, &decoder, playback.position);
                let frame_color = frame.color;
                let frame_color_decision = frame.color_decision;
                let frame_extent = frame.extent;
                let frame_pts = frame.pts;
                let frame_format = frame.format;
                let frame_acquire = frame.acquire;
                let (format, memory) = match prepare_frame_memory(
                    frame_format,
                    frame_extent,
                    frame_color,
                    frame_color_decision,
                    frame.memory,
                    &mut color_generation,
                ) {
                    Ok(Some(prepared)) => prepared,
                    Ok(None) => {
                        dropped_frames.fetch_add(1, Ordering::Relaxed);
                        update_gst_observation(&state, &decoder, frame_sender.len());
                        continue;
                    }
                    Err(FramePreparationError::Unsupported(error)) => {
                        let _ = publish_error(
                            &state,
                            &control_sender,
                            &mut sequence,
                            color_generation_error(error),
                        );
                        shutdown_worker(&mut decoder, &lifecycle_control);
                        return;
                    }
                    Err(FramePreparationError::Decode(message)) => {
                        let _ = publish_error(
                            &state,
                            &control_sender,
                            &mut sequence,
                            SessionError::Decode(message),
                        );
                        shutdown_worker(&mut decoder, &lifecycle_control);
                        return;
                    }
                };
                let descriptor = NativeFrameDescriptor {
                    frame_id,
                    stream_generation: playback.stream_generation,
                    pts: frame_pts,
                    duration: Some(decoder.metadata().frame_duration()),
                    extent: frame_extent,
                    format,
                    color: frame_color,
                };
                let active_tier = decoder.active_tier();
                let settled_tier = if active_tier != CapabilityTier::SystemMemoryUpload
                    && matches!(&memory, NativeMemory::DmaBuf(_))
                {
                    active_tier
                } else {
                    CapabilityTier::SystemMemoryUpload
                };
                if !unsupported_color_recorded
                    && active_tier != CapabilityTier::SystemMemoryUpload
                    && settled_tier == CapabilityTier::SystemMemoryUpload
                    && matches!(frame_color_decision, ColorRenderDecision::CpuRgba(_))
                {
                    unsupported_color_recorded = true;
                    record_downgrade_reason(&state, CapabilityDowngradeReason::UnsupportedColor);
                }
                let acquire = if matches!(&memory, NativeMemory::DmaBuf(_)) {
                    frame_acquire
                } else {
                    AcquireSync::None
                };
                let lease = match NativeFrameLease::new(descriptor, memory, acquire) {
                    Ok(lease) => lease,
                    Err(error) => {
                        if lifecycle_control.is_stop_requested() {
                            publish_ended(&state, &control_sender, &mut sequence);
                        } else if !lifecycle_cancelled(&lifecycle_control) {
                            let _ = publish_error(
                                &state,
                                &control_sender,
                                &mut sequence,
                                SessionError::Decode(error.to_string()),
                            );
                        }
                        shutdown_worker(&mut decoder, &lifecycle_control);
                        return;
                    }
                };
                if settled_tier != CapabilityTier::SystemMemoryUpload {
                    publish_capability_if_changed(&state, &mut published_capability, settled_tier);
                }
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
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                update_gst_observation(&state, &decoder, frame_sender.len());
                sequence = sequence.saturating_add(1);
            }
            Ok(None) if decoder.is_eof() => {
                mark_eos(&mut playback);
                if !publish_state(&state, &control_sender, &mut sequence, SessionState::Ended) {
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                if !send_control(
                    &control_sender,
                    SequencedEvent {
                        sequence,
                        event: SessionEvent::Ended,
                    },
                ) {
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                continue;
            }
            Ok(None) => {
                observe_live_media_progress(
                    &mut live_gap,
                    &decoder,
                    &playback,
                    &mut last_audio_buffers_seen,
                    false,
                );
                let observation = decoder.pipeline_observation();
                if live_gap.expired(
                    std::time::Instant::now(),
                    playback.playing,
                    observation.is_live_known && observation.is_live,
                    lifecycle_timeout,
                ) {
                    let _ = publish_error(
                        &state,
                        &control_sender,
                        &mut sequence,
                        SessionError::Network(format!(
                            "live media gap exceeded {:?}",
                            lifecycle_timeout
                        )),
                    );
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
                update_gst_observation(&state, &decoder, frame_sender.len());
            }
            Err(error @ VideoError::UnsupportedFormat(_)) => {
                if requested_tier == CapabilityTier::DirectAlias && !automatic_downgrade_recorded {
                    automatic_downgrade_recorded = true;
                    record_downgrade_reason(&state, native_downgrade_reason(&error));
                }
                dropped_frames.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(error)
                if is_decode_or_open_failure(&error)
                    && !fallback_attempted
                    && requested_tier == CapabilityTier::DirectAlias
                    && decoder.decode_mode() == DecodeMode::Hardware =>
            {
                fallback_attempted = true;
                record_downgrade_reason(&state, hardware_downgrade_reason(&error));
                match rebuild_system_memory_decoder(
                    &mut decoder,
                    &source,
                    audio_sink,
                    lifecycle_timeout,
                    lifecycle_control.clone(),
                    &tls_ca_file,
                    playback.position,
                    playback.playing,
                ) {
                    Ok(replacement) => {
                        decoder = replacement;
                        last_applied_audio = None;
                        last_audio_buffers_seen = decoder.audio_handle().audio_buffers_seen();
                        color_generation = None;
                        playback.buffering = playback.playing && decoder.buffering_percent() < 100;
                        publish_decode_mode(&state, &decoder);
                        continue;
                    }
                    Err(fallback_error) => {
                        let _ = publish_error(
                            &state,
                            &control_sender,
                            &mut sequence,
                            SessionError::Fatal(format!(
                                "hardware decode failed: {error}; fallback failed: {fallback_error}"
                            )),
                        );
                    }
                }
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
            Err(error) => {
                if lifecycle_control.is_stop_requested() {
                    publish_ended(&state, &control_sender, &mut sequence);
                } else if !lifecycle_cancelled(&lifecycle_control) {
                    let error = if fallback_attempted && is_decode_or_open_failure(&error) {
                        SessionError::Fatal(format!("decoder failed after fallback: {error}"))
                    } else {
                        session_error(error)
                    };
                    let _ = publish_error(&state, &control_sender, &mut sequence, error);
                }
                shutdown_worker(&mut decoder, &lifecycle_control);
                return;
            }
        }
    }
}

/// A Linux GStreamer media session with bounded, nonblocking UI interaction.
pub struct GstMediaSession {
    commands: Sender<SessionCommand>,
    command_drop_receiver: Receiver<SessionCommand>,
    generation_commands: Sender<GenerationIntent>,
    generation_drop_receiver: Receiver<GenerationIntent>,
    control_receiver: ControlReceiver,
    pending_metadata: Option<SequencedEvent>,
    pending_audio_tracks: Option<SequencedEvent>,
    pending_audio_selection: Option<SequencedEvent>,
    pending_state: Option<SequencedEvent>,
    pending_error: Option<SequencedEvent>,
    pending_terminal: Option<SequencedEvent>,
    pending_transient: Option<SequencedEvent>,
    frame_receiver: Receiver<SequencedEvent>,
    state: Arc<SnapshotState>,
    audio_handle: AudioHandle,
    dropped_frames: Arc<AtomicU64>,
    last_delivered_sequence: Option<u64>,
    has_presented_frame: bool,
    worker: Option<JoinHandle<()>>,
    worker_disconnected: bool,
    lifecycle_timeout: Duration,
    lifecycle_control: GstLifecycleControl,
    stream_generation: u64,
    latest_requested_generation: u64,
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

    /// Starts a session with an explicit requested frame capability. Native
    /// layout failures settle the session to system-memory upload once.
    pub fn new_with_requested_tier(
        source: impl Into<String>,
        requested_tier: CapabilityTier,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file_and_tier(
            source,
            false,
            GstAudioSinkMode::Auto,
            DEFAULT_LIFECYCLE_TIMEOUT,
            DEFAULT_OPEN_TIMEOUT,
            0,
            None,
            requested_tier,
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

    /// Starts a session with explicit autoplay, sink, lifecycle, and stream
    /// generation policies.
    pub fn new_with_autoplay_and_audio_sink_and_timeout_and_generation(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        stream_generation: u64,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeouts_and_generation(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            DEFAULT_OPEN_TIMEOUT,
            stream_generation,
        )
    }

    /// Starts a session with separate opening and lifecycle bounds.
    pub fn new_with_autoplay_and_audio_sink_and_timeouts_and_generation(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        open_timeout: Duration,
        stream_generation: u64,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            open_timeout,
            stream_generation,
            None,
        )
    }

    /// Starts a session with an explicit frame capability request.
    pub fn new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        open_timeout: Duration,
        stream_generation: u64,
        requested_tier: CapabilityTier,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file_and_tier(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            open_timeout,
            stream_generation,
            None,
            requested_tier,
        )
    }

    #[cfg(test)]
    fn new_for_test_with_tls_ca_file(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        stream_generation: u64,
        tls_ca_file: String,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            DEFAULT_OPEN_TIMEOUT,
            stream_generation,
            Some(tls_ca_file),
        )
    }

    fn new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        open_timeout: Duration,
        stream_generation: u64,
        tls_ca_file: Option<String>,
    ) -> Self {
        Self::new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file_and_tier(
            source,
            autoplay,
            audio_sink,
            lifecycle_timeout,
            open_timeout,
            stream_generation,
            tls_ca_file,
            CapabilityTier::SystemMemoryUpload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_autoplay_and_audio_sink_and_timeout_and_generation_and_tls_ca_file_and_tier(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
        lifecycle_timeout: Duration,
        open_timeout: Duration,
        stream_generation: u64,
        tls_ca_file: Option<String>,
        requested_tier: CapabilityTier,
    ) -> Self {
        let source = source.into();
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let generation_drop_receiver = generation_receiver.clone();
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let state = Arc::new(SnapshotState::with_capability(requested_tier));
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let audio_handle = AudioHandle::new();
        let lifecycle_control = GstLifecycleControl::new();
        let worker_state = Arc::clone(&state);
        let worker_dropped_frames = Arc::clone(&dropped_frames);
        let worker_audio_handle = audio_handle.clone();
        let worker_lifecycle_control = lifecycle_control.clone();
        let spawn_failure_sender = control_sender.clone();
        let worker_control_sender = control_sender;
        let worker = thread::Builder::new()
            .name("lumina-gst-session".into())
            .spawn(move || {
                run_worker(
                    source,
                    autoplay,
                    lifecycle_timeout,
                    open_timeout,
                    stream_generation,
                    WorkerIo {
                        commands: command_receiver,
                        generation_commands: generation_receiver,
                        control_sender: worker_control_sender,
                        frame_sender,
                        frame_drop_receiver,
                        state: worker_state,
                        dropped_frames: worker_dropped_frames,
                        audio_handle: worker_audio_handle,
                        audio_sink,
                        tls_ca_file,
                        requested_tier,
                        lifecycle_control: worker_lifecycle_control,
                    },
                )
            })
            .ok();
        let worker_disconnected = worker.is_none();
        if worker_disconnected {
            seed_worker_spawn_failure(&state, &spawn_failure_sender);
        }

        Self {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver,
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state,
            audio_handle,
            dropped_frames,
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker,
            worker_disconnected,
            lifecycle_timeout,
            lifecycle_control,
            stream_generation,
            latest_requested_generation: stream_generation,
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

    /// Returns the latest GStreamer live-edge, timing, mailbox, and QoS facts.
    pub fn gst_observation(&self) -> GstObservation {
        let is_live_known = self.state.is_live_known.load(Ordering::Acquire);
        let seekability_known = self.state.seekability_known.load(Ordering::Acquire);
        let pipeline_latency_known = self.state.pipeline_latency_known.load(Ordering::Acquire);
        let pipeline_latency = pipeline_latency_known
            .then(|| Duration::from_nanos(self.state.pipeline_latency_ns.load(Ordering::Relaxed)));
        GstObservation {
            is_live: is_live_known && self.state.is_live.load(Ordering::Relaxed),
            is_live_known,
            seekable: seekability_known && self.state.seekable.load(Ordering::Relaxed),
            seekability_known,
            pipeline_latency,
            frame_mailbox_occupancy: self.state.frame_mailbox_occupancy.load(Ordering::Relaxed)
                as usize,
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            qos_events: self.state.qos_events.load(Ordering::Relaxed),
        }
    }

    /// Returns the latest discoverable audio tracks without consuming events.
    pub fn audio_tracks(&self) -> Vec<AudioTrack> {
        self.state.snapshot.read().audio_tracks.clone()
    }

    /// Returns the latest confirmed audio selection, if known.
    pub fn selected_audio_track_id(&self) -> Option<String> {
        self.state.snapshot.read().selected_audio_track_id.clone()
    }

    /// Returns the decoder mode selected by GStreamer after preroll.
    pub fn decode_mode(&self) -> Option<DecodeMode> {
        decode_decode_mode(self.state.decode_mode.load(Ordering::Relaxed))
    }

    /// Records one renderer outcome without committing frame realization.
    pub fn report_renderer_outcome(&self, outcome: RendererOutcome) {
        record_renderer_outcome(&self.state, outcome);
    }

    /// Commits a frame realization after its first successful presentation.
    pub fn commit_realization(&self, realization: FrameRealization) {
        record_realization(&self.state, realization);
    }

    /// Records the latest typed capability downgrade reason.
    pub fn report_downgrade_reason(&self, reason: CapabilityDowngradeReason) {
        record_downgrade_reason(&self.state, reason);
    }

    /// Returns the current presentation capability committed by the renderer.
    pub fn capability(&self) -> CapabilityTier {
        if let Some(realization) = self.frame_realization() {
            return realization.capability_tier();
        }
        match decode_capability(self.state.capability.load(Ordering::Relaxed)) {
            Some(capability) => capability,
            None => CapabilityTier::SystemMemoryUpload,
        }
    }

    /// Returns the last successfully committed frame realization.
    pub fn frame_realization(&self) -> Option<FrameRealization> {
        decode_frame_realization(self.state.frame_realization.load(Ordering::Relaxed))
    }

    /// Returns the latest typed renderer outcome.
    pub fn latest_renderer_outcome(&self) -> Option<RendererOutcome> {
        decode_renderer_outcome(self.state.latest_renderer_outcome.load(Ordering::Relaxed))
    }

    /// Returns the latest typed capability downgrade reason.
    pub fn latest_downgrade_reason(&self) -> Option<CapabilityDowngradeReason> {
        decode_downgrade_reason(self.state.latest_downgrade_reason.load(Ordering::Relaxed))
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

    fn fill_control_pending(
        pending: &mut Option<SequencedEvent>,
        receiver: &Receiver<SequencedEvent>,
    ) {
        if pending.is_none() {
            if let Ok(event) = receiver.try_recv() {
                *pending = Some(event);
            }
        }
    }

    fn update_frame_mailbox_occupancy(&self) {
        self.state
            .frame_mailbox_occupancy
            .store(self.frame_receiver.len() as u64, Ordering::Relaxed);
    }

    fn fill_pending(&mut self) {
        Self::fill_control_pending(&mut self.pending_metadata, &self.control_receiver.metadata);
        Self::fill_control_pending(
            &mut self.pending_audio_tracks,
            &self.control_receiver.audio_tracks,
        );
        Self::fill_control_pending(
            &mut self.pending_audio_selection,
            &self.control_receiver.audio_selection,
        );
        Self::fill_control_pending(&mut self.pending_state, &self.control_receiver.state);
        Self::fill_control_pending(&mut self.pending_error, &self.control_receiver.error);
        Self::fill_control_pending(&mut self.pending_terminal, &self.control_receiver.terminal);
        Self::fill_control_pending(
            &mut self.pending_transient,
            &self.control_receiver.transient,
        );
    }
}

fn enqueue_latest<T: Send>(
    commands: &Sender<T>,
    command_drop_receiver: &Receiver<T>,
    mut command: T,
) -> Result<(), SessionError> {
    loop {
        match commands.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(next)) => {
                command = next;
                match command_drop_receiver.try_recv() {
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => thread::yield_now(),
                    Err(TryRecvError::Disconnected) => {
                        return Err(SessionError::Fatal("session worker stopped".into()))
                    }
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(SessionError::Fatal("session worker stopped".into()))
            }
        }
    }
}

impl MediaSession for GstMediaSession {
    type Frame = Frame;

    fn snapshot(&self) -> SessionSnapshot {
        let mut snapshot = self.state.snapshot_with_atomics();
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
        if matches!(&command, SessionCommand::Stop) {
            // Stop must wake opening/decoding workers even when the command
            // FIFO is full. The FIFO send below is only a best-effort wake.
            self.lifecycle_control.request_stop(self.lifecycle_timeout);
            let _ = self.commands.try_send(command);
            return Ok(());
        }
        let is_seek = matches!(&command, SessionCommand::Seek { .. });
        if is_seek {
            let observation = self.gst_observation();
            if observation.seekability_known && !observation.seekable {
                return Err(SessionError::Unsupported(
                    "GStreamer reported a non-seekable stream".into(),
                ));
            }
        }
        let is_play = matches!(&command, SessionCommand::Play);
        let was_ended = matches!(self.snapshot().state, SessionState::Ended);
        let is_replay = is_play && was_ended && !self.replay_pending;
        if is_seek || is_replay {
            let target_generation = self.latest_requested_generation.wrapping_add(1);
            enqueue_latest(
                &self.generation_commands,
                &self.generation_drop_receiver,
                GenerationIntent {
                    command,
                    target_generation,
                },
            )?;
            self.latest_requested_generation = target_generation;
        } else {
            enqueue_latest(&self.commands, &self.command_drop_receiver, command)?;
        }
        if is_seek {
            self.replay_pending = was_ended;
        } else if is_replay {
            self.replay_pending = true;
        }
        Ok(())
    }

    fn try_next_event(&mut self) -> Result<Option<Event>, SessionError> {
        self.fill_pending();
        let mut source: Option<(u64, u8)> = None;
        if let Some(event) = self.pending_metadata.as_ref() {
            source = Some((event.sequence, 0));
        }
        if let Some(event) = self.pending_audio_tracks.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 1));
            }
        }
        if let Some(event) = self.pending_audio_selection.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 2));
            }
        }
        if let Some(event) = self.pending_state.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 3));
            }
        }
        if let Some(event) = self.pending_error.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 4));
            }
        }
        if let Some(event) = self.pending_terminal.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 5));
            }
        }
        if let Some(event) = self.pending_transient.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 6));
            }
        }
        let next = match source.map(|(_, kind)| kind) {
            Some(0) => self.pending_metadata.take(),
            Some(1) => self.pending_audio_tracks.take(),
            Some(2) => self.pending_audio_selection.take(),
            Some(3) => self.pending_state.take(),
            Some(4) => self.pending_error.take(),
            Some(5) => self.pending_terminal.take(),
            Some(6) => self.pending_transient.take(),
            _ => None,
        };
        if let Some(event) = next {
            self.update_frame_mailbox_occupancy();
            self.last_delivered_sequence = Some(
                self.last_delivered_sequence
                    .map_or(event.sequence, |last| last.max(event.sequence)),
            );
            if matches!(
                &event.event,
                SessionEvent::Error(_)
                    | SessionEvent::StateChanged {
                        state: SessionState::Error(_)
                    }
            ) {
                self.latest_requested_generation = self.stream_generation;
                self.replay_pending = false;
            }
            return Ok(Some(event.event));
        }

        let frame = match self.frame_receiver.try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => {
                self.update_frame_mailbox_occupancy();
                return Ok(None);
            }
            Err(TryRecvError::Disconnected) => {
                self.worker_disconnected = true;
                self.update_frame_mailbox_occupancy();
                return Ok(None);
            }
        };
        let generation = match &frame.event {
            SessionEvent::Frame { frame, .. } => frame.descriptor.stream_generation,
            _ => self.stream_generation,
        };
        if generation != self.latest_requested_generation {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
            self.update_frame_mailbox_occupancy();
            return Ok(None);
        }
        if self
            .last_delivered_sequence
            .is_some_and(|last| frame.sequence <= last)
        {
            self.dropped_frames.fetch_add(1, Ordering::Relaxed);
            self.update_frame_mailbox_occupancy();
            return Ok(None);
        }
        self.stream_generation = generation;
        self.replay_pending = false;
        self.last_delivered_sequence = Some(frame.sequence);
        self.update_frame_mailbox_occupancy();
        Ok(Some(frame.event))
    }
}

impl Drop for GstMediaSession {
    fn drop(&mut self) {
        // The worker owns all potentially blocking GStreamer calls. The
        // cancellation flag bypasses the command FIFO and starts the one
        // teardown deadline immediately; the worker handle is detached.
        self.lifecycle_control.cancel(self.lifecycle_timeout);
        self.worker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumina_video_native_frame::{
        ChromaHorizontal, ChromaVertical, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange,
        ColorTransfer, CpuMemory, CpuPlane, FrameExtent, NativeMemory,
    };
    use std::fs;
    use std::io::{self, Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct ControlledServer {
        address: SocketAddr,
        stop: Option<mpsc::Sender<()>>,
        worker: Option<JoinHandle<()>>,
    }

    #[derive(Clone)]
    struct LiveFixtureControl {
        outage: Arc<AtomicUsize>,
        playlist_requests: Arc<AtomicUsize>,
        highest_served_sequence: Arc<AtomicUsize>,
    }

    impl LiveFixtureControl {
        fn set_outage(&self, outage: bool) {
            self.outage
                .store(if outage { 1 } else { 0 }, Ordering::Release);
        }

        fn outage_seen(&self) -> bool {
            self.outage.load(Ordering::Acquire) >= 2
        }

        fn highest_served_sequence(&self) -> Option<usize> {
            self.highest_served_sequence
                .load(Ordering::Acquire)
                .checked_sub(1)
        }
    }

    impl ControlledServer {
        fn spawn(root: PathBuf, tls_config: Option<Arc<rustls::ServerConfig>>) -> io::Result<Self> {
            Self::spawn_with_live(root, tls_config, None).map(|(server, _)| server)
        }

        fn spawn_live(root: PathBuf) -> io::Result<(Self, LiveFixtureControl)> {
            let control = LiveFixtureControl {
                outage: Arc::new(AtomicUsize::new(0)),
                playlist_requests: Arc::new(AtomicUsize::new(0)),
                highest_served_sequence: Arc::new(AtomicUsize::new(0)),
            };
            let (server, _) = Self::spawn_with_live(
                root,
                None,
                Some((
                    Arc::clone(&control.outage),
                    Arc::clone(&control.playlist_requests),
                    Arc::clone(&control.highest_served_sequence),
                )),
            )?;
            Ok((server, control))
        }

        fn spawn_with_live(
            root: PathBuf,
            tls_config: Option<Arc<rustls::ServerConfig>>,
            live: Option<(Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>)>,
        ) -> io::Result<(Self, Option<LiveFixtureControl>)> {
            let listener = TcpListener::bind(("127.0.0.1", 0))?;
            listener.set_nonblocking(true)?;
            let address = listener.local_addr()?;
            let (stop, stop_receiver) = mpsc::channel();
            let live_for_worker = live.clone();
            let worker = thread::Builder::new()
                .name("lumina-gst-fixture-server".into())
                .spawn(move || loop {
                    if stop_receiver.try_recv().is_ok() {
                        break;
                    }
                    let (stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(_) => break,
                    };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    if let Some(config) = tls_config.as_ref() {
                        let Ok(connection) = rustls::ServerConnection::new(Arc::clone(config))
                        else {
                            continue;
                        };
                        let mut stream = rustls::StreamOwned::new(connection, stream);
                        let _ = serve_fixture_request(&mut stream, &root, live_for_worker.as_ref());
                    } else {
                        let mut stream = stream;
                        let _ = serve_fixture_request(&mut stream, &root, live_for_worker.as_ref());
                    }
                })?;
            Ok((
                Self {
                    address,
                    stop: Some(stop),
                    worker: Some(worker),
                },
                live.map(|(outage, playlist_requests, highest_served_sequence)| {
                    LiveFixtureControl {
                        outage,
                        playlist_requests,
                        highest_served_sequence,
                    }
                }),
            ))
        }

        fn url(&self, scheme: &str, path: &str) -> String {
            format!("{scheme}://127.0.0.1:{}{path}", self.address.port())
        }
    }

    impl Drop for ControlledServer {
        fn drop(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn serve_fixture_request<S: Read + Write>(
        stream: &mut S,
        root: &Path,
        live: Option<&(Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<AtomicUsize>)>,
    ) -> io::Result<()> {
        let Some(request) = read_fixture_request(stream)? else {
            return Ok(());
        };
        let Some(request_line) = request.lines().next() else {
            return Ok(());
        };
        let mut fields = request_line.split_whitespace();
        let Some(method) = fields.next() else {
            return Ok(());
        };
        let Some(request_target) = fields.next() else {
            return Ok(());
        };
        let head = method.eq_ignore_ascii_case("HEAD");
        if !head && !method.eq_ignore_ascii_case("GET") {
            return write_fixture_response(
                stream,
                "405 Method Not Allowed",
                "text/plain",
                b"method not allowed",
                None,
                true,
            );
        }

        let path = request_target
            .split_once('?')
            .map_or(request_target, |(path, _)| path);
        if path.split('/').any(|part| part == "..") {
            return write_fixture_response(
                stream,
                "403 Forbidden",
                "text/plain",
                b"forbidden",
                None,
                head,
            );
        }
        if path.starts_with("/hls-live/") {
            if let Some((outage, playlist_requests, _)) = live {
                if path == "/hls-live/index.m3u8" {
                    let advance = !observe_live_outage(outage);
                    return serve_live_playlist(stream, root, playlist_requests, advance, head);
                }
                if observe_live_outage(outage) {
                    return write_fixture_response(
                        stream,
                        "503 Service Unavailable",
                        "text/plain",
                        b"controlled live outage",
                        None,
                        head,
                    );
                }
            }
        }
        if path == "/redirect.m3u8" {
            return write_fixture_response(
                stream,
                "302 Found",
                "application/vnd.apple.mpegurl",
                &[],
                Some(("Location", "/hls-vod/index.m3u8")),
                head,
            );
        }

        let relative_path = path.trim_start_matches('/');
        let file_path = if let Some(sequence) = live_segment_sequence(path) {
            let segment_count = fs::read_dir(root.join("hls-live"))?
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "ts")
                })
                .count();
            if segment_count == 0 {
                return write_fixture_response(
                    stream,
                    "404 Not Found",
                    "text/plain",
                    b"live fixture has no segments",
                    None,
                    head,
                );
            }
            if sequence >= segment_count {
                return write_fixture_response(
                    stream,
                    "404 Not Found",
                    "text/plain",
                    b"live fixture sequence is not available",
                    None,
                    head,
                );
            }
            root.join(format!("hls-live/segment-{sequence:03}.ts"))
        } else {
            root.join(relative_path)
        };
        let body = match fs::read(file_path) {
            Ok(body) => body,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return write_fixture_response(
                    stream,
                    "404 Not Found",
                    "text/plain",
                    b"not found",
                    None,
                    head,
                );
            }
            Err(error) => return Err(error),
        };
        let content_type = if path.ends_with(".m3u8") {
            "application/vnd.apple.mpegurl"
        } else if path.ends_with(".ts") {
            "video/mp2t"
        } else {
            "application/octet-stream"
        };
        let range = match fixture_header(&request, "Range") {
            Some(value) => match parse_fixture_range(value, body.len()) {
                Ok(range) => range,
                Err(()) => {
                    return write_fixture_response_with_content_range(
                        stream,
                        "416 Range Not Satisfiable",
                        content_type,
                        &[],
                        Some(("Content-Range", format!("bytes */{}", body.len()))),
                        head,
                    );
                }
            },
            None => None,
        };
        let (status, content) = match range {
            Some((start, end)) => {
                let Some(content) = body.get(start..=end) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "validated fixture range was out of bounds",
                    ));
                };
                ("206 Partial Content", content)
            }
            None => ("200 OK", body.as_slice()),
        };
        let content_range = range.map(|(start, end)| {
            (
                "Content-Range",
                format!("bytes {start}-{end}/{}", body.len()),
            )
        });
        let response = write_fixture_response_with_content_range(
            stream,
            status,
            content_type,
            content,
            content_range,
            head,
        );
        if response.is_ok() {
            if let Some((_, _, highest_served_sequence)) = live {
                if let Some(sequence) = live_segment_sequence(path) {
                    highest_served_sequence.fetch_max(sequence.saturating_add(1), Ordering::AcqRel);
                }
            }
        }
        response
    }

    fn observe_live_outage(outage: &AtomicUsize) -> bool {
        let active = outage.load(Ordering::Acquire) != 0;
        if active {
            outage.fetch_max(2, Ordering::AcqRel);
        }
        active
    }

    fn serve_live_playlist<S: Write>(
        stream: &mut S,
        root: &Path,
        playlist_requests: &AtomicUsize,
        advance: bool,
        head: bool,
    ) -> io::Result<()> {
        let segment_count = fs::read_dir(root.join("hls-live"))?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "ts")
            })
            .count();
        if segment_count == 0 {
            return write_fixture_response(
                stream,
                "404 Not Found",
                "text/plain",
                b"live fixture has no segments",
                None,
                head,
            );
        }
        let requested = if advance {
            playlist_requests.fetch_add(1, Ordering::AcqRel)
        } else {
            playlist_requests.load(Ordering::Acquire).saturating_sub(1)
        };
        let window = segment_count.saturating_div(2).max(1);
        let published = window.saturating_add(requested).min(segment_count);
        let start = published.saturating_sub(window);
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n");
        playlist.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{start}\n"));
        for sequence in start..published {
            playlist.push_str("#EXTINF:1.000000,\n");
            playlist.push_str(&format!("segment-{sequence:03}.ts\n"));
        }
        write_fixture_response(
            stream,
            "200 OK",
            "application/vnd.apple.mpegurl",
            playlist.as_bytes(),
            None,
            head,
        )
    }

    fn live_segment_sequence(path: &str) -> Option<usize> {
        path.strip_prefix("/hls-live/")
            .and_then(|name| name.strip_suffix(".ts"))
            .and_then(|name| name.strip_prefix("segment-"))
            .and_then(|sequence| sequence.parse().ok())
    }

    fn read_fixture_request<S: Read>(stream: &mut S) -> io::Result<Option<String>> {
        let mut request = Vec::with_capacity(1024);
        let mut buffer = [0_u8; 1024];
        loop {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let Some(chunk) = buffer.get(..count) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixture request read exceeded buffer",
                ));
            };
            request.extend_from_slice(chunk);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if request.len() > 64 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fixture request headers too large",
                ));
            }
        }
        if request.is_empty() {
            return Ok(None);
        }
        String::from_utf8(request)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    fn fixture_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
        request.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then_some(value.trim())
        })
    }

    fn parse_fixture_range(value: &str, length: usize) -> Result<Option<(usize, usize)>, ()> {
        let value = value.strip_prefix("bytes=").ok_or(())?;
        if value.contains(',') || length == 0 {
            return Err(());
        }
        let (start, end) = value.split_once('-').ok_or(())?;
        if start.is_empty() {
            let suffix = end.parse::<usize>().map_err(|_| ())?;
            if suffix == 0 {
                return Err(());
            }
            let start = length.saturating_sub(suffix);
            return Ok(Some((start, length - 1)));
        }
        let start = start.parse::<usize>().map_err(|_| ())?;
        if start >= length {
            return Err(());
        }
        let end = if end.is_empty() {
            length - 1
        } else {
            end.parse::<usize>().map_err(|_| ())?.min(length - 1)
        };
        if end < start {
            return Err(());
        }
        Ok(Some((start, end)))
    }

    fn write_fixture_response<S: Write>(
        stream: &mut S,
        status: &str,
        content_type: &str,
        body: &[u8],
        extra_header: Option<(&str, &str)>,
        head: bool,
    ) -> io::Result<()> {
        write_fixture_response_with_content_range(
            stream,
            status,
            content_type,
            body,
            extra_header.map(|(name, value)| (name, value.to_owned())),
            head,
        )
    }

    fn write_fixture_response_with_content_range<S: Write>(
        stream: &mut S,
        status: &str,
        content_type: &str,
        body: &[u8],
        extra_header: Option<(&str, String)>,
        head: bool,
    ) -> io::Result<()> {
        let extra_header = extra_header
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra_header}\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes())?;
        if !head {
            for chunk in body.chunks(2048) {
                stream.write_all(chunk)?;
                stream.flush()?;
                thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(())
    }

    fn fixture_root() -> Option<PathBuf> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/generated");
        if root.join("hls-vod/index.m3u8").is_file() {
            Some(root)
        } else {
            eprintln!(
                "skipping network VOD integration test: generated fixture missing; run fixtures/generate.sh"
            );
            None
        }
    }

    struct TestTlsMaterial {
        config: Arc<rustls::ServerConfig>,
        ca_file: PathBuf,
    }

    impl Drop for TestTlsMaterial {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.ca_file);
        }
    }

    fn test_tls_material() -> Result<TestTlsMaterial, Box<dyn std::error::Error>> {
        let mut ca_params =
            rcgen::CertificateParams::new(vec!["lumina-video-gst-test-ca".to_owned()])?;
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = rcgen::KeyPair::generate()?;
        let ca_cert = ca_params.self_signed(&ca_key)?;
        let ca_issuer = rcgen::Issuer::from_params(&ca_params, ca_key);
        let mut leaf_params =
            rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
        leaf_params.key_usages = vec![
            rcgen::KeyUsagePurpose::DigitalSignature,
            rcgen::KeyUsagePurpose::KeyEncipherment,
        ];
        leaf_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let leaf_key = rcgen::KeyPair::generate()?;
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_issuer)?;
        let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
        );
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone(), ca_cert.der().clone()],
            private_key,
        )?;
        let ca_file = std::env::temp_dir().join(format!(
            "lumina-video-gst-test-ca-{}.pem",
            std::process::id()
        ));
        fs::write(&ca_file, ca_cert.pem())?;
        Ok(TestTlsMaterial {
            config: Arc::new(config),
            ca_file,
        })
    }

    fn pump_session_until(
        session: &mut GstMediaSession,
        timeout: Duration,
        mut predicate: impl FnMut(Option<&SessionEventFrame>, &SessionSnapshot) -> bool,
    ) -> Result<bool, SessionError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let event = session.try_next_event()?;
            let snapshot = session.snapshot();
            if predicate(event.as_ref(), &snapshot) {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(false)
    }

    fn is_typed_session_error(
        event: Option<&SessionEventFrame>,
        snapshot: &SessionSnapshot,
        matches_error: impl Fn(&SessionError) -> bool,
    ) -> bool {
        event.is_some_and(
            |event| matches!(event, SessionEvent::Error(error) if matches_error(error)),
        ) || matches!(&snapshot.state, SessionState::Error(error) if matches_error(error))
    }

    fn fail_on_live_error(
        event: Option<&SessionEventFrame>,
        _snapshot: &SessionSnapshot,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(SessionEvent::Error(error)) = event {
            return Err(Box::new(error.clone()));
        }
        if let SessionState::Error(error) = &_snapshot.state {
            return Err(Box::new(error.clone()));
        }
        Ok(())
    }

    fn run_vod_session(
        source: String,
        tls_ca_file: Option<String>,
    ) -> Result<(bool, bool), Box<dyn std::error::Error>> {
        let mut session = match tls_ca_file {
            Some(ca_file) => GstMediaSession::new_for_test_with_tls_ca_file(
                source,
                true,
                GstAudioSinkMode::Fake,
                Duration::from_secs(5),
                0,
                ca_file,
            ),
            None => GstMediaSession::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
                source,
                true,
                GstAudioSinkMode::Fake,
                Duration::from_secs(5),
                0,
            ),
        };
        let mut buffering_seen = false;
        let mut metadata_seen = false;
        let mut frame_seen = false;
        let mut open_error = None;
        let opened =
            pump_session_until(&mut session, Duration::from_secs(20), |event, snapshot| {
                if let Some(SessionEvent::Error(error)) = event {
                    open_error = Some(error.clone());
                }
                if let SessionState::Error(error) = &snapshot.state {
                    open_error = Some(error.clone());
                }
                if open_error.is_some() {
                    return true;
                }
                buffering_seen |= matches!(
                    event,
                    Some(SessionEvent::StateChanged {
                        state: SessionState::Buffering { .. }
                    })
                ) || matches!(&snapshot.state, SessionState::Buffering { .. });
                metadata_seen |= snapshot.metadata.as_ref().is_some_and(|metadata| {
                    metadata
                        .duration
                        .is_some_and(|duration| duration > Duration::ZERO)
                });
                frame_seen |=
                    event.is_some_and(|event| matches!(event, SessionEvent::Frame { .. }));
                metadata_seen && frame_seen
            })?;
        if let Some(error) = open_error {
            return Err(error.into());
        }
        if !opened {
            return Ok((false, buffering_seen));
        }
        session.command(SessionCommand::Pause)?;
        let paused =
            pump_session_until(&mut session, Duration::from_secs(5), |_event, snapshot| {
                matches!(&snapshot.state, SessionState::Paused { .. })
            })?;
        if !paused {
            return Ok((false, buffering_seen));
        }
        let target = Duration::from_millis(750);
        session.command(SessionCommand::Seek { position: target })?;
        let sought =
            pump_session_until(&mut session, Duration::from_secs(5), |_event, snapshot| {
                matches!(
                    &snapshot.state,
                    SessionState::Paused { position }
                        if *position >= target
                            && *position <= target + Duration::from_millis(250)
                )
            })?;
        if !sought {
            return Ok((false, buffering_seen));
        }
        session.command(SessionCommand::Play)?;
        let resumed =
            pump_session_until(&mut session, Duration::from_secs(5), |event, snapshot| {
                buffering_seen |= matches!(
                    event,
                    Some(SessionEvent::StateChanged {
                        state: SessionState::Buffering { .. }
                    })
                ) || matches!(&snapshot.state, SessionState::Buffering { .. });
                matches!(
                    &snapshot.state,
                    SessionState::Playing { position }
                        | SessionState::Buffering { position }
                        if *position >= target
                )
            })?;
        Ok((resumed, buffering_seen))
    }

    #[test]
    fn public_network_vod_handles_redirect_https_and_typed_failures(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let Some(root) = fixture_root() else {
            return Ok(());
        };
        let http_server = ControlledServer::spawn(root.clone(), None)?;
        let tls_material = test_tls_material()?;
        let https_server = ControlledServer::spawn(root, Some(Arc::clone(&tls_material.config)))?;

        let (http_ok, http_buffering) =
            run_vod_session(http_server.url("http", "/redirect.m3u8"), None)?;
        assert!(http_ok, "HTTP redirect HLS VOD did not complete");

        let mut invalid_tls =
            GstMediaSession::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
                https_server.url("https", "/hls-vod/index.m3u8"),
                true,
                GstAudioSinkMode::Fake,
                Duration::from_secs(5),
                0,
            );
        let tls_seen = pump_session_until(
            &mut invalid_tls,
            Duration::from_secs(10),
            |event, snapshot| {
                is_typed_session_error(event, snapshot, |error| {
                    matches!(error, SessionError::Tls(_))
                })
            },
        )?;
        let invalid_state = invalid_tls.snapshot().state;
        assert!(
            tls_seen,
            "invalid certificate did not report SessionError::Tls; final state: {invalid_state:?}"
        );

        let (https_ok, https_buffering) = run_vod_session(
            https_server.url("https", "/hls-vod/index.m3u8"),
            Some(tls_material.ca_file.to_string_lossy().into_owned()),
        )?;
        assert!(https_ok, "trusted HTTPS HLS VOD did not complete");
        assert!(
            http_buffering || https_buffering,
            "network sessions never published a buffering state"
        );

        let unavailable = TcpListener::bind(("127.0.0.1", 0))?;
        let unavailable_port = unavailable.local_addr()?.port();
        drop(unavailable);
        let mut unreachable =
            GstMediaSession::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
                format!("http://127.0.0.1:{unavailable_port}/hls-vod/index.m3u8"),
                true,
                GstAudioSinkMode::Fake,
                Duration::from_secs(5),
                0,
            );
        let network_seen = pump_session_until(
            &mut unreachable,
            Duration::from_secs(10),
            |event, snapshot| {
                is_typed_session_error(event, snapshot, |error| {
                    matches!(error, SessionError::Network(_))
                })
            },
        )?;
        let unreachable_state = unreachable.snapshot().state;
        assert!(
            network_seen,
            "unreachable HTTP endpoint did not report SessionError::Network; final state: {unreachable_state:?}"
        );
        Ok(())
    }

    #[test]
    fn public_live_hls_observes_live_edge_and_recovers_at_a_higher_sequence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let root = fixture_root().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "generated HLS fixture missing; run fixtures/generate.sh",
            )
        })?;
        if !root.join("hls-live/index.m3u8").is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "generated live HLS fixture missing; run fixtures/generate.sh",
            )
            .into());
        }
        let live_segment_count = fs::read_dir(root.join("hls-live"))?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "ts")
            })
            .count();
        if live_segment_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "generated live HLS fixture has no media segments",
            )
            .into());
        }
        // The fixture's EXTINF duration is one second; use the full generated
        // media duration as an observation bound, never a desired target.
        let latency_bound = Duration::from_secs(live_segment_count as u64);
        let (server, live_control) = ControlledServer::spawn_live(root)?;
        let mut session =
            GstMediaSession::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
                server.url("http", "/hls-live/index.m3u8"),
                true,
                GstAudioSinkMode::Fake,
                Duration::from_secs(2),
                0,
            );
        let startup_deadline = Instant::now() + Duration::from_secs(15);
        let mut last_frame_pts = None;
        while Instant::now() < startup_deadline {
            let event = session.try_next_event()?;
            let snapshot = session.snapshot();
            fail_on_live_error(event.as_ref(), &snapshot)?;
            if let Some(SessionEvent::Frame { pts, .. }) = event {
                last_frame_pts = Some(pts);
            }
            let observation = session.gst_observation();
            if last_frame_pts.is_some()
                && observation.is_live_known
                && observation.is_live
                && observation.seekability_known
            {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        let pre_gap_frame = last_frame_pts.ok_or_else(|| {
            io::Error::new(io::ErrorKind::TimedOut, "live HLS did not produce a frame")
        })?;
        let pre_gap_sequence = live_control.highest_served_sequence().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "live HLS did not serve a media segment",
            )
        })?;
        let observation = session.gst_observation();
        assert!(
            observation.is_live_known,
            "GStreamer live query did not complete"
        );
        assert!(
            observation.is_live,
            "GStreamer did not report a live pipeline"
        );
        assert!(observation.seekability_known);
        assert!(observation.frame_mailbox_occupancy <= FRAME_QUEUE_CAPACITY);
        assert_eq!(observation.dropped_frames, session.dropped_frame_count());
        let initial_pipeline_latency = observation.pipeline_latency.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "GStreamer did not report pipeline latency for live HLS",
            )
        })?;
        assert!(initial_pipeline_latency <= latency_bound);
        assert!(
            observation.seekable,
            "sliding live HLS did not report its DVR window as seekable"
        );
        let initial_qos_events = observation.qos_events;
        let initial_dropped_frames = observation.dropped_frames;

        let dropped_before_poll_pause = session.dropped_frame_count();
        thread::sleep(Duration::from_millis(250));
        let dropped_after_poll_pause = session.dropped_frame_count();
        assert!(
            dropped_after_poll_pause > dropped_before_poll_pause,
            "live mailbox did not drop a frame while polling was paused"
        );
        let _ = session.try_next_event()?;

        live_control.set_outage(true);
        let outage_deadline = Instant::now() + Duration::from_millis(1250);
        while Instant::now() < outage_deadline {
            let event = session.try_next_event()?;
            let snapshot = session.snapshot();
            fail_on_live_error(event.as_ref(), &snapshot)?;
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            live_control.outage_seen(),
            "controlled live outage did not handle a request"
        );
        live_control.set_outage(false);
        let recovery_deadline = Instant::now() + Duration::from_secs(10);
        let mut recovery_sequence = None;
        let mut recovery_sequence_observed_at = None;
        let mut recovered = false;
        while Instant::now() < recovery_deadline {
            let event_observed_at = Instant::now();
            let event = session.try_next_event()?;
            let snapshot = session.snapshot();
            fail_on_live_error(event.as_ref(), &snapshot)?;
            if recovery_sequence.is_none() {
                if let Some(sequence) = live_control.highest_served_sequence() {
                    if sequence > pre_gap_sequence {
                        recovery_sequence = Some(sequence);
                        recovery_sequence_observed_at = Some(Instant::now());
                    }
                }
            }
            if let Some(SessionEvent::Frame { pts, .. }) = event {
                recovered = recovery_sequence_observed_at
                    .is_some_and(|observed_at| event_observed_at >= observed_at)
                    && pts > pre_gap_frame;
            }
            let observation = session.gst_observation();
            assert!(observation.frame_mailbox_occupancy <= FRAME_QUEUE_CAPACITY);
            if recovered {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            recovery_sequence.is_some_and(|sequence| sequence > pre_gap_sequence),
            "live recovery did not serve a higher media sequence"
        );
        assert!(
            recovered,
            "live recovery did not deliver a later frame after the higher sequence response"
        );
        let recovered_observation = session.gst_observation();
        assert!(recovered_observation.frame_mailbox_occupancy <= FRAME_QUEUE_CAPACITY);
        assert!(recovered_observation.qos_events >= initial_qos_events);
        assert!(recovered_observation.dropped_frames >= initial_dropped_frames);
        let recovered_pipeline_latency =
            recovered_observation.pipeline_latency.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "GStreamer stopped reporting live HLS pipeline latency",
                )
            })?;
        assert!(recovered_pipeline_latency <= latency_bound);
        Ok(())
    }

    fn test_frame_with_generation(frame_id: u64, stream_generation: u64) -> Frame {
        match NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id,
                stream_generation,
                pts: Duration::from_millis(frame_id),
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: lumina_video_native_frame::video::PixelFormat::Rgba,
                color: lumina_video_native_frame::ColorMetadata::default(),
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

    fn cpu_color() -> ColorMetadata {
        ColorMetadata {
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: ColorTransfer::Srgb,
            range: ColorRange::Limited,
            chroma_horizontal: ChromaHorizontal::Cosited,
            chroma_vertical: ChromaVertical::Centered,
        }
    }

    #[test]
    fn color_decision_is_once_per_generation_and_caps_change_rebuilds_pool() {
        let mut generation = None;
        let extent = FrameExtent::new(2, 2);
        let decision = lumina_video_native_frame::render_decision(cpu_color());
        assert!(ensure_color_generation(&mut generation, extent, cpu_color(), decision).is_ok());
        let Some(old_first) = generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
        else {
            panic!("first generation payload must be available");
        };
        assert!(ensure_color_generation(&mut generation, extent, cpu_color(), decision).is_ok());
        let Some(old_second) = generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
        else {
            panic!("same generation must retain its second payload");
        };
        assert!(generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
            .is_none());
        let changed = ensure_color_generation(
            &mut generation,
            FrameExtent::new(4, 2),
            cpu_color(),
            decision,
        );
        assert!(changed.is_ok());
        let Some(new_first) = generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
        else {
            panic!("new generation first payload must be available");
        };
        let Some(new_second) = generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
        else {
            panic!("new generation second payload must be available");
        };
        assert_eq!(
            new_first.planes.first().map(|plane| plane.bytes.len()),
            Some(4 * 4 * 2)
        );
        assert!(generation
            .as_ref()
            .and_then(|current| current.rgba_pool.as_ref())
            .and_then(RgbaPool::try_acquire)
            .is_none());
        drop(new_first);
        drop(new_second);
        drop(old_first);
        drop(old_second);
        let unsupported = ColorMetadata {
            matrix: ColorMatrix::Unknown,
            ..cpu_color()
        };
        let unsupported_decision = lumina_video_native_frame::render_decision(unsupported);
        assert!(matches!(
            ensure_color_generation(&mut generation, extent, unsupported, unsupported_decision),
            Err(ColorGenerationError::Unsupported)
        ));
    }

    #[test]
    fn cpu_pool_exhaustion_drops_without_allocating_a_third_payload() {
        let mut generation = None;
        let color = cpu_color();
        let extent = FrameExtent::new(2, 2);
        let decision = lumina_video_native_frame::render_decision(color);
        assert!(ensure_color_generation(&mut generation, extent, color, decision).is_ok());
        let source = || {
            NativeMemory::Cpu(CpuMemory::new(vec![
                CpuPlane::new(vec![16, 235, 16, 235], 2),
                CpuPlane::new(vec![128, 128], 2),
            ]))
        };
        let first = prepare_frame_memory(
            PixelFormat::Nv12,
            extent,
            color,
            decision,
            source(),
            &mut generation,
        );
        let second = prepare_frame_memory(
            PixelFormat::Nv12,
            extent,
            color,
            decision,
            source(),
            &mut generation,
        );
        let third = prepare_frame_memory(
            PixelFormat::Nv12,
            extent,
            color,
            decision,
            source(),
            &mut generation,
        );
        assert!(matches!(first, Ok(Some((PixelFormat::Rgba, _)))));
        assert!(matches!(second, Ok(Some((PixelFormat::Rgba, _)))));
        assert!(matches!(third, Ok(None)));
    }

    #[test]
    fn rgba_pool_reuses_exactly_two_payload_identities() {
        let Some(pool) = RgbaPool::new(FrameExtent::new(2, 2)) else {
            panic!("small RGBA pool must configure");
        };
        let Some(first) = pool.try_acquire() else {
            panic!("first payload must be available");
        };
        let Some(second) = pool.try_acquire() else {
            panic!("second payload must be available");
        };
        let first_identity = first
            .planes
            .first()
            .map(|plane| (plane.bytes.as_ptr(), plane.bytes.capacity()));
        let second_identity = second
            .planes
            .first()
            .map(|plane| (plane.bytes.as_ptr(), plane.bytes.capacity()));
        assert_ne!(first_identity, second_identity);
        assert!(pool.try_acquire().is_none());
        drop(first);
        let Some(recycled_first) = pool.try_acquire() else {
            panic!("dropped payload must recycle");
        };
        assert_eq!(
            recycled_first
                .planes
                .first()
                .map(|plane| (plane.bytes.as_ptr(), plane.bytes.capacity())),
            first_identity
        );
        drop(recycled_first);
        drop(second);
        let Some(final_first) = pool.try_acquire() else {
            panic!("first final payload must be available");
        };
        let Some(final_second) = pool.try_acquire() else {
            panic!("second final payload must be available");
        };
        assert!(pool.try_acquire().is_none());
        drop(final_first);
        drop(final_second);
    }

    #[test]
    fn rgba_pool_rejects_mutated_recycled_shape() {
        let Some(pool) = RgbaPool::new(FrameExtent::new(2, 2)) else {
            panic!("small RGBA pool must configure");
        };
        let Some(mut memory) = pool.try_acquire() else {
            panic!("first payload must be available");
        };
        let Some(second) = pool.try_acquire() else {
            panic!("second payload must be available");
        };
        let Some(plane) = memory.planes.first_mut() else {
            panic!("payload must have one plane");
        };
        let _ = plane.bytes.pop();
        drop(memory);
        assert!(matches!(
            pool.try_acquire_checked(),
            Err("RGBA recycle payload shape changed")
        ));
        drop(second);
    }

    #[test]
    fn rgba_pool_recycles_native_lease_success_and_error_drops() {
        let Some(pool) = RgbaPool::new(FrameExtent::new(1, 1)) else {
            panic!("small RGBA pool must configure");
        };
        let Some(memory) = pool.try_acquire() else {
            panic!("payload must be available");
        };
        let lease = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 1,
                stream_generation: 1,
                pts: Duration::ZERO,
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: PixelFormat::Rgba,
                color: ColorMetadata::default(),
            },
            NativeMemory::Cpu(memory),
            AcquireSync::None,
        );
        assert!(lease.is_ok());
        drop(lease);
        assert!(pool.try_acquire().is_some());

        let Some(memory) = pool.try_acquire() else {
            panic!("recycled payload must be available");
        };
        let invalid = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 2,
                stream_generation: 1,
                pts: Duration::ZERO,
                duration: None,
                extent: FrameExtent::new(0, 1),
                format: PixelFormat::Rgba,
                color: ColorMetadata::default(),
            },
            NativeMemory::Cpu(memory),
            AcquireSync::None,
        );
        assert!(invalid.is_err());
        assert!(pool.try_acquire().is_some());
    }

    #[test]
    fn playback_state_reports_buffering_without_changing_position() {
        let buffering = PlaybackState {
            playing: true,
            buffering: true,
            position: Duration::from_millis(750),
            stream_generation: 3,
        };
        assert!(matches!(
            playback_session_state(&buffering),
            SessionState::Buffering { position } if position == Duration::from_millis(750)
        ));

        let playing = PlaybackState {
            buffering: false,
            ..buffering
        };
        assert!(matches!(
            playback_session_state(&playing),
            SessionState::Playing { position } if position == Duration::from_millis(750)
        ));
    }

    #[test]
    fn buffering_edges_are_single_shot_and_keep_paused_intent() {
        let mut playback = PlaybackState {
            playing: true,
            buffering: false,
            position: Duration::from_millis(750),
            stream_generation: 3,
        };
        assert!(matches!(
            apply_buffering_state(&mut playback, true),
            Some(SessionState::Buffering { position })
                if position == Duration::from_millis(750)
        ));
        assert!(apply_buffering_state(&mut playback, true).is_none());

        playback.playing = false;
        assert!(matches!(
            apply_buffering_state(&mut playback, false),
            Some(SessionState::Paused { position })
                if position == Duration::from_millis(750)
        ));
    }

    #[test]
    fn nonterminal_error_does_not_change_snapshot_state() {
        let state = Arc::new(SnapshotState::new());
        let (control_sender, control_receiver) = control_channels();
        let mut sequence = 7;

        assert!(publish_nonterminal_error(
            &control_sender,
            &mut sequence,
            SessionError::Unsupported("not seekable".into()),
        ));
        assert_eq!(sequence, 8);
        assert!(matches!(
            &state.snapshot.read().state,
            SessionState::Loading
        ));

        match control_receiver.error.try_recv() {
            Ok(event) => {
                assert_eq!(event.sequence, 7);
                assert!(matches!(
                    event.event,
                    SessionEvent::Error(SessionError::Unsupported(message))
                        if message == "not seekable"
                ));
            }
            Err(error) => panic!("nonterminal error event missing: {error}"),
        }
    }

    #[test]
    fn capability_snapshot_publishes_only_transitions() {
        let state = SnapshotState::with_capability(CapabilityTier::DirectAlias);
        let mut published = CapabilityTier::DirectAlias;

        assert!(!publish_capability_if_changed(
            &state,
            &mut published,
            CapabilityTier::DirectAlias
        ));
        assert_eq!(
            state.snapshot_with_atomics().capability,
            CapabilityTier::DirectAlias
        );

        assert!(publish_capability_if_changed(
            &state,
            &mut published,
            CapabilityTier::SystemMemoryUpload
        ));
        assert_eq!(published, CapabilityTier::SystemMemoryUpload);
        assert_eq!(
            state.snapshot_with_atomics().capability,
            CapabilityTier::SystemMemoryUpload
        );
        assert!(!publish_capability_if_changed(
            &state,
            &mut published,
            CapabilityTier::SystemMemoryUpload
        ));
    }

    #[test]
    fn presentation_report_commits_capability_only_after_acceptance() {
        let state = SnapshotState::with_capability(CapabilityTier::DirectAlias);
        record_renderer_outcome(&state, RendererOutcome::Unsupported);
        record_downgrade_reason(&state, CapabilityDowngradeReason::RendererUnsupported);
        let snapshot = state.snapshot_with_atomics();
        assert_eq!(
            snapshot.latest_renderer_outcome,
            Some(RendererOutcome::Unsupported)
        );
        assert_eq!(
            snapshot.latest_downgrade_reason,
            Some(CapabilityDowngradeReason::RendererUnsupported)
        );
        assert_eq!(
            state.snapshot_with_atomics().capability,
            CapabilityTier::DirectAlias
        );

        let realization = FrameRealization {
            decode: DecodeMode::Hardware,
            residency: lumina_video_core::session::DecodeResidency::SystemMemory,
            import: lumina_video_core::session::ImportMode::CpuUpload,
            conversion: lumina_video_core::session::ConversionMode::YuvShader,
            synchronization: lumina_video_core::session::SynchronizationMode::CpuWait,
        };
        record_renderer_outcome(&state, RendererOutcome::Accepted);
        record_realization(&state, realization);
        let snapshot = state.snapshot_with_atomics();
        assert_eq!(snapshot.capability, CapabilityTier::SystemMemoryUpload);
        assert_eq!(snapshot.frame_realization, Some(realization));
        assert_eq!(
            snapshot.latest_downgrade_reason,
            Some(CapabilityDowngradeReason::RendererUnsupported)
        );
    }

    #[test]
    fn live_gap_deadline_arms_once_after_media_and_expires_without_buffering_reset() {
        let base = Instant::now();
        let timeout = Duration::from_secs(2);
        let mut gap = LiveGapDeadline::default();

        assert!(!gap.expired(base, true, true, timeout));
        assert!(gap.deadline.is_none());
        gap.observe_media_progress(false, true, true);
        assert!(!gap.live_media_seen);
        gap.observe_media_progress(true, true, true);
        assert!(!gap.expired(base, true, true, timeout));
        let deadline = gap.deadline;
        assert_eq!(deadline, Some(base + timeout));

        // A buffering poll leaves the same absolute deadline in place.
        assert!(!gap.expired(base + Duration::from_secs(1), true, true, timeout));
        assert_eq!(gap.deadline, deadline);

        // A later audio/video progress update clears the old deadline, so
        // continuous media cannot be mistaken for a gap.
        gap.observe_media_progress(true, true, true);
        assert!(gap.deadline.is_none());
        assert!(!gap.expired(base + Duration::from_secs(1), true, true, timeout));
        assert_eq!(gap.deadline, Some(base + Duration::from_secs(3)));

        // Pausing disarms the old deadline; resuming starts a fresh absolute
        // timeout instead of inheriting time spent paused.
        assert!(!gap.expired(base + Duration::from_secs(2), false, true, timeout));
        assert!(gap.deadline.is_none());
        assert!(!gap.expired(base + Duration::from_secs(2), true, true, timeout));
        assert_eq!(gap.deadline, Some(base + Duration::from_secs(4)));
        assert!(gap.expired(base + Duration::from_secs(4), true, true, timeout));
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
    fn frame_mailbox_stays_single_slot_while_control_event_is_pending() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let state = Arc::new(SnapshotState::new());
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let audio_handle = AudioHandle::new();
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            frame_receiver,
            state,
            audio_handle,
            dropped_frames: Arc::clone(&dropped_frames),
            last_delivered_sequence: None,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(send_frame(
            &frame_sender,
            &frame_drop_receiver,
            SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame(1),
                },
            },
            &dropped_frames,
        ));
        assert!(send_frame(
            &frame_sender,
            &frame_drop_receiver,
            SequencedEvent {
                sequence: 2,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(2),
                    frame: test_frame(2),
                },
            },
            &dropped_frames,
        ));
        assert_eq!(dropped_frames.load(Ordering::Relaxed), 1);
        assert_eq!(session.frame_receiver.len(), FRAME_QUEUE_CAPACITY);

        assert!(send_control(
            &control_sender,
            SequencedEvent {
                sequence: 0,
                event: SessionEvent::StateChanged {
                    state: SessionState::Ready,
                },
            }
        ));

        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::StateChanged {
                state: SessionState::Ready
            }))
        ));
        assert_eq!(session.frame_receiver.len(), FRAME_QUEUE_CAPACITY);
        assert_eq!(session.gst_observation().frame_mailbox_occupancy, 1);
        let next = session.try_next_event();
        assert!(matches!(
            next,
            Ok(Some(SessionEvent::Frame { frame, .. })) if frame.descriptor.frame_id == 2
        ));
        assert_eq!(session.frame_receiver.len(), 0);
        assert_eq!(session.gst_observation().frame_mailbox_occupancy, 0);
        assert_eq!(dropped_frames.load(Ordering::Relaxed), 1);

        drop(frame_drop_receiver);
        drop(frame_sender);
        assert!(matches!(session.try_next_event(), Ok(None)));
        assert!(session.worker_disconnected);
    }

    #[test]
    fn control_sequence_drops_older_queued_frame_and_allows_later_frame() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let dropped_frames = Arc::new(AtomicU64::new(0));
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::clone(&dropped_frames),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(send_frame(
            &frame_sender,
            &frame_drop_receiver,
            SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame(1),
                },
            },
            &dropped_frames,
        ));
        assert!(send_control(
            &control_sender,
            SequencedEvent {
                sequence: 2,
                event: SessionEvent::StateChanged {
                    state: SessionState::Ready,
                },
            }
        ));
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::StateChanged {
                state: SessionState::Ready
            }))
        ));
        assert_eq!(session.frame_receiver.len(), 1);
        assert!(matches!(session.try_next_event(), Ok(None)));
        assert_eq!(dropped_frames.load(Ordering::Relaxed), 1);
        assert_eq!(session.frame_receiver.len(), 0);

        assert!(send_frame(
            &frame_sender,
            &frame_drop_receiver,
            SequencedEvent {
                sequence: 3,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(3),
                    frame: test_frame(3),
                },
            },
            &dropped_frames,
        ));
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Frame { frame, .. })) if frame.descriptor.frame_id == 3
        ));
        assert_eq!(session.last_delivered_sequence, Some(3));
    }

    #[test]
    fn eos_marks_playback_idle_for_restart() {
        let mut playback = PlaybackState {
            playing: true,
            buffering: false,
            position: Duration::from_secs(1),
            stream_generation: 0,
        };
        mark_eos(&mut playback);
        assert!(!playback.playing);
        assert_eq!(playback.position, Duration::from_secs(1));
    }

    #[test]
    fn stale_frames_are_dropped_after_generation_advance() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 1,
            latest_requested_generation: 1,
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
    fn queued_seek_arms_generation_without_waiting_for_worker() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: Duration::from_millis(50),
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 4,
            latest_requested_generation: 4,
            replay_pending: false,
        };

        assert!(session
            .command(SessionCommand::Seek {
                position: Duration::from_secs(1),
            })
            .is_ok());
        assert_eq!(session.stream_generation(), 4);
        assert_eq!(session.latest_requested_generation, 5);
        assert!(matches!(
            generation_receiver.try_recv(),
            Ok(GenerationIntent {
                command: SessionCommand::Seek { position },
                target_generation: 5,
            }) if position == Duration::from_secs(1)
        ));
    }

    #[test]
    fn query_confirmed_nonseekable_seek_is_immediate_and_nonterminal() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let generation_drop_receiver = generation_receiver.clone();
        let (_control_sender, control_receiver) = control_channels();
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let state = Arc::new(SnapshotState::new());
        state.is_live.store(true, Ordering::Relaxed);
        state.is_live_known.store(true, Ordering::Release);
        state.seekable.store(false, Ordering::Relaxed);
        state.seekability_known.store(true, Ordering::Release);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver,
            control_receiver,
            frame_receiver,
            state: Arc::clone(&state),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(matches!(
            session.command(SessionCommand::Seek {
                position: Duration::from_secs(1),
            }),
            Err(SessionError::Unsupported(message))
                if message == "GStreamer reported a non-seekable stream"
        ));
        assert!(matches!(
            generation_receiver.try_recv(),
            Err(TryRecvError::Empty)
        ));
        assert!(!matches!(
            state.snapshot.read().state,
            SessionState::Error(_)
        ));
    }

    #[test]
    fn duplicate_play_while_ended_keeps_one_replay_generation_pending() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let state = Arc::new(SnapshotState::new());
        state.snapshot.write().state = SessionState::Ended;
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            frame_receiver,
            state,
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 7,
            latest_requested_generation: 7,
            replay_pending: false,
        };

        assert!(session.command(SessionCommand::Play).is_ok());
        assert_eq!(session.stream_generation(), 7);
        assert_eq!(session.latest_requested_generation, 8);
        assert!(session.replay_pending);

        assert!(session.command(SessionCommand::Play).is_ok());
        assert_eq!(session.stream_generation(), 7);
        assert!(session.replay_pending);
    }

    #[test]
    fn full_command_queue_drops_oldest_and_keeps_latest_command() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            assert!(commands.send(SessionCommand::Pause).is_ok());
        }
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(session.command(SessionCommand::Play).is_ok());
        let mut queued = Vec::new();
        while let Ok(command) = command_receiver.try_recv() {
            queued.push(command);
        }
        assert_eq!(queued.len(), COMMAND_QUEUE_CAPACITY);
        assert!(matches!(queued.last(), Some(SessionCommand::Play)));
    }

    #[test]
    fn stop_cancels_without_waiting_for_a_full_command_queue() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            assert!(commands.send(SessionCommand::Pause).is_ok());
        }
        let lifecycle_control = GstLifecycleControl::new();
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: Duration::from_secs(2),
            lifecycle_control: lifecycle_control.clone(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(session.command(SessionCommand::Stop).is_ok());
        assert!(lifecycle_control.is_cancelled());
        assert!(lifecycle_control.is_stop_requested());
        assert!(lifecycle_control.deadline().is_some());
    }

    #[test]
    fn control_lanes_coalesce_transient_states_and_keep_terminal_events() {
        let (sender, receiver) = control_channels();
        for sequence in 0..8_u64 {
            assert!(send_control(
                &sender,
                SequencedEvent {
                    sequence,
                    event: SessionEvent::StateChanged {
                        state: SessionState::Playing {
                            position: Duration::from_millis(sequence),
                        },
                    },
                }
            ));
        }
        assert!(send_control(
            &sender,
            SequencedEvent {
                sequence: 8,
                event: SessionEvent::StateChanged {
                    state: SessionState::Paused {
                        position: Duration::from_millis(750),
                    },
                },
            }
        ));
        assert!(send_control(
            &sender,
            SequencedEvent {
                sequence: 100,
                event: SessionEvent::Metadata {
                    metadata: SessionMetadata {
                        width: 1,
                        height: 1,
                        duration: Some(Duration::from_secs(2)),
                        frame_rate: 30.0,
                        codec: "test".into(),
                        pixel_aspect_ratio: 1.0,
                        start_time: None,
                    },
                },
            }
        ));
        assert!(send_control(
            &sender,
            SequencedEvent {
                sequence: 101,
                event: SessionEvent::Error(SessionError::Open("pressure".into())),
            }
        ));
        assert!(send_control(
            &sender,
            SequencedEvent {
                sequence: 102,
                event: SessionEvent::Ended,
            }
        ));

        let mut events = Vec::new();
        while let Ok(event) = receiver.metadata.try_recv() {
            events.push(event.event);
        }
        while let Ok(event) = receiver.state.try_recv() {
            events.push(event.event);
        }
        while let Ok(event) = receiver.error.try_recv() {
            events.push(event.event);
        }
        while let Ok(event) = receiver.terminal.try_recv() {
            events.push(event.event);
        }
        while let Ok(event) = receiver.transient.try_recv() {
            events.push(event.event);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateChanged {
                state: SessionState::Paused { position }
            } if *position == Duration::from_millis(750)
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Metadata { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Error(SessionError::Open(_)))));
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Ended)));
    }

    #[test]
    fn terminal_and_error_lanes_keep_latest_unpolled_events() {
        let (sender, control_receiver) = control_channels();
        for sequence in 0..16_u64 {
            assert!(send_control(
                &sender,
                SequencedEvent {
                    sequence: sequence * 3,
                    event: SessionEvent::Error(SessionError::Open(format!("error-{sequence}"))),
                }
            ));
            assert!(send_control(
                &sender,
                SequencedEvent {
                    sequence: sequence * 3 + 1,
                    event: SessionEvent::StateChanged {
                        state: SessionState::Ended,
                    },
                }
            ));
            assert!(send_control(
                &sender,
                SequencedEvent {
                    sequence: sequence * 3 + 2,
                    event: SessionEvent::Ended,
                }
            ));
        }

        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver: command_receiver.clone(),
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Error(SessionError::Open(message))))
                if message == "error-15"
        ));
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::StateChanged {
                state: SessionState::Ended,
            }))
        ));
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Ended))
        ));
    }

    #[test]
    fn seek_lane_survives_unrelated_command_pressure_and_accepts_new_generation() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(session
            .command(SessionCommand::Seek {
                position: Duration::from_millis(750),
            })
            .is_ok());
        for _ in 0..(COMMAND_QUEUE_CAPACITY + 8) {
            assert!(session.command(SessionCommand::Pause).is_ok());
        }
        assert_eq!(session.stream_generation(), 0);
        assert_eq!(session.latest_requested_generation, 1);
        assert!(matches!(
            generation_receiver.try_recv(),
            Ok(GenerationIntent {
                command: SessionCommand::Seek { position },
                target_generation: 1,
            }) if position == Duration::from_millis(750)
        ));
        let mut ordinary_count = 0;
        while let Ok(SessionCommand::Pause) = command_receiver.try_recv() {
            ordinary_count += 1;
        }
        assert_eq!(ordinary_count, COMMAND_QUEUE_CAPACITY);

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 0,
                event: SessionEvent::Frame {
                    pts: Duration::ZERO,
                    frame: test_frame_with_generation(0, 0),
                },
            })
            .is_ok());
        assert!(matches!(session.try_next_event(), Ok(None)));
        assert_eq!(session.dropped_frame_count(), 1);

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(1),
                    frame: test_frame_with_generation(1, 1),
                },
            })
            .is_ok());
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Frame { frame, .. }))
                if frame.descriptor.stream_generation == 1
        ));
        assert_eq!(session.stream_generation(), 1);
        assert_eq!(session.latest_requested_generation, 1);

        session.latest_requested_generation = session.stream_generation.wrapping_add(1);
        assert!(send_control(
            &control_sender,
            SequencedEvent {
                sequence: 2,
                event: SessionEvent::Error(SessionError::Seek("timeout".into())),
            }
        ));
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Error(SessionError::Seek(message))))
                if message == "timeout"
        ));
        assert_eq!(
            session.latest_requested_generation,
            session.stream_generation
        );
    }

    #[test]
    fn newer_seek_token_rejects_an_inflight_older_frame() {
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (_control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let mut session = GstMediaSession {
            commands,
            command_drop_receiver,
            generation_commands,
            generation_drop_receiver: generation_receiver.clone(),
            control_receiver,
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            frame_receiver,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            last_delivered_sequence: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
            replay_pending: false,
        };

        assert!(session
            .command(SessionCommand::Seek {
                position: Duration::from_millis(100),
            })
            .is_ok());
        assert!(matches!(
            generation_receiver.try_recv(),
            Ok(GenerationIntent {
                command: SessionCommand::Seek { position },
                target_generation: 1,
            }) if position == Duration::from_millis(100)
        ));

        assert!(session
            .command(SessionCommand::Seek {
                position: Duration::from_millis(200),
            })
            .is_ok());
        assert_eq!(session.latest_requested_generation, 2);

        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 0,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(100),
                    frame: test_frame_with_generation(1, 1),
                },
            })
            .is_ok());
        assert!(matches!(session.try_next_event(), Ok(None)));
        assert_eq!(session.dropped_frame_count(), 1);

        assert!(matches!(
            generation_receiver.try_recv(),
            Ok(GenerationIntent {
                command: SessionCommand::Seek { position },
                target_generation: 2,
            }) if position == Duration::from_millis(200)
        ));
        assert!(frame_sender
            .send(SequencedEvent {
                sequence: 1,
                event: SessionEvent::Frame {
                    pts: Duration::from_millis(200),
                    frame: test_frame_with_generation(2, 2),
                },
            })
            .is_ok());
        assert!(matches!(
            session.try_next_event(),
            Ok(Some(SessionEvent::Frame { frame, .. }))
                if frame.descriptor.stream_generation == 2
        ));
        assert_eq!(session.stream_generation(), 2);
    }

    #[test]
    fn worker_spawn_failure_is_observable_as_open_error() {
        let state = Arc::new(SnapshotState::new());
        let (sender, receiver) = control_channels();

        seed_worker_spawn_failure(&state, &sender);

        assert!(matches!(
            &state.snapshot.read().state,
            SessionState::Error(SessionError::Open(_))
        ));
        assert!(matches!(
            receiver.error.try_recv(),
            Ok(SequencedEvent {
                event: SessionEvent::Error(SessionError::Open(_)),
                ..
            })
        ));
    }

    #[test]
    fn audio_track_selection_failure_is_nonterminal_and_explicit() {
        let state = Arc::new(SnapshotState::new());
        let (sender, receiver) = control_channels();
        let mut sequence = 0;

        assert!(publish_audio_selection_failed(
            &state,
            &sender,
            &mut sequence,
            "audio-missing".into(),
            None,
            "rollback failed; current selection unknown".into(),
        ));
        assert!(matches!(
            &state.snapshot.read().state,
            SessionState::Loading
        ));
        assert!(matches!(
            receiver.audio_selection.try_recv(),
            Ok(SequencedEvent {
                event: SessionEvent::AudioTrackSelectionFailed {
                    requested_id,
                    prior_restored_id: None,
                    reason,
                },
                ..
            }) if requested_id == "audio-missing" && reason.contains("current selection unknown")
        ));
    }

    #[test]
    fn audio_track_snapshot_and_selection_event_share_confirmed_id() {
        let state = Arc::new(SnapshotState::new());
        let (sender, receiver) = control_channels();
        let track = AudioTrack {
            id: "audio-eng".into(),
            language: Some("eng".into()),
            title: Some("English".into()),
            codec: "AAC".into(),
        };
        let mut sequence = 0;

        assert!(publish_audio_tracks(
            &state,
            &sender,
            &mut sequence,
            vec![track.clone()],
            Some(track.id.clone()),
        ));
        assert!(publish_audio_selected(
            &state,
            &sender,
            &mut sequence,
            track.clone()
        ));
        assert_eq!(
            state.snapshot.read().selected_audio_track_id.as_deref(),
            Some("audio-eng")
        );
        assert!(matches!(
            receiver.audio_tracks.try_recv(),
            Ok(SequencedEvent {
                event: SessionEvent::AudioTracks { selected_id: Some(id), .. },
                ..
            }) if id == "audio-eng"
        ));
        assert!(matches!(
            receiver.audio_selection.try_recv(),
            Ok(SequencedEvent {
                event: SessionEvent::AudioTrackSelected { track: selected },
                ..
            }) if selected == track
        ));
    }
}
