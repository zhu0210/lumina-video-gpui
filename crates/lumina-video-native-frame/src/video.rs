//! Video playback core types and state machine.
//!
//! This module provides the foundational types for hardware-accelerated video
//! playback across all platforms (macOS, Windows, Linux, Android).

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::time::Duration;

pub use lumina_video_core::video::{VideoError, VideoMetadata, VideoPlayerHandle, VideoState};

/// Pixel format used to describe decoded native frame planes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Yuv420p,
    Nv12,
    Rgb24,
    Rgba,
    Bgra,
}

impl PixelFormat {
    pub fn num_planes(&self) -> usize {
        match self {
            Self::Yuv420p => 3,
            Self::Nv12 => 2,
            Self::Rgb24 | Self::Rgba | Self::Bgra => 1,
        }
    }

    pub fn is_yuv(&self) -> bool {
        matches!(self, Self::Yuv420p | Self::Nv12)
    }
}

/// Hardware acceleration reported by a native decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HwAccelType {
    None,
    VideoToolbox,
    Vaapi,
    Vdpau,
    D3d11va,
    Dxva2,
    MediaCodec,
}

impl HwAccelType {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn platform_default() -> Self {
        Self::VideoToolbox
    }

    #[cfg(target_os = "windows")]
    pub fn platform_default() -> Self {
        Self::D3d11va
    }

    #[cfg(target_os = "linux")]
    pub fn platform_default() -> Self {
        Self::Vaapi
    }

    #[cfg(target_os = "android")]
    pub fn platform_default() -> Self {
        Self::MediaCodec
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        target_os = "linux",
        target_os = "android"
    )))]
    pub fn platform_default() -> Self {
        Self::None
    }
}

// =============================================================================
// Platform-specific GPU surface types for zero-copy rendering
// =============================================================================

/// macOS GPU surface holding an IOSurface reference for zero-copy import.
///
/// The IOSurface is obtained from a CVPixelBuffer. The `_owner` field keeps
/// the CVPixelBuffer alive, ensuring the IOSurface remains valid.
///
/// # Purpose
///
/// This struct enables zero-copy video frame rendering on macOS by wrapping
/// an IOSurface pointer that can be imported directly into Metal/wgpu without
/// copying pixel data through CPU memory.
///
/// # Example
///
/// ```
/// # #[cfg(target_os = "macos")]
/// # fn main() {
/// use lumina_video_native_frame::video::{MacOSGpuSurface, PixelFormat, CpuFrame};
/// use std::sync::Arc;
///
/// // MacOSGpuSurface::new() is unsafe because it requires valid IOSurface pointers.
/// // In practice, surfaces are created by the decoder, not user code.
/// // Here we demonstrate field access patterns:
///
/// fn process_macos_frame(surface: &MacOSGpuSurface) {
///     // Access dimensions and format (safe)
///     let width = surface.width;
///     let height = surface.height;
///     let format = surface.format;
///
///     println!("Frame dimensions: {}x{}", width, height);
///     println!("Pixel format: {:?}", format);
///
///     // Check if IOSurface pointer is valid before import
///     if !surface.io_surface.is_null() {
///         // Import into wgpu via zero_copy::macos::import_iosurface()
///         // The _owner field (not directly accessible) keeps the CVPixelBuffer alive
///     }
///
///     // CPU fallback is available for graceful degradation
///     if let Some(ref cpu_frame) = surface.cpu_fallback {
///         println!("Fallback available: {}x{}", cpu_frame.width, cpu_frame.height);
///     }
/// }
/// # }
/// # #[cfg(not(target_os = "macos"))]
/// # fn main() {}
/// ```
///
/// # Safety
///
/// The [`MacOSGpuSurface::new()`] constructor is unsafe because:
/// - The `io_surface` pointer must be a valid `IOSurfaceRef` from `CVPixelBufferGetIOSurface()`
/// - The `owner` must keep the underlying `CVPixelBuffer` alive for the surface's lifetime
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub struct MacOSGpuSurface {
    /// The IOSurface pointer (from CVPixelBufferGetIOSurface)
    pub io_surface: *mut std::ffi::c_void,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Pixel format
    pub format: PixelFormat,
    /// CPU fallback frame data for when zero-copy import fails.
    /// Populated at decode time to ensure graceful degradation.
    pub cpu_fallback: Option<CpuFrame>,
    /// Reference to keep the underlying CVPixelBuffer alive.
    /// Uses Arc<dyn Any + Send + Sync> to hide objc2 types from public API.
    _owner: Arc<dyn std::any::Any + Send + Sync>,
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl MacOSGpuSurface {
    /// Creates a new macOS GPU surface from an IOSurface.
    ///
    /// # Arguments
    /// * `io_surface` - Valid IOSurfaceRef from CVPixelBufferGetIOSurface()
    /// * `width` - Width in pixels
    /// * `height` - Height in pixels
    /// * `format` - Pixel format
    /// * `cpu_fallback` - Optional CPU frame data for fallback when zero-copy fails
    /// * `owner` - Object that owns the IOSurface (typically the CVPixelBuffer)
    ///
    /// # Safety
    /// - `io_surface` must be a valid IOSurfaceRef from CVPixelBufferGetIOSurface()
    /// - `owner` must be the object that owns the IOSurface (typically the CVPixelBuffer)
    pub unsafe fn new(
        io_surface: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        format: PixelFormat,
        cpu_fallback: Option<CpuFrame>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            io_surface,
            width,
            height,
            format,
            cpu_fallback,
            _owner: owner,
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl Clone for MacOSGpuSurface {
    fn clone(&self) -> Self {
        Self {
            io_surface: self.io_surface,
            width: self.width,
            height: self.height,
            format: self.format,
            cpu_fallback: self.cpu_fallback.clone(),
            _owner: Arc::clone(&self._owner),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl std::fmt::Debug for MacOSGpuSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacOSGpuSurface")
            .field("io_surface", &self.io_surface)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish()
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
// SAFETY: The IOSurface pointer is kept valid by the Send + Sync owner Arc, and
// IOSurface itself supports cross-thread access according to Apple.
unsafe impl Send for MacOSGpuSurface {}
#[cfg(any(target_os = "macos", target_os = "ios"))]
// SAFETY: The IOSurface pointer is kept valid by the Send + Sync owner Arc, and
// IOSurface itself supports cross-thread access according to Apple.
unsafe impl Sync for MacOSGpuSurface {}

/// Windows GPU surface holding a D3D11 shared handle for zero-copy import.
///
/// The shared handle is obtained from IDXGIResource1::CreateSharedHandle() on a D3D11 texture
/// created with D3D11_RESOURCE_MISC_SHARED_NTHANDLE flag.
///
/// # Purpose
///
/// This struct enables zero-copy video frame rendering on Windows by wrapping
/// a D3D11 shared NT handle that can be imported into D3D12/wgpu without
/// copying pixel data through CPU memory.
///
/// # Example
///
/// ```
/// # #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
/// # fn main() {
/// use lumina_video_native_frame::video::{WindowsGpuSurface, PixelFormat};
///
/// // WindowsGpuSurface::new() is unsafe because it requires valid D3D11 handles.
/// // In practice, surfaces are created by the decoder, not user code.
/// // Here we demonstrate field access patterns:
///
/// fn process_windows_frame(surface: &WindowsGpuSurface) {
///     // Access dimensions and format (safe)
///     let width = surface.width;
///     let height = surface.height;
///     let format = surface.format;
///
///     println!("Frame dimensions: {}x{}", width, height);
///     println!("Pixel format: {:?}", format);
///
///     // The shared_handle field contains the NT handle for D3D12 import
///     // Import into wgpu via zero_copy::windows::import_shared_handle()
///     // The _owner field (not directly accessible) keeps the D3D11 texture alive
///
///     // CPU fallback is available for graceful degradation
///     if let Some(ref cpu_frame) = surface.cpu_fallback {
///         println!("Fallback available: {}x{}", cpu_frame.width, cpu_frame.height);
///     }
/// }
/// # }
/// # #[cfg(not(all(target_os = "windows", feature = "windows-native-video")))]
/// # fn main() {}
/// ```
///
/// # Safety
///
/// The [`WindowsGpuSurface::new()`] constructor is unsafe because:
/// - The `shared_handle` must be a valid HANDLE from `IDXGIResource1::CreateSharedHandle()`
/// - The `owner` must keep the underlying D3D11 texture alive for the surface's lifetime
#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub struct WindowsGpuSurface {
    /// The shared NT handle (from CreateSharedHandle)
    pub shared_handle: windows::Win32::Foundation::HANDLE,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Pixel format
    pub format: PixelFormat,
    /// CPU fallback frame data for when zero-copy import fails.
    /// Populated at decode time to ensure graceful degradation.
    pub cpu_fallback: Option<CpuFrame>,
    /// Reference to keep the underlying D3D11 texture alive.
    /// Uses Arc<dyn Any + Send + Sync> to hide D3D11 types from public API.
    _owner: Arc<dyn std::any::Any + Send + Sync>,
    cpu_fallback_requested: Option<Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
impl WindowsGpuSurface {
    /// Creates a new Windows GPU surface from a shared handle.
    ///
    /// # Safety
    /// - `shared_handle` must be a valid HANDLE from IDXGIResource1::CreateSharedHandle()
    /// - `owner` must be the object that owns the D3D11 texture (keeps it alive)
    pub unsafe fn new(
        shared_handle: windows::Win32::Foundation::HANDLE,
        width: u32,
        height: u32,
        format: PixelFormat,
        cpu_fallback: Option<CpuFrame>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            shared_handle,
            width,
            height,
            format,
            cpu_fallback,
            _owner: owner,
            cpu_fallback_requested: None,
        }
    }
    pub(crate) fn with_cpu_fallback_request(
        mut self,
        requested: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.cpu_fallback_requested = Some(requested);
        self
    }

    /// Requests system-memory delivery after a native import failure.
    /// No pixels are read back until the decoder processes its next frame.
    pub fn request_cpu_fallback(&self) {
        if let Some(requested) = &self.cpu_fallback_requested {
            requested.store(true, std::sync::atomic::Ordering::Release);
        }
    }
}

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
impl Clone for WindowsGpuSurface {
    fn clone(&self) -> Self {
        Self {
            shared_handle: self.shared_handle,
            width: self.width,
            height: self.height,
            format: self.format,
            cpu_fallback: self.cpu_fallback.clone(),
            _owner: Arc::clone(&self._owner),
            cpu_fallback_requested: self.cpu_fallback_requested.clone(),
        }
    }
}

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
impl std::fmt::Debug for WindowsGpuSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsGpuSurface")
            .field("shared_handle", &self.shared_handle)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish()
    }
}

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
// SAFETY: The D3D11 texture owner is retained by a Send + Sync Arc, and the
// shared NT handle is an OS handle that may be transferred across threads.
unsafe impl Send for WindowsGpuSurface {}
#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
// SAFETY: The D3D11 texture owner is retained by a Send + Sync Arc, and the
// shared NT handle is an OS handle that may be transferred across threads.
unsafe impl Sync for WindowsGpuSurface {}

/// Android GPU surface holding an AHardwareBuffer reference for zero-copy import.
///
/// The AHardwareBuffer is obtained from AImageReader connected to MediaCodec.
/// The `_owner` field keeps the AImage alive, ensuring the AHardwareBuffer remains valid.
///
/// # Purpose
///
/// This struct enables zero-copy video frame rendering on Android by wrapping
/// an AHardwareBuffer pointer that can be imported directly into Vulkan/wgpu
/// without copying pixel data through CPU memory.
///
/// # Example
///
/// ```
/// # #[cfg(target_os = "android")]
/// # fn main() {
/// use lumina_video_native_frame::video::{AndroidGpuSurface, PixelFormat};
///
/// // AndroidGpuSurface::new() is unsafe because it requires valid AHardwareBuffer pointers.
/// // In practice, surfaces are created by the decoder (MediaCodec), not user code.
/// // Here we demonstrate field access patterns:
///
/// fn process_android_frame(surface: &AndroidGpuSurface) {
///     // Access dimensions and format (safe)
///     let width = surface.width;
///     let height = surface.height;
///     let format = surface.format;
///
///     println!("Frame dimensions: {}x{}", width, height);
///     println!("Pixel format: {:?}", format);
///
///     // Check if AHardwareBuffer pointer is valid before import
///     if !surface.ahardware_buffer.is_null() {
///         // Import into wgpu via zero_copy::android::import_ahardwarebuffer()
///         // The _owner field (not directly accessible) keeps the AImage alive
///     }
///
///     // CPU fallback is available for graceful degradation
///     if let Some(ref cpu_frame) = surface.cpu_fallback {
///         println!("Fallback available: {}x{}", cpu_frame.width, cpu_frame.height);
///     }
/// }
/// # }
/// # #[cfg(not(target_os = "android"))]
/// # fn main() {}
/// ```
///
/// # Safety
///
/// The [`AndroidGpuSurface::new()`] constructor is unsafe because:
/// - The `ahardware_buffer` must be a valid pointer from `AImage_getHardwareBuffer()`
/// - The `owner` must keep the underlying `AImage` alive for the surface's lifetime
#[cfg(target_os = "android")]
pub struct AndroidGpuSurface {
    /// The AHardwareBuffer pointer (from AImage_getHardwareBuffer)
    pub ahardware_buffer: *mut std::ffi::c_void,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Pixel format
    pub format: PixelFormat,
    /// CPU fallback frame data for when zero-copy import fails.
    /// Populated at decode time to ensure graceful degradation.
    pub cpu_fallback: Option<CpuFrame>,
    /// Reference to keep the underlying AImage alive.
    /// Uses Arc<dyn Any + Send + Sync> to hide NDK types from public API.
    _owner: Arc<dyn std::any::Any + Send + Sync>,
}

#[cfg(target_os = "android")]
impl AndroidGpuSurface {
    /// Creates a new Android GPU surface from an AHardwareBuffer.
    ///
    /// # Safety
    /// - `ahardware_buffer` must be a valid AHardwareBuffer pointer from AImage_getHardwareBuffer()
    /// - `owner` must be the object that owns the AHardwareBuffer (typically the AImage)
    pub unsafe fn new(
        ahardware_buffer: *mut std::ffi::c_void,
        width: u32,
        height: u32,
        format: PixelFormat,
        cpu_fallback: Option<CpuFrame>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            ahardware_buffer,
            width,
            height,
            format,
            cpu_fallback,
            _owner: owner,
        }
    }
}

#[cfg(target_os = "android")]
impl Clone for AndroidGpuSurface {
    fn clone(&self) -> Self {
        Self {
            ahardware_buffer: self.ahardware_buffer,
            width: self.width,
            height: self.height,
            format: self.format,
            cpu_fallback: self.cpu_fallback.clone(),
            _owner: Arc::clone(&self._owner),
        }
    }
}

#[cfg(target_os = "android")]
impl std::fmt::Debug for AndroidGpuSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidGpuSurface")
            .field("ahardware_buffer", &self.ahardware_buffer)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish()
    }
}

#[cfg(target_os = "android")]
// SAFETY: The AHardwareBuffer owner is retained by a Send + Sync Arc, and the
// Android NDK documents AHardwareBuffer handles as thread-safe.
unsafe impl Send for AndroidGpuSurface {}
#[cfg(target_os = "android")]
// SAFETY: The AHardwareBuffer owner is retained by a Send + Sync Arc, and the
// Android NDK documents AHardwareBuffer handles as thread-safe.
unsafe impl Sync for AndroidGpuSurface {}

/// Metadata for a single DMABuf plane.
///
/// Multi-plane formats like NV12 and YUV420p have separate metadata for each plane:
/// - NV12: Plane 0 = Y (luma), Plane 1 = UV (interleaved chroma)
/// - YUV420p: Plane 0 = Y, Plane 1 = U, Plane 2 = V
///
/// Each plane may have its own file descriptor (multiple FD case) or share a single
/// FD with different offsets (single FD case). GStreamer/VA-API can produce either.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct DmaBufPlane {
    /// The DMABuf file descriptor for this plane.
    /// Multiple planes may share the same fd (with different offsets) or have separate fds.
    pub fd: std::os::fd::RawFd,
    /// Offset within the buffer where this plane starts (bytes)
    pub offset: u64,
    /// Row pitch/stride in bytes for this plane
    pub stride: u32,
    /// Size of this plane's data in bytes (may be 0 if unknown/not provided)
    pub size: u64,
}

/// Linux GPU surface holding DMABuf file descriptors for zero-copy import.
///
/// DMABuf is the standard Linux kernel mechanism for sharing GPU memory between
/// devices. VA-API and GStreamer can export decoded frames as DMABuf FDs, which
/// can be imported directly into Vulkan/wgpu without CPU copies.
///
/// # Purpose
///
/// This struct enables zero-copy video frame rendering on Linux by wrapping
/// DMABuf file descriptors that can be imported directly into Vulkan/wgpu
/// without copying pixel data through CPU memory.
///
/// # Multi-Plane Support
///
/// This struct supports multi-plane formats like NV12 and YUV420p through the
/// `planes` vector. Each plane has its own fd/offset/stride metadata:
///
/// - **Single-plane (RGBA/BGRA)**: `planes.len() == 1`
/// - **NV12**: `planes.len() == 2` (Y + interleaved UV)
/// - **YUV420p**: `planes.len() == 3` (Y + U + V)
///
/// GStreamer can export planes in two configurations:
/// 1. **Single FD with offsets**: All planes share one fd, distinguished by offset
/// 2. **Multiple FDs**: Each plane has its own fd (offset is typically 0)
///
/// The Vulkan import code must handle both cases appropriately.
///
/// # Example
///
/// ```
/// # #[cfg(target_os = "linux")]
/// # fn main() {
/// use lumina_video_native_frame::video::{LinuxGpuSurface, DmaBufPlane, PixelFormat};
///
/// // LinuxGpuSurface::new() is unsafe because it requires valid DMABuf file descriptors.
/// // In practice, surfaces are created by the decoder (GStreamer/VA-API), not user code.
/// // Here we demonstrate field access patterns:
///
/// fn process_linux_frame(surface: &LinuxGpuSurface) {
///     // Access dimensions and format (safe)
///     let width = surface.width;
///     let height = surface.height;
///     let format = surface.format;
///
///     println!("Frame dimensions: {}x{}", width, height);
///     println!("Pixel format: {:?}", format);
///
///     // Query plane information
///     println!("Number of planes: {}", surface.num_planes());
///     println!("Is multi-plane format: {}", surface.is_multi_plane());
///     println!("Single FD layout: {}", surface.is_single_fd);
///     println!("DRM modifier: 0x{:x}", surface.modifier);
///
///     // Access primary plane (Y/luma for YUV, or only plane for RGB)
///     let primary_fd = surface.primary_fd();
///     let primary_offset = surface.primary_offset();
///     let primary_stride = surface.primary_stride();
///
///     if primary_fd >= 0 {
///         // Import DMABuf into wgpu via zero_copy::linux::import_dmabuf()
///         // The _owner field (not directly accessible) keeps the GStreamer sample alive
///     }
///
///     // Iterate over all planes for multi-plane formats
///     for (i, plane) in surface.planes.iter().enumerate() {
///         println!("Plane {}: fd={}, offset={}, stride={}",
///             i, plane.fd, plane.offset, plane.stride);
///     }
///
///     // CPU fallback is available for graceful degradation
///     if let Some(ref cpu_frame) = surface.cpu_fallback {
///         println!("Fallback available: {}x{}", cpu_frame.width, cpu_frame.height);
///     }
/// }
/// # }
/// # #[cfg(not(target_os = "linux"))]
/// # fn main() {}
/// ```
///
/// # Safety
///
/// The [`LinuxGpuSurface::new()`] and [`LinuxGpuSurface::new_single_plane()`]
/// constructors are unsafe because:
/// - All file descriptors in `planes` must be valid DMABuf FDs
/// - The `owner` must keep the underlying GStreamer sample alive for the surface's lifetime
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct LinuxGpuSurface {
    /// Per-plane DMABuf metadata. Length matches PixelFormat::num_planes().
    pub planes: Vec<DmaBufPlane>,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Pixel format
    pub format: PixelFormat,
    /// DRM format modifier (e.g., linear, tiled). Applies to all planes.
    pub modifier: u64,
    /// True if all planes share a single FD with different offsets.
    /// This is common with VA-API which outputs single-FD multi-plane layouts.
    /// When true, the Vulkan import must use VkImageDrmFormatModifierExplicitCreateInfoEXT
    /// with a pPlaneLayouts array specifying each plane's offset and stride.
    pub is_single_fd: bool,
    /// CPU fallback frame data for when zero-copy import fails.
    /// Populated at decode time to ensure graceful degradation.
    pub cpu_fallback: Option<CpuFrame>,
    /// Reference to keep the underlying GStreamer sample alive.
    /// This ensures the DMABuf fds remain valid.
    _owner: Arc<dyn std::any::Any + Send + Sync>,
}

#[cfg(target_os = "linux")]
impl LinuxGpuSurface {
    /// Creates a new Linux GPU surface from multi-plane DMABuf metadata.
    ///
    /// # Safety
    /// - All fds in `planes` must be valid DMABuf file descriptors
    /// - `owner` must be the object that owns the DMABuf(s) (typically the GStreamer sample)
    /// - The owner must remain alive for the lifetime of this surface
    ///
    /// # Arguments
    /// * `planes` - Per-plane DMABuf metadata (fd, offset, stride, size)
    /// * `width` - Width in pixels
    /// * `height` - Height in pixels
    /// * `format` - Pixel format (determines expected number of planes)
    /// * `modifier` - DRM format modifier (applies to all planes)
    /// * `is_single_fd` - True if all planes share a single FD with different offsets
    /// * `cpu_fallback` - Optional CPU frame for fallback when zero-copy fails
    /// * `owner` - Object that keeps the DMABuf fds valid
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new(
        planes: Vec<DmaBufPlane>,
        width: u32,
        height: u32,
        format: PixelFormat,
        modifier: u64,
        is_single_fd: bool,
        cpu_fallback: Option<CpuFrame>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self {
            planes,
            width,
            height,
            format,
            modifier,
            is_single_fd,
            cpu_fallback,
            _owner: owner,
        }
    }

    /// Creates a new Linux GPU surface from a single DMABuf file descriptor.
    ///
    /// This is a convenience constructor for single-plane formats (RGBA, BGRA).
    /// For multi-plane formats (NV12, YUV420p), use `new()` with explicit plane metadata.
    ///
    /// # Safety
    /// - `fd` must be a valid DMABuf file descriptor
    /// - `owner` must be the object that owns the DMABuf (typically the GStreamer sample)
    /// - The owner must remain alive for the lifetime of this surface
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn new_single_plane(
        fd: std::os::fd::RawFd,
        width: u32,
        height: u32,
        format: PixelFormat,
        modifier: u64,
        size: u64,
        offset: u64,
        stride: u32,
        cpu_fallback: Option<CpuFrame>,
        owner: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        let plane = DmaBufPlane {
            fd,
            offset,
            stride,
            size,
        };
        Self {
            planes: vec![plane],
            width,
            height,
            format,
            modifier,
            is_single_fd: false, // Single-plane, so not applicable
            cpu_fallback,
            _owner: owner,
        }
    }

    /// Returns the primary file descriptor (from plane 0).
    ///
    /// For single-plane formats, this is the only fd.
    /// For multi-plane formats, this is the fd for the Y/luma plane.
    pub fn primary_fd(&self) -> std::os::fd::RawFd {
        self.planes.first().map(|p| p.fd).unwrap_or(-1)
    }

    /// Returns the primary plane's offset (from plane 0).
    pub fn primary_offset(&self) -> u64 {
        self.planes.first().map(|p| p.offset).unwrap_or(0)
    }

    /// Returns the primary plane's stride (from plane 0).
    pub fn primary_stride(&self) -> u32 {
        self.planes.first().map(|p| p.stride).unwrap_or(0)
    }

    /// Returns the primary plane's size (from plane 0).
    pub fn primary_size(&self) -> u64 {
        self.planes.first().map(|p| p.size).unwrap_or(0)
    }

    /// Returns true if this is a multi-plane format (NV12, YUV420p).
    pub fn is_multi_plane(&self) -> bool {
        self.planes.len() > 1
    }

    /// Returns the number of planes.
    pub fn num_planes(&self) -> usize {
        self.planes.len()
    }
}

#[cfg(target_os = "linux")]
// SAFETY: The GStreamer sample owner is retained by a Send + Sync Arc, and
// DMABuf file descriptors are kernel handles usable from any thread.
unsafe impl Send for LinuxGpuSurface {}
#[cfg(target_os = "linux")]
// SAFETY: The GStreamer sample owner is retained by a Send + Sync Arc, and
// DMABuf file descriptors are kernel handles usable from any thread.
unsafe impl Sync for LinuxGpuSurface {}

/// A single plane of pixel data.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    /// Raw pixel data
    pub data: Vec<u8>,
    /// Stride (bytes per row, may include padding)
    pub stride: usize,
}

impl Plane {
    /// Takes ownership of one plane's bytes without copying them.
    pub fn new(data: Vec<u8>, stride: usize) -> Self {
        Self { data, stride }
    }
}

/// A decoded video frame with CPU-accessible pixel data.
#[derive(Debug, Clone)]
pub struct CpuFrame {
    /// Pixel format of the frame
    pub format: PixelFormat,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Pixel data planes
    pub planes: Vec<Plane>,
}

impl CpuFrame {
    /// Creates a new CpuFrame with the given parameters.
    pub fn new(format: PixelFormat, width: u32, height: u32, planes: Vec<Plane>) -> Self {
        Self {
            format,
            width,
            height,
            planes,
        }
    }

    /// Returns the Y plane for YUV formats, or the single plane for RGB formats.
    pub fn plane(&self, index: usize) -> Option<&Plane> {
        self.planes.get(index)
    }
}

/// Copies an Android image plane while honoring row padding and interleaved
/// chroma samples. The caller keeps the Image acquired until this returns.
#[cfg(any(target_os = "android", test))]
pub(crate) fn copy_strided_plane(
    source: &[u8],
    width: usize,
    height: usize,
    row_stride: usize,
    pixel_stride: usize,
) -> Result<Plane, VideoError> {
    let invalid = || VideoError::DecodeFailed("invalid Android image plane layout".into());
    if width == 0 || height == 0 || pixel_stride == 0 {
        return Err(invalid());
    }
    let row_bytes = (width - 1)
        .checked_mul(pixel_stride)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(invalid)?;
    let required = (height - 1)
        .checked_mul(row_stride)
        .and_then(|n| n.checked_add(row_bytes))
        .ok_or_else(invalid)?;
    if row_stride < row_bytes || required > source.len() {
        return Err(invalid());
    }
    let size = width.checked_mul(height).ok_or_else(invalid)?;
    let mut data = Vec::new();
    data.try_reserve_exact(size).map_err(|_| invalid())?;
    data.resize(size, 0);
    for (y, output) in data.chunks_exact_mut(width).enumerate() {
        let start = y * row_stride;
        let row = source.get(start..start + row_bytes).ok_or_else(invalid)?;
        for (out, sample) in output.iter_mut().zip(row.iter().step_by(pixel_stride)) {
            *out = *sample;
        }
    }
    Ok(Plane {
        data,
        stride: width,
    })
}

#[cfg(test)]
#[test]
fn strided_android_plane_copy_preserves_chroma_and_rejects_truncation() {
    let plane = copy_strided_plane(&[1, 9, 2, 9, 0, 0, 3, 9, 4], 2, 2, 6, 2).unwrap();
    assert_eq!(plane.data, [1, 2, 3, 4]);
    assert_eq!(plane.stride, 2);
    assert!(copy_strided_plane(&[1, 9, 2], 2, 2, 6, 2).is_err());
    assert!(copy_strided_plane(&[1, 2], 2, 1, 2, 0).is_err());
}

/// A decoded video frame, either CPU-accessible or platform-specific GPU surface.
#[derive(Debug, Clone)]
pub enum DecodedFrame {
    /// CPU-accessible frame data (works on all platforms)
    Cpu(CpuFrame),

    /// macOS/iOS GPU surface for zero-copy rendering via IOSurface → Metal
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    MacOS(MacOSGpuSurface),

    /// Windows GPU surface for zero-copy rendering via D3D11 shared handle → D3D12
    #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
    Windows(WindowsGpuSurface),

    /// Android GPU surface for zero-copy rendering via AHardwareBuffer → Vulkan
    #[cfg(target_os = "android")]
    Android(AndroidGpuSurface),

    /// Linux GPU surface for zero-copy rendering via DMABuf → Vulkan
    #[cfg(target_os = "linux")]
    Linux(LinuxGpuSurface),
}

impl DecodedFrame {
    /// Returns the frame dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DecodedFrame::Cpu(frame) => (frame.width, frame.height),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecodedFrame::MacOS(surface) => (surface.width, surface.height),
            #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
            DecodedFrame::Windows(surface) => (surface.width, surface.height),
            #[cfg(target_os = "android")]
            DecodedFrame::Android(surface) => (surface.width, surface.height),
            #[cfg(target_os = "linux")]
            DecodedFrame::Linux(surface) => (surface.width, surface.height),
        }
    }

    /// Returns the pixel format.
    pub fn format(&self) -> PixelFormat {
        match self {
            DecodedFrame::Cpu(frame) => frame.format,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecodedFrame::MacOS(surface) => surface.format,
            #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
            DecodedFrame::Windows(surface) => surface.format,
            #[cfg(target_os = "android")]
            DecodedFrame::Android(surface) => surface.format,
            #[cfg(target_os = "linux")]
            DecodedFrame::Linux(surface) => surface.format,
        }
    }

    /// Attempts to get a reference to the CPU frame data.
    pub fn as_cpu(&self) -> Option<&CpuFrame> {
        match self {
            DecodedFrame::Cpu(frame) => Some(frame),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecodedFrame::MacOS(_) => None,
            #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
            DecodedFrame::Windows(_) => None,
            #[cfg(target_os = "android")]
            DecodedFrame::Android(_) => None,
            #[cfg(target_os = "linux")]
            DecodedFrame::Linux(_) => None,
        }
    }

    /// Attempts to get a reference to the macOS/iOS GPU surface.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    pub fn as_macos_surface(&self) -> Option<&MacOSGpuSurface> {
        match self {
            DecodedFrame::MacOS(surface) => Some(surface),
            _ => None,
        }
    }

    /// Attempts to get a reference to the Windows GPU surface.
    #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
    pub fn as_windows_surface(&self) -> Option<&WindowsGpuSurface> {
        match self {
            DecodedFrame::Windows(surface) => Some(surface),
            _ => None,
        }
    }

    /// Attempts to get a reference to the Android GPU surface.
    #[cfg(target_os = "android")]
    pub fn as_android_surface(&self) -> Option<&AndroidGpuSurface> {
        match self {
            DecodedFrame::Android(surface) => Some(surface),
            _ => None,
        }
    }

    /// Attempts to get a reference to the Linux GPU surface.
    #[cfg(target_os = "linux")]
    pub fn as_linux_surface(&self) -> Option<&LinuxGpuSurface> {
        match self {
            DecodedFrame::Linux(surface) => Some(surface),
            _ => None,
        }
    }

    /// Returns true if this is a GPU surface (zero-copy capable).
    pub fn is_gpu_surface(&self) -> bool {
        match self {
            DecodedFrame::Cpu(_) => false,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecodedFrame::MacOS(_) => true,
            #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
            DecodedFrame::Windows(_) => true,
            #[cfg(target_os = "android")]
            DecodedFrame::Android(_) => true,
            #[cfg(target_os = "linux")]
            DecodedFrame::Linux(_) => true,
        }
    }
}

/// A video frame with presentation timestamp.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Presentation timestamp (when this frame should be displayed)
    pub pts: Duration,
    /// The decoded frame data
    pub frame: DecodedFrame,
}

impl VideoFrame {
    /// Creates a new VideoFrame.
    pub fn new(pts: Duration, frame: DecodedFrame) -> Self {
        Self { pts, frame }
    }

    /// Returns the frame dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        self.frame.dimensions()
    }
}

/// Trait for video decoder backends.
///
/// This trait abstracts the platform-specific video decoding implementations,
/// allowing the same video player code to work with FFmpeg on desktop and
/// ExoPlayer on Android.
pub trait VideoDecoderBackend: Send {
    /// Opens a video from a URL or file path.
    fn open(url: &str) -> Result<Self, VideoError>
    where
        Self: Sized;

    /// Decodes and returns the next video frame, or None if no more frames.
    fn decode_next(&mut self) -> Result<Option<VideoFrame>, VideoError>;

    /// Seeks to a specific position in the video.
    fn seek(&mut self, position: Duration) -> Result<(), VideoError>;

    /// Returns the video metadata.
    fn metadata(&self) -> &VideoMetadata;

    /// Pauses playback.
    ///
    /// For decoders with their own playback control (like ExoPlayer on Android),
    /// this actually pauses the underlying player. For decoders like FFmpeg that
    /// don't have playback state, this is a no-op (the decode thread handles pausing).
    fn pause(&mut self) -> Result<(), VideoError> {
        Ok(()) // Default no-op for decoders without playback control
    }

    /// Resumes playback.
    ///
    /// For decoders with their own playback control (like ExoPlayer on Android),
    /// this actually resumes the underlying player. For decoders like FFmpeg that
    /// don't have playback state, this is a no-op (the decode thread handles resuming).
    fn resume(&mut self) -> Result<(), VideoError> {
        Ok(()) // Default no-op for decoders without playback control
    }

    /// Returns the total duration if known.
    fn duration(&self) -> Option<Duration> {
        self.metadata().duration
    }

    /// Sets the muted state for audio playback.
    ///
    /// For decoders with integrated audio (like ExoPlayer on Android),
    /// this mutes/unmutes the audio. For decoders without integrated audio,
    /// this is a no-op (audio is handled separately).
    fn set_muted(&mut self, _muted: bool) -> Result<(), VideoError> {
        Ok(()) // Default no-op
    }

    /// Sets the volume for audio playback.
    ///
    /// For decoders with integrated audio (like ExoPlayer on Android),
    /// this sets the volume. For decoders without integrated audio,
    /// this is a no-op (audio is handled separately).
    fn set_volume(&mut self, _volume: f32) -> Result<(), VideoError> {
        Ok(()) // Default no-op
    }

    /// Returns true if this decoder handles audio playback internally.
    ///
    /// Decoders like macOS VideoToolbox (AVPlayer), GStreamer, and Android ExoPlayer
    /// handle audio internally with proper A/V sync. For these decoders, no separate
    /// audio thread should be started.
    ///
    /// Decoders like FFmpeg only decode video frames and require a separate audio
    /// pipeline (AudioThread + cpal) for audio playback.
    fn handles_audio_internally(&self) -> bool {
        false // Default: decoder does not handle audio (needs separate audio thread)
    }

    /// Returns the audio handle for decoders that manage audio internally.
    ///
    /// Used by MoQ decoders to expose their `AudioHandle` for A/V sync in
    /// `FrameScheduler`. Returns `None` by default.
    fn audio_handle(&self) -> Option<crate::audio::AudioHandle> {
        None
    }

    /// Returns the Android player ID for frame queue routing.
    ///
    /// Non-zero IDs route frames to per-player queues for multi-player isolation.
    /// Returns 0 (legacy shared queue) by default. Only `AndroidVideoDecoder` overrides this.
    fn android_player_id(&self) -> u64 {
        0
    }

    /// Returns the video dimensions.
    fn dimensions(&self) -> (u32, u32) {
        let meta = self.metadata();
        (meta.width, meta.height)
    }

    /// Returns true if the decoder has reached end of stream.
    ///
    /// This is more reliable than counting None results from decode_next(),
    /// as it reflects the actual decoder state rather than buffering timeouts.
    fn is_eof(&self) -> bool {
        false // Default - most decoders signal EOF via decode_next returning None
    }

    /// Returns true if the decoder is in seeking/warmup state.
    ///
    /// During warmup after a seek, the decoder buffers frames before
    /// allowing consumption to prevent queue starvation. The frame queue
    /// should pause consumption while this returns true.
    fn is_seeking(&self) -> bool {
        false // Default - no warmup needed
    }

    /// Returns the current buffering percentage (0-100).
    ///
    /// For network streams, this indicates how much data has been buffered.
    /// Returns 100 for local files or when buffering state is unknown.
    fn buffering_percent(&self) -> i32 {
        100 // Default - assume fully buffered
    }

    /// Returns the current hardware acceleration type.
    fn hw_accel_type(&self) -> HwAccelType {
        HwAccelType::None
    }

    /// Returns the current playback position from the native player.
    ///
    /// For decoders with integrated playback (macOS AVPlayer, Android ExoPlayer),
    /// this returns the actual playback position which can be used for A/V sync.
    /// For decoders without integrated playback (FFmpeg), returns None.
    ///
    /// This is distinct from frame PTS - it represents where the native player
    /// thinks playback is, accounting for audio buffer latency.
    fn current_time(&self) -> Option<Duration> {
        None // Default - decoder doesn't track playback position
    }
}

/// Implementation for boxed trait objects to enable decoder fallback patterns.
impl VideoDecoderBackend for Box<dyn VideoDecoderBackend + Send> {
    fn open(_url: &str) -> Result<Self, VideoError>
    where
        Self: Sized,
    {
        // Not supported on boxed trait objects - use concrete types for open
        Err(VideoError::DecoderInit(
            "Cannot call open() on boxed trait object".to_string(),
        ))
    }

    fn decode_next(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        (**self).decode_next()
    }

    fn seek(&mut self, position: Duration) -> Result<(), VideoError> {
        (**self).seek(position)
    }

    fn metadata(&self) -> &VideoMetadata {
        (**self).metadata()
    }

    fn pause(&mut self) -> Result<(), VideoError> {
        (**self).pause()
    }

    fn resume(&mut self) -> Result<(), VideoError> {
        (**self).resume()
    }

    fn set_muted(&mut self, muted: bool) -> Result<(), VideoError> {
        (**self).set_muted(muted)
    }

    fn set_volume(&mut self, volume: f32) -> Result<(), VideoError> {
        (**self).set_volume(volume)
    }

    fn duration(&self) -> Option<Duration> {
        (**self).duration()
    }

    fn dimensions(&self) -> (u32, u32) {
        (**self).dimensions()
    }

    fn is_eof(&self) -> bool {
        (**self).is_eof()
    }

    fn is_seeking(&self) -> bool {
        (**self).is_seeking()
    }

    fn buffering_percent(&self) -> i32 {
        (**self).buffering_percent()
    }

    fn handles_audio_internally(&self) -> bool {
        (**self).handles_audio_internally()
    }

    fn audio_handle(&self) -> Option<crate::audio::AudioHandle> {
        (**self).audio_handle()
    }

    fn hw_accel_type(&self) -> HwAccelType {
        (**self).hw_accel_type()
    }

    fn current_time(&self) -> Option<Duration> {
        (**self).current_time()
    }

    fn android_player_id(&self) -> u64 {
        (**self).android_player_id()
    }
}
