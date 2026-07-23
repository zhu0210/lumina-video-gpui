//! Video and audio playback modules for lumina-video (GPUI integration layer).
//!
//! - [`GpuiVideoPlayer`] — Main video player for GPUI, using `surface()` for GPU compositing
//! - Core types re-exported from `lumina-video-core`: decoders, audio, A/V sync, etc.

// =============================================================================
// Re-export from lumina-video-core
// =============================================================================

pub use lumina_video_core::audio;
pub use lumina_video_core::subtitles;
pub use lumina_video_core::video;

#[allow(unused_imports)]
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
pub(crate) use lumina_video_core::audio_ring_buffer;

#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::frame_queue;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::network;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::player;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::sync_metrics;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::triple_buffer;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_wgpu::frame_to_texture;

// Platform-specific re-exports
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use lumina_video_core::audio_decoder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use lumina_video_core::macos_video;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use lumina_video_core::video_decoder;

#[cfg(target_os = "linux")]
pub use lumina_video_core::linux_video;
#[cfg(target_os = "linux")]
pub use lumina_video_core::linux_video_gst;

#[cfg(target_os = "android")]
pub use lumina_video_core::android_video;
#[cfg(target_os = "android")]
pub use lumina_video_core::android_vulkan;
#[cfg(all(target_os = "android", feature = "android-zero-copy"))]
pub use lumina_video_core::ndk_image_reader;

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub use lumina_video_core::windows_audio;
#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub use lumina_video_core::windows_video;

// =============================================================================
// GPUI video player (replaces egui VideoPlayer)
// =============================================================================

#[cfg(not(target_arch = "wasm32"))]
pub mod gpui_video_player;
#[cfg(not(target_arch = "wasm32"))]
pub use gpui_video_player::{GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse};

// =============================================================================
// MoQ modules
// =============================================================================

#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod moq;
#[cfg(all(
    feature = "moq",
    any(target_os = "macos", target_os = "linux", target_os = "android")
))]
pub(crate) mod moq_audio;
#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod moq_decoder;
#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod nostr_discovery;

// =============================================================================
// Web/WASM modules (browser APIs — preserved, not GPUI-dependent)
// =============================================================================

#[cfg(target_arch = "wasm32")]
pub mod web_moq_decoder;
#[cfg(target_arch = "wasm32")]
pub mod web_video;

// =============================================================================
// Type re-exports
// =============================================================================

pub use audio::{AudioConfig, AudioHandle, AudioPlayer, AudioSamples, AudioState, AudioSync};
pub use subtitles::{SubtitleCue, SubtitleError, SubtitleStyle, SubtitleTrack};
pub use video::{
    CpuFrame, DecodedFrame, GpuInfo, HwAccelType, PixelFormat, Plane, VideoDecoderBackend,
    VideoError, VideoFrame, VideoMetadata, VideoPlayerHandle, VideoState,
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use video_decoder::{FfmpegDecoder, FfmpegDecoderBuilder, HwAccelConfig};

#[cfg(target_os = "android")]
pub use android_video::{AndroidVideoDecoder, AndroidZeroCopySnapshot, ZeroCopyStatus};

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use macos_video::{MacOSVideoDecoder, MacOSZeroCopyStatsSnapshot};

#[cfg(target_os = "linux")]
pub use linux_video::{LinuxZeroCopyMetricsSnapshot, ZeroCopyGStreamerDecoder};
#[cfg(target_os = "linux")]
pub use linux_video_gst::GStreamerDecoder;

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub use windows_video::WindowsVideoDecoder;

#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub use moq::{MoqError, MoqUrl};

#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub use moq_decoder::{
    MoqAudioStatus, MoqDecoder, MoqDecoderConfig, MoqDecoderState, MoqFrameStatsSnapshot,
    MoqStatsHandle, MoqStatsSnapshot,
};

#[cfg(all(target_os = "android", feature = "moq"))]
pub use moq_decoder::MoqAndroidDecoder;

#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub use nostr_discovery::{DiscoveryEvent, MoqStream, NostrDiscovery, StreamStatus};

#[cfg(not(target_arch = "wasm32"))]
pub use sync_metrics::{SyncMetrics, SyncMetricsSnapshot, SYNC_DRIFT_THRESHOLD_MS};

#[cfg(target_arch = "wasm32")]
pub use web_video::{
    HlsBufferInfo, HlsQualityLevel, WebVideoPlayer, WebVideoPlayerResponse, WebVideoRenderCallback,
    WebVideoRenderResources, WebVideoTexture,
};

#[cfg(target_arch = "wasm32")]
pub use web_moq_decoder::{
    codec_strings, WebMoqAudioRendition, WebMoqCatalog, WebMoqDecoder, WebMoqDecoderState,
    WebMoqFrameInfo, WebMoqSession, WebMoqSessionState, WebMoqStats, WebMoqTexture, WebMoqUrl,
    WebMoqVideoRendition,
};

/// Maximum texture size wgpu can handle without panicking.
pub const MAX_SIZE_WGPU: usize = 8192;
