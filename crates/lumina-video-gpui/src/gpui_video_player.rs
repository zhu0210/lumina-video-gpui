//! GPUI video player widget.
//!
//! Provides a GPU-accelerated video playback component using GPUI's
//! built-in `surface()` element for zero-copy texture compositing.
//!
//! # Architecture
//!
//! ```text
//! Linux: GstMediaSession (GStreamer decode + A/V timing), except MoQ URLs
//! which use CorePlayer's native MoQ decoder.
//!   │ one try_next_event() per animation tick
//!   ├─ Intel Vulkan + NV12: Gst DMABuf + SyncFile
//!   │    → bounded import worker → one multiplanar external texture
//!   │    → same-frame pending surface → normal renderer acquire/render/release
//!   └─ system fallback: native_frame_lease_to_textures()  ← lumina-video-wgpu
//!        NV12: Y(R8) + CbCr(RG8) → surface((y, cbcr, size)) [GPU YUV→RGB]
//!        RGBA: single RGBA8       → surface((tex, desc))     [passthrough]
//!   ▼
//! GPUI surface() element  ←  gpui renderer
//! ```
//!
//! GPUI's `surface()` supports NV12 natively with a built-in shader, so
//! YUV frames (NV12, YUV420p) avoid the CPU YUV→RGB conversion entirely.
//! Linux GStreamer frames use the owned lease API; borrowed native GPU
//! surfaces remain rejected at the non-Linux compatibility seam.

use std::sync::Arc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use gpui::*;
use gpui_wgpu::wgpu;
use lumina_video_core::session::{
    CapabilityDowngradeReason, CapabilityTier, FrameRealization, RendererOutcome,
};

#[cfg(target_os = "linux")]
use crossbeam_channel::{Receiver, Sender, TrySendError};
#[cfg(target_os = "linux")]
use gpui_wgpu::{ExternalFrameRequest, ExternalNv12Frame, ExternalOwnership};
#[cfg(target_os = "linux")]
use lumina_video_core::session::{
    ConversionMode, DecodeResidency, ImportMode, MediaSession, SessionError, SessionEvent,
    SessionState as CoreSessionState, SynchronizationMode,
};
use lumina_video_core::subtitles::{SubtitleError, SubtitleStyle, SubtitleTrack};
#[cfg(target_os = "linux")]
use lumina_video_gst::{GstMediaSession, PresentationDecision, DEFAULT_OPEN_TIMEOUT};
#[cfg(any(not(target_os = "linux"), feature = "moq"))]
use lumina_video_native_frame::player::CorePlayer;
use lumina_video_native_frame::video::{VideoMetadata, VideoState};
#[cfg(any(not(target_os = "linux"), feature = "moq"))]
use lumina_video_wgpu::decoded_frame_to_textures;
use lumina_video_wgpu::GpuFrameTextures;
#[cfg(any(not(target_os = "linux"), feature = "moq"))]
use lumina_video_wgpu::LegacyFrameIngestionError;
#[cfg(target_os = "linux")]
use lumina_video_wgpu::{import_external_dmabuf_nv12, ImportedNv12Texture, Nv12ImportError};
#[cfg(target_os = "linux")]
use lumina_video_wgpu::{native_frame_lease_to_textures, NativeFrameIngestionError};

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ExternalPresentation {
    // The local pending/display slot and GPUI's exact texture Arc are both
    // required owners; the bounded mailbox/renderer slots prevent growth.
    texture: Arc<wgpu::Texture>,
    width: u32,
    height: u32,
    color_transform: [[f32; 4]; 4],
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresentationTransition {
    Direct,
    Downgrading,
    SystemMemory,
    Fatal,
}

#[cfg(target_os = "linux")]
impl PresentationTransition {
    fn after_downgrade(self) -> Self {
        match self {
            Self::Direct => Self::Downgrading,
            Self::Downgrading | Self::SystemMemory | Self::Fatal => Self::Fatal,
        }
    }
}

#[cfg(target_os = "linux")]
fn active_playback_state(state: &VideoState) -> bool {
    matches!(
        state,
        VideoState::Playing { .. } | VideoState::Buffering { .. }
    )
}

#[cfg(target_os = "linux")]
fn sync_downgrade_budget(
    active: bool,
    remaining: Option<Duration>,
    deadline: Option<Instant>,
    now: Instant,
) -> (Option<Duration>, Option<Instant>) {
    if active {
        if deadline.is_some() {
            return (remaining, deadline);
        }
        let Some(remaining) = remaining else {
            return (None, None);
        };
        (
            Some(remaining),
            Some(now.checked_add(remaining).unwrap_or(now)),
        )
    } else {
        let remaining = deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .or(remaining);
        (remaining, None)
    }
}

#[cfg(target_os = "linux")]
fn downgrade_budget_expired(active: bool, deadline: Option<Instant>, now: Instant) -> bool {
    active && deadline.is_some_and(|deadline| now >= deadline)
}

#[cfg(target_os = "linux")]
struct DirectImportWorker {
    input: Sender<lumina_video_native_frame::NativeFrameLease>,
    input_drop: Receiver<lumina_video_native_frame::NativeFrameLease>,
    output: Receiver<Result<ImportedNv12Texture, Nv12ImportError>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
fn try_send_drop_oldest<T>(sender: &Sender<T>, drop_receiver: &Receiver<T>, item: T) -> bool {
    match sender.try_send(item) {
        Ok(()) => true,
        Err(TrySendError::Full(item)) => {
            let _ = drop_receiver.try_recv();
            sender.try_send(item).is_ok()
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

#[cfg(target_os = "linux")]
impl DirectImportWorker {
    fn new(device: Arc<wgpu::Device>) -> Option<Self> {
        let (input, input_receiver) = crossbeam_channel::bounded(1);
        let input_drop = input_receiver.clone();
        let (output, output_receiver) = crossbeam_channel::bounded(1);
        let output_drop = output_receiver.clone();
        let worker_output_drop = output_drop.clone();
        let thread = std::thread::Builder::new()
            .name("lumina-dmabuf-import".into())
            .spawn(move || {
                while let Ok(lease) = input_receiver.recv() {
                    // SAFETY: this route enqueues only GStreamer-owned DMABuf leases whose
                    // export supplied a producer-ready SyncFile and truthful NV12 layout;
                    // the producer has handed the image to GENERAL/FOREIGN ownership and
                    // `device` is the matching Vulkan device. The importer only validates
                    // and wraps the image/fence; it never waits or submits GPU work.
                    let imported = unsafe { import_external_dmabuf_nv12(lease, &device) };
                    let _ = try_send_drop_oldest(&output, &worker_output_drop, imported);
                }
            })
            .ok()?;
        Some(Self {
            input,
            input_drop,
            output: output_receiver,
            thread: Some(thread),
        })
    }

    fn enqueue(&self, lease: lumina_video_native_frame::NativeFrameLease) -> bool {
        try_send_drop_oldest(&self.input, &self.input_drop, lease)
    }

    fn try_take(&self) -> Option<Result<ImportedNv12Texture, Nv12ImportError>> {
        self.output.try_recv().ok()
    }
}

#[cfg(target_os = "linux")]
impl Drop for DirectImportWorker {
    fn drop(&mut self) {
        let _ = self.thread.take();
    }
}

// ---------------------------------------------------------------------------
// Configuration & response types
// ---------------------------------------------------------------------------

/// Response returned after each player update.
#[derive(Debug, Default)]
pub struct GpuiVideoPlayerResponse {
    pub toggle_playback: bool,
    pub toggle_fullscreen: bool,
    pub toggle_mute: bool,
    pub toggle_subtitles: bool,
    pub seek_to: Option<Duration>,
    pub state_changed: bool,
}

/// Player configuration.
#[derive(Clone)]
pub struct GpuiVideoPlayerConfig {
    pub show_controls: bool,
    pub autoplay: bool,
    pub looping: bool,
    pub muted: bool,
    pub volume: f32,
    pub lifecycle_timeout: Duration,
    #[cfg(target_os = "linux")]
    pub open_timeout: Duration,
}

impl Default for GpuiVideoPlayerConfig {
    fn default() -> Self {
        Self {
            show_controls: true,
            autoplay: false,
            looping: false,
            muted: false,
            volume: 1.0,
            lifecycle_timeout: Duration::from_secs(2),
            #[cfg(target_os = "linux")]
            open_timeout: DEFAULT_OPEN_TIMEOUT,
        }
    }
}

#[cfg(target_os = "linux")]
fn video_metadata(metadata: &lumina_video_core::session::SessionMetadata) -> VideoMetadata {
    VideoMetadata {
        width: metadata.width,
        height: metadata.height,
        duration: metadata.duration,
        frame_rate: metadata.frame_rate,
        codec: metadata.codec.clone(),
        pixel_aspect_ratio: metadata.pixel_aspect_ratio,
        start_time: metadata.start_time,
    }
}

#[cfg(target_os = "linux")]
fn video_error(error: &SessionError) -> lumina_video_native_frame::video::VideoError {
    use lumina_video_native_frame::video::VideoError;

    match error {
        SessionError::InvalidCommand(message) | SessionError::Fatal(message) => {
            VideoError::Generic(message.clone())
        }
        SessionError::Open(message) => VideoError::OpenFailed(message.clone()),
        SessionError::Decode(message) => VideoError::DecodeFailed(message.clone()),
        SessionError::Seek(message) => VideoError::SeekFailed(message.clone()),
        SessionError::Network(message) => VideoError::Network(message.clone()),
        SessionError::Tls(message) => VideoError::Tls(message.clone()),
        SessionError::Unsupported(message) => VideoError::UnsupportedFormat(message.clone()),
    }
}

#[cfg(target_os = "linux")]
fn video_state(state: &CoreSessionState) -> VideoState {
    match state {
        CoreSessionState::Loading => VideoState::Loading,
        CoreSessionState::Ready => VideoState::Ready,
        CoreSessionState::Playing { position } => VideoState::Playing {
            position: *position,
        },
        CoreSessionState::Paused { position } => VideoState::Paused {
            position: *position,
        },
        CoreSessionState::Buffering { position } => VideoState::Buffering {
            position: *position,
        },
        CoreSessionState::Error(error) => VideoState::Error(video_error(error)),
        CoreSessionState::Ended => VideoState::Ended,
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxPlaybackRoute {
    Gst,
    #[cfg(feature = "moq")]
    Core,
}

#[cfg(target_os = "linux")]
fn linux_playback_route(url: &str) -> LinuxPlaybackRoute {
    #[cfg(feature = "moq")]
    if url.starts_with("moq://") || url.starts_with("moqs://") {
        return LinuxPlaybackRoute::Core;
    }

    let _ = url;
    LinuxPlaybackRoute::Gst
}

// ---------------------------------------------------------------------------
// Player state
// ---------------------------------------------------------------------------

/// A GPU-accelerated video player for GPUI.
///
/// Wraps the Linux [`GstMediaSession`] or non-Linux [`CorePlayer`] for
/// decode/audio/sync and manages GPU texture upload for GPUI's `surface()`
/// element.
pub struct GpuiVideoPlayer {
    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    background_executor: BackgroundExecutor,
    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    core: Option<CorePlayer>,
    #[cfg(target_os = "linux")]
    session: Option<GstMediaSession>,
    url: String,
    #[cfg(target_os = "linux")]
    pending_frame: Option<lumina_video_native_frame::NativeFrameLease>,
    #[cfg(target_os = "linux")]
    has_presented_frame: bool,
    #[cfg(target_os = "linux")]
    direct_import: Option<DirectImportWorker>,
    #[cfg(target_os = "linux")]
    direct_alias_supported: Option<bool>,
    #[cfg(target_os = "linux")]
    presentation_transition: PresentationTransition,
    #[cfg(target_os = "linux")]
    transition_deadline: Option<Instant>,
    #[cfg(target_os = "linux")]
    remaining_downgrade_budget: Option<Duration>,
    #[cfg(target_os = "linux")]
    pending_external: Option<ExternalPresentation>,
    #[cfg(target_os = "linux")]
    direct_display: Option<ExternalPresentation>,
    #[cfg(target_os = "linux")]
    frame_realization: Option<FrameRealization>,
    #[cfg(target_os = "linux")]
    pending_external_retirement: bool,
    config: GpuiVideoPlayerConfig,

    // GPU state
    gpu_context: Option<GpuContextHandle>,
    /// Current frame as GPU textures (NV12 or RGBA variant).
    frame_textures: Option<GpuFrameTextures>,
    /// Cached textures for reuse (Y, CbCr, RGBA).
    y_cache: Option<Arc<wgpu::Texture>>,
    cbcr_cache: Option<Arc<wgpu::Texture>>,
    rgba_cache: Option<Arc<wgpu::Texture>>,

    // Playback state (synced from the platform session each update)
    position: Duration,
    duration: Option<Duration>,
    metadata: Option<VideoMetadata>,
    state: VideoState,
    buffering_percent: i32,

    // Init state
    loading_started: bool,
    initialized: bool,
    loop_seek_pending: bool,

    // Diagnostics: avoid log spam when GPU context is repeatedly unavailable
    gpu_context_missing_logged: bool,

    // Subtitles
    subtitle_track: Option<SubtitleTrack>,
    show_subtitles: bool,
    subtitle_style: SubtitleStyle,
}

impl GpuiVideoPlayer {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    pub fn new(url: impl Into<String>, cx: &App) -> Self {
        Self::with_config(url, GpuiVideoPlayerConfig::default(), cx)
    }

    pub fn with_config(url: impl Into<String>, config: GpuiVideoPlayerConfig, cx: &App) -> Self {
        let url = url.into();
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        let background_executor = cx.background_executor().clone();
        #[cfg(all(target_os = "linux", not(feature = "moq")))]
        let _ = cx;
        #[cfg(target_os = "linux")]
        let route = linux_playback_route(&url);
        #[cfg(not(target_os = "linux"))]
        let core = Some(CorePlayer::new(url.clone()));
        #[cfg(all(target_os = "linux", feature = "moq"))]
        let core = match route {
            LinuxPlaybackRoute::Core => Some(CorePlayer::new(url.clone())),
            LinuxPlaybackRoute::Gst => None,
        };
        #[cfg(target_os = "linux")]
        let session = match route {
            LinuxPlaybackRoute::Gst => Some(
                GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                    url.clone(),
                    false,
                    lumina_video_gst::GstAudioSinkMode::Auto,
                    config.lifecycle_timeout,
                    config.open_timeout,
                    0,
                    CapabilityTier::DirectAlias,
                ),
            ),
            #[cfg(feature = "moq")]
            LinuxPlaybackRoute::Core => None,
        };
        let muted = config.muted;
        let volume = config.volume;
        #[allow(unused_mut)]
        let mut player = Self {
            #[cfg(any(not(target_os = "linux"), feature = "moq"))]
            background_executor,
            #[cfg(any(not(target_os = "linux"), feature = "moq"))]
            core,
            #[cfg(target_os = "linux")]
            session,
            url,
            #[cfg(target_os = "linux")]
            pending_frame: None,
            #[cfg(target_os = "linux")]
            has_presented_frame: false,
            #[cfg(target_os = "linux")]
            direct_import: None,
            #[cfg(target_os = "linux")]
            direct_alias_supported: None,
            #[cfg(target_os = "linux")]
            presentation_transition: PresentationTransition::Direct,
            #[cfg(target_os = "linux")]
            transition_deadline: None,
            #[cfg(target_os = "linux")]
            remaining_downgrade_budget: None,
            #[cfg(target_os = "linux")]
            pending_external: None,
            #[cfg(target_os = "linux")]
            direct_display: None,
            #[cfg(target_os = "linux")]
            frame_realization: None,
            #[cfg(target_os = "linux")]
            pending_external_retirement: false,
            config,
            gpu_context: None,
            frame_textures: None,
            y_cache: None,
            cbcr_cache: None,
            rgba_cache: None,
            position: Duration::ZERO,
            duration: None,
            metadata: None,
            state: VideoState::Loading,
            buffering_percent: 0,
            loading_started: false,
            initialized: false,
            loop_seek_pending: false,
            gpu_context_missing_logged: false,
            subtitle_track: None,
            show_subtitles: true,
            subtitle_style: SubtitleStyle::default(),
        };
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = player.core.as_mut() {
            core.set_muted(muted);
            core.set_volume((volume * 100.0) as u32);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = player.session.as_ref() {
            let audio = session.audio_handle();
            audio.set_muted(muted);
            audio.set_volume((volume.clamp(0.0, 1.0) * 100.0) as u32);
        }
        player
    }

    /// Asynchronously replaces the current source while retaining the last
    /// uploaded GPU texture until the new session presents a frame.
    pub fn open(&mut self, url: impl Into<String>, _cx: &App) {
        let url = url.into();
        #[cfg(target_os = "linux")]
        let next_generation = self
            .session
            .as_ref()
            .map_or(1, |session| session.stream_generation().saturating_add(1));

        // CorePlayer teardown retains its join semantics, so move the old
        // backend to GPUI's background executor before replacing it. The
        // GStreamer session drop is already fire-and-forget.
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        let old_core = self.core.take();
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(old_core) = old_core {
            self.background_executor
                .spawn(async move { drop(old_core) })
                .detach();
        }
        #[cfg(target_os = "linux")]
        let old_session = self.session.take();
        #[cfg(target_os = "linux")]
        drop(old_session);

        #[cfg(target_os = "linux")]
        let route = linux_playback_route(&url);
        #[cfg(all(target_os = "linux", feature = "moq"))]
        {
            self.core = match route {
                LinuxPlaybackRoute::Core => Some(CorePlayer::new(url.clone())),
                LinuxPlaybackRoute::Gst => None,
            };
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.core = Some(CorePlayer::new(url.clone()));
        }
        #[cfg(target_os = "linux")]
        {
            self.session = match route {
                LinuxPlaybackRoute::Gst => Some(
                    GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                        url.clone(),
                        false,
                        lumina_video_gst::GstAudioSinkMode::Auto,
                        self.config.lifecycle_timeout,
                        self.config.open_timeout,
                        next_generation,
                        CapabilityTier::DirectAlias,
                    ),
                ),
                #[cfg(feature = "moq")]
                LinuxPlaybackRoute::Core => None,
            };
            if let Some(session) = self.session.as_ref() {
                let audio = session.audio_handle();
                audio.set_muted(self.config.muted);
                audio.set_volume((self.config.volume.clamp(0.0, 1.0) * 100.0) as u32);
            }
        }
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.set_muted(self.config.muted);
            core.set_volume((self.config.volume * 100.0) as u32);
        }

        self.url = url;
        self.loading_started = false;
        self.initialized = false;
        self.loop_seek_pending = false;
        self.position = Duration::ZERO;
        self.duration = None;
        self.metadata = None;
        self.state = VideoState::Loading;
        self.buffering_percent = 0;
        #[cfg(target_os = "linux")]
        {
            self.pending_frame = None;
            self.has_presented_frame = false;
            self.direct_import = None;
            self.direct_alias_supported = None;
            self.presentation_transition = PresentationTransition::Direct;
            self.transition_deadline = None;
            self.remaining_downgrade_budget = None;
            self.request_external_retirement();
        }
    }

    // -----------------------------------------------------------------------
    // Builder API
    // -----------------------------------------------------------------------

    pub fn with_controls(mut self, show: bool) -> Self {
        self.config.show_controls = show;
        self
    }

    pub fn with_autoplay(mut self, autoplay: bool) -> Self {
        self.config.autoplay = autoplay;
        self
    }

    pub fn with_looping(mut self, looping: bool) -> Self {
        self.config.looping = looping;
        self
    }

    pub fn with_muted(mut self, muted: bool) -> Self {
        self.config.muted = muted;
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.set_muted(muted);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_ref() {
            session.audio_handle().set_muted(muted);
        }
        self
    }

    pub fn with_volume(mut self, volume: f32) -> Self {
        self.config.volume = volume.clamp(0.0, 1.0);
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.set_volume((self.config.volume * 100.0) as u32);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_ref() {
            session
                .audio_handle()
                .set_volume((self.config.volume * 100.0) as u32);
        }
        self
    }

    pub fn with_lifecycle_timeout(mut self, timeout: Duration) -> Self {
        self.config.lifecycle_timeout = timeout;
        #[cfg(target_os = "linux")]
        if self.session.is_some() {
            let generation = self
                .session
                .as_ref()
                .map_or(0, GstMediaSession::stream_generation);
            let old_session = self.session.take();
            drop(old_session);
            self.pending_frame = None;
            self.direct_import = None;
            self.direct_alias_supported = None;
            self.presentation_transition = PresentationTransition::Direct;
            self.transition_deadline = None;
            self.remaining_downgrade_budget = None;
            self.request_external_retirement();
            self.session = Some(
                GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                    self.url.clone(),
                    false,
                    lumina_video_gst::GstAudioSinkMode::Auto,
                    timeout,
                    self.config.open_timeout,
                    generation,
                    CapabilityTier::DirectAlias,
                ),
            );
            if let Some(session) = self.session.as_ref() {
                let audio = session.audio_handle();
                audio.set_muted(self.config.muted);
                audio.set_volume((self.config.volume.clamp(0.0, 1.0) * 100.0) as u32);
            }
        }
        self
    }

    #[cfg(target_os = "linux")]
    pub fn with_open_timeout(mut self, timeout: Duration) -> Self {
        self.config.open_timeout = timeout;
        if self.session.is_some() {
            let generation = self
                .session
                .as_ref()
                .map_or(0, GstMediaSession::stream_generation);
            let old_session = self.session.take();
            drop(old_session);
            self.pending_frame = None;
            self.direct_import = None;
            self.direct_alias_supported = None;
            self.presentation_transition = PresentationTransition::Direct;
            self.transition_deadline = None;
            self.remaining_downgrade_budget = None;
            self.request_external_retirement();
            self.session = Some(
                GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                    self.url.clone(),
                    false,
                    lumina_video_gst::GstAudioSinkMode::Auto,
                    self.config.lifecycle_timeout,
                    timeout,
                    generation,
                    CapabilityTier::DirectAlias,
                ),
            );
            if let Some(session) = self.session.as_ref() {
                let audio = session.audio_handle();
                audio.set_muted(self.config.muted);
                audio.set_volume((self.config.volume.clamp(0.0, 1.0) * 100.0) as u32);
            }
        }
        self
    }

    // -----------------------------------------------------------------------
    // Playback control
    // -----------------------------------------------------------------------

    pub fn play(&mut self) {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.play();
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            let _ = session.command(lumina_video_core::session::SessionCommand::Play);
        }
    }

    pub fn pause(&mut self) {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.pause();
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            let _ = session.command(lumina_video_core::session::SessionCommand::Pause);
        }
    }

    pub fn toggle_playback(&mut self) {
        match self.state {
            VideoState::Playing { .. } => self.pause(),
            VideoState::Paused { .. } | VideoState::Ready | VideoState::Ended => self.play(),
            _ => {}
        }
    }

    pub fn seek(&mut self, position: Duration) {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.seek(position);
            return;
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            let _ = session.command(lumina_video_core::session::SessionCommand::Seek { position });
        }
    }

    pub fn toggle_mute(&mut self) {
        self.config.muted = !self.config.muted;
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.set_muted(self.config.muted);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_ref() {
            session.audio_handle().set_muted(self.config.muted);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.config.volume = volume.clamp(0.0, 1.0);
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_mut() {
            core.set_volume((self.config.volume * 100.0) as u32);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_ref() {
            session
                .audio_handle()
                .set_volume((self.config.volume * 100.0) as u32);
        }
    }

    // -----------------------------------------------------------------------
    // Subtitles
    // -----------------------------------------------------------------------

    pub fn load_subtitles_srt(&mut self, content: &str) -> Result<(), SubtitleError> {
        let track = SubtitleTrack::from_srt(content)?;
        self.subtitle_track = Some(track);
        Ok(())
    }

    pub fn load_subtitles_vtt(&mut self, content: &str) -> Result<(), SubtitleError> {
        let track = SubtitleTrack::from_vtt(content)?;
        self.subtitle_track = Some(track);
        Ok(())
    }

    pub fn clear_subtitles(&mut self) {
        self.subtitle_track = None;
    }

    pub fn toggle_subtitles(&mut self) {
        self.show_subtitles = !self.show_subtitles;
    }

    pub fn set_subtitles_visible(&mut self, visible: bool) {
        self.show_subtitles = visible;
    }

    pub fn has_subtitles(&self) -> bool {
        self.subtitle_track.is_some()
    }

    pub fn current_subtitle_text(&self) -> Option<&str> {
        let track = self.subtitle_track.as_ref()?;
        track.get_cue_at(self.position).map(|cue| cue.text.as_str())
    }

    // -----------------------------------------------------------------------
    // State queries
    // -----------------------------------------------------------------------

    pub fn position(&self) -> Duration {
        self.position
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    pub fn metadata(&self) -> Option<&VideoMetadata> {
        self.metadata.as_ref()
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.state, VideoState::Playing { .. })
    }

    pub fn is_ready(&self) -> bool {
        self.initialized
    }

    pub fn is_muted(&self) -> bool {
        self.config.muted
    }

    pub fn volume(&self) -> f32 {
        self.config.volume
    }

    pub fn is_ended(&self) -> bool {
        matches!(self.state, VideoState::Ended)
    }

    pub fn is_error(&self) -> bool {
        matches!(self.state, VideoState::Error(_))
    }

    pub fn state(&self) -> &VideoState {
        &self.state
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn buffering_percent(&self) -> i32 {
        self.buffering_percent
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_ref() {
            return core.dimensions();
        }

        self.metadata
            .as_ref()
            .map(|metadata| (metadata.width, metadata.height))
    }

    pub fn frame_rate(&self) -> Option<f32> {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_ref() {
            return core.frame_rate();
        }

        self.metadata.as_ref().map(|metadata| metadata.frame_rate)
    }

    /// Returns the seek bar progress as a 0.0–1.0 fraction.
    pub fn seek_progress(&self) -> f32 {
        match self.duration {
            Some(d) if !d.is_zero() => {
                (self.position.as_secs_f64() / d.as_secs_f64()).clamp(0.0, 1.0) as f32
            }
            _ => 0.0,
        }
    }

    /// Returns the audio handle for external volume/mute control.
    pub fn audio_handle(&self) -> &lumina_video_core::audio::AudioHandle {
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.as_ref() {
            return core.audio_handle();
        }

        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_ref() {
            return session.audio_handle();
        }

        unreachable!("player backend route must be initialized")
    }

    // -----------------------------------------------------------------------
    // Frame textures for external rendering
    // -----------------------------------------------------------------------

    pub fn current_textures(&self) -> Option<&GpuFrameTextures> {
        self.frame_textures.as_ref()
    }

    #[cfg(target_os = "linux")]
    pub fn frame_realization(&self) -> Option<FrameRealization> {
        self.frame_realization
    }

    #[cfg(not(target_os = "linux"))]
    pub fn frame_realization(&self) -> Option<FrameRealization> {
        None
    }

    #[cfg(target_os = "linux")]
    /// Returns the capability tier last committed by the renderer.
    pub fn capability(&self) -> Option<CapabilityTier> {
        self.frame_realization
            .map(FrameRealization::capability_tier)
            .or(match self.presentation_transition {
                PresentationTransition::Direct | PresentationTransition::Downgrading => {
                    Some(CapabilityTier::DirectAlias)
                }
                PresentationTransition::SystemMemory => Some(CapabilityTier::SystemMemoryUpload),
                PresentationTransition::Fatal => Some(CapabilityTier::DirectAlias),
            })
    }

    #[cfg(not(target_os = "linux"))]
    /// Returns the capability tier last committed by the renderer.
    pub fn capability(&self) -> Option<CapabilityTier> {
        None
    }

    #[cfg(target_os = "linux")]
    /// Returns the latest typed renderer outcome.
    pub fn latest_renderer_outcome(&self) -> Option<RendererOutcome> {
        self.session
            .as_ref()
            .and_then(GstMediaSession::latest_renderer_outcome)
    }

    #[cfg(not(target_os = "linux"))]
    /// Returns the latest typed renderer outcome.
    pub fn latest_renderer_outcome(&self) -> Option<RendererOutcome> {
        None
    }

    #[cfg(target_os = "linux")]
    /// Returns the latest typed capability downgrade reason.
    pub fn latest_downgrade_reason(&self) -> Option<CapabilityDowngradeReason> {
        self.session
            .as_ref()
            .and_then(GstMediaSession::latest_downgrade_reason)
    }

    #[cfg(not(target_os = "linux"))]
    /// Returns the latest typed capability downgrade reason.
    pub fn latest_downgrade_reason(&self) -> Option<CapabilityDowngradeReason> {
        None
    }

    #[cfg(target_os = "linux")]
    fn request_external_retirement(&mut self) {
        self.pending_external_retirement = true;
        self.pending_external = None;
        self.direct_display = None;
        self.frame_realization = None;
    }

    #[cfg(target_os = "linux")]
    fn retire_external_frame_now(&mut self, window: &mut Window) {
        let owns_external_frame = self.pending_external_retirement
            || self.pending_external.is_some()
            || self.direct_display.is_some();
        if owns_external_frame {
            let _ = window.clear_external_frame();
            // clear_external_frame() reports Accepted through the same one-shot
            // outcome channel; it must not certify a still-pending submission.
            let _ = window.take_external_frame_outcome();
        }
        self.pending_external_retirement = false;
        self.pending_external = None;
        self.direct_display = None;
    }

    #[cfg(target_os = "linux")]
    fn drain_pending_external_retirement(&mut self, window: &mut Window) {
        if self.pending_external_retirement {
            self.retire_external_frame_now(window);
        }
    }

    /// Retire renderer-owned external frames before replacing or dropping this player.
    ///
    /// `Drop` cannot access a [`Window`], so callers that still have one must use this
    /// method before removing the player. The normal `Drop` implementation still releases
    /// all local resources.
    pub fn retire_external_frame(&mut self, window: &mut Window) {
        #[cfg(target_os = "linux")]
        self.retire_external_frame_now(window);
        #[cfg(not(target_os = "linux"))]
        let _ = window;
    }

    #[cfg(target_os = "linux")]
    fn report_renderer_outcome(&self, outcome: RendererOutcome) {
        if let Some(session) = self.session.as_ref() {
            session.report_renderer_outcome(outcome);
        }
    }

    #[cfg(target_os = "linux")]
    fn commit_realization(&mut self, realization: FrameRealization) -> bool {
        if self.frame_realization == Some(realization) {
            return false;
        }
        self.frame_realization = Some(realization);
        if let Some(session) = self.session.as_ref() {
            session.commit_realization(realization);
        }
        true
    }

    #[cfg(target_os = "linux")]
    fn report_downgrade_reason(&self, reason: CapabilityDowngradeReason) {
        if let Some(session) = self.session.as_ref() {
            session.report_downgrade_reason(reason);
        }
    }

    #[cfg(target_os = "linux")]
    fn fail_presentation_transition(
        &mut self,
        downgrade_reason: Option<CapabilityDowngradeReason>,
        reason: impl Into<String>,
    ) {
        if self.presentation_transition == PresentationTransition::Fatal {
            return;
        }
        let reason = reason.into();
        self.presentation_transition = PresentationTransition::Fatal;
        self.transition_deadline = None;
        self.remaining_downgrade_budget = None;
        self.direct_alias_supported = Some(false);
        self.direct_import = None;
        self.pending_external = None;
        self.pending_frame = None;
        if let Some(downgrade_reason) = downgrade_reason {
            self.report_downgrade_reason(downgrade_reason);
        }
        if let Some(session) = self.session.as_mut() {
            let _ = session.command(lumina_video_core::session::SessionCommand::Stop);
        }
        self.state = VideoState::Error(lumina_video_native_frame::video::VideoError::Generic(
            reason,
        ));
    }

    #[cfg(target_os = "linux")]
    fn set_video_state(&mut self, state: VideoState) {
        self.state = state;
        if self.presentation_transition != PresentationTransition::Downgrading {
            return;
        }
        let (remaining, deadline) = sync_downgrade_budget(
            active_playback_state(&self.state),
            self.remaining_downgrade_budget,
            self.transition_deadline,
            Instant::now(),
        );
        self.remaining_downgrade_budget = remaining;
        self.transition_deadline = deadline;
    }

    #[cfg(target_os = "linux")]
    fn begin_system_memory_downgrade(
        &mut self,
        outcome: Option<RendererOutcome>,
        downgrade_reason: CapabilityDowngradeReason,
        request_command: bool,
    ) {
        match self.presentation_transition {
            PresentationTransition::Direct => {
                self.presentation_transition = self.presentation_transition.after_downgrade();
                self.remaining_downgrade_budget = Some(self.config.lifecycle_timeout);
                let (remaining, deadline) = sync_downgrade_budget(
                    active_playback_state(&self.state),
                    self.remaining_downgrade_budget,
                    None,
                    Instant::now(),
                );
                self.remaining_downgrade_budget = remaining;
                self.transition_deadline = deadline;
                if let Some(outcome) = outcome {
                    self.report_renderer_outcome(outcome);
                }
                self.report_downgrade_reason(downgrade_reason);
                self.direct_alias_supported = Some(false);
                self.direct_import = None;
                // The renderer's currently displayed direct frame is the
                // transition placeholder; retire it only after CPU upload.
                self.pending_external = None;
                if request_command && !self.request_system_memory_downgrade() {
                    self.fail_presentation_transition(
                        Some(downgrade_reason),
                        "system-memory downgrade command was rejected",
                    );
                }
            }
            PresentationTransition::Downgrading | PresentationTransition::SystemMemory => {
                if let Some(outcome) = outcome {
                    self.report_renderer_outcome(outcome);
                }
                self.fail_presentation_transition(
                    Some(downgrade_reason),
                    "renderer failed during system-memory downgrade",
                );
            }
            PresentationTransition::Fatal => {}
        }
    }

    #[cfg(target_os = "linux")]
    fn check_presentation_transition_timeout(&mut self) {
        if self.presentation_transition != PresentationTransition::Downgrading
            || !active_playback_state(&self.state)
        {
            return;
        }
        if downgrade_budget_expired(true, self.transition_deadline, Instant::now()) {
            self.fail_presentation_transition(
                Some(CapabilityDowngradeReason::TransitionTimeout),
                "system-memory presentation transition timed out",
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn observe_worker_downgrade(&mut self) {
        if self.presentation_transition != PresentationTransition::Direct {
            return;
        }
        let Some(reason) = self
            .session
            .as_ref()
            .and_then(GstMediaSession::latest_downgrade_reason)
        else {
            return;
        };
        if matches!(
            reason,
            CapabilityDowngradeReason::HardwareOpenFailure
                | CapabilityDowngradeReason::HardwareDecodeFailure
                | CapabilityDowngradeReason::HardwareUnavailable
                | CapabilityDowngradeReason::UnsafeSync
                | CapabilityDowngradeReason::UnsupportedImport
                | CapabilityDowngradeReason::UnsupportedColor
        ) {
            self.begin_system_memory_downgrade(None, reason, false);
        }
    }

    #[cfg(target_os = "linux")]
    fn commit_cpu_frame(&mut self, window: &mut Window, realization: FrameRealization) {
        self.presentation_transition = PresentationTransition::SystemMemory;
        self.transition_deadline = None;
        self.remaining_downgrade_budget = None;
        self.retire_external_frame_now(window);
        let _ = self.commit_realization(realization);
    }

    #[cfg(target_os = "linux")]
    fn request_system_memory_downgrade(&mut self) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        let result = session.command(lumina_video_core::session::SessionCommand::Renegotiate {
            tier: CapabilityTier::SystemMemoryUpload,
        });
        result.is_ok()
    }

    #[cfg(target_os = "linux")]
    fn disable_direct_import(
        &mut self,
        outcome: RendererOutcome,
        downgrade_reason: CapabilityDowngradeReason,
    ) {
        self.begin_system_memory_downgrade(Some(outcome), downgrade_reason, true);
    }

    #[cfg(target_os = "linux")]
    fn ensure_direct_import_route(&mut self) {
        if self.presentation_transition != PresentationTransition::Direct {
            return;
        }
        if self.session.is_none() {
            return;
        }
        if self.direct_alias_supported.is_some() {
            return;
        }
        let Some(gpu) = self.gpu_context.as_ref() else {
            return;
        };
        let adapter_info = gpu.adapter.get_info();
        let supported = adapter_info.backend == wgpu::Backend::Vulkan
            && adapter_info.vendor == 0x8086
            && gpu
                .device
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12);
        if supported {
            if let Some(worker) = DirectImportWorker::new(Arc::clone(&gpu.device)) {
                self.direct_alias_supported = Some(true);
                self.direct_import = Some(worker);
            } else {
                self.begin_system_memory_downgrade(
                    Some(RendererOutcome::TransientFailure),
                    CapabilityDowngradeReason::TransientImport,
                    true,
                );
            }
        } else {
            self.begin_system_memory_downgrade(
                Some(RendererOutcome::Unsupported),
                CapabilityDowngradeReason::RendererUnsupported,
                true,
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn external_frame_outcome_certifies(
        pending_external: bool,
        outcome: gpui_wgpu::ExternalFrameOutcome,
    ) -> bool {
        pending_external && matches!(outcome, gpui_wgpu::ExternalFrameOutcome::Accepted)
    }

    #[cfg(target_os = "linux")]
    fn renderer_feedback(
        outcome: gpui_wgpu::ExternalFrameOutcome,
    ) -> (RendererOutcome, Option<CapabilityDowngradeReason>) {
        match outcome {
            gpui_wgpu::ExternalFrameOutcome::Accepted => (RendererOutcome::Accepted, None),
            gpui_wgpu::ExternalFrameOutcome::Unsupported => (
                RendererOutcome::Unsupported,
                Some(CapabilityDowngradeReason::RendererUnsupported),
            ),
            gpui_wgpu::ExternalFrameOutcome::TransientFailure => (
                RendererOutcome::TransientFailure,
                Some(CapabilityDowngradeReason::RendererTransientFailure),
            ),
            gpui_wgpu::ExternalFrameOutcome::FatalFailure => (
                RendererOutcome::FatalFailure,
                Some(CapabilityDowngradeReason::RendererFatalFailure),
            ),
        }
    }

    #[cfg(target_os = "linux")]
    fn import_failure_classification(
        error: &Nv12ImportError,
    ) -> (RendererOutcome, CapabilityDowngradeReason) {
        match error {
            Nv12ImportError::UnsupportedAcquireSync(_) => (
                RendererOutcome::Unsupported,
                CapabilityDowngradeReason::UnsafeSync,
            ),
            Nv12ImportError::UnsupportedFrame(_) | Nv12ImportError::InvalidLayout { .. } => (
                RendererOutcome::Unsupported,
                CapabilityDowngradeReason::UnsupportedImport,
            ),
            Nv12ImportError::ImportFailed { .. } => (
                RendererOutcome::TransientFailure,
                CapabilityDowngradeReason::TransientImport,
            ),
        }
    }

    #[cfg(target_os = "linux")]
    fn direct_frame_realization(&self) -> Option<FrameRealization> {
        Some(FrameRealization {
            decode: self.session.as_ref()?.decode_mode()?,
            residency: DecodeResidency::NativeGpu,
            import: ImportMode::DirectAlias,
            conversion: ConversionMode::YuvShader,
            synchronization: SynchronizationMode::Explicit,
        })
    }

    #[cfg(target_os = "linux")]
    fn poll_external_outcome(&mut self, window: &mut Window) {
        if self.pending_external.is_none() {
            return;
        }
        let Some(outcome) = window.take_external_frame_outcome() else {
            return;
        };
        if Self::external_frame_outcome_certifies(self.pending_external.is_some(), outcome) {
            let Some(pending) = self.pending_external.take() else {
                return;
            };
            self.direct_display = Some(pending);
            let Some(realization) = self.direct_frame_realization() else {
                self.fail_presentation_transition(
                    None,
                    "decoder mode unavailable for direct frame",
                );
                return;
            };
            if self.commit_realization(realization) {
                self.report_renderer_outcome(RendererOutcome::Accepted);
            }
        } else {
            self.pending_external = None;
            let (renderer_outcome, Some(reason)) = Self::renderer_feedback(outcome) else {
                return;
            };
            self.disable_direct_import(renderer_outcome, reason);
        }
    }

    #[cfg(target_os = "linux")]
    fn stage_imported_frame(&mut self, window: &mut Window, imported: ImportedNv12Texture) {
        let presentation = ExternalPresentation {
            texture: Arc::clone(&imported.texture),
            width: imported.width,
            height: imported.height,
            color_transform: imported.color_transform,
        };
        // SAFETY: `imported.texture` is the exact NV12 Arc created on this window's
        // matching Vulkan device, with wgpu RESOURCE tracking. Its imported image is
        // GENERAL/FOREIGN, and `imported.sync_file` is the single producer fence moved
        // into GPUI; this call only wraps those owned values for renderer submission.
        let frame = match unsafe {
            ExternalNv12Frame::new(
                Arc::clone(&imported.texture),
                imported.sync_file,
                ExternalOwnership::Foreign,
            )
        } {
            Ok(frame) => frame,
            Err(error) => {
                tracing::warn!("external NV12 frame rejected before staging: {error}");
                self.disable_direct_import(
                    RendererOutcome::Unsupported,
                    CapabilityDowngradeReason::UnsupportedImport,
                );
                return;
            }
        };
        let outcome = window.submit_external_frame(ExternalFrameRequest::Prepared(frame));
        // The immediate result is only the mailbox-stage echo. Drain it so
        // the next tick observes only the renderer-produced outcome.
        let _ = window.take_external_frame_outcome();
        match outcome {
            gpui_wgpu::ExternalFrameOutcome::Accepted => {
                self.pending_external = Some(presentation);
            }
            gpui_wgpu::ExternalFrameOutcome::Unsupported
            | gpui_wgpu::ExternalFrameOutcome::TransientFailure
            | gpui_wgpu::ExternalFrameOutcome::FatalFailure => {
                let (renderer_outcome, Some(reason)) = Self::renderer_feedback(outcome) else {
                    return;
                };
                self.disable_direct_import(renderer_outcome, reason);
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn poll_direct_import(&mut self, window: &mut Window) {
        let result = self
            .direct_import
            .as_ref()
            .and_then(DirectImportWorker::try_take);
        match result {
            Some(Ok(imported)) if self.pending_external.is_none() => {
                self.stage_imported_frame(window, imported);
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                let (outcome, reason) = Self::import_failure_classification(&error);
                self.disable_direct_import(outcome, reason);
            }
            None => {}
        }
    }

    #[cfg(target_os = "linux")]
    fn submit_linux_frame(&mut self, frame: lumina_video_native_frame::NativeFrameLease) {
        if self.presentation_transition == PresentationTransition::Fatal {
            drop(frame);
            return;
        }
        if matches!(
            &frame.memory,
            lumina_video_native_frame::NativeMemory::DmaBuf(_)
        ) {
            if self.presentation_transition == PresentationTransition::Direct
                && self.direct_alias_supported == Some(true)
            {
                let enqueued = self
                    .direct_import
                    .as_ref()
                    .is_some_and(|worker| worker.enqueue(frame));
                if !enqueued {
                    self.disable_direct_import(
                        RendererOutcome::TransientFailure,
                        CapabilityDowngradeReason::TransientImport,
                    );
                }
            } else {
                drop(frame);
            }
        } else {
            self.pending_frame = Some(frame);
        }
    }

    #[cfg(target_os = "linux")]
    fn cpu_frame_realization(
        &self,
        frame: &lumina_video_native_frame::NativeFrameLease,
    ) -> Option<FrameRealization> {
        let decode = self.session.as_ref()?.decode_mode()?;
        Some(FrameRealization {
            decode,
            residency: DecodeResidency::SystemMemory,
            import: ImportMode::CpuUpload,
            conversion: if frame.descriptor.format.is_yuv() {
                ConversionMode::YuvShader
            } else {
                ConversionMode::None
            },
            synchronization: SynchronizationMode::CpuWait,
        })
    }

    // -----------------------------------------------------------------------
    // Per-frame update
    // -----------------------------------------------------------------------

    /// Must be called every frame. Polls the decode pipeline, uploads textures,
    /// and syncs playback state from the Linux GStreamer session or the
    /// non-Linux CorePlayer.
    ///
    /// Linux DirectAlias currently requires this player to be the sole active
    /// presenter for `window`; GPUI exposes a Window-global one-shot outcome.
    /// Multi-presenter correlation is deferred outside issue #16.
    pub fn update(&mut self, window: &mut Window, _cx: &mut App) {
        // Lazy GPU context init — retry every frame until available.
        if self.gpu_context.is_none() {
            self.gpu_context = window.gpu_context();
            if self.gpu_context.is_none() {
                if !self.gpu_context_missing_logged {
                    tracing::warn!(
                        "GPU context not available from platform window; \
                         textures cannot be uploaded. Retrying each frame."
                    );
                    self.gpu_context_missing_logged = true;
                }
            } else {
                tracing::info!("GPU context acquired successfully");
                // Reset the flag so if the context is lost we log again.
                self.gpu_context_missing_logged = false;
            }
        }

        #[cfg(target_os = "linux")]
        {
            if self.presentation_transition == PresentationTransition::Fatal {
                return;
            }
            self.observe_worker_downgrade();
            if self.presentation_transition == PresentationTransition::Fatal {
                return;
            }
            self.drain_pending_external_retirement(window);
            self.check_presentation_transition_timeout();
            if self.presentation_transition == PresentationTransition::Fatal {
                return;
            }
            self.ensure_direct_import_route();
            if self.presentation_transition == PresentationTransition::Fatal {
                return;
            }
            self.poll_external_outcome(window);
            if self.presentation_transition == PresentationTransition::Fatal {
                return;
            }
            #[cfg(feature = "moq")]
            if self.core.is_some() {
                self.update_core();
            } else {
                self.update_linux(window);
            }

            #[cfg(not(feature = "moq"))]
            self.update_linux(window);
        }

        #[cfg(not(target_os = "linux"))]
        self.update_core();
    }

    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    fn update_core(&mut self) {
        // Start async decoder init
        if !self.loading_started {
            self.loading_started = true;
            self.start_async_init();
        }

        // Check init completion
        if !self.initialized {
            self.check_init_complete();
            if self.initialized {
                tracing::info!("Decoder initialization complete, playback ready");
            }
        }

        // Sync metadata from decode thread (lazy metadata like macOS AVPlayer)
        if let Some(core) = self.core.as_mut() {
            core.sync_metadata_from_decode_thread();
        } else {
            return;
        }

        // Poll frames and upload to GPU
        let playback_requested = self
            .core
            .as_ref()
            .is_some_and(CorePlayer::is_playback_requested);
        if playback_requested {
            let qlen = self
                .core
                .as_ref()
                .map_or(0, |core| core.frame_queue().len());
            if qlen > 0 {
                tracing::debug!(
                    "update: playback_requested, queue_len={qlen}, state={:?}",
                    self.state
                );
            }
            self.poll_and_upload_frames();
        } else if matches!(self.state, VideoState::Ready | VideoState::Paused { .. }) {
            // Peek at first frame for preview (don't advance queue)
            if self.frame_textures.is_none() {
                self.try_preview_frame();
            }
        } else if self.initialized {
            let qlen = self
                .core
                .as_ref()
                .map_or(0, |core| core.frame_queue().len());
            tracing::debug!(
                "update: skipping poll — not playing/ready, state={:?}, qlen={}",
                self.state,
                qlen
            );
        }

        // Handle end-of-stream / looping
        let at_end = self
            .core
            .as_ref()
            .is_some_and(|core| core.is_eos() && core.is_queue_empty());
        if at_end {
            if self.config.looping && !self.loop_seek_pending {
                tracing::debug!("Loop: seeking to start");
                self.loop_seek_pending = true;
                self.seek(Duration::ZERO);
                self.play();
            } else if !self.config.looping || self.loop_seek_pending {
                if self.loop_seek_pending {
                    tracing::debug!("Loop seek failed (EOS reappeared), ending");
                    self.loop_seek_pending = false;
                }
                if let Some(core) = self.core.as_mut() {
                    core.set_state(VideoState::Ended);
                }
            }
        }

        // Sync state from core
        if let Some(core) = self.core.as_ref() {
            self.state = core.state().clone();
            self.position = core.position();
            self.duration = core.duration();
            self.buffering_percent = core.buffering_percent();
            if let Some(m) = core.metadata() {
                self.metadata = Some(m.clone());
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn update_linux(&mut self, window: &mut Window) {
        self.loading_started = true;
        if self.presentation_transition == PresentationTransition::Fatal {
            return;
        }

        // Exactly one session poll belongs to one GPUI animation tick. The
        // session's frame mailbox already drops stale frames, so draining
        // here would create a second presentation clock.
        let next_event = match self.session.as_mut() {
            Some(session) => session.try_next_event(),
            None => return,
        };
        let decision = match next_event {
            Ok(event) => {
                if let Some(event) = event {
                    match event {
                        SessionEvent::Metadata { metadata } => {
                            self.metadata = Some(video_metadata(&metadata));
                            self.duration = metadata.duration;
                            self.buffering_percent = 100;
                            if self.has_presented_frame {
                                PresentationDecision::Hold
                            } else {
                                PresentationDecision::Empty
                            }
                        }
                        SessionEvent::AudioTracks { .. }
                        | SessionEvent::AudioTrackSelected { .. }
                        | SessionEvent::AudioTrackSelectionFailed { .. } => {
                            PresentationDecision::Hold
                        }
                        SessionEvent::StateChanged { state } => {
                            self.set_video_state(video_state(&state));
                            if !self.initialized && matches!(state, CoreSessionState::Ready) {
                                self.initialized = true;
                                if self.config.autoplay {
                                    self.play();
                                }
                            }
                            if self.has_presented_frame {
                                PresentationDecision::Hold
                            } else {
                                PresentationDecision::Empty
                            }
                        }
                        SessionEvent::Frame { pts, frame } => {
                            self.position = pts;
                            self.has_presented_frame = true;
                            self.loop_seek_pending = false;
                            PresentationDecision::Advanced(frame)
                        }
                        SessionEvent::Ended => {
                            self.set_video_state(VideoState::Ended);
                            if self.has_presented_frame {
                                PresentationDecision::Hold
                            } else {
                                PresentationDecision::Empty
                            }
                        }
                        SessionEvent::Error(error) => {
                            self.set_video_state(VideoState::Error(video_error(&error)));
                            if self.presentation_transition == PresentationTransition::Downgrading
                                || matches!(&error, SessionError::Fatal(_))
                            {
                                self.fail_presentation_transition(
                                    None,
                                    format!("presentation transition failed: {error}"),
                                );
                            }
                            if self.has_presented_frame {
                                PresentationDecision::Hold
                            } else {
                                PresentationDecision::Empty
                            }
                        }
                    }
                } else {
                    if self.has_presented_frame {
                        PresentationDecision::Hold
                    } else {
                        PresentationDecision::Empty
                    }
                }
            }
            Err(error) => {
                self.set_video_state(VideoState::Error(video_error(&error)));
                if self.presentation_transition == PresentationTransition::Downgrading {
                    self.fail_presentation_transition(
                        None,
                        format!("presentation transition failed: {error}"),
                    );
                }
                if self.has_presented_frame {
                    PresentationDecision::Hold
                } else {
                    PresentationDecision::Empty
                }
            }
        };

        if let PresentationDecision::Advanced(frame) = decision {
            self.submit_linux_frame(frame);
        }
        if self.presentation_transition == PresentationTransition::Fatal {
            return;
        }

        self.poll_direct_import(window);
        if self.presentation_transition == PresentationTransition::Fatal {
            return;
        }

        if let Some(gpu) = self.gpu_context.as_ref() {
            if let Some(frame) = self.pending_frame.take() {
                let realization = self.cpu_frame_realization(&frame);
                match native_frame_lease_to_textures(
                    frame,
                    &gpu.device,
                    &gpu.queue,
                    &mut self.y_cache,
                    &mut self.cbcr_cache,
                    &mut self.rgba_cache,
                ) {
                    Ok(textures) => {
                        self.frame_textures = Some(textures);
                        let Some(realization) = realization else {
                            self.fail_presentation_transition(
                                None,
                                "decoder mode unavailable for CPU frame",
                            );
                            return;
                        };
                        self.commit_cpu_frame(window, realization);
                    }
                    Err(NativeFrameIngestionError::UnsupportedAcquireSync(lease)) => {
                        drop(lease);
                        self.fail_presentation_transition(
                            Some(CapabilityDowngradeReason::UnsafeSync),
                            "CPU frame upload acquire synchronization is unsupported",
                        );
                    }
                    Err(NativeFrameIngestionError::UnsupportedDmaBuf(lease))
                    | Err(NativeFrameIngestionError::UnsupportedCpuFormat(lease))
                    | Err(NativeFrameIngestionError::UnsupportedColorMetadata(lease)) => {
                        drop(lease);
                        self.fail_presentation_transition(
                            Some(CapabilityDowngradeReason::UnsupportedImport),
                            "CPU frame upload failed",
                        );
                    }
                }
            }
        }

        if matches!(self.state, VideoState::Ended) && self.config.looping && !self.loop_seek_pending
        {
            self.loop_seek_pending = true;
            self.seek(Duration::ZERO);
            self.play();
        }
    }

    // -----------------------------------------------------------------------
    // Surface element — the video frame
    // -----------------------------------------------------------------------

    /// Returns the GPUI element for the video frame.
    ///
    /// Uses `surface()` for GPU compositing:
    /// - GPU NV12 frames: `surface((y_tex, cbcr_tex, size, transform))`
    /// - Staged external NV12 frames: one multiplanar texture during the
    ///   renderer ownership window, before realization is certified
    /// - CPU-fallback NV12 frames: RGBA passthrough
    /// - RGBA frames: `surface((tex, desc))` — passthrough
    /// - No frame: black placeholder
    pub fn surface_element(&self) -> impl IntoElement {
        #[cfg(target_os = "linux")]
        if let Some(frame) = self
            .pending_external
            .as_ref()
            .or(self.direct_display.as_ref())
        {
            let native_size = size(
                DevicePixels(frame.width as i32),
                DevicePixels(frame.height as i32),
            );
            let color_transform = gpui::Nv12ColorTransform {
                yuv_to_rgb: frame.color_transform,
            };
            return div()
                .size_full()
                .child(
                    surface((frame.texture.clone(), native_size, color_transform))
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .into_element();
        }
        if let Some(ref textures) = self.frame_textures {
            match textures {
                GpuFrameTextures::Nv12 {
                    y_texture,
                    cb_cr_texture,
                    width,
                    height,
                    color_transform,
                } => {
                    let native_size =
                        size(DevicePixels(*width as i32), DevicePixels(*height as i32));
                    let color_transform = gpui::Nv12ColorTransform {
                        yuv_to_rgb: *color_transform,
                    };
                    // Surface must request explicit size; otherwise flex containers
                    // allocate zero bounds to auto-sized children with only aspect_ratio,
                    // and the resulting paint_bounds cause the scissor rect to clip
                    // everything (Issue #1).
                    div()
                        .size_full()
                        .child(
                            surface((
                                y_texture.clone(),
                                cb_cr_texture.clone(),
                                native_size,
                                color_transform,
                            ))
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                        )
                        .into_element()
                }
                GpuFrameTextures::Rgba {
                    texture,
                    width,
                    height,
                } => {
                    let desc = GpuTextureDescriptor {
                        size: size(DevicePixels(*width as i32), DevicePixels(*height as i32)),
                        format: GpuTextureFormat::Rgba8Unorm,
                        color_space: GpuTextureColorSpace::Srgb,
                    };
                    // Same rationale as NV12 above: explicit size avoids zero
                    // layout bounds inside flex containers.
                    div()
                        .size_full()
                        .child(
                            surface((texture.clone(), desc))
                                .size_full()
                                .object_fit(ObjectFit::Contain),
                        )
                        .into_element()
                }
            }
        } else {
            div().size_full().bg(rgb(0x000000)).into_element()
        }
    }

    // -----------------------------------------------------------------------
    // Loading overlay
    // -----------------------------------------------------------------------

    /// Returns a loading overlay element.
    pub fn loading_overlay(&self) -> impl IntoElement {
        div()
            .absolute()
            .size_full()
            .bg(rgb(0x000000))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                // Simple CSS-animated spinner via text
                div().text_xl().text_color(rgb(0xcccccc)).child("⏳"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0x999999))
                    .child("Loading..."),
            )
    }

    // -----------------------------------------------------------------------
    // Error overlay
    // -----------------------------------------------------------------------

    /// Returns an error overlay element.
    pub fn error_overlay(&self) -> impl IntoElement {
        let error_msg = match &self.state {
            VideoState::Error(e) => format!("{e}"),
            _ => String::new(),
        };
        div()
            .absolute()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(div().text_2xl().text_color(rgb(0xff6464)).child("✕"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(0xcccccc))
                    .px_4()
                    .child(format!("Video Error: {error_msg}")),
            )
    }

    // -----------------------------------------------------------------------
    // Buffering overlay
    // -----------------------------------------------------------------------

    /// Returns a buffering overlay element (shown when playing but buffering < 100%).
    pub fn buffering_overlay(&self) -> Option<impl IntoElement> {
        let pct = self.buffering_percent;
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        let is_audio_stall = self.core.as_ref().is_some_and(CorePlayer::is_audio_stall);
        #[cfg(all(target_os = "linux", not(feature = "moq")))]
        let is_audio_stall = false;
        if (pct >= 100 && !is_audio_stall) || !self.is_playing() {
            return None;
        }
        let display_pct = if is_audio_stall { 90 } else { pct };

        Some(
            div()
                .absolute()
                .size_full()
                .bg(rgba(0x00000088))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    div()
                        .text_lg()
                        .text_color(rgb(0x64b4ff))
                        .child(format!("{display_pct}%")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xcccccc))
                        .child("Buffering..."),
                ),
        )
    }

    // -----------------------------------------------------------------------
    // Subtitle overlay
    // -----------------------------------------------------------------------

    /// Returns a subtitle overlay element.
    pub fn subtitle_overlay(&self) -> Option<impl IntoElement> {
        if !self.show_subtitles {
            return None;
        }
        let track = self.subtitle_track.as_ref()?;
        let cue = track.get_cue_at(self.position)?;

        let style = &self.subtitle_style;
        Some(
            div()
                .absolute()
                .bottom(px(style.bottom_margin))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .bg(rgba(0x000000aa))
                        .rounded_sm()
                        .text_size(px(style.font_size))
                        .text_color(rgba(
                            ((style.text_color[0] as u32) << 24)
                                | ((style.text_color[1] as u32) << 16)
                                | ((style.text_color[2] as u32) << 8)
                                | (style.text_color[3] as u32),
                        ))
                        .child(SharedString::from(cue.text.clone())),
                ),
        )
    }

    // -----------------------------------------------------------------------
    // Private methods
    // -----------------------------------------------------------------------

    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    fn start_async_init(&mut self) {
        let Some(core) = self.core.as_mut() else {
            return;
        };
        if core.is_initialized() || core.is_init_pending() {
            return;
        }

        core.init_decoder();
    }

    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    fn check_init_complete(&mut self) {
        let Some(core) = self.core.as_mut() else {
            return;
        };
        if core.is_initialized() {
            self.initialized = true;
            return;
        }

        let complete = core.check_init_complete();
        if complete {
            self.initialized = true;
            if self.config.autoplay && matches!(core.state(), VideoState::Ready) {
                core.play_with_muted(self.config.muted);
            }
        }
    }

    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    fn poll_and_upload_frames(&mut self) {
        let gpu = match self.gpu_context.as_ref() {
            Some(g) => g,
            None => {
                tracing::debug!("poll_and_upload_frames: GPU context not yet available");
                return;
            }
        };

        // Drain every available frame from the decode queue to prevent
        // back-pressure ("QUEUE FULL branch, sleeping 5ms").  Upload only
        // the *last* frame's textures to the GPU — intermediate frames are
        // just popped and dropped so the decoder thread never stalls.
        let queue_len_before = self
            .core
            .as_ref()
            .map_or(0, |core| core.frame_queue().len());
        let mut last_frame = None;
        let mut drained = 0u32;
        if let Some(core) = self.core.as_mut() {
            while let Some(video_frame) = core.poll_frame() {
                drained += 1;
                self.loop_seek_pending = false;
                last_frame = Some(video_frame);
            }
        }
        if drained > 0 {
            tracing::debug!(
                "poll_and_upload: drained {drained} frames, queue was {queue_len_before}"
            );
        } else if queue_len_before > 0 {
            tracing::warn!(
                "poll_and_upload: drained 0 frames but queue has {queue_len_before} — \
                 scheduler is holding frames back (audio not started?)"
            );
        }

        if let Some(video_frame) = last_frame {
            let textures = decoded_frame_to_textures(
                &video_frame.frame,
                &gpu.device,
                &gpu.queue,
                &mut self.y_cache,
                &mut self.cbcr_cache,
                &mut self.rgba_cache,
            );

            match textures {
                Ok(tex) => {
                    if self.frame_textures.is_none() {
                        tracing::info!("First video frame uploaded to GPU");
                    }
                    self.frame_textures = Some(tex);
                }
                Err(LegacyFrameIngestionError::UnsupportedNativeSurface) => {
                    tracing::warn!(
                        "Frame upload rejected borrowed native GPU surface; keeping previous texture"
                    );
                }
            }
        }
    }

    #[cfg(any(not(target_os = "linux"), feature = "moq"))]
    fn try_preview_frame(&mut self) {
        if self.frame_textures.is_some() {
            return;
        }

        let gpu = match self.gpu_context.as_ref() {
            Some(g) => g,
            None => return,
        };

        let Some(frame) = self.core.as_ref().and_then(CorePlayer::peek_frame) else {
            return;
        };

        let textures = decoded_frame_to_textures(
            &frame.frame,
            &gpu.device,
            &gpu.queue,
            &mut self.y_cache,
            &mut self.cbcr_cache,
            &mut self.rgba_cache,
        );

        match textures {
            Ok(tex) => {
                tracing::debug!("Preview frame uploaded to GPU");
                self.frame_textures = Some(tex);
            }
            Err(LegacyFrameIngestionError::UnsupportedNativeSurface) => {
                tracing::warn!(
                    "Preview rejected borrowed native GPU surface; keeping previous texture"
                );
            }
        }
    }
}

impl Drop for GpuiVideoPlayer {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // Drop has no Window: it releases local/session resources here, while Zed may
            // retain at most its bounded latest + displayed external slots (<=2 leases)
            // until an explicit window-aware retire, replacement, or window teardown.
            self.pending_frame = None;
            self.pending_external = None;
            self.direct_import.take();
        }
        #[cfg(any(not(target_os = "linux"), feature = "moq"))]
        if let Some(core) = self.core.take() {
            self.background_executor
                .spawn(async move { drop(core) })
                .detach();
        }
    }
}

#[cfg(all(test, target_os = "linux", feature = "moq"))]
mod tests {
    use super::{linux_playback_route, LinuxPlaybackRoute};

    #[test]
    fn routes_moq_to_core_and_other_sources_to_gst() {
        assert_eq!(
            linux_playback_route("moq://localhost/live/video"),
            LinuxPlaybackRoute::Core
        );
        assert_eq!(
            linux_playback_route("moqs://relay.example/live/video"),
            LinuxPlaybackRoute::Core
        );

        for source in [
            "sample.mp4",
            "file:///tmp/sample.mp4",
            "http://example.com/sample.mp4",
            "https://example.com/sample.m3u8",
            "hls://example.com/live/stream",
        ] {
            assert_eq!(linux_playback_route(source), LinuxPlaybackRoute::Gst);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod color_boundary_tests {
    #[test]
    fn gpui_boundary_copies_all_asymmetric_matrix_columns() {
        let source = [
            [1.0, 2.0, 3.0, 4.0],
            [5.0, 6.0, 7.0, 8.0],
            [9.0, 10.0, 11.0, 12.0],
            [13.0, 14.0, 15.0, 16.0],
        ];
        let copied = gpui::Nv12ColorTransform { yuv_to_rgb: source };
        assert_eq!(copied.yuv_to_rgb, source);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod direct_route_state_tests {
    use super::{try_send_drop_oldest, GpuiVideoPlayer, PresentationTransition};
    use crossbeam_channel::bounded;
    use gpui_wgpu::ExternalFrameOutcome;
    use lumina_video_core::session::{CapabilityDowngradeReason, RendererOutcome};
    use std::time::Duration;

    #[test]
    fn delayed_acceptance_is_the_only_direct_alias_certification() {
        assert!(GpuiVideoPlayer::external_frame_outcome_certifies(
            true,
            ExternalFrameOutcome::Accepted
        ));
        assert!(!GpuiVideoPlayer::external_frame_outcome_certifies(
            false,
            ExternalFrameOutcome::Accepted
        ));
        assert!(!GpuiVideoPlayer::external_frame_outcome_certifies(
            true,
            ExternalFrameOutcome::TransientFailure
        ));
    }

    #[test]
    fn downgrade_transition_is_one_shot() {
        let transition = PresentationTransition::Direct.after_downgrade();
        assert_eq!(transition, PresentationTransition::Downgrading);
        assert_eq!(transition.after_downgrade(), PresentationTransition::Fatal);
    }

    #[test]
    fn renderer_outcomes_map_without_collapsing_failure_kinds() {
        assert_eq!(
            GpuiVideoPlayer::renderer_feedback(ExternalFrameOutcome::Accepted),
            (RendererOutcome::Accepted, None)
        );
        assert_eq!(
            GpuiVideoPlayer::renderer_feedback(ExternalFrameOutcome::Unsupported),
            (
                RendererOutcome::Unsupported,
                Some(CapabilityDowngradeReason::RendererUnsupported)
            )
        );
        assert_eq!(
            GpuiVideoPlayer::renderer_feedback(ExternalFrameOutcome::TransientFailure),
            (
                RendererOutcome::TransientFailure,
                Some(CapabilityDowngradeReason::RendererTransientFailure)
            )
        );
        assert_eq!(
            GpuiVideoPlayer::renderer_feedback(ExternalFrameOutcome::FatalFailure),
            (
                RendererOutcome::FatalFailure,
                Some(CapabilityDowngradeReason::RendererFatalFailure)
            )
        );
    }

    #[test]
    fn downgrade_budget_pauses_and_resumes_without_background_expiry() {
        let base = std::time::Instant::now();
        let timeout = Duration::from_secs(2);
        let (remaining, deadline) = super::sync_downgrade_budget(false, Some(timeout), None, base);
        assert_eq!(remaining, Some(timeout));
        assert!(deadline.is_none());

        let (remaining, deadline) = super::sync_downgrade_budget(true, remaining, deadline, base);
        assert_eq!(deadline, Some(base + timeout));

        let pause_at = base + Duration::from_millis(750);
        let (remaining, deadline) =
            super::sync_downgrade_budget(false, remaining, deadline, pause_at);
        assert_eq!(remaining, Some(Duration::from_millis(1250)));
        assert!(deadline.is_none());

        let resume_at = pause_at + Duration::from_millis(500);
        let (remaining, deadline) =
            super::sync_downgrade_budget(true, remaining, deadline, resume_at);
        assert_eq!(remaining, Some(Duration::from_millis(1250)));
        assert_eq!(deadline, Some(resume_at + Duration::from_millis(1250)));
        assert!(!super::downgrade_budget_expired(
            false,
            deadline,
            resume_at + timeout
        ));
        assert!(super::downgrade_budget_expired(
            true,
            deadline,
            resume_at + Duration::from_millis(1250)
        ));
    }

    #[test]
    fn bounded_import_mailbox_drops_oldest_without_waiting() {
        let (sender, receiver) = bounded(1);
        let drop_receiver = receiver.clone();
        assert!(try_send_drop_oldest(&sender, &drop_receiver, 1));
        assert!(try_send_drop_oldest(&sender, &drop_receiver, 2));
        assert_eq!(receiver.try_recv(), Ok(2));
    }

    #[test]
    fn queue_send_reports_disconnected_without_retry_loop() {
        let (sender, receiver) = bounded::<u8>(1);
        drop(receiver);
        let (_drop_sender, drop_receiver) = bounded::<u8>(1);
        assert!(!try_send_drop_oldest(&sender, &drop_receiver, 1));
    }
}
