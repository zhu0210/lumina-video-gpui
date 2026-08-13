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
use lumina_video_native_frame::linux_video_gst::GStreamerDecoder;
pub use lumina_video_native_frame::linux_video_gst::GstAudioSinkMode;
use lumina_video_native_frame::video::{
    CpuFrame, DecodedFrame, VideoDecoderBackend, VideoError, VideoFrame,
};
use lumina_video_native_frame::{
    AcquireSync, CpuMemory, FrameExtent, NativeFrameDescriptor, NativeFrameLease, NativeMemory,
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
    snapshot: SessionSnapshot,
    position_us: AtomicU64,
    audio_connected: AtomicBool,
    audio_buffers_seen: AtomicU64,
}

impl SnapshotState {
    fn new() -> Self {
        Self {
            snapshot: SessionSnapshot::new(CapabilityTier::SystemMemoryUpload),
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
    state: &Arc<RwLock<SnapshotState>>,
    audio_handle: &AudioHandle,
    decoder: &GStreamerDecoder,
    position: Duration,
) {
    let native_audio = decoder.audio_handle();
    let connected = native_audio.has_audio();
    let buffers_seen = native_audio.audio_buffers_seen();
    state
        .read()
        .audio_connected
        .store(connected, Ordering::Relaxed);
    state
        .read()
        .audio_buffers_seen
        .store(buffers_seen, Ordering::Relaxed);
    audio_handle.set_available(connected);
    audio_handle.set_native_position(position);
}

fn sync_audio_controls(
    audio_handle: &AudioHandle,
    decoder: &mut GStreamerDecoder,
) -> Result<(), VideoError> {
    decoder.set_muted(audio_handle.is_muted())?;
    decoder.set_volume(audio_handle.effective_volume())
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
        NativeMemory::Cpu(CpuMemory::new(planes)),
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
    state: &Arc<RwLock<SnapshotState>>,
    control_sender: &Sender<SequencedEvent>,
    sequence: &mut u64,
    next_state: SessionState,
) -> bool {
    if let Some(position) = state_position(&next_state) {
        state
            .read()
            .position_us
            .store(position.as_micros() as u64, Ordering::Relaxed);
    }
    state.write().snapshot.state = next_state.clone();
    let event = SequencedEvent {
        sequence: *sequence,
        event: SessionEvent::StateChanged { state: next_state },
    };
    *sequence = sequence.saturating_add(1);
    send_control(control_sender, event)
}

fn publish_error(
    state: &Arc<RwLock<SnapshotState>>,
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
}

fn process_command(
    command: SessionCommand,
    decoder: &mut GStreamerDecoder,
    playback: &mut PlaybackState,
    audio_handle: &AudioHandle,
    state: &Arc<RwLock<SnapshotState>>,
    control_sender: &Sender<SequencedEvent>,
    sequence: &mut u64,
) -> bool {
    match command {
        SessionCommand::Play => match decoder.resume() {
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
        },
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
            if !publish_state(state, control_sender, sequence, SessionState::Ended) {
                return false;
            }
            send_control(
                control_sender,
                SequencedEvent {
                    sequence: *sequence,
                    event: SessionEvent::Ended,
                },
            );
            false
        }
        SessionCommand::Seek { position: target } => match decoder.seek(target) {
            Ok(()) => {
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
            match decoder.set_muted(muted) {
                Ok(()) => true,
                Err(error) => {
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    false
                }
            }
        }
        SessionCommand::SetVolume { volume } => {
            let volume = volume.clamp(0.0, 1.0);
            audio_handle.set_volume((volume * 100.0) as u32);
            match decoder.set_volume(volume) {
                Ok(()) => true,
                Err(error) => {
                    let _ = publish_error(state, control_sender, sequence, session_error(error));
                    false
                }
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
    control_sender: Sender<SequencedEvent>,
    frame_sender: Sender<SequencedEvent>,
    frame_drop_receiver: Receiver<SequencedEvent>,
    state: Arc<RwLock<SnapshotState>>,
    dropped_frames: Arc<AtomicU64>,
    audio_handle: AudioHandle,
    audio_sink: GstAudioSinkMode,
}

fn run_worker(source: String, autoplay: bool, io: WorkerIo) {
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
    let mut decoder = match GStreamerDecoder::new_system_memory_with_audio_sink(&source, audio_sink)
    {
        Ok(decoder) => decoder,
        Err(error) => {
            let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
            return;
        }
    };

    if let Err(error) = sync_audio_controls(&audio_handle, &mut decoder) {
        let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
        return;
    }
    update_audio_observation(&state, &audio_handle, &decoder, Duration::ZERO);

    let metadata = session_metadata(decoder.metadata());
    state.write().snapshot.metadata = Some(metadata.clone());
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
    };
    let mut frame_id = 0_u64;
    let stream_generation = 0_u64;

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
        if let Err(error) = sync_audio_controls(&audio_handle, &mut decoder) {
            let _ = publish_error(&state, &control_sender, &mut sequence, session_error(error));
            return;
        }
        update_audio_observation(&state, &audio_handle, &decoder, playback.position);

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
                    .read()
                    .position_us
                    .store(playback.position.as_micros() as u64, Ordering::Relaxed);
                update_audio_observation(&state, &audio_handle, &decoder, playback.position);
                let lease = match owned_cpu_lease(
                    frame,
                    frame_id,
                    stream_generation,
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
                return;
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
    state: Arc<RwLock<SnapshotState>>,
    audio_handle: AudioHandle,
    dropped_frames: Arc<AtomicU64>,
    pending_control: Option<SequencedEvent>,
    pending_frame: Option<SequencedEvent>,
    has_presented_frame: bool,
    worker: Option<JoinHandle<()>>,
    worker_disconnected: bool,
}

impl GstMediaSession {
    /// Starts opening `source` on a background worker. Playback starts paused.
    pub fn new(source: impl Into<String>) -> Self {
        Self::new_with_autoplay(source, false)
    }

    /// Starts opening `source` on a background worker and optionally autoplays
    /// after GStreamer reaches its preroll-ready state.
    pub fn new_with_autoplay(source: impl Into<String>, autoplay: bool) -> Self {
        Self::new_with_autoplay_and_audio_sink(source, autoplay, GstAudioSinkMode::Auto)
    }

    /// Starts a session with an explicit GStreamer audio sink policy.
    pub fn new_with_autoplay_and_audio_sink(
        source: impl Into<String>,
        autoplay: bool,
        audio_sink: GstAudioSinkMode,
    ) -> Self {
        let source = source.into();
        let (commands, command_receiver) = crossbeam_channel::bounded(COMMAND_QUEUE_CAPACITY);
        let (control_sender, control_receiver) = crossbeam_channel::bounded(CONTROL_QUEUE_CAPACITY);
        let (frame_sender, frame_receiver) = crossbeam_channel::bounded(FRAME_QUEUE_CAPACITY);
        let frame_drop_receiver = frame_receiver.clone();
        let state = Arc::new(RwLock::new(SnapshotState::new()));
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

    /// Returns framework-neutral observations from the GStreamer audio branch.
    pub fn audio_observation(&self) -> AudioObservation {
        self.snapshot().audio
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
                    if self.pending_frame.replace(event).is_some() {
                        self.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.worker_disconnected = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }
}

impl MediaSession for GstMediaSession {
    type Frame = Frame;

    fn snapshot(&self) -> SessionSnapshot {
        let state = self.state.read();
        let mut snapshot = state.snapshot.clone();
        let position = Duration::from_micros(state.position_us.load(Ordering::Relaxed));
        snapshot.state = match snapshot.state {
            SessionState::Playing { .. } => SessionState::Playing { position },
            SessionState::Paused { .. } => SessionState::Paused { position },
            SessionState::Buffering { .. } => SessionState::Buffering { position },
            state => state,
        };
        snapshot.audio = AudioObservation {
            connected: state.audio_connected.load(Ordering::Relaxed),
            buffers_seen: state.audio_buffers_seen.load(Ordering::Relaxed),
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
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => SessionError::InvalidCommand("command queue full".into()),
                TrySendError::Disconnected(_) => {
                    SessionError::Fatal("session worker stopped".into())
                }
            })
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

    fn test_frame(frame_id: u64) -> Frame {
        match NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id,
                stream_generation: 0,
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
        let state = Arc::new(RwLock::new(SnapshotState::new()));
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
}
