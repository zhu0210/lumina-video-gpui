#[path = "../src/macos_video_playback.rs"]
mod playback;

use playback::output_drained_at_end;

#[test]
fn final_frames_and_paused_tail_are_not_eof() {
    for current in [9.9, 9.933, 9.966, 9.999] {
        assert!(!output_drained_at_end(current, 10.0, 1.0, false));
        assert!(!output_drained_at_end(current, 10.0, 0.0, false));
    }
    assert!(!output_drained_at_end(10.0, 10.0, 1.0, false));
    assert!(output_drained_at_end(10.0, 10.0, 0.0, false));
}

#[test]
fn seek_from_end_does_not_relatch_eof_before_completion() {
    assert!(output_drained_at_end(10.0, 10.0, 0.0, false));
    // The old end position remains visible while the replay seek is pending.
    assert!(!output_drained_at_end(10.0, 10.0, 0.0, true));
    assert!(!output_drained_at_end(0.0, 10.0, 0.0, false));
    // A completed seek to the end can finish without waiting for another frame.
    assert!(output_drained_at_end(10.0, 10.0, 0.0, false));
}

#[test]
fn unknown_or_non_numeric_timestamps_cannot_finish_playback() {
    for duration in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(!output_drained_at_end(10.0, duration, 0.0, false));
    }
    for current in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(!output_drained_at_end(current, 10.0, 0.0, false));
    }
}
