//! GStreamer media-session adapter boundary.
//!
//! This crate owns GStreamer pipeline implementation in the migration that
//! follows this interface ticket.  Its public seam will expose only
//! `lumina_video_core::session` values and
//! `lumina_video_native_frame::NativeFrameLease`; GStreamer objects must not
//! cross the crate interface.  No player constructor is promised here yet.
