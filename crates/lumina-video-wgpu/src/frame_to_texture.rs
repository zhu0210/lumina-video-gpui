//! Convert decoded video frames to wgpu textures suitable for GPUI's `surface()`.
//!
//! Two rendering paths are supported:
//!
//! | Path | Format | Textures | GPUI `surface()` call |
//! |------|--------|----------|-----------------------|
//! | **NV12 native** | NV12, YUV420p | Y (R8Unorm) + CbCr (Rg8Unorm) | `surface((y_tex, cbcr_tex, size))` — GPU-side YUV→RGB |
//! | **RGBA passthrough** | RGBA, BGRA, RGB24 | Single RGBA8Unorm | `surface((tex, desc))` |
//!
//! YUV420p frames are cheaply interleaved (U+V → CbCr) to use the NV12 path,
//! avoiding the expensive CPU YUV→RGB conversion. Only NV12 and YUV420p use
//! the NV12 path; RGB formats use RGBA passthrough.
//!
//! Platform GPU surfaces (IOSurface, DMABuf, etc.) are imported zero-copy
//! when possible, falling back to the CPU path via `cpu_fallback`.

#[cfg(test)]
use lumina_video_native_frame::video::Plane;
use lumina_video_native_frame::video::{CpuFrame, DecodedFrame, PixelFormat};
use lumina_video_native_frame::{AcquireSync, CpuMemory, NativeFrameLease, NativeMemory};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Write-texture guard — prevents GPU validation crashes when frame
// metadata (width/height/stride) is inconsistent with actual plane data.
// ---------------------------------------------------------------------------

/// Wrapper around `queue.write_texture` that checks the source buffer is
/// large enough for the described copy.  Logs a warning and skips the
/// upload instead of triggering a wgpu validation error / panic.
fn safe_write_texture(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    layout: wgpu::TexelCopyBufferLayout,
    extent: wgpu::Extent3d,
) {
    let required =
        layout.bytes_per_row.unwrap_or(0) as usize * layout.rows_per_image.unwrap_or(1) as usize;
    if data.len() < required {
        tracing::warn!(
            "write_texture skipped: buffer {} bytes < needed {} bytes \
             (bpr={}, rows={}, extent={}x{}x{})",
            data.len(),
            required,
            layout.bytes_per_row.unwrap_or(0),
            layout.rows_per_image.unwrap_or(0),
            extent.width,
            extent.height,
            extent.depth_or_array_layers,
        );
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        layout,
        extent,
    );
}

// ---------------------------------------------------------------------------
// GPU frame textures — the output of frame upload (fed to GPUI surface())
// ---------------------------------------------------------------------------

/// GPU representation of a decoded video frame.
///
/// Variants map directly to [`gpui::SurfaceSource`] conversions:
/// - `Nv12` → `surface((y_tex, cbcr_tex, native_size))`
/// - `Rgba` → `surface((tex, descriptor))`
#[derive(Clone)]
pub enum GpuFrameTextures {
    /// Two-plane NV12: Y (R8Unorm) + interleaved CbCr (Rg8Unorm).
    /// Use with `surface((y_texture, cb_cr_texture, native_size))`.
    Nv12 {
        y_texture: Arc<wgpu::Texture>,
        cb_cr_texture: Arc<wgpu::Texture>,
        width: u32,
        height: u32,
    },
    /// Single RGBA8Unorm texture.
    /// Use with `surface((texture, descriptor))`.
    Rgba {
        texture: Arc<wgpu::Texture>,
        width: u32,
        height: u32,
    },
}

#[derive(Clone, Copy)]
struct PlaneRef<'a> {
    data: &'a [u8],
    stride: usize,
}

#[derive(Clone, Copy)]
struct CpuFrameRef<'a> {
    format: PixelFormat,
    width: u32,
    height: u32,
    planes: [Option<PlaneRef<'a>>; 3],
}

impl<'a> CpuFrameRef<'a> {
    fn plane(&self, index: usize) -> Option<PlaneRef<'a>> {
        self.planes.get(index).copied().flatten()
    }

    fn from_cpu(frame: &'a CpuFrame) -> Self {
        Self {
            format: frame.format,
            width: frame.width,
            height: frame.height,
            planes: [
                frame.planes.first().map(|p| PlaneRef {
                    data: &p.data,
                    stride: p.stride,
                }),
                frame.planes.get(1).map(|p| PlaneRef {
                    data: &p.data,
                    stride: p.stride,
                }),
                frame.planes.get(2).map(|p| PlaneRef {
                    data: &p.data,
                    stride: p.stride,
                }),
            ],
        }
    }

    fn from_memory(memory: &'a CpuMemory, width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            format,
            width,
            height,
            planes: [
                memory.planes.first().map(|p| PlaneRef {
                    data: &p.bytes,
                    stride: p.stride,
                }),
                memory.planes.get(1).map(|p| PlaneRef {
                    data: &p.bytes,
                    stride: p.stride,
                }),
                memory.planes.get(2).map(|p| PlaneRef {
                    data: &p.bytes,
                    stride: p.stride,
                }),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Convert any `DecodedFrame` to `GpuFrameTextures` for GPUI's `surface()`.
///
/// - NV12/YUV420p CPU frames → NV12 dual-texture (GPU-side YUV→RGB conversion)
/// - RGBA/BGRA/RGB24 CPU frames → single RGBA8Unorm texture
/// - Platform GPU surfaces → CPU fallback path (zero-copy import is TODO)
///
/// If the decode pipeline produces a GPU surface without a CPU fallback and
/// zero-copy import hasn't been implemented for this integration, returns `None`.
pub fn decoded_frame_to_textures(
    frame: &DecodedFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Option<GpuFrameTextures> {
    match frame {
        DecodedFrame::Cpu(cpu) => Some(upload_cpu_frame_as_textures(
            cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
        )),

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        DecodedFrame::MacOS(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                Some(upload_cpu_frame_as_textures(
                    cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
                ))
            } else {
                // No CPU fallback — try zero-copy IOSurface → Metal → wgpu import.
                // IOSurface-backed textures cannot be reliably CPU-mapped.
                tracing::debug!(
                    "macOS IOSurface frame: {}x{} fmt={:?}, attempting zero-copy import",
                    surface.width,
                    surface.height,
                    surface.format
                );
                match import_macos_iosurface_frame(surface, device) {
                    Ok(textures) => {
                        tracing::info!("macOS IOSurface zero-copy import succeeded");
                        Some(textures)
                    }
                    Err(e) => {
                        tracing::warn!("macOS IOSurface zero-copy import failed: {e}");
                        None
                    }
                }
            }
        }

        #[cfg(target_os = "linux")]
        DecodedFrame::Linux(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                Some(upload_cpu_frame_as_textures(
                    cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
                ))
            } else {
                // No CPU fallback — try zero-copy DMABuf → Vulkan → wgpu import.
                // DMABuf memory is GPU-only and cannot be CPU-mapped.
                tracing::debug!(
                    "Linux DMABuf frame: {}x{} fmt={:?}, {} planes, attempting zero-copy import",
                    surface.width,
                    surface.height,
                    surface.format,
                    surface.planes.len()
                );
                match import_linux_dmabuf_frame(surface, device) {
                    Ok(textures) => {
                        tracing::info!("Linux DMABuf zero-copy import succeeded");
                        Some(textures)
                    }
                    Err(e) => {
                        tracing::warn!("Linux DMABuf zero-copy import failed: {e}");
                        None
                    }
                }
            }
        }

        #[cfg(target_os = "android")]
        DecodedFrame::Android(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                Some(upload_cpu_frame_as_textures(
                    cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
                ))
            } else {
                tracing::warn!("Android GPU surface without CPU fallback — frame dropped");
                None
            }
        }

        #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
        DecodedFrame::Windows(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                Some(upload_cpu_frame_as_textures(
                    cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
                ))
            } else {
                tracing::warn!("Windows GPU surface without CPU fallback — frame dropped");
                None
            }
        }
    }
}

/// Failure while handing an owned native frame to the renderer.
#[derive(Debug)]
pub enum NativeFrameIngestionError {
    /// The renderer has no contract for consuming the producer's acquire fence.
    UnsupportedAcquireSync(Box<NativeFrameLease>),
    /// External-memory import is not safe until the renderer supplies layout and sync state.
    UnsupportedDmaBuf(Box<NativeFrameLease>),
}

impl std::fmt::Display for NativeFrameIngestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedAcquireSync(_) => write!(
                f,
                "native frame acquire synchronization is unsupported at the wgpu boundary"
            ),
            Self::UnsupportedDmaBuf(_) => write!(
                f,
                "Linux DMABuf import requires renderer-boundary layout and sync support"
            ),
        }
    }
}

impl std::error::Error for NativeFrameIngestionError {}

impl NativeFrameIngestionError {
    /// Returns the unchanged lease so the caller can retry at a boundary that owns synchronization.
    pub fn into_lease(self) -> NativeFrameLease {
        match self {
            Self::UnsupportedAcquireSync(lease) | Self::UnsupportedDmaBuf(lease) => *lease,
        }
    }
}

/// Uploads an owned native frame without cloning its CPU planes.
pub fn native_frame_lease_to_textures(
    lease: NativeFrameLease,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Result<GpuFrameTextures, NativeFrameIngestionError> {
    let lease = ensure_native_frame_lease_supported(lease)?;

    let NativeFrameLease {
        descriptor,
        memory,
        acquire,
    } = lease;
    match memory {
        NativeMemory::Cpu(memory) => Ok(upload_cpu_frame_ref_as_textures(
            CpuFrameRef::from_memory(
                &memory,
                descriptor.extent.width,
                descriptor.extent.height,
                descriptor.format,
            ),
            device,
            queue,
            y_cache,
            cbcr_cache,
            rgba_cache,
        )),
        #[cfg(target_os = "linux")]
        NativeMemory::DmaBuf(memory) => Err(NativeFrameIngestionError::UnsupportedDmaBuf(
            Box::new(NativeFrameLease {
                descriptor,
                memory: NativeMemory::DmaBuf(memory),
                acquire,
            }),
        )),
    }
}

fn ensure_native_frame_lease_supported(
    lease: NativeFrameLease,
) -> Result<NativeFrameLease, NativeFrameIngestionError> {
    if !matches!(&lease.acquire, AcquireSync::None) {
        return Err(NativeFrameIngestionError::UnsupportedAcquireSync(Box::new(
            lease,
        )));
    }
    #[cfg(target_os = "linux")]
    if matches!(&lease.memory, NativeMemory::DmaBuf(_)) {
        return Err(NativeFrameIngestionError::UnsupportedDmaBuf(Box::new(
            lease,
        )));
    }
    Ok(lease)
}

// ---------------------------------------------------------------------------
// macOS zero-copy IOSurface → wgpu texture import
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn import_macos_iosurface_frame(
    surface: &lumina_video_native_frame::video::MacOSGpuSurface,
    device: &wgpu::Device,
) -> Result<GpuFrameTextures, lumina_video_native_frame::video::VideoError> {
    // SAFETY: The decoder owns the IOSurface and keeps its CVPixelBuffer owner
    // alive through `surface`; the import is only attempted for that live frame.
    let texture = unsafe {
        crate::zero_copy::macos::import_iosurface(
            device,
            surface.io_surface,
            surface.width,
            surface.height,
            wgpu::TextureFormat::Bgra8Unorm,
        )
    }
    .map_err(|e| {
        lumina_video_native_frame::video::VideoError::DecodeFailed(format!(
            "IOSurface zero-copy import failed: {e}"
        ))
    })?;

    // IOSurface textures are always BGRA8Unorm
    Ok(GpuFrameTextures::Rgba {
        texture: std::sync::Arc::new(texture),
        width: surface.width,
        height: surface.height,
    })
}

// ---------------------------------------------------------------------------
// Linux zero-copy DMABuf → wgpu texture import
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn import_linux_dmabuf_frame(
    surface: &lumina_video_native_frame::video::LinuxGpuSurface,
    device: &wgpu::Device,
) -> Result<GpuFrameTextures, lumina_video_native_frame::video::VideoError> {
    use crate::zero_copy::linux::DmaBufHandle;
    use crate::zero_copy::linux::DmaBufPlaneHandle;
    use lumina_video_native_frame::video::PixelFormat;

    let plane_handles: Vec<DmaBufPlaneHandle> = surface
        .planes
        .iter()
        .map(|p| DmaBufPlaneHandle {
            fd: p.fd,
            offset: p.offset,
            stride: p.stride,
            size: p.size,
        })
        .collect();

    let dmabuf_handle = DmaBufHandle::new(plane_handles, surface.modifier);

    // SAFETY: The plane descriptors reference the live DMABuf owner retained by
    // `surface`; the import consumes only the duplicated handles it receives.
    let textures = unsafe {
        crate::zero_copy::linux::import_dmabuf_multi_plane(
            device,
            dmabuf_handle,
            surface.width,
            surface.height,
            surface.format,
        )
    }
    .map_err(|e| {
        lumina_video_native_frame::video::VideoError::DecodeFailed(format!(
            "DMABuf zero-copy import failed: {e}"
        ))
    })?;

    match surface.format {
        PixelFormat::Nv12 => {
            let mut textures = textures.into_iter();
            let Some(y_texture) = textures.next() else {
                return Err(lumina_video_native_frame::video::VideoError::DecodeFailed(
                    "NV12 import returned insufficient textures".into(),
                ));
            };
            let Some(cb_cr_texture) = textures.next() else {
                return Err(lumina_video_native_frame::video::VideoError::DecodeFailed(
                    "NV12 import returned insufficient textures".into(),
                ));
            };
            Ok(GpuFrameTextures::Nv12 {
                y_texture: std::sync::Arc::new(y_texture),
                cb_cr_texture: std::sync::Arc::new(cb_cr_texture),
                width: surface.width,
                height: surface.height,
            })
        }
        _ => Err(
            lumina_video_native_frame::video::VideoError::UnsupportedFormat(format!(
                "Zero-copy import not implemented for {:?} on Linux",
                surface.format
            )),
        ),
    }
}

/// Upload a CPU frame to GPU textures, choosing the best path:
///
/// - NV12 → NV12 dual-texture (no CPU YUV→RGB conversion)
/// - YUV420p → interleaved to NV12 dual-texture
/// - RGBA/BGRA → single RGBA8Unorm texture
/// - RGB24 → converted to RGBA8Unorm
pub fn upload_cpu_frame_as_textures(
    frame: &CpuFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    upload_cpu_frame_ref_as_textures(
        CpuFrameRef::from_cpu(frame),
        device,
        queue,
        y_cache,
        cbcr_cache,
        rgba_cache,
    )
}

fn upload_cpu_frame_ref_as_textures(
    frame: CpuFrameRef<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    match frame.format {
        PixelFormat::Nv12 => upload_nv12(frame, device, queue, y_cache, cbcr_cache),
        PixelFormat::Yuv420p => upload_yuv420p_as_nv12(frame, device, queue, y_cache, cbcr_cache),
        PixelFormat::Rgba | PixelFormat::Bgra => upload_rgba(frame, device, queue, rgba_cache),
        PixelFormat::Rgb24 => upload_rgb24(frame, device, queue, rgba_cache),
    }
}

// ---------------------------------------------------------------------------
// Legacy single-texture path (for backwards compatibility)
// ---------------------------------------------------------------------------

/// Upload a CPU frame as a single RGBA8Unorm texture (CPU YUV→RGB conversion).
///
/// Prefer [`upload_cpu_frame_as_textures`] for the NV12 native path, which
/// avoids the CPU conversion.
pub fn upload_cpu_frame(
    frame: &CpuFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Arc<wgpu::Texture> {
    let frame_ref = CpuFrameRef::from_cpu(frame);
    let rgba = cpu_frame_ref_to_rgba(frame_ref);
    let width = frame.width;
    let height = frame.height;

    let texture = get_or_create_texture(
        device,
        texture_cache,
        width,
        height,
        wgpu::TextureFormat::Rgba8Unorm,
        "lumina_video_frame",
    );
    // Compute bytes_per_row from actual RGBA data to be robust against size mismatches
    let bytes_per_row = if height > 0 {
        (rgba.len() / height as usize) as u32
    } else {
        return texture;
    };
    safe_write_texture(
        queue,
        &texture,
        rgba.as_slice(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    texture
}

/// Convert any DecodedFrame to an Arc<wgpu::Texture> (CPU YUV→RGB conversion).
///
/// Prefer [`decoded_frame_to_textures`] for the NV12 native path.
pub fn decoded_frame_to_texture(
    frame: &DecodedFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Option<Arc<wgpu::Texture>> {
    match frame {
        DecodedFrame::Cpu(cpu) => Some(upload_cpu_frame(cpu, device, queue, texture_cache)),

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        DecodedFrame::MacOS(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                return Some(upload_cpu_frame(cpu, device, queue, texture_cache));
            }
            None
        }

        #[cfg(target_os = "linux")]
        DecodedFrame::Linux(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                return Some(upload_cpu_frame(cpu, device, queue, texture_cache));
            }
            None
        }

        #[cfg(target_os = "android")]
        DecodedFrame::Android(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                return Some(upload_cpu_frame(cpu, device, queue, texture_cache));
            }
            None
        }

        #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
        DecodedFrame::Windows(surface) => {
            if let Some(ref cpu) = surface.cpu_fallback {
                return Some(upload_cpu_frame(cpu, device, queue, texture_cache));
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// NV12 path — upload Y + CbCr as separate textures for GPU-side YUV→RGB
// ---------------------------------------------------------------------------

fn upload_nv12(
    frame: CpuFrameRef<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    let width = frame.width;
    let height = frame.height;

    // Y plane: full resolution, R8Unorm
    let y_texture = get_or_create_texture(
        device,
        y_cache,
        width,
        height,
        wgpu::TextureFormat::R8Unorm,
        "lumina_video_y",
    );

    // CbCr plane: half resolution (NV12 chroma subsampling), Rg8Unorm
    let cbcr_width = width.div_ceil(2);
    let cbcr_height = height.div_ceil(2);
    let cbcr_texture = get_or_create_texture(
        device,
        cbcr_cache,
        cbcr_width.max(1),
        cbcr_height.max(1),
        wgpu::TextureFormat::Rg8Unorm,
        "lumina_video_cbcr",
    );

    // Upload Y plane (R8Unorm: 1 byte/pixel)
    // Compute layout from actual data to be robust against stride mismatches
    if let Some(y_plane) = frame.plane(0) {
        let y_bytes_per_row = if height > 0 {
            (y_plane.data.len() / height as usize) as u32
        } else {
            0
        };
        // For R8Unorm, 1 byte = 1 pixel; extent width ≤ bytes_per_row
        let y_extent_width = y_bytes_per_row.min(width);
        if y_bytes_per_row == 0 || y_extent_width == 0 {
            tracing::warn!("NV12 Y plane: zero size, skipping upload");
            return GpuFrameTextures::Nv12 {
                y_texture,
                cb_cr_texture: cbcr_texture,
                width,
                height,
            };
        }
        if y_bytes_per_row != width {
            tracing::warn!(
                "NV12 Y plane: computed bpr={y_bytes_per_row} differs from width={width} — stride={}, data_len={}, height={height}",
                y_plane.stride,
                y_plane.data.len(),
            );
        }
        safe_write_texture(
            queue,
            &y_texture,
            y_plane.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(y_bytes_per_row),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width: y_extent_width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    // Upload interleaved CbCr plane (Rg8Unorm: 2 bytes/pixel)
    // Compute layout from actual data to be robust against stride mismatches
    if let Some(uv_plane) = frame.plane(1) {
        let uv_bytes_per_row = if cbcr_height > 0 {
            (uv_plane.data.len() / cbcr_height as usize) as u32
        } else {
            0
        };
        // For Rg8Unorm, 2 bytes = 1 pixel pair; extent width = bytes_per_row / 2
        let uv_extent_width = (uv_bytes_per_row / 2).min(cbcr_width.max(1));
        if uv_bytes_per_row == 0 || uv_extent_width == 0 {
            tracing::warn!("NV12 CbCr plane: zero size, skipping upload");
            return GpuFrameTextures::Nv12 {
                y_texture,
                cb_cr_texture: cbcr_texture,
                width,
                height,
            };
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &cbcr_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            uv_plane.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(uv_bytes_per_row),
                rows_per_image: Some(cbcr_height),
            },
            wgpu::Extent3d {
                width: uv_extent_width,
                height: cbcr_height.max(1),
                depth_or_array_layers: 1,
            },
        );
    }

    GpuFrameTextures::Nv12 {
        y_texture,
        cb_cr_texture: cbcr_texture,
        width,
        height,
    }
}

/// Interleave YUV420p's separate U and V planes into a single CbCr plane
/// (Rg8Unorm) and upload as NV12 dual-texture.
///
/// This avoids the expensive CPU YUV→RGB conversion — we just interleave bytes.
fn upload_yuv420p_as_nv12(
    frame: CpuFrameRef<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    let width = frame.width;
    let height = frame.height;

    // CbCr texture: half resolution, Rg8Unorm (create before Y upload for early-return)
    let cbcr_width = width.div_ceil(2);
    let cbcr_height = height.div_ceil(2);
    let cbcr_texture = get_or_create_texture(
        device,
        cbcr_cache,
        cbcr_width.max(1),
        cbcr_height.max(1),
        wgpu::TextureFormat::Rg8Unorm,
        "lumina_video_cbcr",
    );

    // Y texture: full resolution R8Unorm
    let y_texture = get_or_create_texture(
        device,
        y_cache,
        width,
        height,
        wgpu::TextureFormat::R8Unorm,
        "lumina_video_y",
    );

    // Upload Y plane (R8Unorm: 1 byte/pixel)
    // Compute layout from actual data to be robust against stride mismatches
    if let Some(y_plane) = frame.plane(0) {
        let y_bytes_per_row = if height > 0 {
            (y_plane.data.len() / height as usize) as u32
        } else {
            0
        };
        // For R8Unorm, 1 byte = 1 pixel; extent width ≤ bytes_per_row
        let y_extent_width = y_bytes_per_row.min(width);
        if y_bytes_per_row == 0 || y_extent_width == 0 {
            tracing::warn!("YUV420p Y plane: zero size, skipping upload");
            return GpuFrameTextures::Nv12 {
                y_texture,
                cb_cr_texture: cbcr_texture,
                width,
                height,
            };
        }
        safe_write_texture(
            queue,
            &y_texture,
            y_plane.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(y_bytes_per_row),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width: y_extent_width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    // Interleave U and V planes into CbCr (Rg8Unorm)
    let u_plane = frame.plane(1);
    let v_plane = frame.plane(2);

    match (u_plane, v_plane) {
        (Some(u), Some(v)) => {
            // Build interleaved CbCr buffer: [Cb, Cr, Cb, Cr, ...] per row
            let cbcr_stride = (cbcr_width as usize) * 2;
            let mut cbcr_data = vec![0u8; cbcr_stride * cbcr_height as usize];

            for row in 0..cbcr_height as usize {
                for col in 0..cbcr_width as usize {
                    let u_idx = row * u.stride + col;
                    let v_idx = row * v.stride + col;
                    let out_idx = row * cbcr_stride + col * 2;
                    if let (Some(output), Some(&value)) =
                        (cbcr_data.get_mut(out_idx), u.data.get(u_idx))
                    {
                        *output = value; // Cb (maps to R in RG8)
                    }
                    if let (Some(output), Some(&value)) =
                        (cbcr_data.get_mut(out_idx + 1), v.data.get(v_idx))
                    {
                        *output = value; // Cr (maps to G in RG8)
                    }
                }
            }

            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &cbcr_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &cbcr_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cbcr_stride as u32),
                    rows_per_image: Some(cbcr_height),
                },
                wgpu::Extent3d {
                    width: cbcr_width.max(1),
                    height: cbcr_height.max(1),
                    depth_or_array_layers: 1,
                },
            );
        }
        _ => {
            // Missing U or V plane — this shouldn't happen but don't crash
            tracing::warn!("YUV420p frame missing U or V plane");
        }
    }

    GpuFrameTextures::Nv12 {
        y_texture,
        cb_cr_texture: cbcr_texture,
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// RGBA passthrough path
// ---------------------------------------------------------------------------

fn upload_rgba(
    frame: CpuFrameRef<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    let width = frame.width;
    let height = frame.height;

    let texture = get_or_create_texture(
        device,
        cache,
        width,
        height,
        wgpu::TextureFormat::Rgba8Unorm,
        "lumina_video_rgba",
    );

    if let Some(plane) = frame.plane(0) {
        if frame.format == PixelFormat::Bgra {
            // BGRA → RGBA: swizzle in-place
            let rgba = bgra_to_rgba(plane.data);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba.as_slice(),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            // Prevent drop reuse warning
            let _ = rgba;
        } else {
            // RGBA: direct upload (Rgba8Unorm: 4 bytes/pixel)
            // Compute layout from actual data to be robust against stride mismatches
            let rgba_bytes_per_row = if height > 0 {
                (plane.data.len() / height as usize) as u32
            } else {
                0
            };
            // For Rgba8Unorm, 4 bytes = 1 pixel; extent width = bytes_per_row / 4
            let rgba_extent_width = (rgba_bytes_per_row / 4).min(width);
            if rgba_bytes_per_row == 0 || rgba_extent_width == 0 {
                tracing::warn!("RGBA upload: zero size, skipping upload");
                return GpuFrameTextures::Rgba {
                    texture,
                    width,
                    height,
                };
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                plane.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(rgba_bytes_per_row),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width: rgba_extent_width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
    }

    GpuFrameTextures::Rgba {
        texture,
        width,
        height,
    }
}

fn upload_rgb24(
    frame: CpuFrameRef<'_>,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    cache: &mut Option<Arc<wgpu::Texture>>,
) -> GpuFrameTextures {
    let width = frame.width;
    let height = frame.height;

    let texture = get_or_create_texture(
        device,
        cache,
        width,
        height,
        wgpu::TextureFormat::Rgba8Unorm,
        "lumina_video_rgba",
    );

    // Convert RGB24 → RGBA (add alpha=255)
    let rgba = rgb24_to_rgba(frame);
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        rgba.as_slice(),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    GpuFrameTextures::Rgba {
        texture,
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get an existing texture with matching size, or create a new one.
fn get_or_create_texture(
    device: &wgpu::Device,
    cache: &mut Option<Arc<wgpu::Texture>>,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> Arc<wgpu::Texture> {
    let needs_create = match cache {
        Some(ref tex) => tex.width() != width || tex.height() != height || tex.format() != format,
        None => true,
    };

    if needs_create {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        *cache = Some(Arc::new(texture));
    }

    match cache.as_ref() {
        Some(texture) => Arc::clone(texture),
        None => Arc::new(device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })),
    }
}

// ---------------------------------------------------------------------------
// CPU YUV→RGB conversion (kept for the legacy single-texture path)
// ---------------------------------------------------------------------------

/// Convert a CpuFrame to RGBA pixel data (CPU conversion).
///
/// Handles YUV420p, NV12, RGB24, BGRA, and RGBA formats.
/// YUV→RGB uses BT.601 limited-range conversion.
pub fn cpu_frame_to_rgba(frame: &CpuFrame) -> Vec<u8> {
    cpu_frame_ref_to_rgba(CpuFrameRef::from_cpu(frame))
}

fn cpu_frame_ref_to_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    match frame.format {
        PixelFormat::Rgba => frame
            .plane(0)
            .map(|plane| plane.data.to_vec())
            .unwrap_or_default(),
        PixelFormat::Bgra => frame
            .plane(0)
            .map(|plane| bgra_to_rgba(plane.data))
            .unwrap_or_default(),
        PixelFormat::Rgb24 => rgb24_to_rgba(frame),
        PixelFormat::Yuv420p => yuv420p_to_rgba(frame),
        PixelFormat::Nv12 => nv12_to_rgba(frame),
    }
}

/// BT.601 limited-range YUV→RGB conversion with range expansion.
///
/// Y ∈ [16, 235], U/V ∈ [16, 240] → expanded to full range → R,G,B ∈ [0, 255].
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    // Range expansion: limited → full
    let y = ((y as f32 - 16.0) * 255.0 / 219.0).max(0.0);
    let u = (u as f32 - 128.0) * 255.0 / 224.0;
    let v = (v as f32 - 128.0) * 255.0 / 224.0;

    // BT.601 coefficients (applied to full-range Y'CbCr)
    let r = y + 1.402 * v;
    let g = y - 0.344136 * u - 0.714136 * v;
    let b = y + 1.772 * u;

    (
        r.clamp(0.0, 255.0) as u8,
        g.clamp(0.0, 255.0) as u8,
        b.clamp(0.0, 255.0) as u8,
    )
}

fn bgra_to_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks_exact(4) {
        out.push(chunk[2]);
        out.push(chunk[1]);
        out.push(chunk[0]);
        out.push(chunk[3]);
    }
    out
}

fn rgb24_to_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let Some(plane) = frame.plane(0) else {
        return Vec::new();
    };
    let data = plane.data;
    let stride = plane.stride;
    let mut out = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        for col in 0..width {
            let idx = row * stride + col * 3;
            if idx + 2 < data.len() {
                out.push(data[idx]); // R
                out.push(data[idx + 1]); // G
                out.push(data[idx + 2]); // B
                out.push(255); // A
            } else {
                out.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    out
}

fn yuv420p_to_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let Some(y_plane) = frame.plane(0).map(|plane| plane.data) else {
        return Vec::new();
    };
    let Some(u_plane) = frame.plane(1).map(|plane| plane.data) else {
        return Vec::new();
    };
    let Some(v_plane) = frame.plane(2).map(|plane| plane.data) else {
        return Vec::new();
    };

    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let uv_idx = (y / 2) * (width / 2) + (x / 2);
            let y_value = y_plane.get(idx).copied().unwrap_or(0);
            let u_value = u_plane.get(uv_idx).copied().unwrap_or(128);
            let v_value = v_plane.get(uv_idx).copied().unwrap_or(128);
            let (r, g, b) = yuv_to_rgb(y_value, u_value, v_value);
            let out_idx = idx * 4;
            rgba[out_idx] = r;
            rgba[out_idx + 1] = g;
            rgba[out_idx + 2] = b;
            rgba[out_idx + 3] = 255;
        }
    }
    rgba
}

fn nv12_to_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let Some(y_plane) = frame.plane(0).map(|plane| plane.data) else {
        return Vec::new();
    };
    let Some(uv_plane) = frame.plane(1).map(|plane| plane.data) else {
        return Vec::new();
    };

    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let idx = y * width + x;
            let uv_idx = (y / 2) * (width / 2) * 2 + (x / 2) * 2;
            let y_value = y_plane.get(idx).copied().unwrap_or(0);
            let u_value = uv_plane.get(uv_idx).copied().unwrap_or(128);
            let v_value = uv_plane.get(uv_idx + 1).copied().unwrap_or(128);
            let (r, g, b) = yuv_to_rgb(y_value, u_value, v_value);
            let out_idx = idx * 4;
            rgba[out_idx] = r;
            rgba[out_idx + 1] = g;
            rgba[out_idx + 2] = b;
            rgba[out_idx + 3] = 255;
        }
    }
    rgba
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use lumina_video_native_frame::{CpuMemory, CpuPlane, FrameExtent, NativeFrameDescriptor};

    #[test]
    fn test_bgra_to_rgba() {
        let bgra = vec![0u8, 128, 255, 255];
        let rgba = bgra_to_rgba(&bgra);
        assert_eq!(rgba, vec![255, 128, 0, 255]);
    }

    #[test]
    fn test_rgb24_to_rgba() {
        let frame = CpuFrame {
            format: PixelFormat::Rgb24,
            width: 2,
            height: 1,
            planes: vec![Plane {
                data: vec![255, 0, 0, 0, 255, 0],
                stride: 6,
            }],
        };
        let rgba = rgb24_to_rgba(CpuFrameRef::from_cpu(&frame));
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn test_yuv_to_rgb_black() {
        let (r, g, b) = yuv_to_rgb(16, 128, 128);
        assert_eq!((r, g, b), (0, 0, 0));
    }

    #[test]
    fn test_yuv_to_rgb_white() {
        let (r, g, b) = yuv_to_rgb(235, 128, 128);
        assert_eq!((r, g, b), (255, 255, 255));
    }

    #[test]
    fn owned_cpu_lease_view_borrows_plane_bytes() -> Result<(), Box<dyn std::error::Error>> {
        let bytes = vec![1, 2, 3, 4];
        let bytes_ptr = bytes.as_ptr();
        let lease = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 1,
                stream_generation: 1,
                pts: Duration::ZERO,
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: PixelFormat::Rgba,
            },
            NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(bytes, 4)])),
            AcquireSync::None,
        )?;
        let lease = ensure_native_frame_lease_supported(lease)?;
        let NativeMemory::Cpu(memory) = lease.memory else {
            return Err("expected CPU memory".into());
        };
        let view = CpuFrameRef::from_memory(&memory, 1, 1, PixelFormat::Rgba);
        let plane = view.plane(0).ok_or("missing CPU plane")?;
        assert_eq!(plane.data.as_ptr(), bytes_ptr);
        assert_eq!(plane.stride, 4);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sync_file_lease_is_rejected_and_returned() -> Result<(), Box<dyn std::error::Error>> {
        use std::fs::File;
        use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

        let fd = File::open("/dev/null")?.into_raw_fd();
        let lease = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 2,
                stream_generation: 1,
                pts: Duration::ZERO,
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: PixelFormat::Rgba,
            },
            NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(vec![0; 4], 4)])),
            AcquireSync::SyncFile(unsafe {
                // SAFETY: ownership of the descriptor is transferred exactly once.
                OwnedFd::from_raw_fd(fd)
            }),
        )?;
        let error = ensure_native_frame_lease_supported(lease)
            .err()
            .ok_or("accepted sync file")?;
        let returned = error.into_lease();
        assert!(matches!(returned.acquire, AcquireSync::SyncFile(_)));
        Ok(())
    }
}
