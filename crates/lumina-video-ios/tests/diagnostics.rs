//! Global diagnostics run in their own process so parallel FFI tests cannot change the counters.
#![cfg(debug_assertions)]
use lumina_video_ios::handle::LuminaPlayer;
use lumina_video_ios::{
    lumina_diagnostics_snapshot, lumina_player_create, lumina_player_destroy, LuminaDiagnostics,
};
use std::ptr;
const LUMINA_OK: i32 = lumina_video_ios::error::LuminaError::Ok as i32;

#[test]
fn diagnostics_lifecycle_and_leak_counts() {
    diagnostics_tracks_player_lifecycle();
    diagnostics_no_leak_after_100_cycles();
}

fn diagnostics_tracks_player_lifecycle() {
    unsafe {
        let mut before = std::mem::zeroed::<LuminaDiagnostics>();
        lumina_diagnostics_snapshot(&mut before);

        let url = c"https://example.com/test.mp4".as_ptr();
        let mut player: *mut LuminaPlayer = ptr::null_mut();
        let err = lumina_player_create(url, &mut player);
        assert_eq!(err, LUMINA_OK);

        let mut during = std::mem::zeroed::<LuminaDiagnostics>();
        lumina_diagnostics_snapshot(&mut during);
        assert!(during.players_created > before.players_created);
        assert!(during.players_live > before.players_live);

        lumina_player_destroy(&mut player);

        let mut after = std::mem::zeroed::<LuminaDiagnostics>();
        lumina_diagnostics_snapshot(&mut after);
        assert!(after.players_destroyed > before.players_destroyed);
    }
}

fn diagnostics_no_leak_after_100_cycles() {
    unsafe {
        let mut before = std::mem::zeroed::<LuminaDiagnostics>();
        lumina_diagnostics_snapshot(&mut before);
        let initial_live = before.players_live;

        let url = c"https://example.com/test.mp4".as_ptr();
        for _ in 0..100 {
            let mut player: *mut LuminaPlayer = ptr::null_mut();
            let err = lumina_player_create(url, &mut player);
            assert_eq!(err, LUMINA_OK);
            let err = lumina_player_destroy(&mut player);
            assert_eq!(err, LUMINA_OK);
        }

        let mut after = std::mem::zeroed::<LuminaDiagnostics>();
        lumina_diagnostics_snapshot(&mut after);
        // No leaked players
        assert_eq!(after.players_live, initial_live);
    }
}
