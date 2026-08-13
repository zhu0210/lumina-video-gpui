//! Owned native frame leases shared by media adapters and renderers.
//!
//! This crate owns frame resources explicitly.  CPU frames own their bytes;
//! Linux DMABuf frames own memory-object file descriptors and describe format
//! planes separately.  Acquire synchronization is also owned by the lease.
//! During the #4 migration, the legacy GStreamer, wgpu, and runtime modules
//! temporarily live here.  The final boundary will be closed by later adapter
//! tickets, so the current public interface still exposes some of those legacy
//! types; this crate must not be read as the final adapter boundary yet.
//!
//! The legacy player and platform decoder modules are intentionally hosted here
//! during the migration to dedicated native adapters.  The compatibility
//! `lumina-video` facade re-exports these paths; framework-neutral semantics
//! remain owned by `lumina-video-core`.

use std::fmt;
use std::time::Duration;

pub use lumina_video_core::video::{VideoError, VideoMetadata, VideoPlayerHandle, VideoState};

// Legacy runtime modules remain here until their dedicated adapter crates
// take ownership.  Their public paths are preserved by the compatibility
// facade, while framework-neutral semantics stay in lumina-video-core.
pub use lumina_video_core::audio;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
pub use lumina_video_core::audio_ring_buffer;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::sync_metrics;

#[cfg(not(target_arch = "wasm32"))]
pub mod frame_queue;
#[cfg(not(target_arch = "wasm32"))]
pub mod player;

pub mod video;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod audio_decoder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod macos_video;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod video_decoder;

#[cfg(target_os = "linux")]
pub mod linux_video;
#[cfg(target_os = "linux")]
pub mod linux_video_gst;

#[cfg(target_os = "android")]
pub mod android_video;
#[cfg(target_os = "android")]
pub mod android_vulkan;
#[cfg(all(target_os = "android", feature = "android-zero-copy"))]
pub mod ndk_image_reader;

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub mod windows_audio;
#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub mod windows_video;

#[cfg(all(target_os = "linux", feature = "vendored-runtime"))]
pub mod vendored_runtime;

// Native desktop MoQ transport, discovery, and decoder implementation.
#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod moq;
#[cfg(all(
    not(target_arch = "wasm32"),
    feature = "moq",
    any(target_os = "macos", target_os = "linux", target_os = "android")
))]
pub(crate) mod moq_audio;
#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod moq_decoder;
#[cfg(all(not(target_arch = "wasm32"), feature = "moq"))]
pub mod nostr_discovery;

/// A media timestamp in the session master-clock time base.
pub type MediaTime = Duration;

/// Width and height of a native frame in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameExtent {
    pub width: u32,
    pub height: u32,
}

impl FrameExtent {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// One owned CPU format plane.
///
/// [`CpuPlane::new`] takes ownership of the supplied bytes without allocating
/// or copying them.  The producer is responsible for allocation and pooling;
/// this type only carries that ownership through the frame lease.
#[derive(Debug, PartialEq, Eq)]
pub struct CpuPlane {
    /// Bytes owned by this plane until the containing lease is dropped.
    pub bytes: Vec<u8>,
    /// Bytes between the starts of adjacent rows.
    pub stride: usize,
}

impl CpuPlane {
    /// Takes ownership of `bytes` without allocating or copying it.
    pub fn new(bytes: Vec<u8>, stride: usize) -> Self {
        Self { bytes, stride }
    }
}

/// Owned system-memory planes for one decoded frame.
///
/// [`CpuMemory::new`] takes ownership of the supplied plane vector without
/// allocating or copying it.  Allocation and pooling remain producer
/// responsibilities, so constructing this wrapper does not imply a per-frame
/// copy.
#[derive(Debug, PartialEq, Eq)]
pub struct CpuMemory {
    pub planes: Vec<CpuPlane>,
}

impl CpuMemory {
    /// Takes ownership of `planes` without allocating or copying it.
    pub fn new(planes: Vec<CpuPlane>) -> Self {
        Self { planes }
    }
}

/// Errors from constructing an owned frame lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeFrameError {
    InvalidExtent,
    PlaneCount { expected: usize, actual: usize },
    PlaneObject { plane: usize, object: usize },
}

impl fmt::Display for NativeFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExtent => write!(f, "frame extent must be non-zero"),
            Self::PlaneCount { expected, actual } => {
                write!(f, "frame format requires {expected} planes, got {actual}")
            }
            Self::PlaneObject { plane, object } => {
                write!(f, "plane {plane} references missing memory object {object}")
            }
        }
    }
}

impl std::error::Error for NativeFrameError {}

/// One Linux DMABuf memory object.  The descriptor is owned by this value.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmaBufObject {
    pub fd: std::os::fd::OwnedFd,
    pub size: u64,
}

/// A format-plane view into one of a DMABuf frame's memory objects.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufPlane {
    /// Index into [`DmaBufMemory::objects`], not a file descriptor count.
    pub object: usize,
    pub offset: u64,
    pub stride: u32,
    pub size: u64,
}

/// Linux DMABuf memory with memory objects kept separate from format planes.
///
/// [`DmaBufMemory::new`] takes ownership of the supplied memory objects and
/// plane descriptors without allocating or copying them.  It only validates
/// plane-to-object references; allocation and pooling are producer
/// responsibilities.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmaBufMemory {
    pub objects: Vec<DmaBufObject>,
    pub planes: Vec<DmaBufPlane>,
    /// DRM fourcc describing the image format at the import seam.
    pub drm_fourcc: u32,
    /// DRM modifier shared by the image's memory layout.
    pub modifier: u64,
}

#[cfg(target_os = "linux")]
impl DmaBufMemory {
    /// Takes ownership of DMABuf objects and plane descriptors and validates
    /// their references without allocating or copying either vector.
    pub fn new(
        objects: Vec<DmaBufObject>,
        planes: Vec<DmaBufPlane>,
        drm_fourcc: u32,
        modifier: u64,
    ) -> Result<Self, NativeFrameError> {
        for (plane_index, plane) in planes.iter().enumerate() {
            if plane.object >= objects.len() {
                return Err(NativeFrameError::PlaneObject {
                    plane: plane_index,
                    object: plane.object,
                });
            }
        }
        Ok(Self {
            objects,
            planes,
            drm_fourcc,
            modifier,
        })
    }
}

/// Producer-to-consumer acquire synchronization owned by a frame lease.
#[derive(Debug)]
pub enum AcquireSync {
    /// The producer has completed its writes, or no synchronization is needed.
    None,
    /// Linux sync-file descriptor consumed by the importing adapter.
    #[cfg(target_os = "linux")]
    SyncFile(std::os::fd::OwnedFd),
}

/// Owned memory backing a decoded frame.
#[derive(Debug)]
pub enum NativeMemory {
    Cpu(CpuMemory),
    #[cfg(target_os = "linux")]
    DmaBuf(DmaBufMemory),
}

/// Framework-neutral identity, timing, extent, and pixel format for one frame.
/// This is not a complete color description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrameDescriptor {
    pub frame_id: u64,
    pub stream_generation: u64,
    pub pts: MediaTime,
    pub duration: Option<MediaTime>,
    pub extent: FrameExtent,
    pub format: PixelFormat,
}

/// A decoded frame and every producer resource required to keep it valid.
#[derive(Debug)]
pub struct NativeFrameLease {
    /// Identity and timing remain grouped with the frame descriptor.
    pub descriptor: NativeFrameDescriptor,
    pub memory: NativeMemory,
    pub acquire: AcquireSync,
}

impl NativeFrameLease {
    pub fn new(
        descriptor: NativeFrameDescriptor,
        memory: NativeMemory,
        acquire: AcquireSync,
    ) -> Result<Self, NativeFrameError> {
        if descriptor.extent.width == 0 || descriptor.extent.height == 0 {
            return Err(NativeFrameError::InvalidExtent);
        }
        let plane_count = match &memory {
            NativeMemory::Cpu(cpu) => cpu.planes.len(),
            #[cfg(target_os = "linux")]
            NativeMemory::DmaBuf(dmabuf) => dmabuf.planes.len(),
        };
        let expected = descriptor.format.num_planes();
        if plane_count != expected {
            return Err(NativeFrameError::PlaneCount {
                expected,
                actual: plane_count,
            });
        }
        Ok(Self {
            descriptor,
            memory,
            acquire,
        })
    }
}

pub use video::{
    CpuFrame, DecodedFrame, HwAccelType, PixelFormat, Plane, VideoDecoderBackend, VideoFrame,
};

#[cfg(not(target_arch = "wasm32"))]
pub use player::CorePlayer;
