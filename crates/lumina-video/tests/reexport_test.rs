//! Compile-time regression test for the lumina-video public API surface.
//!
//! Verifies that types moved to lumina-video-core remain accessible through
//! the original lumina-video paths. If this file compiles, the re-exports work.

// Core types accessible via lumina_video:: (compile-time import check)
#[allow(unused_imports)]
use lumina_video::{
    AudioConfig, AudioHandle, AudioPlayer, AudioSamples, AudioState, AudioSync, CpuFrame,
    DecodedFrame, HwAccelType, PixelFormat, Plane, VideoDecoderBackend, VideoError, VideoFrame,
    VideoMetadata, VideoState,
};

// GPUI video player types (replaces egui VideoPlayer)
#[allow(unused_imports)]
use lumina_video::{GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse};

// Native-only types (not wasm32) — compile-time import check
#[allow(unused_imports)]
use lumina_video::{SyncMetrics, SyncMetricsSnapshot, SYNC_DRIFT_THRESHOLD_MS};
