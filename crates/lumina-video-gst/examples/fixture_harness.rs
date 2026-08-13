//! Headless deterministic fixture probe for the public GStreamer session seam.

use std::error::Error;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use lumina_video_core::session::{
    CapabilityTier, MediaSession, SessionCommand, SessionEvent, SessionState,
};
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

    let source_kind = source.clone();
    let mut session = GstMediaSession::new_with_autoplay_and_audio_sink_and_timeout_and_generation(
        source,
        true,
        GstAudioSinkMode::Fake,
        Duration::from_secs(2),
        0,
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut polls = 0_u32;
    let mut frames = 0_u32;
    let mut metadata_seen = false;
    let mut playing_seen = false;
    let mut buffering_seen = false;
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
    let seek_target = Duration::from_millis(750);
    let mut seek_pts = None;
    let mut resumed_seen = false;
    let mut replay_requested = false;
    let mut replayed_seen = false;
    let mut frames_before_replay = 0_u32;
    let mut duration_seen = false;
    let mut expected_duration_seen = false;
    let mut position_seen = false;
    let mut post_seek_position_seen = false;
    let mut audio_tracks_seen = false;
    let mut audio_selection_requested = None;
    let mut audio_selection_seen = false;
    let mut missing_selection_requested = false;
    let mut missing_selection_failed = false;
    let mut missing_selection_prior_id = None;
    let mut opus_track_seen = false;

    while Instant::now() < deadline && !replayed_seen {
        // One and only one public session poll per animation-like tick.
        polls = polls.saturating_add(1);
        let next_event = session.try_next_event()?;
        buffering_seen |= matches!(
            next_event.as_ref(),
            Some(SessionEvent::StateChanged {
                state: SessionState::Buffering { .. }
            })
        );
        if let Some(SessionEvent::AudioTrackSelectionFailed {
            requested_id,
            prior_restored_id,
            reason,
        }) = next_event.as_ref()
        {
            if requested_id == "missing-audio-stream" {
                missing_selection_failed = true;
                missing_selection_prior_id = prior_restored_id.clone();
                if !reason.contains("prior audio selection unchanged") {
                    return Err(io::Error::other(
                        "missing audio selection did not report unchanged prior selection",
                    )
                    .into());
                }
            }
        }
        match PresentationDecision::from_event(next_event, presented) {
            PresentationDecision::Advanced(frame) => {
                frames = frames.saturating_add(1);
                presented = true;
                if !pause_requested {
                    session.command(SessionCommand::Pause)?;
                    pause_requested = true;
                }
                if let Some(generation) = seek_generation {
                    if frame.descriptor.stream_generation >= generation {
                        let delta = if frame.descriptor.pts >= seek_target {
                            frame.descriptor.pts - seek_target
                        } else {
                            seek_target - frame.descriptor.pts
                        };
                        if delta <= Duration::from_millis(250) {
                            seek_seen = true;
                            seek_pts = Some(frame.descriptor.pts);
                        }
                    }
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
        buffering_seen |= matches!(&snapshot.state, SessionState::Buffering { .. });
        audio_tracks_seen |= !snapshot.audio_tracks.is_empty();
        opus_track_seen |= snapshot
            .audio_tracks
            .iter()
            .any(|track| track.codec.to_ascii_lowercase().contains("opus"));
        if source_kind.contains("dual-aac")
            && audio_selection_requested.is_none()
            && snapshot.audio_tracks.len() >= 2
        {
            if let Some(alternate) = snapshot.audio_tracks.get(1) {
                let alternate_id = alternate.id.clone();
                session.command(SessionCommand::SelectAudioTrack {
                    id: alternate_id.clone(),
                })?;
                audio_selection_requested = Some(alternate_id);
            }
        }
        if let Some(requested_id) = audio_selection_requested.as_deref() {
            audio_selection_seen |=
                Some(requested_id) == snapshot.selected_audio_track_id.as_deref();
        }
        if source_kind.contains("dual-aac") && audio_selection_seen && !missing_selection_requested
        {
            session.command(SessionCommand::SelectAudioTrack {
                id: "missing-audio-stream".into(),
            })?;
            missing_selection_requested = true;
        }
        duration_seen |= snapshot
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
            .is_some();
        expected_duration_seen |= snapshot
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
            .is_some_and(|duration| {
                (Duration::from_millis(1_800)..=Duration::from_millis(2_200)).contains(&duration)
            });
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
                position: seek_target,
            })?;
            seek_generation = Some(session.stream_generation());
            session.command(SessionCommand::Play)?;
            controls_requested = true;
        }
        if controls_requested {
            resumed_seen |= matches!(&snapshot.state, SessionState::Playing { .. });
            post_seek_position_seen |= matches!(
                &snapshot.state,
                SessionState::Playing { position }
                    if *position >= seek_target
                        && *position <= seek_target + Duration::from_millis(250)
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
        || !expected_duration_seen
        || !position_seen
        || !post_seek_position_seen
        || !audio_tracks_seen
        || (source_kind.contains("vp9-opus") && !opus_track_seen)
        || (source_kind.contains("dual-aac")
            && (!audio_selection_requested.is_some()
                || !audio_selection_seen
                || !missing_selection_requested
                || !missing_selection_failed
                || missing_selection_prior_id != audio_selection_requested))
    {
        return Err(io::Error::other(format!(
            "fixture session incomplete: metadata={metadata_seen} audio_tracks={audio_tracks_seen} opus={opus_track_seen} audio_selection_requested={audio_selection_requested:?} audio_selection_seen={audio_selection_seen} missing_selection_requested={missing_selection_requested} missing_selection_failed={missing_selection_failed} missing_selection_prior_id={missing_selection_prior_id:?} playing={playing_seen} buffering={buffering_seen} frames={frames} ended={ended_seen} replayed={replayed_seen} paused={paused_seen} resumed={resumed_seen} seek={seek_seen} seek_pts={seek_pts:?} duration={duration_seen} expected_duration={expected_duration_seen} position={position_seen} post_seek_position={post_seek_position_seen}"
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

    let audio_switch_evidence = if source_kind.contains("dual-aac") {
        if audio_selection_seen {
            "confirmed"
        } else {
            "missing"
        }
    } else {
        "not-applicable"
    };
    let invalid_id_evidence = if source_kind.contains("dual-aac") {
        if missing_selection_failed {
            "nonterminal-preflight-failure"
        } else {
            "missing"
        }
    } else {
        "not-applicable"
    };

    println!(
        "metadata={} playing={} buffering={} frames={} polls={} ended={} replayed={} paused={} resumed={} seek={} seek_pts={:?} duration={} expected_duration={} position={} post_seek_position={} muted=true volume=25 dropped_frames={} audio=gstreamer connected={} buffers_seen={} capability=SystemMemoryUpload audio_switch={} invalid_id={} invalid_id_prior={:?}",
        metadata_seen,
        playing_seen,
        buffering_seen,
        frames,
        polls,
        ended_seen,
        replayed_seen,
        paused_seen,
        resumed_seen,
        seek_seen,
        seek_pts,
        duration_seen,
        expected_duration_seen,
        position_seen,
        post_seek_position_seen,
        session.dropped_frame_count(),
        audio_connected_seen,
        audio_buffers_seen,
        audio_switch_evidence,
        invalid_id_evidence,
        missing_selection_prior_id,
    );
    Ok(())
}
