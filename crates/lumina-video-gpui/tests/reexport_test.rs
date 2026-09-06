//! Compile-time contract for the GPUI crate's public player entry point.

use gpui::App;
#[allow(unused_imports)]
use lumina_video_gpui::{
    AudioTrack, GpuiVideoPlayer, GpuiVideoPlayerConfig, GpuiVideoPlayerResponse, SessionError,
};

#[test]
fn gpui_entry_point_is_constructible() {
    let _: fn(String, &App) -> GpuiVideoPlayer = GpuiVideoPlayer::new;
}

#[test]
fn audio_track_api_exposes_confirmation_and_errors() {
    let _: fn(&GpuiVideoPlayer) -> &[AudioTrack] = GpuiVideoPlayer::audio_tracks;
    let _: fn(&GpuiVideoPlayer) -> Option<&str> = GpuiVideoPlayer::selected_audio_track_id;
    let _: fn(&GpuiVideoPlayer) -> Option<&str> = GpuiVideoPlayer::audio_track_selection_error;
    let _: fn(&mut GpuiVideoPlayer, String) -> Result<(), SessionError> =
        GpuiVideoPlayer::select_audio_track;
}
