//! Owned native frame leases shared by media adapters and renderers.
//!
//! This crate owns frame resources explicitly.  CPU frames own their bytes;
//! Linux DMABuf frames own memory-object file descriptors and describe format
//! planes separately.  Acquire synchronization is also owned by the lease.
//! No GStreamer, wgpu, or UI type is part of this interface.

use std::fmt;
use std::time::Duration;

use lumina_video_core::video::PixelFormat;

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
#[derive(Debug, PartialEq, Eq)]
pub struct CpuPlane {
    /// Bytes owned by this plane until the containing lease is dropped.
    pub bytes: Vec<u8>,
    /// Bytes between the starts of adjacent rows.
    pub stride: usize,
}

impl CpuPlane {
    pub fn new(bytes: Vec<u8>, stride: usize) -> Self {
        Self { bytes, stride }
    }
}

/// Owned system-memory planes for one decoded frame.
#[derive(Debug, PartialEq, Eq)]
pub struct CpuMemory {
    pub planes: Vec<CpuPlane>,
}

impl CpuMemory {
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

/// A decoded frame and every producer resource required to keep it valid.
#[derive(Debug)]
pub struct NativeFrameLease {
    pub frame_id: u64,
    pub stream_generation: u64,
    pub pts: MediaTime,
    pub duration: Option<MediaTime>,
    pub extent: FrameExtent,
    pub format: PixelFormat,
    pub memory: NativeMemory,
    pub acquire: AcquireSync,
}

impl NativeFrameLease {
    pub fn new(
        frame_id: u64,
        stream_generation: u64,
        pts: MediaTime,
        duration: Option<MediaTime>,
        extent: FrameExtent,
        format: PixelFormat,
        memory: NativeMemory,
        acquire: AcquireSync,
    ) -> Result<Self, NativeFrameError> {
        if extent.width == 0 || extent.height == 0 {
            return Err(NativeFrameError::InvalidExtent);
        }
        let plane_count = match &memory {
            NativeMemory::Cpu(cpu) => cpu.planes.len(),
            #[cfg(target_os = "linux")]
            NativeMemory::DmaBuf(dmabuf) => dmabuf.planes.len(),
        };
        let expected = format.num_planes();
        if plane_count != expected {
            return Err(NativeFrameError::PlaneCount {
                expected,
                actual: plane_count,
            });
        }
        Ok(Self {
            frame_id,
            stream_generation,
            pts,
            duration,
            extent,
            format,
            memory,
            acquire,
        })
    }
}
