//! Headless deterministic fixture probe for the public GStreamer session seam.

use std::error::Error;
use std::io;
use std::thread;
use std::time::{Duration, Instant};

use lumina_video_core::session::{CapabilityTier, MediaSession, SessionState};
use lumina_video_gst::{GstMediaSession, PresentationDecision};
use lumina_video_native_frame::NativeMemory;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(source) = std::env::args().nth(1) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: fixture_harness <fixture path or URL>",
        )
        .into());
    };

    let mut session = GstMediaSession::new_with_autoplay(source, true);
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut polls = 0_u32;
    let mut frames = 0_u32;
    let mut metadata_seen = false;
    let mut playing_seen = false;
    let mut ended_seen = false;
    let mut presented = false;

    while Instant::now() < deadline && !ended_seen {
        // One and only one public session poll per animation-like tick.
        polls = polls.saturating_add(1);
        match session.try_next_presentation()? {
            PresentationDecision::Advanced(frame) => {
                frames = frames.saturating_add(1);
                presented = true;
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
            }
        }
        let snapshot = session.snapshot();
        metadata_seen |= snapshot.metadata.is_some();
        playing_seen |= matches!(snapshot.state, SessionState::Playing { .. });
        ended_seen = matches!(snapshot.state, SessionState::Ended);
        if !metadata_seen || !playing_seen || !ended_seen {
            thread::sleep(Duration::from_millis(5));
        }
    }

    if !metadata_seen || !playing_seen || frames == 0 {
        return Err(io::Error::other(format!(
            "fixture session incomplete: metadata={metadata_seen} playing={playing_seen} frames={frames}"
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
        "metadata={} playing={} frames={} polls={} dropped_frames={} audio=gstreamer capability=SystemMemoryUpload",
        metadata_seen,
        playing_seen,
        frames,
        polls,
        session.dropped_frame_count(),
    );
    Ok(())
}
