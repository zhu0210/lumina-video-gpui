//! Headless deterministic fixture probe for the public GStreamer session seam.

use std::error::Error;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use lumina_video_core::session::{CapabilityTier, MediaSession, SessionCommand, SessionState};
use lumina_video_gst::{GstAudioSinkMode, GstMediaSession, PresentationDecision};
use lumina_video_native_frame::NativeMemory;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(source) = std::env::args().nth(1) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: fixture_harness <fixture path or URL>",
        )
        .into());
    };

    let mut session =
        GstMediaSession::new_with_autoplay_and_audio_sink(source, true, GstAudioSinkMode::Fake);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut polls = 0_u32;
    let mut frames = 0_u32;
    let mut metadata_seen = false;
    let mut playing_seen = false;
    let mut ended_seen = false;
    let mut presented = false;
    let mut empty_before_frame_seen = false;
    let mut audio_connected_seen = false;
    let mut audio_buffers_seen = 0_u64;
    let mut pause_requested = false;
    let mut paused_seen = false;
    let mut controls_requested = false;
    let mut seek_seen = false;
    let mut seek_generation = None;
    let mut resumed_seen = false;
    let mut replay_requested = false;
    let mut replayed_seen = false;
    let mut frames_before_replay = 0_u32;
    let mut duration_seen = false;
    let mut position_seen = false;

    while Instant::now() < deadline && !replayed_seen {
        // One and only one public session poll per animation-like tick.
        polls = polls.saturating_add(1);
        match session.try_next_presentation()? {
            PresentationDecision::Advanced(frame) => {
                frames = frames.saturating_add(1);
                presented = true;
                if !pause_requested {
                    session.command(SessionCommand::Pause)?;
                    pause_requested = true;
                }
                if let Some(generation) = seek_generation {
                    seek_seen |= frame.descriptor.stream_generation >= generation;
                }
                if !matches!(frame.memory, NativeMemory::Cpu(_)) {
                    return Err(io::Error::other("fixture frame crossed as non-CPU memory").into());
                }
            }
            PresentationDecision::Hold => {
                if !presented {
                    return Err(io::Error::other("presentation held before first frame").into());
                }
            }
            PresentationDecision::Empty => {
                if presented {
                    return Err(io::Error::other("presentation became empty after a frame").into());
                }
                empty_before_frame_seen = true;
            }
        }
        let snapshot = session.snapshot();
        if let SessionState::Error(error) = &snapshot.state {
            return Err(io::Error::other(format!("fixture session error: {error}")).into());
        }
        metadata_seen |= snapshot.metadata.is_some();
        duration_seen |= snapshot
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
            .is_some();
        playing_seen |= matches!(&snapshot.state, SessionState::Playing { .. });
        position_seen |= matches!(
            &snapshot.state,
            SessionState::Playing { .. }
                | SessionState::Paused { .. }
                | SessionState::Buffering { .. }
        );
        if matches!(&snapshot.state, SessionState::Paused { .. }) {
            paused_seen = true;
        }
        if paused_seen && !controls_requested {
            session.command(SessionCommand::SetMuted { muted: true })?;
            session.command(SessionCommand::SetVolume { volume: 0.25 })?;
            session.command(SessionCommand::Seek {
                position: Duration::ZERO,
            })?;
            seek_generation = Some(session.stream_generation());
            session.command(SessionCommand::Play)?;
            controls_requested = true;
        }
        if controls_requested {
            resumed_seen |= matches!(&snapshot.state, SessionState::Playing { .. });
            seek_seen |= matches!(
                &snapshot.state,
                SessionState::Playing { position } if position.is_zero()
            );
        }
        if matches!(&snapshot.state, SessionState::Ended) {
            ended_seen = true;
            if resumed_seen && !replay_requested {
                frames_before_replay = frames;
                session.command(SessionCommand::Play)?;
                replay_requested = true;
            }
        }
        if replay_requested && frames > frames_before_replay {
            replayed_seen = true;
        }
        let audio = session.audio_observation();
        audio_connected_seen |= audio.connected;
        audio_buffers_seen = audio_buffers_seen.max(audio.buffers_seen);
        if !metadata_seen || !playing_seen || !ended_seen || !replayed_seen {
            thread::sleep(Duration::from_millis(5));
        }
    }

    if !metadata_seen
        || !playing_seen
        || frames == 0
        || !ended_seen
        || !replayed_seen
        || !paused_seen
        || !resumed_seen
        || !seek_seen
        || !duration_seen
        || !position_seen
    {
        return Err(io::Error::other(format!(
            "fixture session incomplete: metadata={metadata_seen} playing={playing_seen} frames={frames} ended={ended_seen} replayed={replayed_seen} paused={paused_seen} resumed={resumed_seen} seek={seek_seen} duration={duration_seen} position={position_seen}"
        ))
        .into());
    }
    if !session.audio_handle().is_muted() || session.audio_handle().volume() != 25 {
        return Err(io::Error::other(format!(
            "fixture audio controls incomplete: muted={} volume={}",
            session.audio_handle().is_muted(),
            session.audio_handle().volume(),
        ))
        .into());
    }
    if !empty_before_frame_seen {
        return Err(io::Error::other("presentation never reported initial Empty").into());
    }
    if !audio_connected_seen || audio_buffers_seen == 0 {
        return Err(io::Error::other(format!(
            "fixture audio branch incomplete: connected={audio_connected_seen} buffers_seen={audio_buffers_seen}"
        ))
        .into());
    }
    if session.snapshot().capability != CapabilityTier::SystemMemoryUpload {
        return Err(io::Error::other("fixture capability was not system-memory upload").into());
    }
    if !GstMediaSession::handles_audio_internally() {
        return Err(io::Error::other("GStreamer audio path is not enabled").into());
    }

    println!(
        "metadata={} playing={} frames={} polls={} ended={} replayed={} paused={} resumed={} seek={} duration={} position={} muted=true volume=25 dropped_frames={} audio=gstreamer connected={} buffers_seen={} capability=SystemMemoryUpload",
        metadata_seen,
        playing_seen,
        frames,
        polls,
        ended_seen,
        replayed_seen,
        paused_seen,
        resumed_seen,
        seek_seen,
        duration_seen,
        position_seen,
        session.dropped_frame_count(),
        audio_connected_seen,
        audio_buffers_seen,
    );
    Ok(())
}
