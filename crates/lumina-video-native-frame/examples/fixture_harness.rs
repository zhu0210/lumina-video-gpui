//! Observe fixture playback through lumina-video-core's public seam.

use std::error::Error;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use lumina_video_native_frame::player::CorePlayer;
use lumina_video_native_frame::video::{DecodedFrame, VideoError, VideoState};
use url::Url;

#[cfg(target_os = "linux")]
use lumina_video_native_frame::linux_video::ZeroCopyGStreamerDecoder;
#[cfg(target_os = "linux")]
use lumina_video_native_frame::video::VideoDecoderBackend;

fn source_url(input: &str) -> Result<String, io::Error> {
    if input.contains("://") {
        return Ok(input.to_string());
    }

    let path = std::fs::canonicalize(input)?;
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path cannot be a file URL"))
}

#[cfg(target_os = "linux")]
fn open_player(source: &str) -> Result<CorePlayer, VideoError> {
    let decoder = ZeroCopyGStreamerDecoder::open(source)?;
    println!(
        "backend=ZeroCopyGStreamerDecoder native_audio={} dimensions={:?}",
        decoder.handles_audio_internally(),
        decoder.dimensions()
    );
    Ok(CorePlayer::with_decoder(source, Box::new(decoder)))
}

#[cfg(not(target_os = "linux"))]
fn open_player(source: &str) -> Result<CorePlayer, VideoError> {
    println!("backend=CorePlayer::new platform_default trait_seam=internal");
    Ok(CorePlayer::new(source))
}

fn wait_for_initialization(player: &mut CorePlayer) -> Result<(), io::Error> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !player.is_initialized() {
        if player.check_init_complete() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "media session initialization timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn frame_storage(frame: &DecodedFrame) -> &'static str {
    if frame.as_cpu().is_some() {
        "system-memory"
    } else if frame.is_gpu_surface() {
        "native-surface"
    } else {
        "unavailable"
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let Some(input) = std::env::args().nth(1) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: fixture_harness <fixture path or URL>",
        )
        .into());
    };
    let source = source_url(&input)?;
    println!("source={source}");

    let mut player = match open_player(&source) {
        Ok(player) => player,
        Err(error) => {
            println!("error={error}");
            return Err(error.into());
        }
    };
    player.init_decoder();
    wait_for_initialization(&mut player)?;

    if let VideoState::Error(error) = player.state() {
        println!("state_error={error}");
        return Err(error.clone().into());
    }

    player.sync_metadata_from_decode_thread();
    println!("state={:?}", player.state());
    println!(
        "metadata dimensions={:?} duration={:?} frame_rate={:?} buffering={}%",
        player.dimensions(),
        player.duration(),
        player.frame_rate(),
        player.buffering_percent()
    );

    player.set_volume(37);
    player.set_muted(true);
    println!(
        "controls volume={} muted={} (public CorePlayer audio handle)",
        player.audio_handle().volume(),
        player.audio_handle().is_muted()
    );
    player.set_muted(false);
    player.play();
    println!("state_after_play={:?}", player.state());

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut frames_seen = 0_u32;
    while frames_seen < 12 && Instant::now() < deadline {
        if let Some(frame) = player.poll_frame() {
            let (width, height) = frame.dimensions();
            println!(
                "frame={} pts={:?} dimensions={}x{} format={:?} frame_storage={} frame_realization=unavailable",
                frames_seen,
                frame.pts,
                width,
                height,
                frame.frame.format(),
                frame_storage(&frame.frame)
            );
            frames_seen += 1;
        } else if player.is_eos() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    player.pause();
    println!("state_after_pause={:?}", player.state());
    if player.duration().is_some() {
        player.seek(Duration::from_millis(250));
        println!("state_after_seek={:?}", player.state());
    } else {
        println!("seek=partial unavailable_without_duration");
    }
    println!("error_state={:?} frames_seen={frames_seen}", player.state());

    if frames_seen == 0 {
        return Err(io::Error::other("fixture produced no public video frames").into());
    }
    Ok(())
}
