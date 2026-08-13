//! Compile-time contract for the GPUI crate's public player entry point.

use gpui::App;
#[allow(unused_imports)]
use lumina_video_gpui::{GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse};

#[test]
fn gpui_entry_point_is_constructible() {
    let _: fn(String, &App) -> GpuiVideoPlayer = GpuiVideoPlayer::new;
}
