//! wgpu frame-import boundary.
//!
//! This crate will own native-memory import, GPU conversion, and completion
//! lifetime.  wgpu types remain private to this crate's future implementation;
//! the migration seam consumes core session descriptions and owned native
//! frame leases.  This ticket intentionally adds no importer or player stub.
//! wgpu rendering boundary for native video frames.

mod frame_to_texture;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "windows"
))]
pub mod zero_copy;

#[allow(deprecated)]
pub use frame_to_texture::{
    cpu_frame_to_rgba, decoded_frame_to_texture, decoded_frame_to_textures,
    native_frame_lease_to_textures, upload_cpu_frame, upload_cpu_frame_as_textures,
    GpuFrameTextures, LegacyFrameIngestionError, NativeFrameIngestionError,
};
