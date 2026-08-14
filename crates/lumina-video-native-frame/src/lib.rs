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
//! The native player and platform decoder modules are hosted here while
//! dedicated adapters consume the shared frame contracts. Framework-neutral
//! semantics remain owned by `lumina-video-core`.

use std::fmt;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use crossbeam_channel::{Sender, TrySendError};

pub use lumina_video_core::video::{VideoError, VideoMetadata, VideoPlayerHandle, VideoState};

// Shared runtime modules remain here until dedicated adapter crates take
// ownership, while framework-neutral semantics stay in lumina-video-core.
pub use lumina_video_core::audio;
#[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
pub use lumina_video_core::audio_ring_buffer;
#[cfg(not(target_arch = "wasm32"))]
pub use lumina_video_core::sync_metrics;

#[cfg(not(target_arch = "wasm32"))]
pub mod frame_queue;
#[cfg(not(target_arch = "wasm32"))]
pub mod player;

pub mod color;
pub mod video;
pub use color::{
    apply_yuv_matrix, nv12_bytes_to_rgba_into, nv12_to_rgba_into, render_decision,
    yuv420p_bytes_to_rgba_into, yuv_to_rgb_matrix, ChromaHorizontal, ChromaVertical,
    ColorConvertError, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange, ColorRenderDecision,
    ColorTransfer,
};

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod audio_decoder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod macos_video;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod video_decoder;

#[cfg(target_os = "linux")]
pub mod linux_sync;
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
/// or copying them. The producer is responsible for allocation and pooling;
/// this type only carries that ownership through the frame lease.
#[repr(C)]
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

/// Transfers decoder planes into the owned lease representation without a
/// second plane-vector allocation. This narrow bridge is public only because
/// the GStreamer adapter lives in a separate crate.
#[doc(hidden)]
pub fn into_cpu_planes(planes: Vec<video::Plane>) -> Vec<CpuPlane> {
    let planes = ManuallyDrop::new(planes);
    let pointer = planes.as_ptr() as *mut CpuPlane;
    let length = planes.len();
    let capacity = planes.capacity();

    // SAFETY: `Plane` and `CpuPlane` are both `#[repr(C)]` with the identical
    // field sequence `(Vec<u8>, usize)`, and their size/alignment are asserted
    // below. The allocation, length, and capacity came from this same Vec, so
    // the returned Vec owns exactly the original elements. `ManuallyDrop`
    // prevents the source Vec from freeing or dropping those elements twice.
    unsafe { Vec::from_raw_parts(pointer, length, capacity) }
}

/// Owned system-memory planes for one decoded frame.
///
/// [`CpuMemory::new`] takes ownership of the supplied plane vector without
/// allocating or copying it. A worker-created pool may attach a bounded,
/// nonblocking recycle sender; dropping this value then returns the complete
/// plane vector to that pool on every producer and consumer exit path.
pub struct CpuMemory {
    pub planes: Vec<CpuPlane>,
    #[cfg(not(target_arch = "wasm32"))]
    #[doc(hidden)]
    recycle: Option<Sender<Vec<CpuPlane>>>,
}

impl CpuMemory {
    /// Takes ownership of `planes` without allocating or copying it.
    pub fn new(planes: Vec<CpuPlane>) -> Self {
        Self {
            planes,
            #[cfg(not(target_arch = "wasm32"))]
            recycle: None,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[doc(hidden)]
    pub fn new_recyclable(planes: Vec<CpuPlane>, recycle: Sender<Vec<CpuPlane>>) -> Self {
        Self {
            planes,
            recycle: Some(recycle),
        }
    }

    /// Takes the owned planes out of this memory value without recycling them.
    ///
    /// This is the narrow escape hatch for callers that need to consume the
    /// public `planes` field by value. A pooled payload is deliberately
    /// detached from its generation rather than returning an empty vector to
    /// the pool.
    pub fn into_planes(mut self) -> Vec<CpuPlane> {
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.recycle.take();
        std::mem::take(&mut self.planes)
    }
}

impl fmt::Debug for CpuMemory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CpuMemory")
            .field("planes", &self.planes)
            .finish()
    }
}

impl PartialEq for CpuMemory {
    fn eq(&self, other: &Self) -> bool {
        self.planes == other.planes
    }
}

impl Eq for CpuMemory {}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for CpuMemory {
    fn drop(&mut self) {
        let Some(recycle) = self.recycle.take() else {
            return;
        };
        let planes = std::mem::take(&mut self.planes);
        match recycle.try_send(planes) {
            Ok(()) => {}
            Err(TrySendError::Full(_planes)) => {}
            Err(TrySendError::Disconnected(_planes)) => {}
        }
    }
}

#[cfg(test)]
mod plane_layout_tests {
    use super::*;

    #[test]
    fn decoder_and_lease_plane_layouts_match() {
        assert_eq!(
            std::mem::size_of::<video::Plane>(),
            std::mem::size_of::<CpuPlane>()
        );
        assert_eq!(
            std::mem::align_of::<video::Plane>(),
            std::mem::align_of::<CpuPlane>()
        );
    }

    #[test]
    fn decoder_planes_transfer_without_reallocation() {
        let planes = vec![video::Plane::new(vec![1, 2, 3, 4], 4)];
        let pointer = planes.as_ptr();
        let capacity = planes.capacity();
        let planes = into_cpu_planes(planes);
        assert_eq!(planes.as_ptr() as *const video::Plane, pointer);
        assert_eq!(planes.capacity(), capacity);
        assert_eq!(
            planes.first().map(|plane| plane.bytes.as_slice()),
            Some([1, 2, 3, 4].as_slice())
        );
        assert_eq!(planes.first().map(|plane| plane.stride), Some(4));
    }

    #[test]
    fn cpu_memory_into_planes_preserves_owned_payload() {
        let memory = CpuMemory::new(vec![CpuPlane::new(vec![1, 2, 3, 4], 4)]);
        let planes = memory.into_planes();
        assert_eq!(
            planes.first().map(|plane| plane.bytes.as_slice()),
            Some([1, 2, 3, 4].as_slice())
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cpu_memory_drop_safely_discards_full_or_disconnected_recycles() {
        let (recycle, available) = crossbeam_channel::bounded(1);
        assert!(recycle.try_send(vec![CpuPlane::new(vec![1], 1)]).is_ok());
        drop(CpuMemory::new_recyclable(
            vec![CpuPlane::new(vec![2], 1)],
            recycle,
        ));
        assert!(available.try_recv().is_ok());

        let (recycle, available) = crossbeam_channel::bounded(1);
        drop(available);
        drop(CpuMemory::new_recyclable(
            vec![CpuPlane::new(vec![3], 1)],
            recycle,
        ));
    }
}

/// Errors from constructing an owned frame lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeFrameError {
    InvalidExtent,
    PlaneCount {
        expected: usize,
        actual: usize,
    },
    MemoryPlaneObject {
        memory_plane: usize,
        object: usize,
    },
    FormatPlaneMemory {
        format_plane: usize,
        memory_plane: usize,
    },
    MemoryPlaneOutOfBounds {
        memory_plane: usize,
        offset: u64,
        size: u64,
        object_size: u64,
    },
    FormatPlaneOutOfBounds {
        format_plane: usize,
        offset: u64,
        size: u64,
        memory_plane_size: u64,
    },
}

impl fmt::Display for NativeFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExtent => write!(f, "frame extent must be non-zero"),
            Self::PlaneCount { expected, actual } => {
                write!(f, "frame format requires {expected} planes, got {actual}")
            }
            Self::MemoryPlaneObject {
                memory_plane,
                object,
            } => {
                write!(
                    f,
                    "memory plane {memory_plane} references missing object {object}"
                )
            }
            Self::FormatPlaneMemory {
                format_plane,
                memory_plane,
            } => {
                write!(
                    f,
                    "format plane {format_plane} references missing memory plane {memory_plane}"
                )
            }
            Self::MemoryPlaneOutOfBounds {
                memory_plane,
                offset,
                size,
                object_size,
            } => {
                write!(
                    f,
                    "memory plane {memory_plane} view [{offset}, {}) exceeds object size {object_size}",
                    offset.saturating_add(*size)
                )
            }
            Self::FormatPlaneOutOfBounds {
                format_plane,
                offset,
                size,
                memory_plane_size,
            } => {
                write!(
                    f,
                    "format plane {format_plane} view [{offset}, {}) exceeds memory plane size {memory_plane_size}",
                    offset.saturating_add(*size)
                )
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
    /// Maximum known extent of the underlying DMABuf object. `None` means
    /// that the producer could not prove a bound from allocator metadata.
    pub size: Option<u64>,
}

/// One GstMemory-like view into a DMABuf memory object.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufMemoryPlane {
    /// Index into [`DmaBufMemory::objects`].
    pub object: usize,
    /// Offset of this memory view within the object.
    pub offset: u64,
    /// Size of this memory view, when the producer knows it.
    pub size: Option<u64>,
}

/// A format-plane view into one of a DMABuf frame's memory views.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBufFormatPlane {
    /// Index into [`DmaBufMemory::memory_planes`].
    pub memory_plane: usize,
    /// Offset local to the referenced memory view.
    pub offset: u64,
    /// Bytes between the starts of adjacent rows.
    pub stride: u32,
    /// Size of this format-plane view, when derivable from negotiated layout.
    pub size: Option<u64>,
}

/// Linux DMABuf memory with object, memory-view, and format-plane layers kept
/// separate.
///
/// [`DmaBufMemory::new`] takes ownership of the supplied memory objects and
/// plane descriptors without allocating or copying them. It validates both
/// reference layers; allocation and pooling remain producer responsibilities.
#[cfg(target_os = "linux")]
#[derive(Clone)]
pub struct ProducerOwner(Arc<dyn Send + Sync>);

#[cfg(target_os = "linux")]
impl ProducerOwner {
    /// Retains an opaque producer resource until the native frame lease drops.
    pub fn new<T: Send + Sync + 'static>(owner: T) -> Self {
        Self(Arc::new(owner))
    }
}

#[cfg(target_os = "linux")]
impl fmt::Debug for ProducerOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProducerOwner").finish_non_exhaustive()
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct DmaBufMemory {
    pub objects: Vec<DmaBufObject>,
    pub memory_planes: Vec<DmaBufMemoryPlane>,
    pub format_planes: Vec<DmaBufFormatPlane>,
    /// DRM fourcc describing the image format at the import seam.
    pub drm_fourcc: Option<u32>,
    /// DRM modifier shared by the image's memory layout.
    pub modifier: Option<u64>,
    /// Opaque producer resource retained through renderer completion.
    pub owner: Option<ProducerOwner>,
}

#[cfg(target_os = "linux")]
impl DmaBufMemory {
    /// Takes ownership of DMABuf objects and plane descriptors and validates
    /// their references without allocating or copying either vector.
    pub fn new(
        objects: Vec<DmaBufObject>,
        memory_planes: Vec<DmaBufMemoryPlane>,
        format_planes: Vec<DmaBufFormatPlane>,
        drm_fourcc: Option<u32>,
        modifier: Option<u64>,
    ) -> Result<Self, NativeFrameError> {
        Self::validate_references(&objects, &memory_planes, &format_planes)?;
        Ok(Self {
            objects,
            memory_planes,
            format_planes,
            drm_fourcc,
            modifier,
            owner: None,
        })
    }

    /// Attaches an opaque producer owner without exposing its framework type.
    pub fn with_owner(mut self, owner: ProducerOwner) -> Self {
        self.owner = Some(owner);
        self
    }

    /// Validates both reference layers after construction.
    pub fn validate(&self) -> Result<(), NativeFrameError> {
        Self::validate_references(&self.objects, &self.memory_planes, &self.format_planes)
    }

    fn validate_references(
        objects: &[DmaBufObject],
        memory_planes: &[DmaBufMemoryPlane],
        format_planes: &[DmaBufFormatPlane],
    ) -> Result<(), NativeFrameError> {
        for (memory_plane, plane) in memory_planes.iter().enumerate() {
            let Some(object) = objects.get(plane.object) else {
                return Err(NativeFrameError::MemoryPlaneObject {
                    memory_plane,
                    object: plane.object,
                });
            };
            if let (Some(object_size), Some(view_size)) = (object.size, plane.size) {
                let Some(view_end) = plane.offset.checked_add(view_size) else {
                    return Err(NativeFrameError::MemoryPlaneOutOfBounds {
                        memory_plane,
                        offset: plane.offset,
                        size: view_size,
                        object_size,
                    });
                };
                if view_end > object_size {
                    return Err(NativeFrameError::MemoryPlaneOutOfBounds {
                        memory_plane,
                        offset: plane.offset,
                        size: view_size,
                        object_size,
                    });
                }
            }
        }
        for (format_plane, plane) in format_planes.iter().enumerate() {
            let Some(memory) = memory_planes.get(plane.memory_plane) else {
                return Err(NativeFrameError::FormatPlaneMemory {
                    format_plane,
                    memory_plane: plane.memory_plane,
                });
            };
            if let (Some(memory_size), Some(plane_size)) = (memory.size, plane.size) {
                let Some(plane_end) = plane.offset.checked_add(plane_size) else {
                    return Err(NativeFrameError::FormatPlaneOutOfBounds {
                        format_plane,
                        offset: plane.offset,
                        size: plane_size,
                        memory_plane_size: memory_size,
                    });
                };
                if plane_end > memory_size {
                    return Err(NativeFrameError::FormatPlaneOutOfBounds {
                        format_plane,
                        offset: plane.offset,
                        size: plane_size,
                        memory_plane_size: memory_size,
                    });
                }
            }
        }
        Ok(())
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

/// Framework-neutral identity, timing, extent, pixel format, and copy-only SDR
/// color metadata for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrameDescriptor {
    pub frame_id: u64,
    pub stream_generation: u64,
    pub pts: MediaTime,
    pub duration: Option<MediaTime>,
    pub extent: FrameExtent,
    pub format: PixelFormat,
    pub color: ColorMetadata,
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
            NativeMemory::DmaBuf(dmabuf) => {
                dmabuf.validate()?;
                dmabuf.format_planes.len()
            }
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
