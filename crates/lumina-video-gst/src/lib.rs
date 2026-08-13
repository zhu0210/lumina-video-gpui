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
pub use lumina_video_core::session::{AudioObservation, AudioTrack};
use lumina_video_core::session::{
    CapabilityTier, MediaSession, SessionCommand, SessionError, SessionEvent, SessionMetadata,
    SessionSnapshot, SessionState,
};
pub use lumina_video_native_frame::linux_video_gst::GstAudioSinkMode;
use lumina_video_native_frame::linux_video_gst::{
    AudioTrackSelectionResult, GStreamerDecoder, GstLifecycleControl, DEFAULT_LIFECYCLE_TIMEOUT,
};
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
const COMMAND_QUEUE_CAPACITY: usize = 32;

pub type Frame = NativeFrameLease;
type Event = SessionEvent<Frame>;
pub type SessionEventFrame = SessionEvent<Frame>;

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

struct PlaybackState {
    playing: bool,
    position: Duration,
    stream_generation: u64,
}

fn mark_eos(playback: &mut PlaybackState) {
    playback.playing = false;
}

fn process_command(
    worker_command: WorkerCommand,
    decoder: &mut GStreamerDecoder,
    playback: &mut PlaybackState,
    audio_handle: &AudioHandle,
    state: &Arc<SnapshotState>,
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
                playback.stream_generation = target_generation;
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
    generation_commands: Receiver<GenerationIntent>,
    control_sender: ControlSender,
    frame_sender: Sender<SequencedEvent>,
    frame_drop_receiver: Receiver<SequencedEvent>,
    state: Arc<SnapshotState>,
    dropped_frames: Arc<AtomicU64>,
    audio_handle: AudioHandle,
    audio_sink: GstAudioSinkMode,
    lifecycle_control: GstLifecycleControl,
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

fn run_worker(
    source: String,
    autoplay: bool,
    lifecycle_timeout: Duration,
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
    let mut decoder =
        match GStreamerDecoder::new_system_memory_with_audio_sink_and_timeout_and_control(
            &source,
            audio_sink,
            lifecycle_timeout,
            lifecycle_control.clone(),
        ) {
            Ok(decoder) => decoder,
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
            match commands.recv_timeout(Duration::from_millis(25)) {
                Ok(command) => {
                    if !process_command(
                        WorkerCommand::Ordinary(command),
                        &mut decoder,
                        &mut playback,
                        &audio_handle,
                        &state,
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

        match decoder.decode_next() {
            Ok(Some(frame)) => {
                if lifecycle_cancelled(&lifecycle_control) {
                    if lifecycle_control.is_stop_requested() {
                        publish_ended(&state, &control_sender, &mut sequence);
                    }
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
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
                        if lifecycle_control.is_stop_requested() {
                            publish_ended(&state, &control_sender, &mut sequence);
                        } else if !lifecycle_cancelled(&lifecycle_control) {
                            let _ = publish_error(&state, &control_sender, &mut sequence, error);
                        }
                        shutdown_worker(&mut decoder, &lifecycle_control);
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
                    shutdown_worker(&mut decoder, &lifecycle_control);
                    return;
                }
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
            Ok(None) => {}
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
    pending_frame: Option<SequencedEvent>,
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
        let source = source.into();
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let generation_drop_receiver = generation_receiver.clone();
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let state = Arc::new(SnapshotState::new());
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
            pending_frame: None,
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

    /// Returns the latest discoverable audio tracks without consuming events.
    pub fn audio_tracks(&self) -> Vec<AudioTrack> {
        self.state.snapshot.read().audio_tracks.clone()
    }

    /// Returns the latest confirmed audio selection, if known.
    pub fn selected_audio_track_id(&self) -> Option<String> {
        self.state.snapshot.read().selected_audio_track_id.clone()
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
        match self.frame_receiver.try_recv() {
            Ok(event) => {
                let generation = match &event.event {
                    SessionEvent::Frame { frame, .. } => frame.descriptor.stream_generation,
                    _ => self.stream_generation,
                };
                if generation != self.latest_requested_generation {
                    self.dropped_frames.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.stream_generation = generation;
                    self.replay_pending = false;
                    if self.pending_frame.replace(event).is_some() {
                        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(TryRecvError::Disconnected) => self.worker_disconnected = true,
            Err(TryRecvError::Empty) => {}
        }
        if self.pending_frame.as_ref().is_some_and(|event| {
            matches!(
                &event.event,
                SessionEvent::Frame { frame, .. }
                    if frame.descriptor.stream_generation != self.latest_requested_generation
            )
        }) {
            self.pending_frame = None;
        }
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
        if matches!(&command, SessionCommand::Stop) {
            // Stop must wake opening/decoding workers even when the command
            // FIFO is full. The FIFO send below is only a best-effort wake.
            self.lifecycle_control.request_stop(self.lifecycle_timeout);
            let _ = self.commands.try_send(command);
            return Ok(());
        }
        let is_seek = matches!(&command, SessionCommand::Seek { .. });
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
            self.pending_frame = None;
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
        if let Some(event) = self.pending_frame.as_ref() {
            if source.is_none_or(|(sequence, _)| event.sequence < sequence) {
                source = Some((event.sequence, 7));
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
            Some(7) => self.pending_frame.take(),
            _ => None,
        };
        if let Some(event) = next {
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
        if self.worker_disconnected {
            return Ok(None);
        }
        Ok(None)
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
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let command_drop_receiver = command_receiver.clone();
        let (generation_commands, generation_receiver) = crossbeam_channel::bounded(1);
        let (control_sender, control_receiver) = control_channels();
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
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
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            pending_frame: None,
            has_presented_frame: false,
            worker: None,
            worker_disconnected: false,
            lifecycle_timeout: DEFAULT_LIFECYCLE_TIMEOUT,
            lifecycle_control: GstLifecycleControl::new(),
            stream_generation: 0,
            latest_requested_generation: 0,
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

        assert!(send_control(
            &control_sender,
            SequencedEvent {
                sequence: 0,
                event: SessionEvent::StateChanged {
                    state: SessionState::Ready,
                },
            }
        ));
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
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            pending_frame: None,
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
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            pending_frame: None,
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
            pending_metadata: None,
            pending_audio_tracks: None,
            pending_audio_selection: None,
            pending_state: None,
            pending_error: None,
            pending_terminal: None,
            pending_transient: None,
            pending_frame: None,
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
            frame_receiver: crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY).1,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            pending_frame: None,
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
            frame_receiver: crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY).1,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            pending_frame: None,
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
            frame_receiver: crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY).1,
            state: Arc::new(SnapshotState::new()),
            audio_handle: AudioHandle::new(),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            pending_frame: None,
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
            pending_frame: None,
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
            pending_frame: None,
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
