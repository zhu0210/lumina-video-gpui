//! GPUI video player widget.
//!
//! Provides a GPU-accelerated video playback component using GPUI's
//! built-in `surface()` element for zero-copy texture compositing.
//!
//! # Architecture
//!
//! ```text
//! Linux: GstMediaSession owns GStreamer decode and A/V timing, including MoQ.
//!   │ one try_next_event() per animation tick
//!   ├─ Intel Vulkan + NV12: Gst DMABuf + SyncFile
//!   │    → bounded import worker → one multiplanar external texture
//!   │    → same-frame pending surface → normal renderer acquire/render/release
//!   └─ system fallback: native_frame_lease_to_textures()  ← lumina-video-wgpu
//!        NV12: Y(R8) + CbCr(RG8) → surface((y, cbcr, size)) [GPU YUV→RGB]
//!        RGBA: single RGBA8       → surface(RgbaTextureSource)     [passthrough]
//!   ▼
//! GPUI surface() element  ←  gpui renderer
//! ```
//!
//! GPUI's `surface()` supports NV12 natively with a built-in shader, so
//! YUV frames (NV12, YUV420p) avoid the CPU YUV→RGB conversion entirely.
//! Linux GStreamer frames use the owned lease API; borrowed native GPU
//! surfaces on Apple retain their producer leases through GPU-tracked texture destruction.

use std::sync::Arc;
use std::time::Duration;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::time::Instant;

use gpui::*;
use gpui_wgpu::wgpu;
use lumina_video_core::session::{
    AudioTrack, CapabilityDowngradeReason, CapabilityTier, FrameRealization, RendererOutcome,
    SessionError,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
use crossbeam_channel::{Receiver, Sender, TrySendError};
#[cfg(target_os = "linux")]
use gpui_wgpu::{ExternalFrameRequest, ExternalNv12Frame, ExternalOwnership};
#[cfg(target_os = "linux")]
use lumina_video_core::session::{
    ConversionMode, DecodeResidency, ImportMode, MediaSession, SessionEvent,
    SessionState as CoreSessionState, SynchronizationMode,
};
use lumina_video_core::subtitles::{SubtitleError, SubtitleStyle, SubtitleTrack};
#[cfg(target_os = "linux")]
use lumina_video_gst::{GstMediaSession, PresentationDecision, DEFAULT_OPEN_TIMEOUT};
#[cfg(not(target_os = "linux"))]
use lumina_video_native_frame::player::CorePlayer;
use lumina_video_native_frame::video::{VideoMetadata, VideoState};
#[cfg(not(target_os = "linux"))]
use lumina_video_wgpu::decoded_frame_to_textures;
use lumina_video_wgpu::GpuFrameTextures;
#[cfg(target_os = "linux")]
use lumina_video_wgpu::{import_external_dmabuf_nv12, ImportedNv12Texture, Nv12ImportError};
#[cfg(target_os = "linux")]
use lumina_video_wgpu::{native_frame_lease_to_textures, NativeFrameIngestionError};

// Preview selection is independent of the retained display texture: a paused seek
// or source replacement must refresh it once a new frame becomes available.
#[cfg(any(not(target_os = "linux"), test))]
struct PreviewState {
    refresh_requested: bool,
}

#[cfg(any(not(target_os = "linux"), test))]
impl PreviewState {
    fn new() -> Self {
        Self {
            refresh_requested: true,
        }
    }

    fn invalidate(&mut self) {
        self.refresh_requested = true;
    }

    fn frame(
        &self,
        queue: &lumina_video_native_frame::frame_queue::FrameQueue,
    ) -> Option<lumina_video_native_frame::video::VideoFrame> {
        self.refresh_requested.then(|| queue.peek()).flatten()
    }

    fn presented(&mut self) {
        self.refresh_requested = false;
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct ExternalPresentation {
    // The local pending/display slot and GPUI's exact texture Arc are both
    // required owners; the bounded mailbox/renderer slots prevent growth.
    texture: Arc<wgpu::Texture>,
    width: u32,
    height: u32,
    color_transform: [[f32; 4]; 4],
    color_transfer: gpui::VideoTransferFunction,
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
    input: Sender<(u64, lumina_video_native_frame::NativeFrameLease)>,
    input_drop: Receiver<(u64, lumina_video_native_frame::NativeFrameLease)>,
    output: Receiver<(u64, Result<ImportedNv12Texture, Nv12ImportError>)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(any(target_os = "linux", target_os = "android"))]
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
                while let Ok((epoch, lease)) = input_receiver.recv() {
                    // SAFETY: this route enqueues only GStreamer-owned DMABuf leases whose
                    // export supplied a producer-ready SyncFile and truthful NV12 layout;
                    // the producer has handed the image to GENERAL/FOREIGN ownership and
                    // `device` is the matching Vulkan device. The importer only validates
                    // and wraps the image/fence; it never waits or submits GPU work.
                    let imported = unsafe { import_external_dmabuf_nv12(lease, &device) };
                    let _ = try_send_drop_oldest(&output, &worker_output_drop, (epoch, imported));
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

    fn enqueue(&self, epoch: u64, lease: lumina_video_native_frame::NativeFrameLease) -> bool {
        try_send_drop_oldest(&self.input, &self.input_drop, (epoch, lease))
    }

    fn try_take(&self, epoch: u64) -> Option<Result<ImportedNv12Texture, Nv12ImportError>> {
        take_import_for_epoch(&self.output, epoch)
    }
}

// A seek can overtake an import already running on the worker. Filter both
// successful textures and errors before either can affect the new presentation.
#[cfg(target_os = "linux")]
fn take_import_for_epoch<T>(receiver: &Receiver<(u64, T)>, epoch: u64) -> Option<T> {
    let (import_epoch, result) = receiver.try_recv().ok()?;
    (import_epoch == epoch).then_some(result)
}

#[cfg(target_os = "linux")]
impl Drop for DirectImportWorker {
    fn drop(&mut self) {
        let _ = self.thread.take();
    }
}

#[cfg(target_os = "android")]
struct AndroidImportWorker {
    input: Sender<Arc<lumina_video_native_frame::android_video::AndroidVideoFrame>>,
    input_drop: Receiver<Arc<lumina_video_native_frame::android_video::AndroidVideoFrame>>,
    output: Receiver<(
        Arc<lumina_video_native_frame::android_video::AndroidVideoFrame>,
        Result<lumina_video_wgpu::PreparedAndroidFrame, String>,
    )>,
}

#[cfg(target_os = "android")]
impl AndroidImportWorker {
    fn new(device: Arc<wgpu::Device>) -> Option<Self> {
        let (input, receiver) = crossbeam_channel::bounded(1);
        let input_drop = receiver.clone();
        let (sender, output) = crossbeam_channel::bounded(1);
        let output_drop = output.clone();
        std::thread::Builder::new()
            .name("lumina-ahb-import".into())
            .spawn(move || {
                let mut importer = lumina_video_wgpu::AndroidFrameImporter::default();
                while let Ok(frame) = receiver.recv() {
                    // SAFETY: only unmodified ImageReader frames enter this
                    // mailbox. The bridge retains Image ownership and completes
                    // producer acquire before delivery; GPUI submits commands
                    // before sampling the returned texture.
                    let prepared = unsafe { importer.prepare(Arc::clone(&frame), &device) }
                        .map_err(|error| error.to_string());
                    let _ = try_send_drop_oldest(&sender, &output_drop, (frame, prepared));
                }
            })
            .ok()?;
        Some(Self {
            input,
            input_drop,
            output,
        })
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

// ---------------------------------------------------------------------------
// Player state
// ---------------------------------------------------------------------------

/// A GPU-accelerated video player for GPUI.
///
/// Wraps the Linux [`GstMediaSession`] or non-Linux [`CorePlayer`] for
/// decode/audio/sync and manages GPU texture upload for GPUI's `surface()`
/// element.
pub struct GpuiVideoPlayer {
    #[cfg(not(target_os = "linux"))]
    background_executor: BackgroundExecutor,
    #[cfg(not(target_os = "linux"))]
    core: Option<CorePlayer>,
    #[cfg(not(target_os = "linux"))]
    preview: PreviewState,
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
    presentation_epoch: u64,
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
    #[cfg(target_os = "android")]
    android_import: Option<AndroidImportWorker>,
    #[cfg(target_os = "android")]
    android_last_frame: Option<Arc<lumina_video_native_frame::android_video::AndroidVideoFrame>>,
    #[cfg(target_os = "android")]
    android_import_disabled: bool,
    #[cfg(target_os = "android")]
    android_fallback_pending: bool,
    #[cfg(target_os = "android")]
    android_previous_textures: Option<GpuFrameTextures>,
    #[cfg(target_os = "android")]
    android_pending: bool,
    #[cfg(target_os = "android")]
    android_import_deadline: Option<Instant>,
    #[cfg(target_os = "android")]
    android_presentation_deadline: Option<Instant>,
    #[cfg(target_os = "android")]
    android_native_presented: bool,
    #[cfg(target_os = "android")]
    android_retirement_pending: bool,

    // GPU state
    gpu_context: Option<WgpuContextHandle>,
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
    audio_tracks: Vec<AudioTrack>,
    selected_audio_track_id: Option<String>,
    audio_track_selection_error: Option<String>,

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
        #[cfg(not(target_os = "linux"))]
        let background_executor = cx.background_executor().clone();
        #[cfg(target_os = "linux")]
        let _ = cx;
        #[cfg(not(target_os = "linux"))]
        let core = Some(CorePlayer::new(url.clone()));
        #[cfg(target_os = "linux")]
        let session = Some(
            GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                url.clone(),
                false,
                lumina_video_gst::GstAudioSinkMode::Auto,
                config.lifecycle_timeout,
                config.open_timeout,
                0,
                CapabilityTier::DirectAlias,
            ),
        );
        let muted = config.muted;
        let volume = config.volume;
        #[allow(unused_mut)]
        let mut player = Self {
            #[cfg(not(target_os = "linux"))]
            background_executor,
            #[cfg(not(target_os = "linux"))]
            core,
            #[cfg(not(target_os = "linux"))]
            preview: PreviewState::new(),
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
            presentation_epoch: 0,
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
            #[cfg(target_os = "android")]
            android_import: None,
            #[cfg(target_os = "android")]
            android_last_frame: None,
            #[cfg(target_os = "android")]
            android_import_disabled: false,
            #[cfg(target_os = "android")]
            android_fallback_pending: false,
            #[cfg(target_os = "android")]
            android_previous_textures: None,
            #[cfg(target_os = "android")]
            android_pending: false,
            #[cfg(target_os = "android")]
            android_import_deadline: None,
            #[cfg(target_os = "android")]
            android_presentation_deadline: None,
            #[cfg(target_os = "android")]
            android_native_presented: false,
            #[cfg(target_os = "android")]
            android_retirement_pending: false,
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
            audio_tracks: Vec::new(),
            selected_audio_track_id: None,
            audio_track_selection_error: None,
            loading_started: false,
            initialized: false,
            loop_seek_pending: false,
            gpu_context_missing_logged: false,
            subtitle_track: None,
            show_subtitles: true,
            subtitle_style: SubtitleStyle::default(),
        };
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
        self.preview.invalidate();
        self.audio_tracks.clear();
        self.selected_audio_track_id = None;
        self.audio_track_selection_error = None;
        #[cfg(target_os = "android")]
        {
            self.reset_android_import();
            self.android_import_disabled = false;
            self.android_fallback_pending = false;
        }
        #[cfg(target_os = "linux")]
        let next_generation = self
            .session
            .as_ref()
            .map_or(1, |session| session.stream_generation().saturating_add(1));

        // CorePlayer teardown retains its join semantics, so move the old
        // backend to GPUI's background executor before replacing it. The
        // GStreamer session drop is already fire-and-forget.
        #[cfg(not(target_os = "linux"))]
        let old_core = self.core.take();
        #[cfg(not(target_os = "linux"))]
        if let Some(old_core) = old_core {
            self.background_executor
                .spawn(async move { drop(old_core) })
                .detach();
        }
        #[cfg(target_os = "linux")]
        let old_session = self.session.take();
        #[cfg(target_os = "linux")]
        drop(old_session);

        #[cfg(not(target_os = "linux"))]
        {
            self.core = Some(CorePlayer::new(url.clone()));
        }
        #[cfg(target_os = "linux")]
        {
            self.session = Some(
                    GstMediaSession::new_with_autoplay_and_audio_sink_and_timeouts_and_generation_and_tier(
                        url.clone(),
                        false,
                        lumina_video_gst::GstAudioSinkMode::Auto,
                        self.config.lifecycle_timeout,
                        self.config.open_timeout,
                        next_generation,
                        CapabilityTier::DirectAlias,
                    ),
                );
            if let Some(session) = self.session.as_ref() {
                let audio = session.audio_handle();
                audio.set_muted(self.config.muted);
                audio.set_volume((self.config.volume.clamp(0.0, 1.0) * 100.0) as u32);
            }
        }
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
        if let Some(core) = self.core.as_mut() {
            core.play();
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            let replay = matches!(session.snapshot().state, CoreSessionState::Ended);
            if session
                .command(lumina_video_core::session::SessionCommand::Play)
                .is_ok()
                && replay
            {
                self.invalidate_pending_linux_imports();
            }
        }
    }

    pub fn pause(&mut self) {
        #[cfg(not(target_os = "linux"))]
        if let Some(core) = self.core.as_mut() {
            core.pause();
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
        #[cfg(not(target_os = "linux"))]
        self.preview.invalidate();
        #[cfg(target_os = "android")]
        self.reset_android_import();
        #[cfg(not(target_os = "linux"))]
        if let Some(core) = self.core.as_mut() {
            core.seek(position);
        }
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            if session
                .command(lumina_video_core::session::SessionCommand::Seek { position })
                .is_ok()
            {
                self.invalidate_pending_linux_imports();
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn invalidate_pending_linux_imports(&mut self) {
        self.presentation_epoch = self.presentation_epoch.wrapping_add(1);
        self.pending_frame = None;
        // Already submitted/displayed textures remain the seek placeholder.
    }

    /// Discovered source audio tracks. Currently populated by GStreamer on Linux.
    pub fn audio_tracks(&self) -> &[AudioTrack] {
        &self.audio_tracks
    }

    /// The last confirmed selection; a queued request does not change this value.
    pub fn selected_audio_track_id(&self) -> Option<&str> {
        self.selected_audio_track_id.as_deref()
    }

    /// Most recent asynchronous selection failure, cleared by a new request or success.
    pub fn audio_track_selection_error(&self) -> Option<&str> {
        self.audio_track_selection_error.as_deref()
    }

    /// Request selection by the stable id returned by `audio_tracks`.
    /// Success means queued; observe `selected_audio_track_id` for confirmation.
    pub fn select_audio_track(&mut self, id: impl Into<String>) -> Result<(), SessionError> {
        let id = id.into();
        #[cfg(target_os = "linux")]
        if let Some(session) = self.session.as_mut() {
            session.command(lumina_video_core::session::SessionCommand::SelectAudioTrack { id })?;
            self.audio_track_selection_error = None;
            return Ok(());
        }
        let _ = id;
        Err(SessionError::Unsupported(
            "audio track selection is unavailable for this backend".into(),
        ))
    }

    pub fn toggle_mute(&mut self) {
        self.config.muted = !self.config.muted;
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
        if let Some(core) = self.core.as_ref() {
            return core.dimensions();
        }

        self.metadata
            .as_ref()
            .map(|metadata| (metadata.width, metadata.height))
    }

    pub fn frame_rate(&self) -> Option<f32> {
        #[cfg(not(target_os = "linux"))]
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
        #[cfg(not(target_os = "linux"))]
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

    #[cfg(target_os = "android")]
    pub fn frame_realization(&self) -> Option<FrameRealization> {
        use lumina_video_core::session::{
            ConversionMode, DecodeMode, DecodeResidency, ImportMode, SynchronizationMode,
        };
        let textures = self.frame_textures.as_ref()?;
        let native = self.android_native_presented;
        Some(FrameRealization {
            decode: if self
                .core
                .as_ref()
                .is_some_and(|core| core.android_player_id() != 0)
            {
                DecodeMode::Hardware
            } else {
                DecodeMode::Software
            },
            residency: if native {
                DecodeResidency::NativeGpu
            } else {
                DecodeResidency::SystemMemory
            },
            import: if native {
                ImportMode::GpuCopy
            } else {
                ImportMode::CpuUpload
            },
            conversion: if native {
                ConversionMode::GpuBlit
            } else if matches!(textures, GpuFrameTextures::Nv12 { .. }) {
                ConversionMode::YuvShader
            } else {
                ConversionMode::None
            },
            synchronization: if native {
                SynchronizationMode::CpuWait
            } else {
                SynchronizationMode::None
            },
        })
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
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

    #[cfg(target_os = "android")]
    pub fn capability(&self) -> Option<CapabilityTier> {
        self.frame_realization()
            .map(FrameRealization::capability_tier)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
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
        #[cfg(target_os = "android")]
        {
            if self.android_pending
                || self.android_native_presented
                || self.android_retirement_pending
            {
                let _ = window.clear_external_frame();
                let _ = window.take_external_frame_outcome();
            }
            self.reset_android_import();
            self.android_retirement_pending = false;
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
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
        let adapter_info = gpu.adapter().get_info();
        let supported = adapter_info.backend == wgpu::Backend::Vulkan
            && adapter_info.vendor == 0x8086
            && gpu
                .device()
                .features()
                .contains(wgpu::Features::TEXTURE_FORMAT_NV12);
        if supported {
            if let Some(worker) = DirectImportWorker::new(Arc::clone(gpu.device())) {
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
        use lumina_video_native_frame::ColorTransfer;
        let color_transfer = match imported.color_transfer {
            ColorTransfer::Srgb => gpui::VideoTransferFunction::Srgb,
            ColorTransfer::Bt601 | ColorTransfer::Bt709 => gpui::VideoTransferFunction::Bt709,
            _ => {
                self.disable_direct_import(
                    RendererOutcome::Unsupported,
                    CapabilityDowngradeReason::UnsupportedColor,
                );
                return;
            }
        };
        let presentation = ExternalPresentation {
            texture: Arc::clone(&imported.texture),
            width: imported.width,
            height: imported.height,
            color_transform: imported.color_transform,
            color_transfer,
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
            .and_then(|worker| worker.try_take(self.presentation_epoch));
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
                    .is_some_and(|worker| worker.enqueue(self.presentation_epoch, frame));
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
            self.update_linux(window);
        }

        #[cfg(not(target_os = "linux"))]
        self.update_core();
        #[cfg(target_os = "android")]
        self.update_android_import(window);
    }

    #[cfg(target_os = "android")]
    fn observe_android_cpu_upload(&mut self) {
        // The decoder only emits CPU frames after an explicit native-path failure
        // or on sources/platform versions that cannot provide an owned GPU frame.
        self.android_retirement_pending |= self.android_pending || self.android_native_presented;
        self.android_import = None;
        self.android_last_frame = None;
        self.android_import_deadline = None;
        self.android_presentation_deadline = None;
        self.android_import_disabled = true;
        self.android_fallback_pending = false;
        self.android_pending = false;
        self.android_previous_textures = None;
        self.android_native_presented = false;
    }

    #[cfg(target_os = "android")]
    fn reset_android_import(&mut self) {
        self.preview.invalidate();
        self.android_import_deadline = None;
        self.android_presentation_deadline = None;
        self.android_last_frame = None;
        self.android_import = None;
        if self.android_pending {
            self.frame_textures = self.android_previous_textures.take();
        }
        self.android_pending = false;
        self.android_retirement_pending = true;
    }

    #[cfg(target_os = "android")]
    fn downgrade_android_import(&mut self, reason: &str) {
        if self.android_import_disabled {
            return;
        }
        tracing::warn!(
            "Android native import unavailable; requesting system-memory delivery: {reason}"
        );
        self.android_import_disabled = true;
        self.reset_android_import();
        self.android_fallback_pending = true;
    }

    #[cfg(target_os = "android")]
    fn queue_android_frame(
        &mut self,
        surface: &lumina_video_native_frame::video::AndroidGpuSurface,
    ) {
        if self.android_import_disabled {
            return;
        }
        let Some(frame) = surface.native_frame() else {
            self.downgrade_android_import("native frame has no owned ImageReader lease");
            return;
        };
        if self
            .android_last_frame
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, &frame))
        {
            return;
        }
        if self.android_import.is_none() {
            let Some(gpu) = self.gpu_context.as_ref() else {
                return;
            };
            self.android_import = AndroidImportWorker::new(Arc::clone(gpu.device()));
        }
        let sent = self.android_import.as_ref().is_some_and(|worker| {
            try_send_drop_oldest(&worker.input, &worker.input_drop, Arc::clone(&frame))
        });
        if sent {
            self.android_last_frame = Some(frame);
            let now = Instant::now();
            self.android_import_deadline.get_or_insert(
                now.checked_add(self.config.lifecycle_timeout)
                    .unwrap_or(now),
            );
        } else {
            self.downgrade_android_import("import worker unavailable");
        }
    }

    #[cfg(target_os = "android")]
    fn update_android_import(&mut self, window: &mut Window) {
        use gpui::{ExternalFrameRequest, ExternalRgbaFrame};
        use gpui_wgpu::ExternalFrameOutcome;
        if self.android_retirement_pending {
            let _ = window.clear_external_frame();
            let _ = window.take_external_frame_outcome();
            self.android_retirement_pending = false;
        }
        if self.android_pending {
            match window.take_external_frame_outcome() {
                Some(ExternalFrameOutcome::Accepted) => {
                    self.android_pending = false;
                    self.android_presentation_deadline = None;
                    self.preview.presented();
                    if !self.android_native_presented {
                        tracing::info!(
                            "Android native frame presented via GPU conversion; no CPU pixel upload"
                        );
                    }
                    self.android_native_presented = true;
                    self.android_previous_textures = None;
                }
                Some(_) => {
                    self.downgrade_android_import("renderer rejected prepared GPU conversion")
                }
                None => {}
            }
        }
        if self.android_fallback_pending {
            if let Some(core) = self.core.as_ref() {
                self.android_fallback_pending =
                    !lumina_video_native_frame::android_video::request_cpu_fallback_for_player(
                        core.android_player_id(),
                    );
            }
        }
        if self.android_import_disabled {
            return;
        }
        if self.android_pending {
            if self
                .android_presentation_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.downgrade_android_import("timed out waiting for renderer acknowledgement");
            }
            return;
        }
        let Some(worker) = self.android_import.as_ref() else {
            return;
        };
        let (source, prepared) = match worker.output.try_recv() {
            Ok((source, Ok(frame))) => (source, frame),
            Ok((_, Err(error))) => {
                self.downgrade_android_import(&error);
                return;
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.downgrade_android_import("import worker disconnected");
                return;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                if self
                    .android_import_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                {
                    self.downgrade_android_import("timed out waiting for native frame import");
                }
                return;
            }
        };
        // A newer frame may still be queued/in flight. Bound lack of progress,
        // without extending the budget merely because new input keeps arriving.
        let now = Instant::now();
        self.android_import_deadline = self
            .android_last_frame
            .as_ref()
            .filter(|last| !Arc::ptr_eq(last, &source))
            .map(|_| {
                now.checked_add(self.config.lifecycle_timeout)
                    .unwrap_or(now)
            });
        let texture = Arc::clone(&prepared.texture);
        let (width, height) = (prepared.width, prepared.height);
        // SAFETY: the importer records conversion on this window's exact device.
        // Commands initialize this texture to RESOURCE before sampling; its HAL
        // callback retains every Vulkan resource and the producer Image lease.
        let frame = match unsafe {
            ExternalRgbaFrame::new_with_commands(prepared.texture, prepared.commands)
        } {
            Ok(frame) => frame,
            Err(error) => {
                self.downgrade_android_import(&error.to_string());
                return;
            }
        };
        let outcome = window.submit_external_frame(ExternalFrameRequest::PreparedRgba(frame));
        // Discard only the staging echo; certification arrives after queue submit.
        let _ = window.take_external_frame_outcome();
        if outcome != ExternalFrameOutcome::Accepted {
            self.downgrade_android_import("could not stage prepared GPU conversion");
            return;
        }
        self.android_previous_textures = self.frame_textures.replace(GpuFrameTextures::Rgba {
            texture,
            width,
            height,
        });
        self.android_pending = true;
        let now = Instant::now();
        self.android_presentation_deadline = Some(
            now.checked_add(self.config.lifecycle_timeout)
                .unwrap_or(now),
        );
    }

    #[cfg(not(target_os = "linux"))]
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
            // A retained display texture must not suppress a paused seek/source preview.
            self.try_preview_frame();
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
                        SessionEvent::AudioTracks {
                            tracks,
                            selected_id,
                        } => {
                            self.audio_tracks = tracks;
                            self.selected_audio_track_id = selected_id;
                            PresentationDecision::Hold
                        }
                        SessionEvent::AudioTrackSelected { track } => {
                            self.selected_audio_track_id = Some(track.id);
                            self.audio_track_selection_error = None;
                            PresentationDecision::Hold
                        }
                        SessionEvent::AudioTrackSelectionFailed {
                            requested_id,
                            prior_restored_id,
                            reason,
                        } => {
                            self.selected_audio_track_id = prior_restored_id;
                            self.audio_track_selection_error =
                                Some(format!("{requested_id}: {reason}"));
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
                    gpu.device(),
                    gpu.queue(),
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
    /// - RGBA frames: `surface(RgbaTextureSource)` — passthrough
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
                transfer: frame.color_transfer,
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
                        transfer: gpui::VideoTransferFunction::Srgb,
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
                    let Ok(source) = RgbaTextureSource::new(
                        texture.clone(),
                        size(DevicePixels(*width as i32), DevicePixels(*height as i32)),
                        GpuTextureAlphaMode::Opaque,
                        GpuTextureColorSpace::Srgb,
                    ) else {
                        return div().size_full().bg(rgb(0x000000)).into_element();
                    };
                    // Same rationale as NV12 above: explicit size avoids zero
                    // layout bounds inside flex containers.
                    div()
                        .size_full()
                        .child(surface(source).size_full().object_fit(ObjectFit::Contain))
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
        #[cfg(not(target_os = "linux"))]
        let is_audio_stall = self.core.as_ref().is_some_and(CorePlayer::is_audio_stall);
        #[cfg(target_os = "linux")]
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

    #[cfg(not(target_os = "linux"))]
    fn start_async_init(&mut self) {
        let Some(core) = self.core.as_mut() else {
            return;
        };
        if core.is_initialized() || core.is_init_pending() {
            return;
        }

        core.init_decoder();
    }

    #[cfg(not(target_os = "linux"))]
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

    #[cfg(not(target_os = "linux"))]
    fn poll_and_upload_frames(&mut self) {
        let gpu = match self.gpu_context.as_ref() {
            Some(g) => g,
            None => {
                tracing::debug!("poll_and_upload_frames: GPU context not yet available");
                return;
            }
        };

        // The scheduler owns frame timing and can return the current frame again.
        // Poll once per refresh; draining until None can spin forever on that frame.
        let last_frame = self.core.as_mut().and_then(CorePlayer::poll_frame);
        if last_frame.is_some() {
            self.loop_seek_pending = false;
        }

        if let Some(video_frame) = last_frame {
            #[cfg(target_os = "android")]
            if let lumina_video_native_frame::video::DecodedFrame::Android(surface) =
                &video_frame.frame
            {
                self.queue_android_frame(surface);
                return;
            }
            let textures = decoded_frame_to_textures(
                &video_frame.frame,
                gpu.device(),
                gpu.queue(),
                &mut self.y_cache,
                &mut self.cbcr_cache,
                &mut self.rgba_cache,
            );

            match textures {
                Ok(tex) => {
                    if self.frame_textures.is_none() {
                        tracing::info!("First video frame prepared for GPU rendering");
                    }
                    #[cfg(target_os = "android")]
                    self.observe_android_cpu_upload();
                    self.preview.presented();
                    self.frame_textures = Some(tex);
                }
                Err(error) => {
                    tracing::warn!("Frame upload failed: {error}; keeping previous texture");
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn try_preview_frame(&mut self) {
        let gpu = match self.gpu_context.as_ref() {
            Some(g) => g,
            None => return,
        };

        let Some(frame) = self
            .core
            .as_ref()
            .and_then(|core| self.preview.frame(core.frame_queue()))
        else {
            return;
        };

        #[cfg(target_os = "android")]
        if let lumina_video_native_frame::video::DecodedFrame::Android(surface) = &frame.frame {
            self.queue_android_frame(surface);
            return;
        }

        let textures = decoded_frame_to_textures(
            &frame.frame,
            gpu.device(),
            gpu.queue(),
            &mut self.y_cache,
            &mut self.cbcr_cache,
            &mut self.rgba_cache,
        );

        match textures {
            Ok(tex) => {
                tracing::debug!("Preview frame prepared for GPU rendering");
                #[cfg(target_os = "android")]
                self.observe_android_cpu_upload();
                self.preview.presented();
                self.frame_textures = Some(tex);
            }
            Err(error) => {
                tracing::warn!("Preview upload failed: {error}; keeping previous texture");
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
        #[cfg(not(target_os = "linux"))]
        if let Some(core) = self.core.take() {
            self.background_executor
                .spawn(async move { drop(core) })
                .detach();
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
        let copied = gpui::Nv12ColorTransform {
            yuv_to_rgb: source,
            transfer: gpui::VideoTransferFunction::Srgb,
        };
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
    fn seek_epoch_rejects_in_flight_success_and_failure_without_restarting_worker() {
        let (sender, receiver) = bounded(1);
        let before_seek = 0;
        let after_seek = 1;
        // Simulate imports that complete after a seek has been accepted.
        sender
            .send((before_seek, Ok::<_, &str>("old texture")))
            .unwrap();
        assert_eq!(super::take_import_for_epoch(&receiver, after_seek), None);
        sender
            .send((before_seek, Err::<&str, _>("old import failure")))
            .unwrap();
        assert_eq!(super::take_import_for_epoch(&receiver, after_seek), None);
        // The same mailbox still carries current successes and errors.
        sender
            .send((after_seek, Ok::<_, &str>("seek texture")))
            .unwrap();
        assert_eq!(
            super::take_import_for_epoch(&receiver, after_seek),
            Some(Ok("seek texture"))
        );
        sender
            .send((after_seek, Err::<&str, _>("current failure")))
            .unwrap();
        assert_eq!(
            super::take_import_for_epoch(&receiver, after_seek),
            Some(Err("current failure"))
        );
    }

    #[test]
    fn queue_send_reports_disconnected_without_retry_loop() {
        let (sender, receiver) = bounded::<u8>(1);
        drop(receiver);
        let (_drop_sender, drop_receiver) = bounded::<u8>(1);
        assert!(!try_send_drop_oldest(&sender, &drop_receiver, 1));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::PreviewState;
    use lumina_video_native_frame::{
        frame_queue::FrameQueue,
        video::{CpuFrame, DecodedFrame, PixelFormat, Plane, VideoFrame},
    };
    use std::time::Duration;

    #[test]
    fn paused_preview_refresh_survives_retained_image_and_empty_seek_queue() {
        let queue = FrameQueue::new(2);
        let frame = |seconds| {
            VideoFrame::new(
                Duration::from_secs(seconds),
                DecodedFrame::Cpu(CpuFrame::new(
                    PixelFormat::Rgba,
                    1,
                    1,
                    vec![Plane::new(vec![0, 0, 0, 255], 4)],
                )),
            )
        };
        let mut preview = PreviewState::new();
        assert!(queue.try_push(frame(1)));
        let displayed = preview.frame(&queue).unwrap();
        preview.presented();
        assert!(preview.frame(&queue).is_none());
        // CorePlayer flushes on seek. The display keeps its owned old image,
        // while the preview request waits across arbitrary empty refreshes.
        queue.flush();
        preview.invalidate();
        assert!(preview.frame(&queue).is_none());
        assert!(preview.frame(&queue).is_none());
        assert_eq!(displayed.pts, Duration::from_secs(1));
        assert!(queue.try_push(frame(10)));
        let pending = preview.frame(&queue).unwrap();
        assert_eq!(pending.pts, Duration::from_secs(10));
        assert_eq!(queue.len(), 1); // preview does not advance the presentation clock
        assert_eq!(displayed.pts, Duration::from_secs(1));
        // Only presentation completes the request; a queued native import does not.
        assert!(preview.frame(&queue).is_some());
        preview.presented();
        assert!(preview.frame(&queue).is_none());
        // Source replacement/fallback uses the same invalidation while paused.
        queue.flush();
        preview.invalidate();
        assert!(queue.try_push(frame(0)));
        assert_eq!(preview.frame(&queue).unwrap().pts, Duration::ZERO);
    }
}
