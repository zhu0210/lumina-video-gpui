//! Compile-time contract for the GPUI crate's public player entry point.

#[allow(unused_imports)]
use lumina_video_gpui::{GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse};

#[test]
fn gpui_entry_point_is_constructible() {
    let _: fn(String) -> GpuiVideoPlayer = GpuiVideoPlayer::new;
}
