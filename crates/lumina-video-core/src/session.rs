//! Framework-neutral media-session contracts.
//!
//! A [`MediaSession`] is the seam shared by native playback adapters.  The
//! adapter owns source handling, timing, buffering, and synchronization; the
//! core only observes values in this module.  No framework handle, decoder
//! object, or renderer resource can cross this seam.

use std::fmt;
use std::time::Duration;

/// A media timestamp expressed in the session's master-clock time base.
pub type MediaTime = Duration;

/// The frame-delivery tier negotiated for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityTier {
    /// The renderer directly aliases producer-owned native memory.
    DirectAlias,
    /// The renderer imports native memory and performs a GPU conversion/copy.
    GpuConversion,
    /// Frames arrive in system memory and are uploaded by the renderer.
    SystemMemoryUpload,
}

/// How decoding was performed for an observed frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeMode {
    Hardware,
    Software,
}

/// Where decoded frame data resides before presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeResidency {
    NativeGpu,
    SystemMemory,
}

/// How the renderer consumed the decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    DirectAlias,
    GpuCopy,
    CpuUpload,
}

/// Any conversion performed between decode and presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionMode {
    None,
    YuvShader,
    GpuBlit,
}

/// Producer/consumer synchronization observed for a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynchronizationMode {
    None,
    Explicit,
    VerifiedImplicit,
    CpuWait,
}

/// Observable description of how a frame reached presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRealization {
    pub decode: DecodeMode,
    pub residency: DecodeResidency,
    pub import: ImportMode,
    pub conversion: ConversionMode,
    pub synchronization: SynchronizationMode,
}

/// Current lifecycle state of a media session.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Loading,
    Ready,
    Playing { position: MediaTime },
    Paused { position: MediaTime },
    Buffering { position: MediaTime },
    Error(SessionError),
    Ended,
}

/// Errors reported by a media-session adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    InvalidCommand(String),
    Open(String),
    Decode(String),
    Seek(String),
    Network(String),
    Unsupported(String),
    Fatal(String),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommand(message) => write!(f, "invalid session command: {message}"),
            Self::Open(message) => write!(f, "failed to open media: {message}"),
            Self::Decode(message) => write!(f, "media decode failed: {message}"),
            Self::Seek(message) => write!(f, "media seek failed: {message}"),
            Self::Network(message) => write!(f, "media network error: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported media: {message}"),
            Self::Fatal(message) => write!(f, "fatal media-session error: {message}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Commands accepted by a media-session adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    Play,
    Pause,
    Stop,
    Seek {
        position: MediaTime,
    },
    SetMuted {
        muted: bool,
    },
    SetVolume {
        volume: f32,
    },
    /// Selects one of the source's discovered audio streams by its stable
    /// GStreamer stream id.
    SelectAudioTrack {
        id: String,
    },
    /// Request a session-level capability renegotiation.
    Renegotiate {
        tier: CapabilityTier,
    },
}

/// A framework-neutral event emitted by a media-session adapter.
#[derive(Debug, Clone)]
pub enum SessionEvent<F> {
    Metadata {
        metadata: SessionMetadata,
    },
    /// The complete discoverable audio-track set, with the currently
    /// selected stream id when one is known.
    AudioTracks {
        tracks: Vec<AudioTrack>,
        selected_id: Option<String>,
    },
    /// Confirms an audio-track selection, including the selected metadata.
    AudioTrackSelected {
        track: AudioTrack,
    },
    /// Reports a rejected selection without terminating the session.
    AudioTrackSelectionFailed {
        requested_id: String,
        prior_restored_id: Option<String>,
        reason: String,
    },
    StateChanged {
        state: SessionState,
    },
    Frame {
        pts: MediaTime,
        frame: F,
    },
    Ended,
    Error(SessionError),
}

/// Metadata for one source audio stream.
///
/// `id` is the raw source stream id. It is intentionally not an index or a
/// value derived from the order in a stream collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    pub id: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub codec: String,
}

/// Framework-neutral metadata observed for one media session.
///
/// This intentionally owns the session vocabulary instead of aliasing the
/// legacy decoder-facing video metadata. Native adapters can translate their
/// metadata into this shape without leaking decoder types through the core
/// session contract.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetadata {
    pub width: u32,
    pub height: u32,
    pub duration: Option<MediaTime>,
    pub frame_rate: f32,
    pub codec: String,
    pub pixel_aspect_ratio: f32,
    pub start_time: Option<MediaTime>,
}

/// Audio output observed by a media session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioObservation {
    /// A source audio branch has connected to the session's sink.
    pub connected: bool,
    /// Number of buffers observed on that branch.
    pub buffers_seen: u64,
}

/// Snapshot of session state that can be read without consuming events.
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub state: SessionState,
    pub metadata: Option<SessionMetadata>,
    pub capability: CapabilityTier,
    pub audio_tracks: Vec<AudioTrack>,
    pub selected_audio_track_id: Option<String>,
}

impl SessionSnapshot {
    /// Creates an initial loading snapshot for a negotiated capability tier.
    pub const fn new(capability: CapabilityTier) -> Self {
        Self {
            state: SessionState::Loading,
            metadata: None,
            capability,
            audio_tracks: Vec::new(),
            selected_audio_track_id: None,
        }
    }
}

/// Shared adapter seam for file, native, and live media sessions.
///
/// Implementations must keep these operations nonblocking: `command` may
/// enqueue work and report only immediate validation failures, while
/// `try_next_event` must return promptly with `None` when no event is ready.
/// Events are returned in producer order; frame events carry their master-clock
/// timestamp, and asynchronous failures are reported as
/// [`SessionEvent::Error`].  Adapters must not silently change capability tier
/// per frame; renegotiation is requested through [`SessionCommand::Renegotiate`].
pub trait MediaSession: Send {
    /// The framework-neutral frame lease produced by this adapter.
    type Frame: Send;

    /// Returns the latest state snapshot without waiting for new media.
    fn snapshot(&self) -> SessionSnapshot;

    /// Enqueues one command, returning only immediate validation errors.
    fn command(&mut self, command: SessionCommand) -> Result<(), SessionError>;

    /// Polls one queued event without blocking; events are FIFO.
    fn try_next_event(&mut self) -> Result<Option<SessionEvent<Self::Frame>>, SessionError>;
}
