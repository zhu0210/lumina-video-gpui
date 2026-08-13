//! lumina-video: Cross-platform video playback for GPUI with hardware acceleration.
//!
//! This crate provides hardware-accelerated video playback for GPUI applications
//! using **native platform media frameworks** — no FFmpeg required by default.
//!
//! # Native Platform Support
//!
//! Each platform uses its native media stack for optimal performance:
//!
//! | Platform | Native Framework | Hardware Acceleration |
//! |----------|------------------|----------------------|
//! | macOS | AVFoundation + VideoToolbox | Apple Silicon / Intel QuickSync |
//! | Linux | GStreamer | VA-API, NVDEC (varies by GPU/drivers) |
//! | Windows | Media Foundation | DXVA2, D3D11VA |
//! | Android | MediaCodec | Device hardware codecs |
//!
//! # Example
//!
//! ```ignore
//! use lumina_video::GpuiVideoPlayer;
//!
//! // Store the player in your app state:
//! let mut player = GpuiVideoPlayer::new("https://example.com/video.mp4")
//!     .with_autoplay(true);
//!
//! // In your Render implementation, update each frame:
//! fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
//!     self.player.update(window, cx);
//!     div().size_full().child(self.player.surface_element())
//! }
//! ```

pub mod media;

// Re-export core types for convenience
pub use media::{
    // GPUI video player (replaces the egui VideoPlayer)
    GpuiVideoPlayer,
    GpuiVideoPlayerConfig,
    GpuiVideoPlayerResponse,
};

// Re-export video core types
pub use lumina_video_core::audio::{
    AudioConfig, AudioHandle, AudioPlayer, AudioSamples, AudioState, AudioSync,
};
pub use lumina_video_native_frame::{
    CpuFrame, DecodedFrame, HwAccelType, PixelFormat, Plane, VideoDecoderBackend, VideoError,
    VideoFrame, VideoMetadata, VideoState,
};

// Native-only exports
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::sync_metrics::{
    SyncMetrics, SyncMetricsSnapshot, SYNC_DRIFT_THRESHOLD_MS,
};
pub use lumina_video_native_frame::frame_to_texture::{self, GpuFrameTextures};

// macOS/iOS FFmpeg decoder (when available)
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use lumina_video_native_frame::video_decoder::{
    FfmpegDecoder, FfmpegDecoderBuilder, HwAccelConfig,
};

// Android decoder
#[cfg(target_os = "android")]
pub use lumina_video_native_frame::android_video::{
    android_zero_copy_snapshot, AndroidZeroCopySnapshot, ZeroCopyStatus,
};
#[cfg(target_os = "android")]
pub use media::AndroidVideoDecoder;

// Web/WASM video player
#[cfg(target_arch = "wasm32")]
pub use media::{HlsBufferInfo, HlsQualityLevel, WebVideoPlayer};
