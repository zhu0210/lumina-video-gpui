//! Zero-copy GPU texture import for video frames.
//!
//! This module enables importing video decoder output directly into wgpu textures
//! without CPU memory copies, using platform-specific GPU interop:
//!
//! - **macOS**: IOSurface → Metal texture → wgpu
//! - **Linux**: DMABuf → Vulkan image → wgpu
//! - **Android**: AHardwareBuffer → Vulkan image → wgpu
//! - **Windows**: D3D11 shared handle → D3D12 → wgpu
//!
//! # Current Implementation Status
//!
//! - **macOS**: Fully implemented. IOSurface import to Metal works for supported formats (BGRA, NV12).
//! - **Linux**: Partial. Only single-plane formats (RGBA/BGRA) work. NV12 falls back
//!   to CPU copy because `LinuxGpuSurface` only stores single-plane metadata (see
//!   lumina-video-4m8 for the multi-plane limitation).
//! - **Android**: Rust-side Vulkan import is ready. Waiting for Java/Kotlin ExoPlayer
//!   integration to expose AHardwareBuffer via ImageReader (see lumina-video-6dn).
//! - **Windows**: Implemented but missing proper D3D11→D3D12 fence synchronization.
//!   May cause visual artifacts or crashes under heavy load without sync primitives.
//!
//! # Platform Support
//!
//! | Platform | Backend | Import Method | Status |
//! |----------|---------|---------------|--------|
//! | macOS | Metal | IOSurface | Supported |
//! | Linux | Vulkan | DMABuf (VA-API, V4L2) | Partial (single-plane only) |
//! | Android | Vulkan | AHardwareBuffer (MediaCodec) | Rust ready, Java pending |
//! | Windows | D3D12 | D3D11 Shared Handle | Partial (missing fence sync) |
//! | iOS | Metal | IOSurface | Shared with macOS (Metal/IOSurface) |
//! | Web/WASM | WebGPU | N/A | Not supported (no external memory) |
//!
//! # Feature Flag
//!
//! The `zero-copy` feature is enabled by default on supported platforms
//! (macOS, Linux, Android, Windows). No additional configuration is needed:
//!
//! ```toml
//! [dependencies]
//! lumina-video-gpui = { version = "..." }
//! ```
//!
//! To disable zero-copy (e.g., for unsupported platforms like WASM),
//! use `default-features = false`:
//!
//! ```toml
//! [dependencies]
//! lumina-video-gpui = { version = "...", default-features = false }
//! ```
//!
//! **Note:** On unsupported platforms (WASM, etc.), compilation will fail
//! with a clear error message about platform support if zero-copy is enabled.
//!
//! # How It Works
//!
//! wgpu-hal v24+ provides `Device::texture_from_raw()` on each backend:
//!
//! 1. Get the raw HAL device via `device.as_hal::<Metal>()`
//! 2. Create a native texture from the external resource
//! 3. Wrap with `Device::texture_from_raw()` to get a HAL Texture
//! 4. Call `device.create_texture_from_hal()` to get a wgpu::Texture
//!
//! # References
//!
//! - wgpu #2320: Texture memory import API
//! - wgpu #4067: Proposal for API interoperability
//! - wgpu PR #6161: D3D11 shared handle import (merged)

use std::fmt;

// =============================================================================
// Platform Support Verification
// =============================================================================
//
// The zero-copy feature requires platform-specific GPU interop APIs that are
// only available on certain platforms. This section provides compile-time
// verification with clear error messages for unsupported configurations.

/// Compile-time check for WebAssembly - not supported
#[cfg(target_family = "wasm")]
compile_error!(
    "The `zero-copy` feature is not supported on WebAssembly/WASM. \
     WebGPU does not provide external memory import APIs, so zero-copy \
     texture import is not possible. Please disable the `zero-copy` feature \
     when targeting WASM."
);

/// Compile-time check for other unsupported Unix platforms (FreeBSD, OpenBSD, etc.)
#[cfg(all(
    target_family = "unix",
    not(any(
        target_os = "macos",
        target_os = "linux",
        target_os = "android",
        target_os = "ios"
    ))
))]
compile_error!(
    "Zero-copy video is not supported on this Unix platform. \
     Currently supported platforms: macOS (Metal/IOSurface), Linux (Vulkan/DMABuf), \
     Android (Vulkan/AHardwareBuffer). Please contribute an implementation for your platform."
);

/// Returns whether zero-copy import is supported on the current platform.
///
/// This is a compile-time constant that can be used for conditional logic.
/// On unsupported platforms with the `zero-copy` feature enabled, compilation
/// will fail with a clear error message before this function is ever called.
///
/// # Supported Platforms
///
/// - macOS/iOS: Metal backend with IOSurface
/// - Linux: Vulkan backend with DMABuf
/// - Android: Vulkan backend with AHardwareBuffer
/// - Windows: D3D12 backend with D3D11 shared handles
pub const fn is_platform_supported() -> bool {
    cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android",
        target_os = "windows"
    ))
}

/// Error type for zero-copy texture import operations.
///
/// This error is returned when zero-copy texture import fails. Each variant
/// provides specific information about what went wrong.
///
/// # Example
///
/// ```ignore
/// use crate::zero_copy::ZeroCopyError;
///
/// fn handle_import_error(err: ZeroCopyError) {
///     match err {
///         ZeroCopyError::NotAvailable(reason) => println!("Fallback needed: {}", reason),
///         ZeroCopyError::TextureCreationFailed(msg) => println!("GPU error: {}", msg),
///         ZeroCopyError::UnsupportedBackend(msg) => println!("Wrong backend: {}", msg),
///         ZeroCopyError::HalAccessFailed(msg) => println!("HAL error: {}", msg),
///         ZeroCopyError::InvalidResource(msg) => println!("Invalid resource: {}", msg),
///         ZeroCopyError::FormatMismatch(msg) => println!("Format error: {}", msg),
///     }
/// }
/// ```
#[derive(Debug)]

pub enum ZeroCopyError {
    /// The wgpu device doesn't support the required backend
    UnsupportedBackend(String),
    /// Failed to access the HAL device
    HalAccessFailed(String),
    /// The external resource is invalid or incompatible
    InvalidResource(String),
    /// Texture creation failed
    TextureCreationFailed(String),
    /// Feature not available on this platform
    NotAvailable(String),
    /// Resource format doesn't match expected format
    FormatMismatch(String),
    /// Import operation failed
    ImportFailed(String),
    /// GPU device is busy (lock contention) - caller should retry or use cached frame
    DeviceBusy,
}

impl fmt::Display for ZeroCopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZeroCopyError::UnsupportedBackend(msg) => write!(f, "Unsupported backend: {}", msg),
            ZeroCopyError::HalAccessFailed(msg) => write!(f, "HAL access failed: {}", msg),
            ZeroCopyError::InvalidResource(msg) => write!(f, "Invalid resource: {}", msg),
            ZeroCopyError::TextureCreationFailed(msg) => {
                write!(f, "Texture creation failed: {}", msg)
            }
            ZeroCopyError::NotAvailable(msg) => write!(f, "Not available: {}", msg),
            ZeroCopyError::FormatMismatch(msg) => write!(f, "Format mismatch: {}", msg),
            ZeroCopyError::ImportFailed(msg) => write!(f, "Import failed: {}", msg),
            ZeroCopyError::DeviceBusy => write!(f, "GPU device busy (lock contention)"),
        }
    }
}

impl std::error::Error for ZeroCopyError {}

/// Statistics for zero-copy operations.
///
/// Tracks the number of frames processed via zero-copy vs fallback paths,
/// useful for performance monitoring and debugging.
///
/// # Example
///
/// ```ignore
/// use crate::zero_copy::ZeroCopyStats;
///
/// let stats = ZeroCopyStats {
///     total_frames: 1000,
///     zero_copy_frames: 985,
///     fallback_frames: 15,
/// };
///
/// println!("Zero-copy efficiency: {:.1}%", stats.zero_copy_percentage());
/// // Output: "Zero-copy efficiency: 98.5%"
/// ```
#[derive(Debug, Clone, Default)]

pub struct ZeroCopyStats {
    /// Total frames processed
    pub total_frames: u64,
    /// Frames imported via zero-copy path
    pub zero_copy_frames: u64,
    /// Frames that fell back to CPU copy
    pub fallback_frames: u64,
}

impl ZeroCopyStats {
    /// Returns the percentage of frames imported via zero-copy path (0.0 - 100.0).
    ///
    /// This is useful for monitoring zero-copy effectiveness and identifying
    /// when fallback paths are being used. A low percentage may indicate:
    /// - Unsupported pixel format from the decoder
    /// - Missing GPU extensions (e.g., DMABuf import on Linux)
    /// - Driver limitations
    ///
    /// Returns `0.0` if no frames have been processed yet.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let stats = decoder.zero_copy_stats();
    /// println!("Zero-copy usage: {:.1}%", stats.zero_copy_percentage());
    /// // Output: "Zero-copy usage: 98.5%"
    /// ```
    pub fn zero_copy_percentage(&self) -> f64 {
        if self.total_frames == 0 {
            return 0.0;
        }
        (self.zero_copy_frames as f64 / self.total_frames as f64) * 100.0
    }
}

// =============================================================================
// macOS: IOSurface → Metal → wgpu
// =============================================================================

/// macOS-specific zero-copy import via IOSurface and Metal.
///
/// This module provides functions to import IOSurface-backed video frames
/// directly into wgpu textures without CPU memory copies.
///
/// # Requirements
///
/// - macOS 10.11+ (for Metal support)
/// - wgpu using Metal backend
/// - Video frames backed by IOSurface (e.g., from VideoToolbox)
///
/// # Example
///
/// ```ignore
/// use crate::zero_copy::macos;
///
/// // Check if Metal backend is available
/// if macos::is_metal_backend(&device) {
///     // Import IOSurface from VideoToolbox decoder
///     let texture = unsafe {
///         macos::import_iosurface(&device, io_surface, width, height, format)?
///     };
/// }
/// ```
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod macos {
    use super::ZeroCopyError;
    use objc2::{msg_send, rc::Retained, runtime::ProtocolObject};
    use objc2_metal::{
        MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
        MTLTextureType, MTLTextureUsage,
    };
    use std::ffi::c_void;
    use tracing::{debug, warn};

    /// Opaque handle to an IOSurface (from CoreVideo).
    /// This is obtained from CVPixelBufferGetIOSurface().
    pub type IOSurfaceRef = *mut c_void;

    /// Checks if the current wgpu device supports Metal backend.
    ///
    /// This verifies that the HAL APIs are accessible for zero-copy import.
    pub fn is_metal_backend(device: &wgpu::Device) -> bool {
        // SAFETY: `device` is a live wgpu device borrowed for this call; the
        // HAL query only inspects its backend and does not retain raw handles.
        unsafe { device.as_hal::<wgpu::hal::api::Metal>().is_some() }
    }

    /// Gets information about the Metal device for diagnostics.
    ///
    /// Returns the device name if Metal backend is available, None otherwise.
    pub fn get_metal_device_info(device: &wgpu::Device) -> Option<String> {
        // SAFETY: `device` is live for the duration of the callback, and the
        // HAL device exposes its raw Metal device only while that borrow lasts.
        unsafe {
            device
                .as_hal::<wgpu::hal::api::Metal>()
                .map(|d| d.raw_device().name().to_string())
        }
    }

    /// Imports an IOSurface into wgpu as a texture (zero-copy).
    ///
    /// This function creates a wgpu::Texture that directly references the IOSurface's
    /// GPU memory, enabling zero-copy video frame display.
    ///
    /// # Safety
    ///
    /// - `io_surface` must be a valid IOSurfaceRef obtained from CVPixelBufferGetIOSurface()
    /// - The IOSurface must remain valid for the lifetime of the returned texture
    /// - The caller must retain the CVPixelBuffer that owns the IOSurface
    ///
    /// # Arguments
    ///
    /// * `device` - The wgpu Device (must be Metal backend)
    /// * `io_surface` - A valid IOSurfaceRef
    /// * `width` - Texture width in pixels
    /// * `height` - Texture height in pixels
    /// * `format` - The wgpu texture format (should match IOSurface pixel format)
    ///
    /// # Returns
    ///
    /// A wgpu::Texture that references the IOSurface memory directly.
    pub unsafe fn import_iosurface(
        device: &wgpu::Device,
        io_surface: IOSurfaceRef,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<wgpu::Texture, ZeroCopyError> {
        // SAFETY: the caller supplies the documented IOSurface lifetime contract.
        unsafe {
            import_iosurface_with_drop_callback(device, io_surface, width, height, format, None)
        }
    }

    /// Retains an owned producer lease until wgpu's GPU-tracked texture destruction.
    /// The caller must provide the same IOSurface invariants as `import_iosurface`.
    pub(crate) unsafe fn import_iosurface_with_drop_callback(
        device: &wgpu::Device,
        io_surface: IOSurfaceRef,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        drop_callback: Option<wgpu::hal::DropCallback>,
    ) -> Result<wgpu::Texture, ZeroCopyError> {
        if io_surface.is_null() {
            return Err(ZeroCopyError::InvalidResource(
                "IOSurface is null".to_string(),
            ));
        }

        // Access the Metal HAL device
        let hal_device = device.as_hal::<wgpu::hal::api::Metal>().ok_or_else(|| {
            ZeroCopyError::HalAccessFailed("wgpu not using Metal backend".to_string())
        })?;

        // Get the raw Metal device
        let metal_device = hal_device.raw_device();

        debug!(
            "Creating Metal texture from IOSurface ({}x{} {:?}) on {}",
            width,
            height,
            format,
            metal_device.name()
        );

        // Create Metal texture descriptor
        let descriptor = MTLTextureDescriptor::new();
        descriptor.setTextureType(MTLTextureType::Type2D);
        let metal_format = wgpu_format_to_metal(format)?;
        descriptor.setPixelFormat(metal_format);
        descriptor.setWidth(width as usize);
        descriptor.setHeight(height as usize);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        #[cfg(target_os = "macos")]
        descriptor.setStorageMode(MTLStorageMode::Managed);
        #[cfg(target_os = "ios")]
        descriptor.setStorageMode(MTLStorageMode::Shared);

        // SAFETY: the caller provides a live IOSurface and matching extent/format.
        // The `new` selector returns an owned Metal texture; objc2 retains that
        // ownership in the exact ProtocolObject type expected by wgpu 30.
        let metal_texture: Option<Retained<ProtocolObject<dyn MTLTexture>>> = unsafe {
            msg_send![&**metal_device, newTextureWithDescriptor: &*descriptor,
                iosurface: io_surface, plane: 0usize]
        };
        let metal_texture = metal_texture.ok_or_else(|| {
            ZeroCopyError::TextureCreationFailed(
                "Metal failed to create texture from IOSurface".into(),
            )
        })?;

        // Wrap as wgpu_hal::metal::Texture
        let hal_texture = wgpu::hal::metal::Device::texture_from_raw(
            metal_texture,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width,
                height,
                depth: 1,
            },
            drop_callback,
        );

        // Create wgpu texture descriptor
        let texture_desc = wgpu::TextureDescriptor {
            label: Some("zero-copy IOSurface texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };

        // Wrap the HAL texture as a wgpu::Texture
        let wgpu_texture = device.create_texture_from_hal::<wgpu::hal::api::Metal>(
            hal_texture,
            &texture_desc,
            wgpu::TextureUses::RESOURCE,
            // Native producer contents are already defined; do not clear imported video.
            true,
        );

        Ok(wgpu_texture)
    }

    /// Converts wgpu TextureFormat to Metal MTLPixelFormat.
    ///
    /// Returns an error for unsupported formats instead of silently defaulting.
    pub fn wgpu_format_to_metal(
        format: wgpu::TextureFormat,
    ) -> Result<MTLPixelFormat, ZeroCopyError> {
        match format {
            wgpu::TextureFormat::Bgra8Unorm => Ok(MTLPixelFormat::BGRA8Unorm),
            wgpu::TextureFormat::Rgba8Unorm => Ok(MTLPixelFormat::RGBA8Unorm),
            wgpu::TextureFormat::R8Unorm => Ok(MTLPixelFormat::R8Unorm),
            wgpu::TextureFormat::Rg8Unorm => Ok(MTLPixelFormat::RG8Unorm),
            wgpu::TextureFormat::Bgra8UnormSrgb => Ok(MTLPixelFormat::BGRA8Unorm_sRGB),
            wgpu::TextureFormat::Rgba8UnormSrgb => Ok(MTLPixelFormat::RGBA8Unorm_sRGB),
            _ => {
                warn!("Unsupported texture format {:?}; refusing import", format);
                Err(ZeroCopyError::InvalidResource(format!(
                    "Unsupported texture format {:?}",
                    format
                )))
            }
        }
    }
}

// =============================================================================
// Linux: DMABuf → Vulkan → wgpu
// =============================================================================

/// Linux-specific zero-copy import via DMABuf and Vulkan.
///
/// This module provides functions to import DMABuf file descriptors
/// directly into wgpu textures without CPU memory copies.
///
/// # Requirements
///
/// - Linux kernel with DMABuf support
/// - wgpu using Vulkan backend
/// - Vulkan extensions: `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf`
/// - Video frames as DMABuf fd (e.g., from VA-API, V4L2, GStreamer)
///
/// # Supported DRM Modifiers
///
/// The module supports both linear and tiled memory layouts via DRM format modifiers:
/// - `DRM_FORMAT_MOD_LINEAR` - Universal, works everywhere
/// - Intel: X-tiled, Y-tiled, Tile4
/// - AMD: GFX9 64KB tiled
/// - NVIDIA: Block linear
///
/// # Example
///
/// ```ignore
/// use crate::zero_copy::linux::{self, DmaBufHandle, drm_modifiers};
///
/// let dmabuf = DmaBufHandle {
///     fd: va_surface_fd,
///     size: width * height * 4,
///     offset: 0,
///     stride: width * 4,
///     modifier: drm_modifiers::DRM_FORMAT_MOD_LINEAR,
/// };
///
/// let texture = unsafe {
///     linux::import_dmabuf(&device, dmabuf, width, height, format)?
/// };
/// ```
#[cfg(target_os = "linux")]
pub mod linux {
    use super::ZeroCopyError;
    use ash::vk;
    use std::ffi::CStr;
    use std::os::fd::RawFd;
    use tracing::{debug, info, warn};

    /// Per-plane DMABuf metadata for multi-plane import.
    ///
    /// Multi-plane formats like NV12 require separate metadata for each plane.
    /// This struct mirrors `DmaBufPlane` from video.rs for use in the import API.
    #[derive(Debug, Clone, Copy)]
    pub struct DmaBufPlaneHandle {
        /// The file descriptor for this plane.
        /// Multiple planes may share the same fd (with different offsets) or have separate fds.
        pub fd: RawFd,
        /// Offset within the buffer where this plane starts (bytes)
        pub offset: u64,
        /// Row pitch/stride in bytes for this plane
        pub stride: u32,
        /// Size of this plane's data in bytes (may be 0 if unknown)
        pub size: u64,
    }

    /// DMABuf handle for zero-copy import with multi-plane support.
    ///
    /// DMABuf (DMA Buffer Sharing) is a Linux kernel mechanism for sharing
    /// memory buffers between devices. Video decoders like VA-API and V4L2
    /// export decoded frames as DMABuf file descriptors.
    ///
    /// # Multi-Plane Support (lumina-video-s0e)
    ///
    /// This struct now supports multi-plane formats like NV12 and YUV420p:
    /// - **Single-plane (RGBA/BGRA)**: Use `DmaBufHandle::single_plane()`
    /// - **Multi-plane (NV12)**: Use `DmaBufHandle::new()` with plane metadata
    ///
    /// Note: Actual Vulkan multi-plane import requires VkSamplerYcbcrConversion
    /// which is not yet implemented. The current import_dmabuf() only handles
    /// single-plane formats. Multi-plane metadata is preserved for future use.
    #[derive(Debug, Clone)]
    pub struct DmaBufHandle {
        /// Per-plane metadata. For single-plane formats, this has length 1.
        pub planes: Vec<DmaBufPlaneHandle>,
        /// DRM format modifier (e.g., I915_FORMAT_MOD_Y_TILED)
        /// Use DRM_FORMAT_MOD_LINEAR (0) for linear layout
        pub modifier: u64,
    }

    impl DmaBufHandle {
        /// Creates a multi-plane DmaBufHandle.
        pub fn new(planes: Vec<DmaBufPlaneHandle>, modifier: u64) -> Self {
            Self { planes, modifier }
        }

        /// Creates a single-plane DmaBufHandle (convenience for RGBA/BGRA).
        pub fn single_plane(fd: RawFd, size: u64, offset: u64, stride: u32, modifier: u64) -> Self {
            Self {
                planes: vec![DmaBufPlaneHandle {
                    fd,
                    offset,
                    stride,
                    size,
                }],
                modifier,
            }
        }

        /// Returns the primary file descriptor (plane 0).
        pub fn fd(&self) -> RawFd {
            self.planes.first().map(|p| p.fd).unwrap_or(-1)
        }

        /// Returns the primary plane's size.
        pub fn size(&self) -> u64 {
            self.planes.first().map(|p| p.size).unwrap_or(0)
        }

        /// Returns the primary plane's offset.
        pub fn offset(&self) -> u64 {
            self.planes.first().map(|p| p.offset).unwrap_or(0)
        }

        /// Returns the primary plane's stride.
        pub fn stride(&self) -> u32 {
            self.planes.first().map(|p| p.stride).unwrap_or(0)
        }

        /// Returns the number of planes.
        pub fn num_planes(&self) -> usize {
            self.planes.len()
        }

        /// Returns true if this is a multi-plane format.
        pub fn is_multi_plane(&self) -> bool {
            self.planes.len() > 1
        }
    }

    /// Extension names required for DMABuf import
    pub(crate) const EXT_EXTERNAL_MEMORY_DMA_BUF: &CStr = c"VK_EXT_external_memory_dma_buf";
    pub(crate) const KHR_EXTERNAL_MEMORY_FD: &CStr = c"VK_KHR_external_memory_fd";
    pub(crate) const EXT_IMAGE_DRM_FORMAT_MODIFIER: &CStr = c"VK_EXT_image_drm_format_modifier";

    /// Checks if the wgpu device is using the Vulkan backend.
    ///
    /// DMABuf zero-copy import requires Vulkan. This function checks whether
    /// the wgpu device was created with the Vulkan backend, which is necessary
    /// for using `VK_EXT_external_memory_dma_buf` and related extensions.
    ///
    /// Returns `false` if using OpenGL, software rendering, or another backend.
    pub fn is_vulkan_backend(device: &wgpu::Device) -> bool {
        // SAFETY: `device` is a live wgpu device borrowed for this call; the
        // HAL query only inspects its backend and does not retain raw handles.
        unsafe { device.as_hal::<wgpu::hal::api::Vulkan>().is_some() }
    }

    /// Gets information about the Vulkan device for diagnostics.
    ///
    /// Returns the device name if Vulkan backend is available, None otherwise.
    pub fn get_vulkan_device_info(device: &wgpu::Device) -> Option<String> {
        // SAFETY: `device` and the HAL instance/physical device are live for
        // the callback; Vulkan guarantees the fixed device-name field is NUL
        // terminated before it is viewed as a C string.
        unsafe {
            device.as_hal::<wgpu::hal::api::Vulkan>().map(|d| {
                let instance = d.shared_instance();
                let raw_instance = instance.raw_instance();
                let physical_device = d.raw_physical_device();

                let properties = raw_instance.get_physical_device_properties(physical_device);
                let device_name = CStr::from_ptr(properties.device_name.as_ptr())
                    .to_string_lossy()
                    .into_owned();
                format!(
                    "{} (Vulkan {}.{}.{})",
                    device_name,
                    vk::api_version_major(properties.api_version),
                    vk::api_version_minor(properties.api_version),
                    vk::api_version_patch(properties.api_version)
                )
            })
        }
    }

    /// Checks if Vulkan DMABuf import extensions are available.
    ///
    /// This checks for VK_EXT_external_memory_dma_buf extension support.
    /// The extension allows importing Linux DMABuf file descriptors as
    /// Vulkan external memory.
    ///
    /// # Required Extensions
    ///
    /// - VK_KHR_external_memory (Vulkan 1.1 core)
    /// - VK_KHR_external_memory_fd
    /// - VK_EXT_external_memory_dma_buf
    /// - VK_EXT_image_drm_format_modifier (for tiled formats)
    pub fn is_dmabuf_import_available(device: &wgpu::Device) -> bool {
        // SAFETY: `device` is live for the callback, and the HAL device's
        // enabled-extension set is only borrowed while that callback runs.
        unsafe {
            device
                .as_hal::<wgpu::hal::api::Vulkan>()
                .is_some_and(|hal_device| {
                    let extensions = hal_device.enabled_device_extensions();
                    let has_dma_buf = extensions.contains(&EXT_EXTERNAL_MEMORY_DMA_BUF);
                    let has_fd = extensions.contains(&KHR_EXTERNAL_MEMORY_FD);
                    has_dma_buf && has_fd
                })
        }
    }

    /// Finds a suitable memory type index for the given requirements.
    pub(crate) fn find_memory_type_index(
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        type_bits_req: u32,
        flags_req: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        let mem_properties =
            // SAFETY: `physical_device` belongs to `instance` and remains valid
            // for this raw Vulkan query.
            unsafe { instance.get_physical_device_memory_properties(physical_device) };

        for i in 0..mem_properties.memory_type_count {
            let type_bits = 1 << i;
            let is_required_type = type_bits_req & type_bits != 0;
            // Use safe .get() to avoid potential panic if memory_type_count mismatches array
            let has_required_flags = mem_properties
                .memory_types
                .get(i as usize)
                .map(|mt| mt.property_flags & flags_req == flags_req)
                .unwrap_or(false);

            if is_required_type && has_required_flags {
                return Some(i);
            }
        }
        None
    }

    /// Executes a one-shot command buffer to transition an image layout and perform
    /// queue family ownership transfer for externally imported memory.
    ///
    /// This is required for external memory imports (DMABuf) to ensure proper
    /// synchronization and layout before the image can be sampled in shaders.
    ///
    /// # Safety
    ///
    /// - `vk_device` must be a valid Vulkan device handle
    /// - `vk_queue` must be a valid queue from the same device
    /// - `vk_image` must be a valid image created on this device
    /// - `queue_family_index` must be the queue family index of `vk_queue`
    unsafe fn transition_image_layout_external(
        vk_device: &ash::Device,
        vk_queue: vk::Queue,
        queue_family_index: u32,
        vk_image: vk::Image,
    ) -> Result<(), ZeroCopyError> {
        // Create a one-shot command pool
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family_index)
            .flags(vk::CommandPoolCreateFlags::TRANSIENT);

        let command_pool = vk_device
            .create_command_pool(&pool_info, None)
            .map_err(|e| {
                ZeroCopyError::TextureCreationFailed(format!(
                    "Failed to create command pool for layout transition: {:?}",
                    e
                ))
            })?;

        // Allocate a command buffer
        let alloc_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);

        let command_buffers = vk_device
            .allocate_command_buffers(&alloc_info)
            .map_err(|e| {
                vk_device.destroy_command_pool(command_pool, None);
                ZeroCopyError::TextureCreationFailed(format!(
                    "Failed to allocate command buffer for layout transition: {:?}",
                    e
                ))
            })?;
        let cmd_buf = *command_buffers.first().ok_or_else(|| {
            vk_device.destroy_command_pool(command_pool, None);
            ZeroCopyError::TextureCreationFailed(
                "No command buffer returned from allocation".to_string(),
            )
        })?;

        // Begin the command buffer
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);

        vk_device
            .begin_command_buffer(cmd_buf, &begin_info)
            .map_err(|e| {
                vk_device.destroy_command_pool(command_pool, None);
                ZeroCopyError::TextureCreationFailed(format!(
                    "Failed to begin command buffer: {:?}",
                    e
                ))
            })?;

        // Record the image memory barrier for layout transition and queue ownership transfer
        // VK_QUEUE_FAMILY_EXTERNAL is defined as (~1U) = 0xFFFFFFFE
        const VK_QUEUE_FAMILY_EXTERNAL: u32 = !1u32;

        let image_barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::MEMORY_READ) // External memory may have been read
            .dst_access_mask(vk::AccessFlags::SHADER_READ) // Will be sampled in fragment shader
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(VK_QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(queue_family_index)
            .image(vk_image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .base_mip_level(0)
                    .level_count(1)
                    .base_array_layer(0)
                    .layer_count(1),
            );

        vk_device.cmd_pipeline_barrier(
            cmd_buf,
            vk::PipelineStageFlags::TOP_OF_PIPE, // No prior stage within this queue
            vk::PipelineStageFlags::FRAGMENT_SHADER, // Will be used in fragment shader
            vk::DependencyFlags::empty(),
            &[], // No memory barriers
            &[], // No buffer barriers
            &[image_barrier],
        );

        // End and submit the command buffer
        vk_device.end_command_buffer(cmd_buf).map_err(|e| {
            vk_device.destroy_command_pool(command_pool, None);
            ZeroCopyError::TextureCreationFailed(format!("Failed to end command buffer: {:?}", e))
        })?;

        let cmd_bufs = [cmd_buf];
        let submit_info = vk::SubmitInfo::default().command_buffers(&cmd_bufs);

        // Create a fence to wait for completion
        let fence_info = vk::FenceCreateInfo::default();
        let fence = vk_device.create_fence(&fence_info, None).map_err(|e| {
            vk_device.destroy_command_pool(command_pool, None);
            ZeroCopyError::TextureCreationFailed(format!(
                "Failed to create fence for layout transition: {:?}",
                e
            ))
        })?;

        vk_device
            .queue_submit(vk_queue, &[submit_info], fence)
            .map_err(|e| {
                vk_device.destroy_fence(fence, None);
                vk_device.destroy_command_pool(command_pool, None);
                ZeroCopyError::TextureCreationFailed(format!(
                    "Failed to submit layout transition command: {:?}",
                    e
                ))
            })?;

        // Wait for the command to complete (with 1 second timeout)
        vk_device
            .wait_for_fences(&[fence], true, 1_000_000_000)
            .map_err(|e| {
                vk_device.destroy_fence(fence, None);
                vk_device.destroy_command_pool(command_pool, None);
                ZeroCopyError::TextureCreationFailed(format!(
                    "Timeout waiting for layout transition: {:?}",
                    e
                ))
            })?;

        // Cleanup
        vk_device.destroy_fence(fence, None);
        vk_device.destroy_command_pool(command_pool, None);

        debug!("Successfully transitioned image layout to SHADER_READ_ONLY_OPTIMAL with queue ownership transfer");
        Ok(())
    }

    /// Imports a DMABuf file descriptor into wgpu as a texture (zero-copy).
    ///
    /// This function creates a wgpu::Texture that directly references the DMABuf's
    /// GPU memory, enabling zero-copy video frame display from hardware decoders
    /// like VA-API, V4L2, or GStreamer with dmabuf export.
    ///
    /// # Implementation Notes
    ///
    /// The Vulkan import process:
    /// 1. Create VkImage with VkExternalMemoryImageCreateInfo specifying DMA_BUF handle type
    /// 2. Query memory requirements for the image
    /// 3. Import external memory via VkImportMemoryFdInfoKHR with the DMABuf fd
    /// 4. Bind the imported memory to the image
    /// 5. Wrap with wgpu-hal's texture_from_raw and create_texture_from_hal
    ///
    /// # Safety
    ///
    /// - `dmabuf` must contain a valid DMABuf file descriptor
    /// - The DMABuf must remain valid for the lifetime of the returned texture
    /// - The caller must ensure the DMABuf fd is not closed while the texture is in use
    /// - The fd ownership IS transferred to Vulkan - do NOT close it after this call
    /// - The DMABuf content must match the specified width, height, and format
    ///
    /// # Required Vulkan Extensions
    ///
    /// - VK_KHR_external_memory_fd
    /// - VK_EXT_external_memory_dma_buf
    /// - VK_EXT_image_drm_format_modifier (for non-linear formats)
    ///
    /// # Arguments
    ///
    /// * `device` - The wgpu Device (must be Vulkan backend)
    /// * `dmabuf` - DMABuf handle containing fd, size, offset, stride, and modifier
    /// * `width` - Texture width in pixels
    /// * `height` - Texture height in pixels
    /// * `format` - The wgpu texture format (should match DMABuf pixel format)
    ///
    /// # Returns
    ///
    /// A wgpu::Texture that references the DMABuf memory directly.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // From VA-API: vaSyncSurface + vaExportSurfaceHandle
    /// // From GStreamer: gst_buffer_get_dmabuf_memory
    /// let dmabuf = DmaBufHandle::single_plane(
    ///     va_surface_fd,
    ///     width * height * 4,  // size
    ///     0,                   // offset
    ///     width * 4,           // stride
    ///     drm_modifiers::DRM_FORMAT_MOD_LINEAR,
    /// );
    ///
    /// let texture = unsafe {
    ///     import_dmabuf(&device, dmabuf, 1920, 1080, wgpu::TextureFormat::Bgra8Unorm)?
    /// };
    /// ```
    ///
    /// # Multi-Plane Formats (Not Yet Supported)
    ///
    /// Multi-plane formats like NV12 require VkSamplerYcbcrConversion which is not
    /// yet implemented. For now, multi-plane handles will be rejected with an error.
    /// The infrastructure is in place for future implementation.
    pub unsafe fn import_dmabuf(
        device: &wgpu::Device,
        dmabuf: DmaBufHandle,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Result<wgpu::Texture, ZeroCopyError> {
        // Validate the handle
        if dmabuf.fd() < 0 {
            return Err(ZeroCopyError::InvalidResource(
                "DMABuf fd is invalid (negative)".to_string(),
            ));
        }

        // Multi-plane formats require VkSamplerYcbcrConversion - not yet implemented
        if dmabuf.is_multi_plane() {
            return Err(ZeroCopyError::NotAvailable(
                "Multi-plane DMABuf import not yet implemented. \
                 Requires VkSamplerYcbcrConversion for NV12/YUV420p formats."
                    .to_string(),
            ));
        }

        // Access the Vulkan HAL device
        let hal_device = device.as_hal::<wgpu::hal::api::Vulkan>().ok_or_else(|| {
            ZeroCopyError::HalAccessFailed("wgpu not using Vulkan backend".to_string())
        })?;

        // Check for required extensions
        let extensions = hal_device.enabled_device_extensions();
        let has_dma_buf = extensions.contains(&EXT_EXTERNAL_MEMORY_DMA_BUF);
        let has_fd = extensions.contains(&KHR_EXTERNAL_MEMORY_FD);
        let has_drm_modifier = extensions.contains(&EXT_IMAGE_DRM_FORMAT_MODIFIER);

        if !has_dma_buf || !has_fd {
            return Err(ZeroCopyError::NotAvailable(
                "VK_EXT_external_memory_dma_buf or VK_KHR_external_memory_fd not available"
                    .to_string(),
            ));
        }

        // Use DRM modifier extension when:
        // 1. Non-linear modifier (always needs the extension), OR
        // 2. Non-zero offset (linear tiling without extension can't specify offsets)
        //
        // For single-FD multi-plane layouts, each plane has a different offset.
        // Without explicit plane layout, Vulkan binds memory at offset 0, causing
        // planes to read wrong data (e.g., UV plane reading Y data → color corruption).
        let has_nonzero_offset = dmabuf.offset() != 0;
        let use_drm_modifier = has_drm_modifier
            && (dmabuf.modifier != drm_modifiers::DRM_FORMAT_MOD_LINEAR || has_nonzero_offset);

        debug!(
                        "Importing DMABuf fd={} ({}x{} {:?}, modifier=0x{:x}, offset={}, use_drm={}) into Vulkan",
                        dmabuf.fd(), width, height, format, dmabuf.modifier, dmabuf.offset(), use_drm_modifier
                    );

        let vk_device = hal_device.raw_device();
        let physical_device = hal_device.raw_physical_device();
        let instance = hal_device.shared_instance().raw_instance();
        let vk_queue = hal_device.raw_queue();
        let queue_family_index = hal_device.queue_family_index();
        let vk_format = wgpu_format_to_vulkan(format)?;

        // Step 1: Create VkImage with external memory info
        let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        // For tiled formats, specify the DRM format modifier
        let plane_layout;
        let mut drm_modifier_info;
        let mut drm_modifier_list_info;
        let modifiers;

        let mut image_create_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory_info);

        if use_drm_modifier {
            // Use explicit DRM format modifier
            plane_layout = vk::SubresourceLayout {
                offset: dmabuf.offset(),
                size: dmabuf.size(),
                row_pitch: dmabuf.stride() as u64,
                array_pitch: 0,
                depth_pitch: 0,
            };

            drm_modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
                .drm_format_modifier(dmabuf.modifier)
                .plane_layouts(std::slice::from_ref(&plane_layout));

            image_create_info = image_create_info
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .push_next(&mut drm_modifier_info);
        } else if dmabuf.modifier == drm_modifiers::DRM_FORMAT_MOD_LINEAR {
            // Linear format - use LINEAR tiling
            // IMPORTANT: Linear tiling without DRM modifier extension cannot honor
            // non-zero offsets. If we have an offset, we reach this branch because
            // has_drm_modifier is false, and we must fail fast to avoid color corruption.
            if has_nonzero_offset {
                return Err(ZeroCopyError::NotAvailable(format!(
                                "DMABuf with non-zero offset ({}) requires VK_EXT_image_drm_format_modifier extension \
                                 for correct plane binding. Without it, planes would read from wrong memory locations.",
                                dmabuf.offset()
                            )));
            }
            image_create_info = image_create_info.tiling(vk::ImageTiling::LINEAR);
        } else {
            // Non-linear modifier requires DRM modifier extension
            // Falling back to OPTIMAL tiling would silently corrupt sampling
            if !has_drm_modifier {
                return Err(ZeroCopyError::NotAvailable(format!(
                                "Non-linear DMABuf modifier 0x{:x} requires VK_EXT_image_drm_format_modifier extension",
                                dmabuf.modifier
                            )));
            }

            // Store modifier in outer-scoped variable so the slice lives until create_image
            modifiers = [dmabuf.modifier];
            drm_modifier_list_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                .drm_format_modifiers(&modifiers);

            image_create_info = image_create_info
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .push_next(&mut drm_modifier_list_info);
        }

        let vk_image = vk_device
            .create_image(&image_create_info, None)
            .map_err(|e| {
                ZeroCopyError::TextureCreationFailed(format!("vkCreateImage failed: {:?}", e))
            })?;

        // Step 2: Get memory requirements
        let mem_requirements = vk_device.get_image_memory_requirements(vk_image);

        // Step 3: Import external memory from DMABuf fd
        let mut import_memory_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dmabuf.fd());

        // Find suitable memory type (device local preferred)
        let memory_type_index = find_memory_type_index(
            instance,
            physical_device,
            mem_requirements.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .or_else(|| {
            // Fallback: try without device local requirement
            find_memory_type_index(
                instance,
                physical_device,
                mem_requirements.memory_type_bits,
                vk::MemoryPropertyFlags::empty(),
            )
        })
        .ok_or_else(|| {
            // Clean up the image before returning error
            vk_device.destroy_image(vk_image, None);
            ZeroCopyError::TextureCreationFailed(
                "No suitable memory type for DMABuf import".to_string(),
            )
        })?;

        let memory_allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_memory_info);

        let device_memory = vk_device
            .allocate_memory(&memory_allocate_info, None)
            .map_err(|e| {
                vk_device.destroy_image(vk_image, None);
                ZeroCopyError::TextureCreationFailed(format!(
                    "vkAllocateMemory (DMABuf import) failed: {:?}",
                    e
                ))
            })?;

        // Step 4: Bind memory to image
        vk_device
            .bind_image_memory(vk_image, device_memory, 0)
            .map_err(|e| {
                vk_device.free_memory(device_memory, None);
                vk_device.destroy_image(vk_image, None);
                ZeroCopyError::TextureCreationFailed(format!("vkBindImageMemory failed: {:?}", e))
            })?;

        info!(
            "Successfully created Vulkan image from DMABuf fd={} ({}x{} {:?})",
            dmabuf.fd(),
            width,
            height,
            format
        );

        // Step 5: Transition image layout and acquire queue ownership
        // External memory requires explicit layout transition from UNDEFINED
        // to SHADER_READ_ONLY_OPTIMAL and queue family ownership transfer
        // from VK_QUEUE_FAMILY_EXTERNAL to our graphics queue family.
        transition_image_layout_external(vk_device, vk_queue, queue_family_index, vk_image)
            .inspect_err(|_| {
                vk_device.free_memory(device_memory, None);
                vk_device.destroy_image(vk_image, None);
            })?;

        // Step 6: Wrap as wgpu-hal Texture
        // Create a TextureDescriptor for texture_from_raw
        let texture_desc = wgpu::hal::TextureDescriptor {
            label: Some("zero-copy DMABuf texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };

        // Create a drop callback to free imported Vulkan resources when
        // the texture is destroyed. Clone the device handle for the callback.
        let device_clone = vk_device.clone();
        let drop_callback = Box::new(move || {
            debug!("Freeing imported DMABuf Vulkan resources");
            // SAFETY: vk_image and device_memory were allocated by us and are valid
            // until this callback is invoked when the texture is dropped.
            // destroy_image must be called before free_memory.
            unsafe {
                device_clone.destroy_image(vk_image, None);
                device_clone.free_memory(device_memory, None);
            }
        });

        // drop_callback is called when wgpu is done with the texture,
        // allowing us to free the externally managed VkDeviceMemory
        let hal_texture = hal_device.texture_from_raw(
            vk_image,
            &texture_desc,
            Some(drop_callback),
            wgpu::hal::vulkan::TextureMemory::External,
        );

        // Create wgpu texture descriptor
        let texture_desc = wgpu::TextureDescriptor {
            label: Some("zero-copy DMABuf texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };

        // Wrap the HAL texture as a wgpu::Texture
        let wgpu_texture = device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            hal_texture,
            &texture_desc,
            wgpu::TextureUses::RESOURCE,
            // Native producer contents are already defined; do not clear imported video.
            true,
        );

        info!("Successfully imported DMABuf as wgpu texture (zero-copy)");

        Ok(wgpu_texture)
    }

    /// Imports a multi-plane DMABuf (NV12/YUV420p) into wgpu as separate textures.
    ///
    /// This function creates separate wgpu::Textures for each YUV plane, enabling
    /// zero-copy video frame display from hardware decoders that output multi-plane
    /// formats like NV12 (2 planes: Y + interleaved UV) or YUV420p (3 planes: Y + U + V).
    ///
    /// # Implementation Notes
    ///
    /// Instead of using VkSamplerYcbcrConversion (which requires immutable samplers
    /// incompatible with wgpu), this function:
    /// 1. Imports each plane as a separate single-channel VkImage
    /// 2. Returns a vector of textures that can be bound to shader slots
    /// 3. Lets the existing WGSL shaders perform YUV→RGB conversion
    ///
    /// # Safety
    ///
    /// - `dmabuf` must contain valid DMABuf file descriptors for all planes
    /// - The DMABufs must remain valid for the lifetime of the returned textures
    /// - The fd ownership IS transferred to Vulkan - do NOT close them after this call
    ///
    /// # Arguments
    ///
    /// * `device` - The wgpu Device (must be Vulkan backend)
    /// * `dmabuf` - Multi-plane DMABuf handle with per-plane metadata
    /// * `width` - Full frame width in pixels
    /// * `height` - Full frame height in pixels
    /// * `format` - The pixel format (NV12, YUV420p, etc.)
    ///
    /// # Returns
    ///
    /// A vector of wgpu::Textures:
    /// - NV12: `[Y (R8, full size), UV (RG8, half size)]`
    /// - YUV420p: `[Y (R8, full size), U (R8, half size), V (R8, half size)]`
    ///
    /// # Example
    ///
    /// ```ignore
    /// use crate::zero_copy::linux::{self, DmaBufHandle, DmaBufPlaneHandle};
    /// use lumina_video_native_frame::video::PixelFormat;
    ///
    /// let planes = vec![
    ///     DmaBufPlaneHandle { fd: y_fd, offset: 0, stride: 1920, size: 1920 * 1080 },
    ///     DmaBufPlaneHandle { fd: uv_fd, offset: 0, stride: 1920, size: 960 * 540 * 2 },
    /// ];
    /// let dmabuf = DmaBufHandle::new(planes, drm_modifiers::DRM_FORMAT_MOD_LINEAR);
    ///
    /// let textures = unsafe {
    ///     import_dmabuf_multi_plane(&device, dmabuf, 1920, 1080, PixelFormat::Nv12)?
    /// };
    /// // textures[0] = Y plane, textures[1] = UV plane
    /// ```
    pub unsafe fn import_dmabuf_multi_plane(
        device: &wgpu::Device,
        dmabuf: DmaBufHandle,
        width: u32,
        height: u32,
        format: lumina_video_native_frame::video::PixelFormat,
    ) -> Result<Vec<wgpu::Texture>, ZeroCopyError> {
        use lumina_video_native_frame::video::PixelFormat;

        // Validate plane count matches format
        let expected_planes = match format {
            PixelFormat::Nv12 => 2,
            PixelFormat::Yuv420p => 3,
            _ => {
                return Err(ZeroCopyError::InvalidResource(format!(
                    "Format {:?} is not a multi-plane YUV format",
                    format
                )));
            }
        };

        if dmabuf.num_planes() != expected_planes {
            return Err(ZeroCopyError::InvalidResource(format!(
                "Expected {} planes for {:?}, got {}",
                expected_planes,
                format,
                dmabuf.num_planes()
            )));
        }

        // Validate all FDs
        for (i, plane) in dmabuf.planes.iter().enumerate() {
            if plane.fd < 0 {
                return Err(ZeroCopyError::InvalidResource(format!(
                    "Plane {} has invalid fd (negative)",
                    i
                )));
            }
        }

        // Check for single-FD multi-plane layout (not supported due to offset handling)
        if dmabuf.planes.len() > 1 {
            // Check if all planes share the same FD (single-FD layout)
            let Some(first_plane) = dmabuf.planes.first() else {
                return Err(ZeroCopyError::InvalidResource(
                    "DMABuf has no planes".to_string(),
                ));
            };
            let first_fd = first_plane.fd;
            let is_single_fd = dmabuf.planes.iter().all(|p| p.fd == first_fd);
            if is_single_fd {
                warn!(
                    "Multi-plane single-FD DMABuf layout detected (all {} planes share fd={}). \
                     Per-plane offsets not honored by vkBindImageMemory. Falling back to CPU path.",
                    dmabuf.planes.len(),
                    first_fd
                );
                return Err(ZeroCopyError::NotAvailable(
                    "Multi-plane single-FD DMABuf layout not supported (per-plane offsets not honored). \
                     Use multi-FD layout or CPU fallback.".to_string()
                ));
            }
        }

        info!(
            "Importing multi-plane DMABuf: {:?} ({}x{}, {} planes)",
            format,
            width,
            height,
            dmabuf.num_planes()
        );

        // Build per-plane import specifications
        let plane_specs: Vec<(u32, u32, wgpu::TextureFormat)> = match format {
            PixelFormat::Nv12 => vec![
                // Plane 0: Y (luma) - full resolution, R8
                (width, height, wgpu::TextureFormat::R8Unorm),
                // Plane 1: UV (chroma) - half resolution, RG8 (interleaved)
                (width / 2, height / 2, wgpu::TextureFormat::Rg8Unorm),
            ],
            PixelFormat::Yuv420p => vec![
                // Plane 0: Y (luma) - full resolution, R8
                (width, height, wgpu::TextureFormat::R8Unorm),
                // Plane 1: U (Cb) - half resolution, R8
                (width / 2, height / 2, wgpu::TextureFormat::R8Unorm),
                // Plane 2: V (Cr) - half resolution, R8
                (width / 2, height / 2, wgpu::TextureFormat::R8Unorm),
            ],
            _ => unreachable!(),
        };

        // Import each plane as a separate texture
        let mut textures = Vec::with_capacity(plane_specs.len());

        for (i, ((plane_width, plane_height, wgpu_format), plane)) in
            plane_specs.iter().zip(dmabuf.planes.iter()).enumerate()
        {
            debug!(
                "Importing plane {}: {}x{} {:?} (fd={}, offset={}, stride={})",
                i, plane_width, plane_height, wgpu_format, plane.fd, plane.offset, plane.stride
            );

            // Create a single-plane handle for this plane
            let single_plane_handle = DmaBufHandle::single_plane(
                plane.fd,
                plane.size,
                plane.offset,
                plane.stride,
                dmabuf.modifier,
            );

            // Import using the existing single-plane function
            match import_dmabuf(
                device,
                single_plane_handle,
                *plane_width,
                *plane_height,
                *wgpu_format,
            ) {
                Ok(texture) => {
                    textures.push(texture);
                }
                Err(e) => {
                    // Clean up already-imported textures on failure
                    // (wgpu::Texture drops automatically when Vec is dropped)
                    warn!("Failed to import plane {}: {:?}", i, e);
                    return Err(ZeroCopyError::TextureCreationFailed(format!(
                        "Failed to import plane {}: {}",
                        i, e
                    )));
                }
            }
        }

        info!(
            "Successfully imported {} planes as wgpu textures (zero-copy multi-plane)",
            textures.len()
        );

        Ok(textures)
    }

    /// Imports a single-FD multi-plane DMABuf (NV12/YUV420p) into wgpu as separate textures.
    ///
    /// This function handles the case where all planes share a single file descriptor but
    /// have different offsets within that buffer. This is common with VA-API which outputs
    /// single-FD multi-plane layouts.
    ///
    /// # Implementation Notes
    ///
    /// For single-FD multi-plane layouts, the Vulkan import uses:
    /// 1. VkImageDrmFormatModifierExplicitCreateInfoEXT with pPlaneLayouts array
    /// 2. VkSubresourceLayout per plane specifying offset, size, and rowPitch
    /// 3. VK_FORMAT_G8_B8R8_2PLANE_420_UNORM for NV12 (uses VkSamplerYcbcrConversion)
    ///
    /// However, VkSamplerYcbcrConversion requires immutable samplers which are incompatible
    /// with wgpu's sampler model. Instead, this implementation imports each plane as a
    /// separate single-channel texture:
    /// - For the Y plane: Create image from FD at plane 0's offset
    /// - For the UV plane: Create separate image from same FD at plane 1's offset
    ///
    /// # Safety
    ///
    /// - `planes` must contain valid DMABuf plane handles with the same FD
    /// - The DMABuf must remain valid for the lifetime of the returned textures
    /// - The fd ownership IS transferred to Vulkan - do NOT close it after this call
    ///
    /// # Arguments
    ///
    /// * `device` - The wgpu Device (must be Vulkan backend)
    /// * `fd` - The shared DMABuf file descriptor
    /// * `planes` - Per-plane metadata (offset, stride, size) - all must reference same fd
    /// * `width` - Full frame width in pixels
    /// * `height` - Full frame height in pixels
    /// * `format` - The pixel format (NV12, YUV420p, etc.)
    /// * `modifier` - DRM format modifier
    ///
    /// # Returns
    ///
    /// A vector of wgpu::Textures:
    /// - NV12: `[Y (R8, full size), UV (RG8, half size)]`
    /// - YUV420p: `[Y (R8, full size), U (R8, half size), V (R8, half size)]`
    pub unsafe fn import_dmabuf_single_fd_multi_plane(
        device: &wgpu::Device,
        fd: std::os::fd::RawFd,
        planes: &[DmaBufPlaneHandle],
        width: u32,
        height: u32,
        format: lumina_video_native_frame::video::PixelFormat,
        modifier: u64,
    ) -> Result<Vec<wgpu::Texture>, ZeroCopyError> {
        use lumina_video_native_frame::video::PixelFormat;

        // Validate FD
        if fd < 0 {
            return Err(ZeroCopyError::InvalidResource(
                "DMABuf fd is invalid (negative)".to_string(),
            ));
        }

        // Validate plane count matches format
        let expected_planes = match format {
            PixelFormat::Nv12 => 2,
            PixelFormat::Yuv420p => 3,
            _ => {
                return Err(ZeroCopyError::InvalidResource(format!(
                    "Format {:?} is not a multi-plane YUV format",
                    format
                )));
            }
        };

        if planes.len() != expected_planes {
            return Err(ZeroCopyError::InvalidResource(format!(
                "Expected {} planes for {:?}, got {}",
                expected_planes,
                format,
                planes.len()
            )));
        }

        info!(
            "Importing single-FD multi-plane DMABuf: {:?} ({}x{}, {} planes, fd={})",
            format,
            width,
            height,
            planes.len(),
            fd
        );

        // Build per-plane import specifications
        let plane_specs: Vec<(u32, u32, wgpu::TextureFormat)> = match format {
            PixelFormat::Nv12 => vec![
                // Plane 0: Y (luma) - full resolution, R8
                (width, height, wgpu::TextureFormat::R8Unorm),
                // Plane 1: UV (chroma) - half resolution, RG8 (interleaved)
                (width / 2, height / 2, wgpu::TextureFormat::Rg8Unorm),
            ],
            PixelFormat::Yuv420p => vec![
                // Plane 0: Y (luma) - full resolution, R8
                (width, height, wgpu::TextureFormat::R8Unorm),
                // Plane 1: U (Cb) - half resolution, R8
                (width / 2, height / 2, wgpu::TextureFormat::R8Unorm),
                // Plane 2: V (Cr) - half resolution, R8
                (width / 2, height / 2, wgpu::TextureFormat::R8Unorm),
            ],
            _ => unreachable!(),
        };

        // Import each plane as a separate texture using dup'd FD
        let mut textures = Vec::with_capacity(plane_specs.len());

        for (i, ((plane_width, plane_height, wgpu_format), plane)) in
            plane_specs.iter().zip(planes.iter()).enumerate()
        {
            debug!(
                "Importing plane {}: {}x{} {:?} (offset={}, stride={}, size={})",
                i, plane_width, plane_height, wgpu_format, plane.offset, plane.stride, plane.size
            );

            // Each plane import needs its own FD since Vulkan takes ownership
            // For the first plane, we use the original fd
            // For subsequent planes, we dup the fd
            let plane_fd = if i == 0 {
                fd
            } else {
                let dup_fd = libc::dup(fd);
                if dup_fd < 0 {
                    warn!(
                        "Failed to dup DMABuf fd {} for plane {}: {}",
                        fd,
                        i,
                        std::io::Error::last_os_error()
                    );
                    return Err(ZeroCopyError::InvalidResource(format!(
                        "Failed to dup DMABuf fd for plane {}: {}",
                        i,
                        std::io::Error::last_os_error()
                    )));
                }
                dup_fd
            };

            // Create a single-plane handle for this plane with the correct offset
            let single_plane_handle = DmaBufHandle::single_plane(
                plane_fd,
                plane.size,
                plane.offset,
                plane.stride,
                modifier,
            );

            // Import using the existing single-plane function
            match import_dmabuf(
                device,
                single_plane_handle,
                *plane_width,
                *plane_height,
                *wgpu_format,
            ) {
                Ok(texture) => {
                    textures.push(texture);
                }
                Err(e) => {
                    // Close the dup'd FD for this plane since import_dmabuf failed
                    // (import_dmabuf takes ownership on success, but on failure we must clean up)
                    // Note: plane 0 uses the original fd which caller owns, so only close if i > 0
                    if i > 0 {
                        // SAFETY: `plane_fd` is the duplicated descriptor for this
                        // plane and import failed, so ownership remains here.
                        unsafe {
                            libc::close(plane_fd);
                        }
                    }
                    warn!(
                        "Failed to import plane {} from single-FD layout: {:?}",
                        i, e
                    );
                    return Err(ZeroCopyError::TextureCreationFailed(format!(
                        "Failed to import plane {} from single-FD layout: {}",
                        i, e
                    )));
                }
            }
        }

        info!(
            "Successfully imported {} planes from single-FD DMABuf as wgpu textures (zero-copy)",
            textures.len()
        );

        Ok(textures)
    }

    /// Converts wgpu TextureFormat to Vulkan VkFormat.
    ///
    /// Returns an error for unsupported formats instead of silently defaulting.
    /// NV12 is explicitly rejected because it is a multi-plane format requiring
    /// VK_KHR_sampler_ycbcr_conversion which is not yet implemented.
    pub fn wgpu_format_to_vulkan(format: wgpu::TextureFormat) -> Result<vk::Format, ZeroCopyError> {
        match format {
            wgpu::TextureFormat::Bgra8Unorm => Ok(vk::Format::B8G8R8A8_UNORM),
            wgpu::TextureFormat::Rgba8Unorm => Ok(vk::Format::R8G8B8A8_UNORM),
            wgpu::TextureFormat::R8Unorm => Ok(vk::Format::R8_UNORM),
            wgpu::TextureFormat::Rg8Unorm => Ok(vk::Format::R8G8_UNORM),
            wgpu::TextureFormat::Bgra8UnormSrgb => Ok(vk::Format::B8G8R8A8_SRGB),
            wgpu::TextureFormat::Rgba8UnormSrgb => Ok(vk::Format::R8G8B8A8_SRGB),
            // NV12 is a multi-plane format (Y plane + interleaved UV plane) that requires
            // VK_KHR_sampler_ycbcr_conversion for proper handling. The current single-plane
            // DMABuf import cannot handle NV12 correctly. Proper support would require:
            // - Creating a multi-plane VkImage with VK_IMAGE_CREATE_DISJOINT_BIT
            // - Setting up VkSamplerYcbcrConversion for Y'CbCr color space conversion
            // - Using separate plane layouts for Y and UV data
            // - Binding memory to each plane separately
            wgpu::TextureFormat::NV12 => {
                warn!("NV12 format requested but multi-plane import not yet supported");
                Err(ZeroCopyError::InvalidResource(
                    "NV12 multi-plane format not yet supported for zero-copy import. \
                     Proper support requires VK_KHR_sampler_ycbcr_conversion."
                        .to_string(),
                ))
            }
            _ => {
                warn!("Unsupported texture format {:?}; refusing import", format);
                Err(ZeroCopyError::InvalidResource(format!(
                    "Unsupported texture format {:?}",
                    format
                )))
            }
        }
    }

    /// Common DRM format modifiers for reference.
    ///
    /// These are from drm_fourcc.h and are used with VK_EXT_image_drm_format_modifier.
    pub mod drm_modifiers {
        /// Linear layout (no tiling)
        pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

        /// Invalid/unspecified modifier
        pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

        // Intel modifiers (I915_FORMAT_MOD_*)
        /// Intel X-tiled
        pub const I915_FORMAT_MOD_X_TILED: u64 = 0x0100000000000001;
        /// Intel Y-tiled
        pub const I915_FORMAT_MOD_Y_TILED: u64 = 0x0100000000000002;
        /// Intel Yf-tiled
        pub const I915_FORMAT_MOD_YF_TILED: u64 = 0x0100000000000003;
        /// Intel Y-tiled with CCS (Tile4)
        pub const I915_FORMAT_MOD_Y_TILED_CCS: u64 = 0x0100000000000004;
        /// Intel Tile4 (DG2+)
        pub const I915_FORMAT_MOD_4_TILED: u64 = 0x0100000000000009;

        // AMD modifiers (AMD_FMT_MOD)
        /// AMD GFX9 64KB tiled
        pub const AMD_FMT_MOD_TILE_GFX9_64K_S: u64 = 0x0200000000000001;

        // NVIDIA modifiers
        /// NVIDIA block linear (16Bx2)
        pub const NVIDIA_FORMAT_MOD_BLOCK_LINEAR_2D: u64 = 0x0300000000000010;
    }
}

// =============================================================================
// Windows: D3D11 shared handle → D3D12 → wgpu
// =============================================================================

/// Windows-specific zero-copy import via D3D11/D3D12 shared handles.
///
/// This module provides functions to import D3D11 shared textures
/// directly into wgpu (via D3D12) without CPU memory copies.
///
/// # Requirements
///
/// - Windows 10 or later
/// - wgpu using D3D12 backend
/// - D3D11 texture created with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE`
///
/// # D3D11/D3D12 Interop
///
/// This uses the standard Windows cross-API texture sharing:
/// 1. D3D11 creates a texture with `SHARED_NTHANDLE` flag
/// 2. D3D11 calls `CreateSharedHandle()` to get an NT handle
/// 3. D3D12 calls `OpenSharedHandle()` to get `ID3D12Resource`
/// 4. wgpu wraps the resource via `texture_from_raw()`
///
/// # Synchronization
///
/// D3D11/D3D12 cross-API synchronization is required to ensure D3D11 decode
/// completes before D3D12/wgpu reads the texture.
///
/// ## Current Implementation: Query Polling (Safe but Suboptimal)
///
/// The current implementation in `windows_video.rs` uses:
/// - `ID3D11DeviceContext::Flush()` to submit all pending D3D11 work
/// - `D3D11_QUERY_EVENT` to poll for completion on the decode thread
/// - CPU blocks until the query signals, then passes handle to D3D12
///
/// This is safe and prevents visual corruption, but adds ~0.1-0.5ms latency
/// per frame due to CPU polling.
///
/// ## Future Improvement: True GPU Fence
///
/// For optimal performance, cross-API fence sync could be implemented:
/// - Create `ID3D11Fence` from `ID3D11Device5` (Windows 10+)
/// - Share fence via HANDLE between D3D11 and D3D12
/// - D3D11 signals fence after decode completes
/// - D3D12 waits on fence before reading shared texture
///
/// This would eliminate CPU blocking and enable true GPU-GPU synchronization.
/// See Microsoft docs on "Sharing Surfaces Between Windows Graphics APIs".
///
/// # Example
///
/// ```ignore
/// use crate::zero_copy::windows;
///
/// // D3D11 side: Create shared texture
/// let shared_handle = device11.CreateSharedHandle(&d3d11_texture, ...)?;
///
/// // wgpu/D3D12 side: Import the texture
/// let texture = unsafe {
///     windows::import_d3d11_shared_handle(&device, shared_handle, width, height, format, None)?
/// };
/// ```
#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
pub mod windows {
    use super::ZeroCopyError;
    use tracing::{debug, warn};
    use windows::Win32::{
        Foundation::HANDLE,
        Graphics::{
            Direct3D12::{ID3D12Device, ID3D12Resource},
            Dxgi::Common as DxgiCommon,
        },
    };

    /// Opaque handle to a D3D11 shared texture (HANDLE from CreateSharedHandle).
    /// This is obtained from ID3D11Device1::CreateSharedHandle() on a D3D11 texture
    /// created with D3D11_RESOURCE_MISC_SHARED_NTHANDLE.
    pub type SharedHandle = HANDLE;

    /// Checks if the current wgpu device supports D3D12 backend.
    pub fn is_d3d12_backend(device: &wgpu::Device) -> bool {
        // SAFETY: `device` is a live wgpu device borrowed for this call; the
        // HAL query only inspects its backend and does not retain raw handles.
        unsafe { device.as_hal::<wgpu::hal::api::Dx12>().is_some() }
    }

    /// Gets information about the D3D12 device for diagnostics.
    ///
    /// Returns a description if D3D12 backend is available, None otherwise.
    pub fn get_d3d12_device_info(device: &wgpu::Device) -> Option<String> {
        // SAFETY: `device` is live for the callback and the HAL device is only
        // borrowed while the callback executes.
        unsafe {
            device
                .as_hal::<wgpu::hal::api::Dx12>()
                .map(|_| "D3D12 Device".to_string())
        }
    }

    /// Checks if D3D11/D3D12 shared handle import is available.
    ///
    /// This always returns true on Windows when using D3D12 backend,
    /// as shared handle import is a core D3D12 feature (no extensions needed).
    pub fn is_shared_handle_import_available(device: &wgpu::Device) -> bool {
        is_d3d12_backend(device)
    }

    /// Imports a D3D11 shared handle into wgpu as a texture (zero-copy).
    ///
    /// This function creates a wgpu::Texture that directly references the D3D11 texture's
    /// GPU memory via D3D12 interop, enabling zero-copy video frame display.
    ///
    /// # D3D11/D3D12 Interop Flow
    ///
    /// 1. D3D11 decoder creates a texture with SHARED_NTHANDLE flag
    /// 2. D3D11 calls CreateSharedHandle() to get an NT handle
    /// 3. D3D12 calls OpenSharedHandle() to get ID3D12Resource
    /// 4. wgpu wraps the resource via texture_from_raw()
    ///
    /// # Handle Ownership
    ///
    /// **The caller retains ownership of the shared handle.** This function does not
    /// close the handle; `OpenSharedHandle` only creates a reference to the underlying
    /// D3D11 resource without transferring handle ownership.
    ///
    /// The caller must close the handle via `CloseHandle()` when done, but only AFTER:
    /// - The returned wgpu::Texture has been dropped, AND
    /// - All GPU operations using the texture have completed
    ///
    /// Closing the handle prematurely while the texture is in use results in undefined
    /// behavior (typically GPU hangs or access violations).
    ///
    /// # Safety
    ///
    /// - `shared_handle` must be a valid HANDLE from ID3D11Device1::CreateSharedHandle()
    /// - The D3D11 texture must have been created with D3D11_RESOURCE_MISC_SHARED_NTHANDLE
    /// - The D3D11 texture must remain valid for the lifetime of the returned texture
    /// - The caller is responsible for synchronization between D3D11 and D3D12 access
    ///   (use ID3D11Fence for cross-API synchronization)
    /// - The handle must not be closed until the returned texture is dropped and GPU idle
    ///
    /// # Errors
    ///
    /// Returns [`ZeroCopyError::InvalidResource`] if the handle is invalid (null or closed).
    /// Returns [`ZeroCopyError::TextureCreationFailed`] if `OpenSharedHandle` fails, which
    /// can occur if the handle was closed or refers to an incompatible resource.
    ///
    /// # Arguments
    ///
    /// * `device` - The wgpu Device (must be D3D12 backend)
    /// * `shared_handle` - A valid HANDLE from CreateSharedHandle()
    /// * `width` - Texture width in pixels
    /// * `height` - Texture height in pixels
    /// * `format` - The wgpu texture format (should match D3D11 texture format)
    /// * `drop_callback` - Retains the producer lease until GPU-tracked destruction
    ///
    /// # Returns
    ///
    /// A wgpu::Texture that references the D3D11 texture memory via D3D12 interop.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // D3D11 side: Create shared texture
    /// let d3d11_texture = device11.CreateTexture2D(&desc, None)?;
    /// let shared_handle = device11.CreateSharedHandle(&d3d11_texture, None, GENERIC_ALL, None)?;
    ///
    /// // wgpu/D3D12 side: Import the texture
    /// let wgpu_texture = unsafe {
    ///     import_d3d11_shared_handle(&wgpu_device, shared_handle, width, height, format, None)?
    /// };
    /// ```
    pub unsafe fn import_d3d11_shared_handle(
        device: &wgpu::Device,
        shared_handle: HANDLE,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
        drop_callback: Option<wgpu::hal::DropCallback>,
    ) -> Result<wgpu::Texture, ZeroCopyError> {
        if shared_handle.is_invalid() {
            return Err(ZeroCopyError::InvalidResource(
                "Shared handle is invalid".to_string(),
            ));
        }

        // Access the D3D12 HAL device and open the shared handle
        let hal_device = device.as_hal::<wgpu::hal::api::Dx12>();
        let hal_texture_result = (|hal_device: Option<&wgpu::hal::dx12::Device>| {
            let Some(hal_device) = hal_device else {
                warn!("Failed to get D3D12 HAL device");
                return Err(ZeroCopyError::HalAccessFailed(
                    "wgpu not using D3D12 backend".to_string(),
                ));
            };

            // Get the raw D3D12 device
            let d3d12_device: &ID3D12Device = hal_device.raw_device();

            debug!(
                "Opening D3D11 shared handle via D3D12 ({}x{} {:?})",
                width, height, format
            );

            // Open the shared handle as a D3D12 resource
            // This is the key interop call: D3D12 can open NT handles from D3D11
            let mut d3d12_resource: Option<ID3D12Resource> = None;
            d3d12_device
                .OpenSharedHandle(shared_handle, &mut d3d12_resource)
                .map_err(|e| {
                    warn!("D3D12 OpenSharedHandle failed: {:?}", e);
                    ZeroCopyError::TextureCreationFailed(format!(
                        "D3D12 OpenSharedHandle failed: {:?}",
                        e
                    ))
                })?;
            let d3d12_resource = d3d12_resource.ok_or_else(|| {
                ZeroCopyError::TextureCreationFailed("D3D12 returned no shared resource".into())
            })?;

            let description = d3d12_resource.GetDesc();
            if description.Width != u64::from(width)
                || description.Height != height
                || description.DepthOrArraySize != 1
                || description.MipLevels != 1
                || description.SampleDesc.Count != 1
                || description.Format != wgpu_format_to_dxgi(format)?
                || !description.Flags.contains(
                    windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
                )
            {
                return Err(ZeroCopyError::InvalidResource(
                    "shared texture layout or simultaneous-access contract does not match".into(),
                ));
            }

            // Wrap as wgpu_hal::dx12::Texture using the existing API
            // Note: texture_from_raw is unsafe because it trusts the caller
            // to provide valid parameters matching the actual resource
            let hal_texture = wgpu::hal::dx12::Device::texture_from_raw(
                d3d12_resource,
                format,
                wgpu::TextureDimension::D2,
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                1, // mip_level_count
                1, // sample_count
                drop_callback,
            );

            Ok(hal_texture)
        })(hal_device.as_deref());

        // Get the HAL texture from the closure result
        let hal_texture = hal_texture_result?;

        // Create wgpu texture descriptor
        let texture_desc = wgpu::TextureDescriptor {
            label: Some("zero-copy D3D11 shared texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };

        // Wrap the HAL texture as a wgpu::Texture
        let wgpu_texture = device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
            hal_texture,
            &texture_desc,
            // Shared D3D11 textures use simultaneous access. COMMON implicitly
            // promotes for shader reads and decays after each ExecuteCommandLists;
            // no explicit state transition may outlive the producer lease.
            wgpu::TextureUses::RESOURCE,
            // Native producer contents are already defined; do not clear imported video.
            true,
        );

        Ok(wgpu_texture)
    }

    /// Converts wgpu TextureFormat to DXGI_FORMAT.
    ///
    /// Used for validation and debugging when creating shared textures.
    pub fn wgpu_format_to_dxgi(
        format: wgpu::TextureFormat,
    ) -> Result<DxgiCommon::DXGI_FORMAT, ZeroCopyError> {
        match format {
            wgpu::TextureFormat::Bgra8Unorm => Ok(DxgiCommon::DXGI_FORMAT_B8G8R8A8_UNORM),
            wgpu::TextureFormat::Rgba8Unorm => Ok(DxgiCommon::DXGI_FORMAT_R8G8B8A8_UNORM),
            wgpu::TextureFormat::R8Unorm => Ok(DxgiCommon::DXGI_FORMAT_R8_UNORM),
            wgpu::TextureFormat::Rg8Unorm => Ok(DxgiCommon::DXGI_FORMAT_R8G8_UNORM),
            wgpu::TextureFormat::Bgra8UnormSrgb => Ok(DxgiCommon::DXGI_FORMAT_B8G8R8A8_UNORM_SRGB),
            wgpu::TextureFormat::Rgba8UnormSrgb => Ok(DxgiCommon::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB),
            _ => {
                warn!("Unsupported texture format {:?}; refusing import", format);
                Err(ZeroCopyError::InvalidResource(format!(
                    "Unsupported texture format {:?}",
                    format
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zero_copy_stats() {
        let mut stats = ZeroCopyStats::default();
        assert_eq!(stats.zero_copy_percentage(), 0.0);

        stats.total_frames = 100;
        stats.zero_copy_frames = 75;
        stats.fallback_frames = 25;
        assert!((stats.zero_copy_percentage() - 75.0).abs() < 0.01);
    }
}
