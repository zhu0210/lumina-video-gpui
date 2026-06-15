//! GPUI video player widget.
//!
//! Provides a GPU-accelerated video playback component using GPUI's
//! built-in `surface()` element for zero-copy texture compositing.
//!
//! # Architecture
//!
//! ```text
//! CorePlayer (decode + A/V sync)
//!   │ poll_frame()
//!   ▼
//! decoded_frame_to_textures()  ← frame_to_texture.rs
//!   │ NV12: Y(R8) + CbCr(RG8)  →  surface((y, cbcr, size))  [GPU YUV→RGB]
//!   │ RGBA: single RGBA8        →  surface((tex, desc))      [passthrough]
//!   ▼
//! GPUI surface() element  ←  gpui renderer
//! ```
//!
//! GPUI's `surface()` supports NV12 natively with a built-in shader, so
//! YUV frames (NV12, YUV420p) avoid the CPU YUV→RGB conversion entirely.

use std::sync::Arc;
use std::time::Duration;

use gpui::*;
use gpui_wgpu::wgpu;

use lumina_video_core::frame_to_texture::{self, GpuFrameTextures};
use lumina_video_core::player::CorePlayer;
use lumina_video_core::subtitles::{SubtitleError, SubtitleStyle, SubtitleTrack};
#[cfg(feature = "moq")]
use lumina_video_core::video::VideoDecoderBackend;
use lumina_video_core::video::{VideoMetadata, VideoState};

#[cfg(feature = "moq")]
use super::moq_decoder::MoqDecoder;

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

// ---------------------------------------------------------------------------
// Player state
// ---------------------------------------------------------------------------

/// A GPU-accelerated video player for GPUI.
///
/// Wraps [`CorePlayer`] for decode/audio/sync and manages GPU texture
/// upload for GPUI's `surface()` element.
pub struct GpuiVideoPlayer {
    core: CorePlayer,
    config: GpuiVideoPlayerConfig,

    // GPU state
    gpu_context: Option<GpuContextHandle>,
    /// Current frame as GPU textures (NV12 or RGBA variant).
    frame_textures: Option<GpuFrameTextures>,
    /// Cached textures for reuse (Y, CbCr, RGBA).
    y_cache: Option<Arc<wgpu::Texture>>,
    cbcr_cache: Option<Arc<wgpu::Texture>>,
    rgba_cache: Option<Arc<wgpu::Texture>>,

    // Playback state (synced from CorePlayer each update)
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

    // MoQ state
    #[cfg(feature = "moq")]
    moq_stats: Option<super::moq_decoder::MoqStatsHandle>,
    #[cfg(feature = "moq")]
    moq_audio_bound: bool,
    #[cfg(feature = "moq")]
    moq_init_promise: Option<
        poll_promise::Promise<
            Result<Box<dyn VideoDecoderBackend + Send>, lumina_video_core::video::VideoError>,
        >,
    >,
    #[cfg(feature = "moq")]
    moq_init_thread: Option<std::thread::JoinHandle<()>>,
}

impl GpuiVideoPlayer {
    // -----------------------------------------------------------------------
    // Constructors
    // -----------------------------------------------------------------------

    pub fn new(url: impl Into<String>) -> Self {
        Self::with_config(url, GpuiVideoPlayerConfig::default())
    }

    pub fn with_config(url: impl Into<String>, config: GpuiVideoPlayerConfig) -> Self {
        let core = CorePlayer::new(url);
        let muted = config.muted;
        let volume = config.volume;
        let mut player = Self {
            core,
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
            #[cfg(feature = "moq")]
            moq_stats: None,
            #[cfg(feature = "moq")]
            moq_audio_bound: false,
            #[cfg(feature = "moq")]
            moq_init_promise: None,
            #[cfg(feature = "moq")]
            moq_init_thread: None,
        };
        player.core.set_muted(muted);
        player.core.set_volume((volume * 100.0) as u32);
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
        self.core.set_muted(muted);
        self
    }

    pub fn with_volume(mut self, volume: f32) -> Self {
        self.config.volume = volume.clamp(0.0, 1.0);
        self.core.set_volume((self.config.volume * 100.0) as u32);
        self
    }

    // -----------------------------------------------------------------------
    // Playback control
    // -----------------------------------------------------------------------

    pub fn play(&mut self) {
        self.core.play();
    }

    pub fn pause(&mut self) {
        self.core.pause();
    }

    pub fn toggle_playback(&mut self) {
        match self.state {
            VideoState::Playing { .. } => self.pause(),
            VideoState::Paused { .. } | VideoState::Ready | VideoState::Ended => self.play(),
            _ => {}
        }
    }

    pub fn seek(&mut self, position: Duration) {
        self.core.seek(position);
    }

    pub fn toggle_mute(&mut self) {
        self.config.muted = !self.config.muted;
        self.core.set_muted(self.config.muted);
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.config.volume = volume.clamp(0.0, 1.0);
        self.core.set_volume((self.config.volume * 100.0) as u32);
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
        self.core.url()
    }

    pub fn buffering_percent(&self) -> i32 {
        self.buffering_percent
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.core.dimensions()
    }

    pub fn frame_rate(&self) -> Option<f32> {
        self.core.frame_rate()
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
        self.core.audio_handle()
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
    /// and syncs playback state from `CorePlayer`.
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

        // Late-bind MoQ audio handle
        #[cfg(feature = "moq")]
        if self.initialized {
            self.poll_moq_audio_handle();
        }

        // Poll frames and upload to GPU
        if self.core.is_playback_requested() {
            self.poll_and_upload_frames();
        } else if matches!(self.state, VideoState::Ready | VideoState::Paused { .. }) {
            // Peek at first frame for preview (don't advance queue)
            if self.frame_textures.is_none() {
                self.try_preview_frame();
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
        let is_audio_stall = self.core.is_audio_stall();
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

    fn start_async_init(&mut self) {
        if self.core.is_initialized() || self.core.is_init_pending() {
            return;
        }

        #[cfg(feature = "moq")]
        if self.moq_init_promise.is_some() {
            return;
        }

        #[cfg(feature = "moq")]
        if MoqDecoder::is_moq_url(self.core.url()) {
            let url = self.core.url().to_string();
            let (sender, promise) = poll_promise::Promise::new();
            let handle = std::thread::spawn(move || {
                tracing::info!("MoQ decoder init: {url}");
                let result: Result<
                    Box<dyn VideoDecoderBackend + Send>,
                    lumina_video_core::video::VideoError,
                > = match MoqDecoder::new(&url) {
                    Ok(decoder) => {
                        // Stats stored via moq_stats field
                        Ok(Box::new(decoder) as Box<dyn VideoDecoderBackend + Send>)
                    }
                    Err(e) => Err(e),
                };
                sender.send(result);
            });
            self.moq_init_promise = Some(promise);
            self.moq_init_thread = Some(handle);
            return;
        }

        self.core.init_decoder();
    }

    fn check_init_complete(&mut self) {
        if self.core.is_initialized() {
            self.initialized = true;
            return;
        }

        #[cfg(feature = "moq")]
        if let Some(ref promise) = self.moq_init_promise {
            if promise.ready().is_some() {
                let Some(promise) = self.moq_init_promise.take() else {
                    return;
                };
                self.moq_init_thread = None;
                match promise.try_take() {
                    Ok(Ok(decoder)) => {
                        let url = self.core.url().to_string();
                        self.core = CorePlayer::with_decoder(url, decoder);
                        if self.config.autoplay {
                            self.core.play_with_muted(self.config.muted);
                        }
                        self.initialized = true;
                        return;
                    }
                    Ok(Err(e)) => {
                        self.core.set_state(VideoState::Error(e));
                        self.initialized = true;
                        return;
                    }
                    Err(_) => {
                        self.core.set_state(VideoState::Error(
                            lumina_video_core::video::VideoError::Generic(
                                "MoQ init thread crashed".into(),
                            ),
                        ));
                        self.initialized = true;
                        return;
                    }
                }
            }
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

    #[cfg(feature = "moq")]
    fn poll_moq_audio_handle(&mut self) {
        // Simplified MoQ audio binding — full implementation in the egui
        // version has more detailed state management.
        if self.moq_audio_bound {
            return;
        }
        // The MoqDecoder creates its own audio handle internally via CorePlayer.
        // For now, just mark as bound after init — CorePlayer manages this.
        self.moq_audio_bound = true;
    }

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
        let mut last_frame = None;
        while let Some(video_frame) = self.core.poll_frame() {
            self.loop_seek_pending = false;
            last_frame = Some(video_frame);
        }

        if let Some(video_frame) = last_frame {
            let textures = frame_to_texture::decoded_frame_to_textures(
                &video_frame.frame,
                &gpu.device,
                &gpu.queue,
                &mut self.y_cache,
                &mut self.cbcr_cache,
                &mut self.rgba_cache,
            );

            // Only replace textures if we got a valid upload (don't clear on None)
            if let Some(tex) = textures {
                if self.frame_textures.is_none() {
                    tracing::info!("First video frame uploaded to GPU");
                }
                self.frame_textures = Some(tex);
            } else {
                tracing::warn!(
                    "Frame upload returned None — decoded frame could not be \
                     converted to GPU textures (missing CPU fallback?)"
                );
            }
        }
    }

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

        let textures = frame_to_texture::decoded_frame_to_textures(
            &frame.frame,
            &gpu.device,
            &gpu.queue,
            &mut self.y_cache,
            &mut self.cbcr_cache,
            &mut self.rgba_cache,
        );

        if let Some(tex) = textures {
            tracing::debug!("Preview frame uploaded to GPU");
            self.frame_textures = Some(tex);
        } else {
            tracing::warn!("Preview frame upload returned None — missing CPU fallback?");
        }
    }
}
