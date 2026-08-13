//! GPUI presentation composition boundary.
//!
//! GPUI owns presentation and repaint integration.  It must not decode media,
//! import native memory, or block while polling a session.  The concrete entry
//! point is added when the GStreamer and wgpu adapters migrate; this ticket
//! intentionally adds no player or constructor stub.
