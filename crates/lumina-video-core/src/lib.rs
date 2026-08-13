//! lumina-video-core: Framework-neutral video and media-session semantics.
//!
//! This crate provides framework-neutral contracts consumed by native playback
//! adapters. It contains:
//!
//! - Core semantics: [`video`], [`audio`], [`session`], [`subtitles`]
//! - Shared timing primitives: [`triple_buffer`], [`sync_metrics`]
//! - Network utilities: [`network`]
//!
//! This crate has no UI framework dependency. It is consumed by:
//! - `lumina-video-gpui` (GPUI integration layer)
//! - `lumina-video-ios` (C FFI for iOS/Swift)

// === Universal modules (compile on all targets including wasm32) ===

pub mod audio;
pub mod session;
pub mod subtitles;
pub mod video;

/// Internal bridge API — public only for cross-crate use by native adapters.
/// NOT semver-stable. Do not depend on this module directly from external crates.
/// May change or be removed in any minor version.
#[doc(hidden)]
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
pub mod audio_ring_buffer;

// === Native-only semantic utilities (not available on wasm32) ===

#[cfg(not(target_arch = "wasm32"))]
pub mod network;
#[cfg(not(target_arch = "wasm32"))]
pub mod sync_metrics;
#[cfg(not(target_arch = "wasm32"))]
pub mod triple_buffer;
