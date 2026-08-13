use std::collections::VecDeque;
use std::time::Duration;

use lumina_video_core::session::{
    AudioTrack, CapabilityTier, MediaSession, SessionCommand, SessionError, SessionEvent,
    SessionSnapshot, SessionState,
};

struct FakeSession {
    snapshot: SessionSnapshot,
    events: VecDeque<SessionEvent<u8>>,
}

impl MediaSession for FakeSession {
    type Frame = u8;

    fn snapshot(&self) -> SessionSnapshot {
        self.snapshot.clone()
    }

    fn command(&mut self, command: SessionCommand) -> Result<(), SessionError> {
        if matches!(command, SessionCommand::Play) {
            self.snapshot.state = SessionState::Playing {
                position: Duration::ZERO,
            };
        }
        Ok(())
    }

    fn try_next_event(&mut self) -> Result<Option<SessionEvent<Self::Frame>>, SessionError> {
        Ok(self.events.pop_front())
    }
}

#[test]
fn session_adapter_seam_exposes_snapshot_commands_and_nonblocking_events() {
    let mut session = FakeSession {
        snapshot: SessionSnapshot::new(CapabilityTier::SystemMemoryUpload),
        events: VecDeque::from([
            SessionEvent::Frame {
                pts: Duration::from_millis(20),
                frame: 7,
            },
            SessionEvent::Ended,
        ]),
    };

    assert_eq!(
        session.snapshot().capability,
        CapabilityTier::SystemMemoryUpload
    );
    let first = session.try_next_event().ok().flatten();
    assert!(matches!(first, Some(SessionEvent::Frame { .. })));
    assert!(matches!(
        session.try_next_event().ok().flatten(),
        Some(SessionEvent::Ended)
    ));
    assert!(session.try_next_event().ok().flatten().is_none());
    assert!(session.command(SessionCommand::Play).is_ok());
    assert!(matches!(
        session.snapshot().state,
        SessionState::Playing { .. }
    ));
}

#[test]
fn audio_track_contract_keeps_stable_id_and_nonterminal_failure_fields() {
    let track = AudioTrack {
        id: "audio-raw-id".into(),
        language: Some("eng".into()),
        title: Some("English".into()),
        codec: "AAC".into(),
    };
    let event = SessionEvent::<u8>::AudioTracks {
        tracks: vec![track.clone()],
        selected_id: Some(track.id.clone()),
    };
    assert!(matches!(
        event,
        SessionEvent::AudioTracks { tracks, selected_id }
            if tracks.first() == Some(&track)
                && selected_id.as_deref() == Some("audio-raw-id")
    ));

    let failure = SessionEvent::<u8>::AudioTrackSelectionFailed {
        requested_id: "missing".into(),
        prior_restored_id: Some(track.id),
        reason: "selection rejected".into(),
    };
    assert!(matches!(
        failure,
        SessionEvent::AudioTrackSelectionFailed {
            requested_id,
            prior_restored_id: Some(_),
            reason,
        } if requested_id == "missing" && reason == "selection rejected"
    ));
}
