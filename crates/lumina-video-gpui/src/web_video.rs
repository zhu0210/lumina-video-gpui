//! Web video playback using browser's native HTMLVideoElement.
//!
//! This module provides hardware-accelerated video playback on web browsers
//! by leveraging the browser's built-in video decoder. Unlike native platforms
//! where we implement `VideoDecoderBackend`, web uses a fundamentally different
//! architecture:
//!
//! - Browser's `<video>` element handles decoding (hardware-accelerated)
//! - `requestVideoFrameCallback` provides frame-accurate timing
//! - Texture upload via `copyExternalImageToTexture` (WebGPU) or `texImage2D` (WebGL)
//! - HLS streaming via native support (Safari) or hls.js (Chrome/Firefox/Edge)
//!
//! # Architecture
//!
//! ```text
//! Rust/WASM (this module)          JavaScript (video-bridge.js)
//! ┌─────────────────────┐          ┌─────────────────────────────┐
//! │ WebVideoPlayer      │◄────────►│ Hidden <video> element      │
//! │   - state           │          │ requestVideoFrameCallback   │
//! │   - frame_ready     │          │ HLS.js for adaptive streaming│
//! │   - dimensions      │          │ Audio sync (native)         │
//! └─────────────────────┘          └─────────────────────────────┘
//! ```

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{window, HtmlVideoElement};
use wgpu;

use lumina_video_core::video::{VideoError, VideoMetadata, VideoState};

/// Callback type for frame-ready notifications from JavaScript.
pub type FrameReadyCallback = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

/// Web video player that wraps browser's HTMLVideoElement.
///
/// This provides a Rust-friendly interface to browser video playback,
/// handling HLS streaming, frame synchronization, and state management.
pub struct WebVideoPlayer {
    /// The underlying HTML video element
    video: HtmlVideoElement,
    /// Current playback state
    state: VideoState,
    /// Video metadata (populated after loadedmetadata event)
    metadata: Option<VideoMetadata>,
    /// HLS.js instance handle (for non-Safari browsers)
    hls_handle: Option<JsValue>,
    /// Callback closure for requestVideoFrameCallback
    frame_callback: Option<Closure<dyn FnMut(f64, JsValue)>>,
    /// Flag indicating a new frame is ready for texture upload
    frame_ready: Rc<RefCell<bool>>,
    /// Current video time in seconds (updated by frame callback).
    /// Reserved for future A/V sync - provides more precise timing than video.currentTime.
    #[allow(dead_code)]
    current_time: Rc<RefCell<f64>>,
    /// Whether this is an HLS stream
    is_hls: bool,
}

impl WebVideoPlayer {
    /// Creates a new web video player for the given URL.
    ///
    /// Automatically detects HLS streams (.m3u8) and uses hls.js on
    /// browsers without native HLS support.
    pub fn new(url: &str) -> Result<Self, VideoError> {
        let window = window()
            .ok_or_else(|| VideoError::DecoderInit("No window object available".to_string()))?;

        let document = window
            .document()
            .ok_or_else(|| VideoError::DecoderInit("No document available".to_string()))?;

        // Create hidden video element
        let video: HtmlVideoElement = document
            .create_element("video")
            .map_err(|e| {
                VideoError::DecoderInit(format!("Failed to create video element: {:?}", e))
            })?
            .dyn_into()
            .map_err(|_| VideoError::DecoderInit("Element is not a video".to_string()))?;

        // Configure video element for optimal playback
        video.set_cross_origin(Some("anonymous")); // Enable CORS for texture upload
        video.set_preload("auto");
        // set_plays_inline is the correct method name in web-sys
        video.set_attribute("playsinline", "true").ok(); // Required for iOS
                                                         // Start muted to comply with browser autoplay policies.
                                                         // User must explicitly unmute via UI or call set_muted(false) after user gesture.
        video.set_muted(true);

        // Hide from DOM but keep functional
        let style = video.style();
        let _ = style.set_property("position", "absolute");
        let _ = style.set_property("width", "1px");
        let _ = style.set_property("height", "1px");
        let _ = style.set_property("opacity", "0");
        let _ = style.set_property("pointer-events", "none");

        // Append to document body
        document
            .body()
            .ok_or_else(|| VideoError::DecoderInit("No document body".to_string()))?
            .append_child(&video)
            .map_err(|e| VideoError::DecoderInit(format!("Failed to append video: {:?}", e)))?;

        let is_hls = url.contains(".m3u8") || url.contains("application/vnd.apple.mpegurl");
        let frame_ready = Rc::new(RefCell::new(false));
        let current_time = Rc::new(RefCell::new(0.0));

        let mut player = Self {
            video,
            state: VideoState::Loading,
            metadata: None,
            hls_handle: None,
            frame_callback: None,
            frame_ready,
            current_time,
            is_hls,
        };

        player.setup_event_listeners()?;
        player.load_source(url)?;

        Ok(player)
    }

    /// Sets up event listeners for video state changes.
    ///
    /// Currently relies on polling via `update_state()`. Event-based listeners
    /// for loadedmetadata, canplay, error, and ended events would improve
    /// responsiveness but are not yet implemented.
    fn setup_event_listeners(&mut self) -> Result<(), VideoError> {
        // Polling-based state updates are sufficient for current use cases.
        // Event listeners could be added for more responsive state changes.
        Ok(())
    }

    /// Loads the video source, using HLS.js if needed.
    fn load_source(&mut self, url: &str) -> Result<(), VideoError> {
        if self.is_hls {
            // Check for native HLS support (Safari)
            if !self
                .video
                .can_play_type("application/vnd.apple.mpegurl")
                .is_empty()
            {
                // Safari: native HLS
                self.video.set_src(url);
            } else {
                // Chrome/Firefox/Edge: use hls.js
                self.hls_handle = Some(init_hls_js(&self.video, url)?);
            }
        } else {
            // Direct video source (MP4, WebM, etc.)
            self.video.set_src(url);
        }

        Ok(())
    }

    /// Starts frame callback registration for accurate frame timing.
    pub fn start_frame_callbacks(&mut self) -> Result<(), VideoError> {
        let frame_ready = self.frame_ready.clone();
        let current_time = self.current_time.clone();

        let callback = Closure::new(move |now: f64, metadata: JsValue| {
            *frame_ready.borrow_mut() = true;
            // Extract mediaTime from metadata for accurate video time, fallback to now
            let media_time = js_sys::Reflect::get(&metadata, &"mediaTime".into())
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(now / 1000.0);
            *current_time.borrow_mut() = media_time;
        });

        // Register the callback
        request_video_frame_callback(&self.video, &callback)?;

        self.frame_callback = Some(callback);
        Ok(())
    }

    /// Returns true if a new frame is ready for texture upload.
    pub fn is_frame_ready(&self) -> bool {
        *self.frame_ready.borrow()
    }

    /// Clears the frame-ready flag after texture upload.
    pub fn clear_frame_ready(&self) {
        *self.frame_ready.borrow_mut() = false;
    }

    /// Returns a reference to the underlying video element for texture upload.
    pub fn video_element(&self) -> &HtmlVideoElement {
        &self.video
    }

    /// Starts or resumes video playback.
    pub fn play(&self) -> Result<(), VideoError> {
        js_play_video(&self.video)
            .map_err(|error| VideoError::Generic(format!("Play failed: {error:?}")))?;
        Ok(())
    }

    /// Pauses video playback.
    pub fn pause(&self) {
        js_pause_video(&self.video);
    }

    /// Seeks to a specific position.
    pub fn seek(&self, position: Duration) {
        self.video.set_current_time(position.as_secs_f64());
    }

    /// Sets the volume (0.0 to 1.0).
    pub fn set_volume(&self, volume: f32) {
        self.video.set_volume(volume.clamp(0.0, 1.0) as f64);
    }

    /// Returns the current volume (0.0 to 1.0).
    pub fn volume(&self) -> f32 {
        self.video.volume() as f32
    }

    /// Sets the muted state.
    pub fn set_muted(&self, muted: bool) {
        self.video.set_muted(muted);
    }

    /// Returns true if the video is muted.
    pub fn is_muted(&self) -> bool {
        self.video.muted()
    }

    /// Toggles the muted state.
    pub fn toggle_mute(&self) {
        self.video.set_muted(!self.video.muted());
    }

    // ========================================================================
    // Audio/Video Synchronization
    // ========================================================================
    //
    // HTMLVideoElement handles A/V sync natively - the browser's media pipeline
    // ensures audio and video frames are presented in sync. No additional
    // synchronization code is needed.
    //
    // ## Autoplay Policy
    //
    // Modern browsers require user interaction before playing audio. The video
    // element is created with `muted: true` initially to allow autoplay. Call
    // `set_muted(false)` after a user gesture (click/tap) to enable audio.
    //
    // ## Best Practice
    //
    // 1. Start videos muted (done automatically)
    // 2. Show an "unmute" button in the UI
    // 3. Call `set_muted(false)` when user clicks the unmute button
    // 4. If play() fails with NotAllowedError, video needs user gesture

    /// Returns the current playback position.
    pub fn position(&self) -> Duration {
        Duration::from_secs_f64(self.video.current_time())
    }

    /// Returns the total duration if known.
    pub fn duration(&self) -> Option<Duration> {
        let dur = self.video.duration();
        if dur.is_nan() || dur.is_infinite() {
            None
        } else {
            Some(Duration::from_secs_f64(dur))
        }
    }

    /// Returns the video dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.video.video_width(), self.video.video_height())
    }

    /// Returns true if the video is currently playing.
    pub fn is_playing(&self) -> bool {
        !self.video.paused() && !self.video.ended()
    }

    /// Returns true if the video has ended.
    pub fn is_ended(&self) -> bool {
        self.video.ended()
    }

    /// Returns the current buffering percentage (0-100).
    ///
    /// Note: This uses the end of the last buffered range, which may overestimate
    /// progress for sparse buffering (multiple non-contiguous ranges). For more
    /// accurate buffering info with HLS, use `hls_buffer_info()` instead.
    pub fn buffering_percent(&self) -> i32 {
        let buffered = self.video.buffered();
        let duration = self.video.duration();

        if duration.is_nan() || duration <= 0.0 || buffered.length() == 0 {
            return 0;
        }

        // Get the end of the last buffered range (may overestimate for sparse buffering)
        if let Ok(end) = buffered.end(buffered.length() - 1) {
            ((end / duration) * 100.0) as i32
        } else {
            0
        }
    }

    /// Returns the video metadata.
    pub fn metadata(&self) -> Option<&VideoMetadata> {
        self.metadata.as_ref()
    }

    /// Updates metadata from the video element (call after loadedmetadata event).
    pub fn update_metadata(&mut self) {
        let (width, height) = self.dimensions();
        if width > 0 && height > 0 {
            self.metadata = Some(VideoMetadata {
                width,
                height,
                duration: self.duration(),
                frame_rate: 30.0, // Browsers don't expose frame rate directly
                codec: "browser-native".to_string(),
                pixel_aspect_ratio: 1.0,
                start_time: None, // Browser doesn't expose stream start time
            });
        }
    }

    /// Returns the current playback state.
    pub fn state(&self) -> &VideoState {
        &self.state
    }

    /// Updates the playback state based on video element state.
    pub fn update_state(&mut self) {
        self.state = if let Some(error) = js_get_video_error(&self.video) {
            VideoState::Error(VideoError::Generic(error))
        } else if self.video.ended() {
            VideoState::Ended
        } else if self.video.paused() {
            VideoState::Paused {
                position: self.position(),
            }
        } else if self.video.ready_state() < 3 {
            // HAVE_FUTURE_DATA = 3
            VideoState::Buffering {
                position: self.position(),
            }
        } else {
            VideoState::Playing {
                position: self.position(),
            }
        };
    }
}

impl Drop for WebVideoPlayer {
    fn drop(&mut self) {
        js_cancel_video_frame_callback(&self.video);
        let _ = self.video.pause();
        // Clean up HLS.js instance
        if let Some(hls) = self.hls_handle.take() {
            destroy_hls_js(&hls);
        }

        // Remove video element from DOM
        if let Some(parent) = self.video.parent_node() {
            let _ = parent.remove_child(&self.video);
        }
    }
}

// ============================================================================
// JavaScript interop functions
// ============================================================================

/// Initializes HLS.js for the given video element.
fn init_hls_js(video: &HtmlVideoElement, url: &str) -> Result<JsValue, VideoError> {
    js_init_hls(video, url)
        .map_err(|e| VideoError::DecoderInit(format!("HLS.js initialization failed: {:?}", e)))
}

/// Destroys an HLS.js instance.
fn destroy_hls_js(hls: &JsValue) {
    js_destroy_hls(hls);
}

/// Registers a requestVideoFrameCallback.
fn request_video_frame_callback(
    video: &HtmlVideoElement,
    callback: &Closure<dyn FnMut(f64, JsValue)>,
) -> Result<(), VideoError> {
    js_request_video_frame_callback(video, callback.as_ref().unchecked_ref())
        .map_err(|e| VideoError::Generic(format!("requestVideoFrameCallback failed: {:?}", e)))?;
    Ok(())
}

// ============================================================================
// wasm-bindgen extern declarations
// ============================================================================

#[wasm_bindgen(module = "/web/video-bridge.js")]
extern "C" {
    #[wasm_bindgen(catch, js_name = "playVideo")]
    fn js_play_video(video: &HtmlVideoElement) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = "pauseVideo")]
    fn js_pause_video(video: &HtmlVideoElement);

    #[wasm_bindgen(js_name = "getVideoError")]
    fn js_get_video_error(video: &HtmlVideoElement) -> Option<String>;

    /// Initializes HLS.js and attaches it to the video element.
    /// Returns the Hls instance handle.
    #[wasm_bindgen(catch, js_name = "initHls")]
    fn js_init_hls(video: &HtmlVideoElement, url: &str) -> Result<JsValue, JsValue>;

    /// Destroys an HLS.js instance and cleans up resources.
    #[wasm_bindgen(js_name = "destroyHls")]
    fn js_destroy_hls(hls: &JsValue);

    /// Registers a requestVideoFrameCallback on the video element.
    /// The callback receives (now, metadata) where now is timestamp in ms.
    #[wasm_bindgen(catch, js_name = "requestVideoFrameCallback")]
    fn js_request_video_frame_callback(
        video: &HtmlVideoElement,
        callback: &js_sys::Function,
    ) -> Result<(), JsValue>;

    #[wasm_bindgen(js_name = "cancelVideoFrameCallback")]
    fn js_cancel_video_frame_callback(video: &HtmlVideoElement);

    /// Gets the current HLS quality levels.
    #[wasm_bindgen(catch, js_name = "getHlsLevels")]
    fn js_get_hls_levels(hls: &JsValue) -> Result<JsValue, JsValue>;

    /// Sets the current HLS quality level (-1 for auto).
    #[wasm_bindgen(catch, js_name = "setHlsLevel")]
    fn js_set_hls_level(hls: &JsValue, level: i32) -> Result<(), JsValue>;

    /// Gets HLS.js buffer statistics.
    #[wasm_bindgen(catch, js_name = "getHlsBufferInfo")]
    fn js_get_hls_buffer_info(hls: &JsValue) -> Result<JsValue, JsValue>;
}

// ============================================================================
// HLS quality level management
// ============================================================================

/// Represents an HLS quality level.
#[derive(Debug, Clone)]
pub struct HlsQualityLevel {
    /// Level index (0-based)
    pub index: i32,
    /// Bitrate in bits per second
    pub bitrate: u32,
    /// Resolution width
    pub width: u32,
    /// Resolution height
    pub height: u32,
    /// Codec string
    pub codec: String,
}

impl WebVideoPlayer {
    /// Returns available HLS quality levels.
    pub fn hls_quality_levels(&self) -> Vec<HlsQualityLevel> {
        if let Some(hls) = &self.hls_handle {
            if let Ok(levels) = js_get_hls_levels(hls) {
                // Parse levels from JsValue (array of level objects)
                parse_hls_levels(&levels)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    /// Sets the HLS quality level. Use -1 for automatic bitrate selection.
    pub fn set_hls_quality_level(&self, level: i32) {
        if let Some(hls) = &self.hls_handle {
            let _ = js_set_hls_level(hls, level);
        }
    }

    /// Returns the current HLS buffer info (buffer length, etc.).
    pub fn hls_buffer_info(&self) -> Option<HlsBufferInfo> {
        if let Some(hls) = &self.hls_handle {
            if let Ok(info) = js_get_hls_buffer_info(hls) {
                return parse_hls_buffer_info(&info);
            }
        }
        None
    }
}

/// HLS buffer statistics.
#[derive(Debug, Clone)]
pub struct HlsBufferInfo {
    /// Length of buffered content in seconds
    pub buffer_length: f64,
    /// Estimated bandwidth in bits per second
    pub bandwidth: u32,
    /// Current quality level index
    pub current_level: i32,
}

fn parse_hls_levels(levels: &JsValue) -> Vec<HlsQualityLevel> {
    let mut result = Vec::new();

    // Check if levels is an array
    if !js_sys::Array::is_array(levels) {
        return result;
    }

    let array = js_sys::Array::from(levels);
    for i in 0..array.length() {
        let item = array.get(i);
        if item.is_undefined() || item.is_null() {
            continue;
        }

        // Extract fields from the level object
        let index = js_sys::Reflect::get(&item, &"index".into())
            .ok()
            .and_then(|v| v.as_f64())
            .map(|v| v as i32)
            .unwrap_or(i as i32);

        let bitrate = js_sys::Reflect::get(&item, &"bitrate".into())
            .ok()
            .and_then(|v| v.as_f64())
            .map(|v| v as u32)
            .unwrap_or(0);

        let width = js_sys::Reflect::get(&item, &"width".into())
            .ok()
            .and_then(|v| v.as_f64())
            .map(|v| v as u32)
            .unwrap_or(0);

        let height = js_sys::Reflect::get(&item, &"height".into())
            .ok()
            .and_then(|v| v.as_f64())
            .map(|v| v as u32)
            .unwrap_or(0);

        let codec = js_sys::Reflect::get(&item, &"codec".into())
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "unknown".to_string());

        result.push(HlsQualityLevel {
            index,
            bitrate,
            width,
            height,
            codec,
        });
    }

    result
}

impl WebVideoPlayer {
    /// Toggles between play and pause.
    pub fn toggle_playback(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            let _ = self.play();
        }
    }

    /// Updates a GPUI-compatible WebGPU texture when the browser reports a
    /// decoded frame. The returned texture can be wrapped in GPUI's validated
    /// `RgbaTextureSource`.
    pub fn update_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &mut Option<WebVideoTexture>,
    ) -> Result<Option<std::sync::Arc<wgpu::Texture>>, VideoError> {
        let (width, height) = self.dimensions();
        if width == 0 || height == 0 {
            return Ok(None);
        }

        if texture
            .as_ref()
            .is_none_or(|texture| texture.dimensions() != (width, height))
        {
            *texture = Some(WebVideoTexture::new(device, width, height));
        }

        if self.is_frame_ready() {
            if let Some(texture) = texture.as_ref() {
                if texture.upload_frame(queue, &self.video)? {
                    self.clear_frame_ready();
                }
            }
        }

        Ok(texture.as_ref().map(|texture| texture.texture().clone()))
    }
}

fn parse_hls_buffer_info(info: &JsValue) -> Option<HlsBufferInfo> {
    if info.is_undefined() || info.is_null() {
        return None;
    }

    let buffer_length = js_sys::Reflect::get(info, &"bufferLength".into())
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let bandwidth = js_sys::Reflect::get(info, &"bandwidth".into())
        .ok()
        .and_then(|v| v.as_f64())
        .map(|v| v as u32)
        .unwrap_or(0);

    let current_level = js_sys::Reflect::get(info, &"currentLevel".into())
        .ok()
        .and_then(|v| v.as_f64())
        .map(|v| v as i32)
        .unwrap_or(-1);

    Some(HlsBufferInfo {
        buffer_length,
        bandwidth,
        current_level,
    })
}

// ============================================================================
// Web Video Texture Pipeline
// ============================================================================

/// Texture pipeline for uploading video frames to GPU on web.
///
/// Uses `wgpu::Queue::copy_external_image_to_texture` for efficient GPU-to-GPU
/// copy from the browser's video decoder to a wgpu texture. This is NOT true
/// zero-copy (WebGPU has no external memory import), but avoids CPU-side pixel
/// access.
///
/// # Performance
/// The browser compositor already has the decoded video frame in GPU memory.
/// `copyExternalImageToTexture` performs a GPU-to-GPU blit, typically sub-1ms
/// for 1080p content.
pub struct WebVideoTexture {
    /// The wgpu texture for rendering
    texture: std::sync::Arc<wgpu::Texture>,
    /// Texture view for shader access
    view: wgpu::TextureView,
    /// Current texture dimensions
    width: u32,
    height: u32,
}

/// GPUI integration for browser-native video and HLS playback.
///
/// Decoding and audio remain owned by `HTMLVideoElement`; decoded frames are
/// copied GPU-to-GPU into a texture created on GPUI's wgpu device.
pub struct GpuiWebVideoPlayer {
    player: WebVideoPlayer,
    upload_texture: Option<WebVideoTexture>,
    current_texture: Option<std::sync::Arc<wgpu::Texture>>,
}

impl GpuiWebVideoPlayer {
    pub fn new(url: &str) -> Result<Self, VideoError> {
        let mut player = WebVideoPlayer::new(url)?;
        player.start_frame_callbacks()?;
        Ok(Self {
            player,
            upload_texture: None,
            current_texture: None,
        })
    }

    pub fn player(&self) -> &WebVideoPlayer {
        &self.player
    }

    pub fn player_mut(&mut self) -> &mut WebVideoPlayer {
        &mut self.player
    }

    pub fn update(&mut self, window: &mut gpui::Window) -> Result<(), VideoError> {
        self.player.update_state();
        if self.player.metadata().is_none() {
            self.player.update_metadata();
        }
        let Some(context) = window.gpu_context() else {
            return Ok(());
        };
        self.current_texture = self.player.update_texture(
            context.device(),
            context.queue(),
            &mut self.upload_texture,
        )?;

        if matches!(
            self.player.state(),
            VideoState::Loading | VideoState::Playing { .. } | VideoState::Buffering { .. }
        ) || self.player.video.seeking()
            || (self.player.video.ready_state() < 2 && self.player.video.error().is_none())
        {
            window.request_animation_frame();
        }
        Ok(())
    }

    pub fn surface_element(&self) -> gpui::AnyElement {
        use gpui::{DevicePixels, GpuTextureAlphaMode, GpuTextureColorSpace, IntoElement, Styled};

        let Some(texture) = &self.current_texture else {
            return gpui::div()
                .size_full()
                .bg(gpui::rgb(0x000000))
                .into_any_element();
        };
        let (width, height) = self.player.dimensions();
        let source = gpui::RgbaTextureSource::new(
            texture.clone(),
            gpui::size(DevicePixels(width as i32), DevicePixels(height as i32)),
            GpuTextureAlphaMode::Opaque,
            GpuTextureColorSpace::Srgb,
        );
        match source {
            Ok(source) => gpui::surface(source)
                .size_full()
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            Err(_) => gpui::div()
                .size_full()
                .bg(gpui::rgb(0x000000))
                .into_any_element(),
        }
    }
}

impl WebVideoTexture {
    /// Creates a new web video texture with the given dimensions.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("web_video_texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // RGBA8 is the format browsers provide via copyExternalImageToTexture
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });

        #[expect(
            clippy::arc_with_non_send_sync,
            reason = "GPUI texture sources require Arc; browser textures stay on the UI thread"
        )]
        let texture = std::sync::Arc::new(texture);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            texture,
            view,
            width,
            height,
        }
    }

    /// Returns the underlying wgpu texture.
    pub fn texture(&self) -> &std::sync::Arc<wgpu::Texture> {
        &self.texture
    }

    /// Returns the texture view.
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Returns current dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Uploads a video frame from the HTMLVideoElement to the texture.
    ///
    /// Uses `copy_external_image_to_texture` for efficient GPU-to-GPU copy.
    /// Returns true if upload succeeded, false if video dimensions changed
    /// (caller should recreate the texture).
    pub fn upload_frame(
        &self,
        queue: &wgpu::Queue,
        video: &HtmlVideoElement,
    ) -> Result<bool, VideoError> {
        let video_width = video.video_width();
        let video_height = video.video_height();

        // Check if dimensions changed
        if video_width != self.width || video_height != self.height {
            return Ok(false); // Signal that texture needs recreation
        }

        // Skip upload if video has no content yet
        if video_width == 0 || video_height == 0 {
            return Ok(true);
        }

        // Use wgpu's copy_external_image_to_texture for efficient GPU-to-GPU copy
        // This is available on WebGPU backend
        queue.copy_external_image_to_texture(
            &wgpu::CopyExternalImageSourceInfo {
                source: wgpu::ExternalImageSource::HTMLVideoElement(video.clone()),
                origin: wgpu::Origin2d::ZERO,
                flip_y: false,
            },
            wgpu::CopyExternalImageDestInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
                color_space: wgpu::PredefinedColorSpace::Srgb,
                premultiplied_alpha: false,
            },
            wgpu::Extent3d {
                width: video_width,
                height: video_height,
                depth_or_array_layers: 1,
            },
        );

        Ok(true)
    }
}
