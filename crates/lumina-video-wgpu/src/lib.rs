//! wgpu frame-import boundary.
//!
//! The public external-memory seam accepts an owned
//! [`lumina_video_native_frame::NativeFrameLease`]. Its CPU path uploads owned
//! RGBA or NV12 bytes without cloning them; unsupported acquire fences, DMABuf
//! memory, and other CPU formats return the lease unchanged.
//!
//! Borrowed [`lumina_video_native_frame::video::DecodedFrame`] values remain a
//! compatibility path for CPU frames. Borrowed native GPU surfaces return a
//! typed unsupported error until #7 connects producers to the owned seam.
//! Platform-specific import backends stay in the private `zero_copy` module;
//! they are maintained internally and are not part of this crate's public API.

mod frame_to_texture;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "windows"
))]
#[expect(
    dead_code,
    reason = "raw backends remain private until owned lease adapters land in #15/#16"
)]
mod zero_copy;

#[allow(deprecated)]
pub use frame_to_texture::{
    cpu_frame_to_rgba, decoded_frame_to_texture, decoded_frame_to_textures,
    native_frame_lease_to_textures, upload_cpu_frame, upload_cpu_frame_as_textures,
    GpuFrameTextures, LegacyFrameIngestionError, NativeFrameIngestionError,
};
