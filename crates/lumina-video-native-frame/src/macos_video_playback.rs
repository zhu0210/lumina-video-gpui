/// Called only after both output-time probes report no new pixel buffer.
pub(crate) fn output_drained_at_end(
    current_secs: f64,
    duration_secs: f64,
    player_rate: f32,
    seek_in_flight: bool,
) -> bool {
    !seek_in_flight
        && player_rate == 0.0
        && current_secs.is_finite()
        && duration_secs.is_finite()
        && duration_secs > 0.0
        && current_secs >= duration_secs
}
