//! GPUI video player widget.
//!
//! Provides a GPU-accelerated video playback component using GPUI's
//! built-in `surface()` element for zero-copy texture compositing.
//!
//! # Architecture
//!
//! ```text
//! Linux: GstMediaSession (GStreamer decode + A/V timing)
//!   │ one try_next_event() per animation tick
//!   ▼
//! native_frame_lease_to_textures()  ← lumina-video-wgpu
//!   │
//!   ├─ non-Linux: CorePlayer → decoded_frame_to_textures()
//!   ▼
//!   │ NV12: Y(R8) + CbCr(RG8)  →  surface((y, cbcr, size))  [GPU YUV→RGB]
//!   │ RGBA: single RGBA8        →  surface((tex, desc))      [passthrough]
//!   ▼
//! GPUI surface() element  ←  gpui renderer
//! ```
//!
//! GPUI's `surface()` supports NV12 natively with a built-in shader, so
//! YUV frames (NV12, YUV420p) avoid the CPU YUV→RGB conversion entirely.
//! Linux GStreamer frames use the owned lease API; borrowed native GPU
//! surfaces remain rejected at the legacy non-Linux seam.

use std::sync::Arc;
use std::time::Duration;

use gpui::*;
use gpui_wgpu::wgpu;

#[cfg(target_os = "linux")]
use lumina_video_core::session::{
    MediaSession, SessionError, SessionEvent, SessionState as CoreSessionState,
};
use lumina_video_core::subtitles::{SubtitleError, SubtitleStyle, SubtitleTrack};
#[cfg(target_os = "linux")]
use lumina_video_gst::{GstMediaSession, PresentationDecision};
#[cfg(not(target_os = "linux"))]
use lumina_video_native_frame::player::CorePlayer;
use lumina_video_native_frame::video::{VideoMetadata, VideoState};
#[cfg(not(target_os = "linux"))]
use lumina_video_wgpu::decoded_frame_to_textures;
use lumina_video_wgpu::GpuFrameTextures;
#[cfg(not(target_os = "linux"))]
use lumina_video_wgpu::LegacyFrameIngestionError;
#[cfg(target_os = "linux")]
use lumina_video_wgpu::{native_frame_lease_to_textures, NativeFrameIngestionError};

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
}

impl Default for GpuiVideoPlayerConfig {
    fn default() -> Self {
        Self {
            show_controls: true,
            autoplay: false,
            looping: false,
            muted: false,
            volume: 1.0,
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
    core: CorePlayer,
    #[cfg(target_os = "linux")]
    session: GstMediaSession,
    #[cfg(target_os = "linux")]
    url: String,
    #[cfg(target_os = "linux")]
    pending_frame: Option<lumina_video_native_frame::NativeFrameLease>,
    #[cfg(target_os = "linux")]
    has_presented_frame: bool,
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

    pub fn new(url: impl Into<String>) -> Self {
        Self::with_config(url, GpuiVideoPlayerConfig::default())
    }

    pub fn with_config(url: impl Into<String>, config: GpuiVideoPlayerConfig) -> Self {
        let url = url.into();
        #[cfg(not(target_os = "linux"))]
        let core = CorePlayer::new(url);
        #[cfg(target_os = "linux")]
        let session = GstMediaSession::new(url.clone());
        let muted = config.muted;
        let volume = config.volume;
        #[allow(unused_mut)]
        let mut player = Self {
            #[cfg(not(target_os = "linux"))]
            core,
            #[cfg(target_os = "linux")]
            session,
            #[cfg(target_os = "linux")]
            url,
            #[cfg(target_os = "linux")]
            pending_frame: None,
            #[cfg(target_os = "linux")]
            has_presented_frame: false,
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
        #[cfg(not(target_os = "linux"))]
        player.core.set_muted(muted);
        #[cfg(not(target_os = "linux"))]
        player.core.set_volume((volume * 100.0) as u32);
        #[cfg(target_os = "linux")]
        {
            let audio = player.session.audio_handle();
            audio.set_muted(muted);
            audio.set_volume((volume.clamp(0.0, 1.0) * 100.0) as u32);
        }
        player
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
        self.core.set_muted(muted);
        #[cfg(target_os = "linux")]
        self.session.audio_handle().set_muted(muted);
        self
    }

    pub fn with_volume(mut self, volume: f32) -> Self {
        self.config.volume = volume.clamp(0.0, 1.0);
        #[cfg(not(target_os = "linux"))]
        self.core.set_volume((self.config.volume * 100.0) as u32);
        #[cfg(target_os = "linux")]
        self.session
            .audio_handle()
            .set_volume((self.config.volume * 100.0) as u32);
        self
    }

    // -----------------------------------------------------------------------
    // Playback control
    // -----------------------------------------------------------------------

    pub fn play(&mut self) {
        #[cfg(not(target_os = "linux"))]
        self.core.play();
        #[cfg(target_os = "linux")]
        {
            let _ = self
                .session
                .command(lumina_video_core::session::SessionCommand::Play);
        }
    }

    pub fn pause(&mut self) {
        #[cfg(not(target_os = "linux"))]
        self.core.pause();
        #[cfg(target_os = "linux")]
        {
            let _ = self
                .session
                .command(lumina_video_core::session::SessionCommand::Pause);
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
        self.core.seek(position);
        #[cfg(target_os = "linux")]
        {
            let _ = self
                .session
                .command(lumina_video_core::session::SessionCommand::Seek { position });
        }
    }

    pub fn toggle_mute(&mut self) {
        self.config.muted = !self.config.muted;
        #[cfg(not(target_os = "linux"))]
        self.core.set_muted(self.config.muted);
        #[cfg(target_os = "linux")]
        self.session.audio_handle().set_muted(self.config.muted);
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.config.volume = volume.clamp(0.0, 1.0);
        #[cfg(not(target_os = "linux"))]
        self.core.set_volume((self.config.volume * 100.0) as u32);
        #[cfg(target_os = "linux")]
        self.session
            .audio_handle()
            .set_volume((self.config.volume * 100.0) as u32);
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
        #[cfg(target_os = "linux")]
        let url = self.url.as_str();
        #[cfg(not(target_os = "linux"))]
        let url = self.core.url();
        url
    }

    pub fn buffering_percent(&self) -> i32 {
        self.buffering_percent
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        #[cfg(target_os = "linux")]
        let dimensions = self
            .metadata
            .as_ref()
            .map(|metadata| (metadata.width, metadata.height));
        #[cfg(not(target_os = "linux"))]
        let dimensions = self.core.dimensions();
        dimensions
    }

    pub fn frame_rate(&self) -> Option<f32> {
        #[cfg(target_os = "linux")]
        let frame_rate = self.metadata.as_ref().map(|metadata| metadata.frame_rate);
        #[cfg(not(target_os = "linux"))]
        let frame_rate = self.core.frame_rate();
        frame_rate
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
        #[cfg(target_os = "linux")]
        {
            self.session.audio_handle()
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.core.audio_handle()
        }
    }

    // -----------------------------------------------------------------------
    // Frame textures for external rendering
    // -----------------------------------------------------------------------

    pub fn current_textures(&self) -> Option<&GpuFrameTextures> {
        self.frame_textures.as_ref()
    }

    // -----------------------------------------------------------------------
    // Per-frame update
    // -----------------------------------------------------------------------

    /// Must be called every frame. Polls the decode pipeline, uploads textures,
    /// and syncs playback state from the Linux GStreamer session or the
    /// non-Linux CorePlayer.
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
            self.update_linux();
        }

        #[cfg(not(target_os = "linux"))]
        {
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
            self.core.sync_metadata_from_decode_thread();

            // Poll frames and upload to GPU
            if self.core.is_playback_requested() {
                let qlen = self.core.frame_queue().len();
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
            } else {
                // Neither playing nor ready — log why
                if !self.initialized {
                    // Still initializing — expected
                } else {
                    tracing::debug!(
                        "update: skipping poll — not playing/ready, state={:?}, qlen={}",
                        self.state,
                        self.core.frame_queue().len()
                    );
                }
            }

            // Handle end-of-stream / looping
            if self.core.is_eos() && self.core.is_queue_empty() {
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
                    self.core.set_state(VideoState::Ended);
                }
            }

            // Sync state from core
            self.state = self.core.state().clone();
            self.position = self.core.position();
            self.duration = self.core.duration();
            self.buffering_percent = self.core.buffering_percent();
            if let Some(m) = self.core.metadata() {
                self.metadata = Some(m.clone());
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn update_linux(&mut self) {
        self.loading_started = true;

        // Exactly one session poll belongs to one GPUI animation tick. The
        // session's frame mailbox already drops stale frames, so draining
        // here would create a second presentation clock.
        let decision = match self.session.try_next_event() {
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
                        SessionEvent::StateChanged { state } => {
                            self.state = video_state(&state);
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
                            PresentationDecision::Advanced(frame)
                        }
                        SessionEvent::Ended => {
                            self.state = VideoState::Ended;
                            if self.has_presented_frame {
                                PresentationDecision::Hold
                            } else {
                                PresentationDecision::Empty
                            }
                        }
                        SessionEvent::Error(error) => {
                            self.state = VideoState::Error(video_error(&error));
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
                self.state = VideoState::Error(video_error(&error));
                if self.has_presented_frame {
                    PresentationDecision::Hold
                } else {
                    PresentationDecision::Empty
                }
            }
        };

        if let PresentationDecision::Advanced(frame) = decision {
            self.pending_frame = Some(frame);
        }

        if let Some(gpu) = self.gpu_context.as_ref() {
            if let Some(frame) = self.pending_frame.take() {
                match native_frame_lease_to_textures(
                    frame,
                    &gpu.device,
                    &gpu.queue,
                    &mut self.y_cache,
                    &mut self.cbcr_cache,
                    &mut self.rgba_cache,
                ) {
                    Ok(textures) => self.frame_textures = Some(textures),
                    Err(NativeFrameIngestionError::UnsupportedAcquireSync(lease))
                    | Err(NativeFrameIngestionError::UnsupportedDmaBuf(lease))
                    | Err(NativeFrameIngestionError::UnsupportedCpuFormat(lease)) => {
                        let _ = lease;
                        tracing::warn!(
                            "GStreamer session frame was unsupported by the GPU upload seam; keeping previous texture"
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
    /// - NV12 frames: `surface((y_tex, cbcr_tex, size))` — GPU-side YUV→RGB
    /// - RGBA frames: `surface((tex, desc))` — passthrough
    /// - No frame: black placeholder
    pub fn surface_element(&self) -> impl IntoElement {
        if let Some(ref textures) = self.frame_textures {
            match textures {
                GpuFrameTextures::Nv12 {
                    y_texture,
                    cb_cr_texture,
                    width,
                    height,
                } => {
                    let native_size =
                        size(DevicePixels(*width as i32), DevicePixels(*height as i32));
                    // Surface must request explicit size; otherwise flex containers
                    // allocate zero bounds to auto-sized children with only aspect_ratio,
                    // and the resulting paint_bounds cause the scissor rect to clip
                    // everything (Issue #1).
                    div()
                        .size_full()
                        .child(
                            surface((y_texture.clone(), cb_cr_texture.clone(), native_size))
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
        #[cfg(not(target_os = "linux"))]
        let is_audio_stall = self.core.is_audio_stall();
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
        if self.core.is_initialized() || self.core.is_init_pending() {
            return;
        }

        self.core.init_decoder();
    }

    #[cfg(not(target_os = "linux"))]
    fn check_init_complete(&mut self) {
        if self.core.is_initialized() {
            self.initialized = true;
            return;
        }

        let complete = self.core.check_init_complete();
        if complete {
            self.initialized = true;
            if self.config.autoplay && matches!(self.core.state(), VideoState::Ready) {
                self.core.play_with_muted(self.config.muted);
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

        // Drain every available frame from the decode queue to prevent
        // back-pressure ("QUEUE FULL branch, sleeping 5ms").  Upload only
        // the *last* frame's textures to the GPU — intermediate frames are
        // just popped and dropped so the decoder thread never stalls.
        let queue_len_before = self.core.frame_queue().len();
        let mut last_frame = None;
        let mut drained = 0u32;
        while let Some(video_frame) = self.core.poll_frame() {
            drained += 1;
            self.loop_seek_pending = false;
            last_frame = Some(video_frame);
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

    #[cfg(not(target_os = "linux"))]
    fn try_preview_frame(&mut self) {
        if self.frame_textures.is_some() {
            return;
        }

        let gpu = match self.gpu_context.as_ref() {
            Some(g) => g,
            None => return,
        };

        let Some(frame) = self.core.peek_frame() else {
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
