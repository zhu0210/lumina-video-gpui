//! GPUI presentation boundary for lumina-video.

#[cfg(not(target_arch = "wasm32"))]
pub mod gpui_video_player;

#[cfg(not(target_arch = "wasm32"))]
pub use gpui_video_player::{GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse};

// Public metadata and error types used by the player audio-track API.
pub use lumina_video_core::session::{AudioTrack, SessionError};
