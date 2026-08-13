use std::collections::VecDeque;
use std::time::Duration;

use lumina_video_core::session::{
    CapabilityTier, MediaSession, SessionCommand, SessionError, SessionEvent, SessionSnapshot,
    SessionState,
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
