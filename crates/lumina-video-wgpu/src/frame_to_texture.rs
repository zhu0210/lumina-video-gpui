//! Convert decoded video frames to wgpu textures suitable for GPUI's `surface()`.
//!
//! Two rendering paths are supported:
//!
//! | Path | Format | Textures | GPUI `surface()` call |
//! |------|--------|----------|-----------------------|
//! | **NV12 native** | NV12, YUV420p | Y (R8Unorm) + CbCr (Rg8Unorm) | `surface((y_tex, cbcr_tex, size, transform))` — GPU-side YUV→RGB |
//! | **RGBA passthrough** | RGBA, BGRA, RGB24 | Single RGBA8Unorm | `surface((tex, desc))` |
//!
//! YUV420p frames are cheaply interleaved (U+V → CbCr) to use the NV12 path,
//! avoiding the expensive CPU YUV→RGB conversion. Only NV12 and YUV420p use
//! the NV12 path; RGB formats use RGBA passthrough.
//!
//! Platform GPU surfaces remain outside the borrowed compatibility boundary;
//! producers must hand an owned lease to [`native_frame_lease_to_textures`].

#[cfg(test)]
use lumina_video_native_frame::apply_yuv_matrix;
#[cfg(test)]
use lumina_video_native_frame::video::Plane;
use lumina_video_native_frame::video::{CpuFrame, DecodedFrame, PixelFormat};
use lumina_video_native_frame::{
    nv12_bytes_to_rgba_into, render_decision, yuv420p_bytes_to_rgba_into, yuv_to_rgb_matrix,
    AcquireSync, ChromaHorizontal, ChromaVertical, ColorMatrix, ColorMetadata, ColorRange,
    ColorRenderDecision, ColorTransfer, CpuMemory, NativeFrameDescriptor, NativeFrameLease,
    NativeMemory,
};
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
    let Some(bytes_per_pixel) = texture.format().block_copy_size(None) else {
        return;
    };
    if !valid_upload_layout(data.len(), layout, extent, bytes_per_pixel) {
        tracing::warn!("write_texture skipped: invalid plane stride or truncated buffer");
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

// These uploads use uncompressed, single-layer planes. The final row need not
// include padding, and trailing allocation bytes do not describe the row stride.
fn valid_upload_layout(
    data_len: usize,
    layout: wgpu::TexelCopyBufferLayout,
    extent: wgpu::Extent3d,
    bytes_per_pixel: u32,
) -> bool {
    let row_bytes = u64::from(extent.width) * u64::from(bytes_per_pixel);
    let stride = u64::from(layout.bytes_per_row.unwrap_or(0));
    if bytes_per_pixel == 0
        || !stride.is_multiple_of(u64::from(bytes_per_pixel))
        || !layout.offset.is_multiple_of(u64::from(bytes_per_pixel))
        || extent.width == 0
        || extent.height == 0
        || extent.depth_or_array_layers != 1
        || stride < row_bytes
        || layout
            .rows_per_image
            .is_some_and(|rows| rows < extent.height)
    {
        return false;
    }
    layout
        .offset
        .checked_add(stride * u64::from(extent.height - 1))
        .and_then(|size| size.checked_add(row_bytes))
        .is_some_and(|size| size <= data_len as u64)
}

fn upload_plane(queue: &wgpu::Queue, texture: &wgpu::Texture, plane: PlaneRef<'_>) {
    let Ok(stride) = u32::try_from(plane.stride) else {
        return;
    };
    safe_write_texture(
        queue,
        texture,
        plane.data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(texture.height()),
        },
        texture.size(),
    );
}

// ---------------------------------------------------------------------------
// GPU frame textures — the output of frame upload (fed to GPUI surface())
// ---------------------------------------------------------------------------

/// GPU representation of a decoded video frame.
///
/// Variants map directly to [`gpui::SurfaceSource`] conversions:
/// - `Nv12` → `surface((y_tex, cbcr_tex, native_size, color_transform))`
/// - `Rgba` → `surface((tex, descriptor))`
#[derive(Clone)]
pub enum GpuFrameTextures {
    /// Two-plane NV12: Y (R8Unorm) + interleaved CbCr (Rg8Unorm).
    /// Use with `surface((y_texture, cb_cr_texture, native_size, color_transform))`.
    Nv12 {
        y_texture: Arc<wgpu::Texture>,
        cb_cr_texture: Arc<wgpu::Texture>,
        width: u32,
        height: u32,
        color_transform: [[f32; 4]; 4],
    },
    /// Single RGBA8Unorm or BGRA8Unorm texture (sampling handles channel order).
    /// Use with `surface((texture, descriptor))`.
    Rgba {
        texture: Arc<wgpu::Texture>,
        width: u32,
        height: u32,
    },
}

fn default_nv12_color_transform() -> [[f32; 4]; 4] {
    yuv_to_rgb_matrix(ColorMatrix::Bt601, ColorRange::Full).unwrap_or([[0.0; 4]; 4])
}

fn legacy_cpu_nv12_color_transform() -> [[f32; 4]; 4] {
    yuv_to_rgb_matrix(ColorMatrix::Bt601, ColorRange::Limited).unwrap_or([[0.0; 4]; 4])
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

/// A decoded frame could not cross the legacy rendering boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyFrameIngestionError {
    /// Native GPU surfaces are reserved for the owned producer seam in #7.
    UnsupportedNativeSurface,
    /// A supported native surface could not be imported by this renderer.
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        all(target_os = "windows", feature = "windows-native-video")
    ))]
    NativeImportFailed,
}

impl std::fmt::Display for LegacyFrameIngestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedNativeSurface => write!(
                f,
                "borrowed native GPU surfaces are unsupported; use the owned lease seam"
            ),
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                all(target_os = "windows", feature = "windows-native-video")
            ))]
            Self::NativeImportFailed => write!(f, "native GPU surface import failed"),
        }
    }
}

impl std::error::Error for LegacyFrameIngestionError {}

/// Uploads CPU frames or aliases Apple IOSurfaces with GPU-tracked producer leases.
/// Apple imports do not copy pixels; unsupported native formats return an error.
pub fn decoded_frame_to_textures(
    frame: &DecodedFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Result<GpuFrameTextures, LegacyFrameIngestionError> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if let DecodedFrame::MacOS(surface) = frame {
        return import_macos_frame(surface, device);
    }
    #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
    if let DecodedFrame::Windows(surface) = frame {
        return import_windows_frame(surface, device);
    }
    let cpu = classify_legacy_frame(frame)?;
    Ok(upload_cpu_frame_as_textures(
        cpu, device, queue, y_cache, cbcr_cache, rgba_cache,
    ))
}

#[cfg(all(target_os = "windows", feature = "windows-native-video"))]
fn import_windows_frame(
    surface: &lumina_video_native_frame::video::WindowsGpuSurface,
    device: &wgpu::Device,
) -> Result<GpuFrameTextures, LegacyFrameIngestionError> {
    let format = match surface.format {
        PixelFormat::Bgra => wgpu::TextureFormat::Bgra8Unorm,
        PixelFormat::Rgba => wgpu::TextureFormat::Rgba8Unorm,
        _ => {
            surface.request_cpu_fallback();
            return Err(LegacyFrameIngestionError::UnsupportedNativeSurface);
        }
    };
    let owner = surface.clone();
    // SAFETY: the decoder completes its D3D11 copy before publishing the frame.
    // The callback retains the pool lease and shared handle until wgpu retires
    // GPU uses; the importer validates the D3D12 layout and shared-state contract.
    let texture = unsafe {
        crate::zero_copy::windows::import_d3d11_shared_handle(
            device,
            windows::Win32::Foundation::HANDLE(surface.shared_handle.0),
            surface.width,
            surface.height,
            format,
            Some(Box::new(move || drop(owner))),
        )
    }
    .map_err(|error| {
        surface.request_cpu_fallback();
        tracing::warn!("D3D11 shared texture import failed; requested CPU fallback: {error}");
        LegacyFrameIngestionError::NativeImportFailed
    })?;
    Ok(GpuFrameTextures::Rgba {
        texture: Arc::new(texture),
        width: surface.width,
        height: surface.height,
    })
}

/// Bind the decoder pool lease to wgpu's GPU-tracked texture destruction.
/// Retaining only IOSurface would not prevent CVPixelBuffer pool reuse.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn import_macos_frame(
    surface: &lumina_video_native_frame::video::MacOSGpuSurface,
    device: &wgpu::Device,
) -> Result<GpuFrameTextures, LegacyFrameIngestionError> {
    let format = match surface.format {
        PixelFormat::Bgra => wgpu::TextureFormat::Bgra8Unorm,
        PixelFormat::Rgba => wgpu::TextureFormat::Rgba8Unorm,
        _ => return Err(LegacyFrameIngestionError::UnsupportedNativeSurface),
    };
    let owner = surface.clone();
    // SAFETY: MacOSGpuSurface retains the decoder-completed CVPixelBuffer and
    // guarantees its extent/format. The HAL drop callback keeps its pool lease
    // alive through GPU-tracked texture destruction, preventing producer reuse
    // while any renderer submission still samples the image.
    let texture = unsafe {
        crate::zero_copy::macos::import_iosurface_with_drop_callback(
            device,
            surface.io_surface,
            surface.width,
            surface.height,
            format,
            Some(Box::new(move || drop(owner))),
        )
    }
    .map_err(|error| {
        tracing::warn!("IOSurface import failed: {error}");
        LegacyFrameIngestionError::NativeImportFailed
    })?;
    Ok(GpuFrameTextures::Rgba {
        texture: Arc::new(texture),
        width: surface.width,
        height: surface.height,
    })
}

fn classify_legacy_frame(frame: &DecodedFrame) -> Result<&CpuFrame, LegacyFrameIngestionError> {
    match frame {
        DecodedFrame::Cpu(cpu) => Ok(cpu),

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        DecodedFrame::MacOS(_) => Err(LegacyFrameIngestionError::UnsupportedNativeSurface),

        #[cfg(target_os = "linux")]
        DecodedFrame::Linux(_) => Err(LegacyFrameIngestionError::UnsupportedNativeSurface),

        #[cfg(target_os = "android")]
        DecodedFrame::Android(_) => Err(LegacyFrameIngestionError::UnsupportedNativeSurface),

        #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
        DecodedFrame::Windows(_) => Err(LegacyFrameIngestionError::UnsupportedNativeSurface),
    }
}

/// Failure while handing an owned native frame to the renderer.
#[derive(Debug)]
pub enum NativeFrameIngestionError {
    /// The renderer has no contract for consuming the producer's acquire fence.
    UnsupportedAcquireSync(NativeFrameLease),
    /// External-memory import is not safe until the renderer supplies layout and sync state.
    UnsupportedDmaBuf(NativeFrameLease),
    /// The direct upload seam intentionally accepts only owned RGBA or NV12 bytes.
    UnsupportedCpuFormat(NativeFrameLease),
    /// The native frame carries color metadata outside the supported SDR
    /// matrix/range contract.
    UnsupportedColorMetadata(NativeFrameLease),
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
            Self::UnsupportedCpuFormat(_) => write!(
                f,
                "owned CPU ingestion supports only direct RGBA and NV12 uploads"
            ),
            Self::UnsupportedColorMetadata(_) => write!(
                f,
                "owned NV12 ingestion requires supported SDR color metadata"
            ),
        }
    }
}

impl std::error::Error for NativeFrameIngestionError {}

impl NativeFrameIngestionError {
    /// Returns the unchanged lease so the caller can retry at a boundary that owns synchronization.
    pub fn into_lease(self) -> NativeFrameLease {
        match self {
            Self::UnsupportedAcquireSync(lease)
            | Self::UnsupportedDmaBuf(lease)
            | Self::UnsupportedCpuFormat(lease)
            | Self::UnsupportedColorMetadata(lease) => lease,
        }
    }
}

fn owned_nv12_color_transform(color: ColorMetadata) -> Option<[[f32; 4]; 4]> {
    // The legacy two-plane result carries only a matrix, unlike the native
    // DMA-BUF result which also carries its transfer function to the renderer.
    if color.transfer != ColorTransfer::Srgb {
        return None;
    }
    match render_decision(color) {
        ColorRenderDecision::Gpu(transform) => Some(transform),
        ColorRenderDecision::CpuRgba(_)
        | ColorRenderDecision::Unsupported
        | ColorRenderDecision::UnsupportedSdrColor => None,
    }
}

/// Uploads an owned native frame without cloning its CPU planes.
///
/// The GStreamer worker has already made the one-time color decision. This
/// seam therefore only uploads worker-produced RGBA or an exact GPUI-shader
/// NV12 frame; it never allocates a CPU conversion scratch buffer.
#[allow(clippy::result_large_err)]
pub fn native_frame_lease_to_textures(
    lease: NativeFrameLease,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_cache: &mut Option<Arc<wgpu::Texture>>,
    cbcr_cache: &mut Option<Arc<wgpu::Texture>>,
    rgba_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Result<GpuFrameTextures, NativeFrameIngestionError> {
    let (descriptor, memory) = classify_native_frame_lease(lease)?;
    if descriptor.format == PixelFormat::Nv12 {
        let Some(transform) = owned_nv12_color_transform(descriptor.color) else {
            return Err(NativeFrameIngestionError::UnsupportedColorMetadata(
                NativeFrameLease {
                    descriptor,
                    memory: NativeMemory::Cpu(memory),
                    acquire: AcquireSync::None,
                },
            ));
        };
        let frame = CpuFrameRef::from_memory(
            &memory,
            descriptor.extent.width,
            descriptor.extent.height,
            descriptor.format,
        );
        return Ok(upload_nv12(
            frame, device, queue, y_cache, cbcr_cache, transform,
        ));
    }
    let frame = CpuFrameRef::from_memory(
        &memory,
        descriptor.extent.width,
        descriptor.extent.height,
        descriptor.format,
    );
    Ok(upload_cpu_frame_ref_as_textures(
        frame, device, queue, y_cache, cbcr_cache, rgba_cache,
    ))
}

#[allow(clippy::result_large_err)]
fn classify_native_frame_lease(
    lease: NativeFrameLease,
) -> Result<(NativeFrameDescriptor, CpuMemory), NativeFrameIngestionError> {
    if !matches!(&lease.acquire, AcquireSync::None) {
        return Err(NativeFrameIngestionError::UnsupportedAcquireSync(lease));
    }
    let NativeFrameLease {
        descriptor,
        memory,
        acquire,
    } = lease;
    match memory {
        NativeMemory::Cpu(memory) => {
            if !matches!(descriptor.format, PixelFormat::Rgba | PixelFormat::Nv12) {
                return Err(NativeFrameIngestionError::UnsupportedCpuFormat(
                    NativeFrameLease {
                        descriptor,
                        memory: NativeMemory::Cpu(memory),
                        acquire,
                    },
                ));
            }
            Ok((descriptor, memory))
        }
        #[cfg(target_os = "linux")]
        NativeMemory::DmaBuf(memory) => Err(NativeFrameIngestionError::UnsupportedDmaBuf(
            NativeFrameLease {
                descriptor,
                memory: NativeMemory::DmaBuf(memory),
                acquire,
            },
        )),
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
        PixelFormat::Nv12 => upload_nv12(
            frame,
            device,
            queue,
            y_cache,
            cbcr_cache,
            default_nv12_color_transform(),
        ),
        PixelFormat::Yuv420p => upload_yuv420p_as_nv12(
            frame,
            device,
            queue,
            y_cache,
            cbcr_cache,
            default_nv12_color_transform(),
        ),
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

/// Converts a borrowed CPU `DecodedFrame` to one RGBA texture.
///
/// Native GPU surfaces are rejected; this compatibility helper never imports
/// them or consumes their optional CPU fallback. Prefer
/// [`decoded_frame_to_textures`] for the GPUI NV12 path.
#[deprecated(note = "use decoded_frame_to_textures for the legacy CPU boundary")]
pub fn decoded_frame_to_texture(
    frame: &DecodedFrame,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture_cache: &mut Option<Arc<wgpu::Texture>>,
) -> Result<Arc<wgpu::Texture>, LegacyFrameIngestionError> {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    if let DecodedFrame::MacOS(surface) = frame {
        return match import_macos_frame(surface, device)? {
            GpuFrameTextures::Rgba { texture, .. } => Ok(texture),
            GpuFrameTextures::Nv12 { .. } => {
                Err(LegacyFrameIngestionError::UnsupportedNativeSurface)
            }
        };
    }
    #[cfg(all(target_os = "windows", feature = "windows-native-video"))]
    if let DecodedFrame::Windows(surface) = frame {
        return match import_windows_frame(surface, device)? {
            GpuFrameTextures::Rgba { texture, .. } => Ok(texture),
            GpuFrameTextures::Nv12 { .. } => {
                Err(LegacyFrameIngestionError::UnsupportedNativeSurface)
            }
        };
    }
    let cpu = classify_legacy_frame(frame)?;
    Ok(upload_cpu_frame(cpu, device, queue, texture_cache))
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
    color_transform: [[f32; 4]; 4],
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

    if let Some(plane) = frame.plane(0) {
        upload_plane(queue, &y_texture, plane);
    }
    if let Some(plane) = frame.plane(1) {
        upload_plane(queue, &cbcr_texture, plane);
    }

    GpuFrameTextures::Nv12 {
        y_texture,
        cb_cr_texture: cbcr_texture,
        width,
        height,
        color_transform,
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
    color_transform: [[f32; 4]; 4],
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

    if let Some(plane) = frame.plane(0) {
        upload_plane(queue, &y_texture, plane);
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
        color_transform,
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
    // Native BGRA sampling performs the swizzle without a per-frame CPU copy.
    let format = if frame.format == PixelFormat::Bgra {
        wgpu::TextureFormat::Bgra8Unorm
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let texture = get_or_create_texture(device, cache, width, height, format, "lumina_video_rgba");
    if let Some(plane) = frame.plane(0) {
        upload_plane(queue, &texture, plane);
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
        PixelFormat::Rgba | PixelFormat::Bgra => packed_rgba(frame),
        PixelFormat::Rgb24 => rgb24_to_rgba(frame),
        PixelFormat::Yuv420p => yuv420p_to_rgba(frame),
        PixelFormat::Nv12 => nv12_to_rgba(frame),
    }
}

#[cfg(test)]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let [r, g, b] = apply_yuv_matrix(&legacy_cpu_nv12_color_transform(), y, u, v);
    (r, g, b)
}

fn packed_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    let Some(plane) = frame.plane(0) else {
        return Vec::new();
    };
    let Some(row_bytes) = (frame.width as usize).checked_mul(4) else {
        return Vec::new();
    };
    let Some(size) = row_bytes.checked_mul(frame.height as usize) else {
        return Vec::new();
    };
    if row_bytes == 0 || plane.stride < row_bytes {
        return Vec::new();
    }
    let mut out = vec![0; size];
    for (row, output) in out.chunks_exact_mut(row_bytes).enumerate() {
        let Some(start) = row.checked_mul(plane.stride) else {
            break;
        };
        let Some(end) = start.checked_add(row_bytes) else {
            break;
        };
        if let Some(input) = plane.data.get(start..end) {
            output.copy_from_slice(input);
            if frame.format == PixelFormat::Bgra {
                for pixel in output.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
            }
        }
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
            if let Some(pixel) = data.get(idx..idx.saturating_add(3)) {
                let (Some(&r), Some(&g), Some(&b)) = (pixel.first(), pixel.get(1), pixel.get(2))
                else {
                    out.extend_from_slice(&[0, 0, 0, 255]);
                    continue;
                };
                out.extend_from_slice(&[r, g, b, 255]);
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
    let Some(y_plane_ref) = frame.plane(0) else {
        return Vec::new();
    };
    let Some(u_plane_ref) = frame.plane(1) else {
        return Vec::new();
    };
    let Some(v_plane_ref) = frame.plane(2) else {
        return Vec::new();
    };
    let y_plane = y_plane_ref.data;
    let u_plane = u_plane_ref.data;
    let v_plane = v_plane_ref.data;
    let y_stride = y_plane_ref.stride;
    let u_stride = u_plane_ref.stride;
    let v_stride = v_plane_ref.stride;

    let mut rgba = vec![0u8; width * height * 4];
    let _ = yuv420p_bytes_to_rgba_into(
        y_plane,
        y_stride,
        u_plane,
        u_stride,
        v_plane,
        v_stride,
        lumina_video_native_frame::FrameExtent::new(frame.width, frame.height),
        &legacy_cpu_nv12_color_transform(),
        &mut rgba,
    );
    rgba
}

fn nv12_to_rgba(frame: CpuFrameRef<'_>) -> Vec<u8> {
    let mut rgba = Vec::new();
    cpu_frame_ref_to_rgba_into(frame, &mut rgba, legacy_cpu_nv12_color_transform());
    rgba
}

fn cpu_frame_ref_to_rgba_into(
    frame: CpuFrameRef<'_>,
    rgba: &mut Vec<u8>,
    transform: [[f32; 4]; 4],
) {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let Some(pixel_count) = width.checked_mul(height) else {
        rgba.clear();
        return;
    };
    let Some(byte_count) = pixel_count.checked_mul(4) else {
        rgba.clear();
        return;
    };
    rgba.resize(byte_count, 0);
    let Some(y_plane) = frame.plane(0) else {
        return;
    };
    let Some(uv_plane) = frame.plane(1) else {
        return;
    };
    let color = ColorMetadata {
        matrix: ColorMatrix::Bt601,
        primaries: lumina_video_native_frame::ColorPrimaries::Bt709,
        transfer: ColorTransfer::Srgb,
        range: ColorRange::Limited,
        chroma_horizontal: ChromaHorizontal::Centered,
        chroma_vertical: ChromaVertical::Centered,
    };
    let _ = nv12_bytes_to_rgba_into(
        y_plane.data,
        y_plane.stride,
        uv_plane.data,
        uv_plane.stride,
        lumina_video_native_frame::FrameExtent::new(frame.width, frame.height),
        color,
        &transform,
        rgba,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    use lumina_video_native_frame::video::LinuxGpuSurface;
    use lumina_video_native_frame::{
        ChromaHorizontal, ChromaVertical, ColorMatrix, ColorMetadata, ColorPrimaries, ColorRange,
        ColorTransfer, CpuMemory, CpuPlane, FrameExtent, NativeFrameDescriptor,
    };
    #[cfg(target_os = "linux")]
    use lumina_video_native_frame::{
        DmaBufFormatPlane, DmaBufMemory, DmaBufMemoryPlane, DmaBufObject,
    };

    #[test]
    fn upload_layout_preserves_stride_without_requiring_last_row_padding() {
        let layout = wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(12),
            rows_per_image: Some(2),
        };
        let extent = wgpu::Extent3d {
            width: 2,
            height: 2,
            depth_or_array_layers: 1,
        };
        assert!(valid_upload_layout(20, layout, extent, 4));
        assert!(valid_upload_layout(32, layout, extent, 4));
        assert!(!valid_upload_layout(19, layout, extent, 4));
        assert!(!valid_upload_layout(
            32,
            wgpu::TexelCopyBufferLayout {
                bytes_per_row: Some(4),
                ..layout
            },
            extent,
            4
        ));
        assert!(!valid_upload_layout(
            32,
            wgpu::TexelCopyBufferLayout {
                bytes_per_row: Some(9),
                ..layout
            },
            extent,
            4
        ));
        assert!(!valid_upload_layout(
            32,
            wgpu::TexelCopyBufferLayout {
                offset: u64::MAX,
                ..layout
            },
            extent,
            4
        ));
    }

    #[test]
    fn packed_rgba_respects_padding_and_bgra_channel_order() {
        for (format, expected) in [
            (PixelFormat::Rgba, vec![1, 2, 3, 4, 5, 6, 7, 8]),
            (PixelFormat::Bgra, vec![3, 2, 1, 4, 7, 6, 5, 8]),
        ] {
            let frame = CpuFrameRef {
                format,
                width: 1,
                height: 2,
                planes: [
                    Some(PlaneRef {
                        data: &[1, 2, 3, 4, 99, 99, 99, 99, 5, 6, 7, 8],
                        stride: 8,
                    }),
                    None,
                    None,
                ],
            };
            assert_eq!(cpu_frame_ref_to_rgba(frame), expected);
        }
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
    fn owned_gpu_decision_is_consumed_as_a_plain_column_matrix() {
        let metadata = ColorMetadata {
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: ColorTransfer::Srgb,
            range: ColorRange::Limited,
            chroma_horizontal: ChromaHorizontal::Centered,
            chroma_vertical: ChromaVertical::Centered,
        };
        assert!(matches!(
            lumina_video_native_frame::render_decision(metadata),
            lumina_video_native_frame::ColorRenderDecision::Gpu(_)
        ));
    }

    #[test]
    fn owned_nv12_boundary_rejects_non_shader_color_contracts() {
        let centered = ColorMetadata {
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: ColorTransfer::Srgb,
            range: ColorRange::Limited,
            chroma_horizontal: ChromaHorizontal::Centered,
            chroma_vertical: ChromaVertical::Centered,
        };
        let Some(expected) = yuv_to_rgb_matrix(centered.matrix, centered.range) else {
            panic!("BT.709 limited transform must exist");
        };
        assert_eq!(owned_nv12_color_transform(centered), Some(expected));

        let mut cosited = centered;
        cosited.chroma_horizontal = ChromaHorizontal::Cosited;
        let mut non_srgb = centered;
        non_srgb.transfer = ColorTransfer::Bt709;
        let mut adobe = centered;
        adobe.primaries = ColorPrimaries::Adobergb;
        let hdr = ColorMetadata {
            matrix: ColorMatrix::Bt2020,
            primaries: ColorPrimaries::Bt2020,
            transfer: ColorTransfer::Smpte2084,
            ..centered
        };
        for rejected in [cosited, non_srgb, adobe, hdr] {
            assert!(owned_nv12_color_transform(rejected).is_none());
        }
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
                color: lumina_video_native_frame::ColorMetadata::default(),
            },
            NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(bytes, 4)])),
            AcquireSync::None,
        )?;
        let (_, memory) = classify_native_frame_lease(lease)?;
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
        use std::os::fd::OwnedFd;

        let file = File::open("/dev/null")?;
        let lease = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 2,
                stream_generation: 1,
                pts: Duration::ZERO,
                duration: None,
                extent: FrameExtent::new(1, 1),
                format: PixelFormat::Rgba,
                color: lumina_video_native_frame::ColorMetadata::default(),
            },
            NativeMemory::Cpu(CpuMemory::new(vec![CpuPlane::new(vec![0; 4], 4)])),
            AcquireSync::SyncFile(OwnedFd::from(file)),
        )?;
        let error = classify_native_frame_lease(lease)
            .err()
            .ok_or("accepted sync file")?;
        assert!(matches!(
            &error,
            NativeFrameIngestionError::UnsupportedAcquireSync(_)
        ));
        let returned = error.into_lease();
        let AcquireSync::SyncFile(fd) = returned.acquire else {
            return Err("sync file was not returned".into());
        };
        assert!(File::from(fd).metadata().is_ok());
        Ok(())
    }

    #[test]
    fn unsupported_owned_cpu_formats_return_the_lease() -> Result<(), Box<dyn std::error::Error>> {
        for format in [PixelFormat::Yuv420p, PixelFormat::Bgra, PixelFormat::Rgb24] {
            let first_bytes = vec![1, 2, 3, 4];
            let first_bytes_ptr = first_bytes.as_ptr();
            let mut planes = vec![CpuPlane::new(first_bytes, 4)];
            planes.extend((1..format.num_planes()).map(|_| CpuPlane::new(vec![0; 4], 4)));
            let lease = NativeFrameLease::new(
                NativeFrameDescriptor {
                    frame_id: 3,
                    stream_generation: 1,
                    pts: Duration::ZERO,
                    duration: None,
                    extent: FrameExtent::new(1, 1),
                    format,
                    color: lumina_video_native_frame::ColorMetadata::default(),
                },
                NativeMemory::Cpu(CpuMemory::new(planes)),
                AcquireSync::None,
            )?;
            let error = classify_native_frame_lease(lease)
                .err()
                .ok_or("accepted unsupported CPU format")?;
            assert!(matches!(
                &error,
                NativeFrameIngestionError::UnsupportedCpuFormat(_)
            ));
            let returned = error.into_lease();
            assert_eq!(returned.descriptor.format, format);
            let NativeMemory::Cpu(memory) = returned.memory else {
                return Err("expected CPU memory".into());
            };
            let first_plane = memory.planes.first().ok_or("missing CPU plane")?;
            assert_eq!(first_plane.bytes.as_ptr(), first_bytes_ptr);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn owned_dmabuf_lease_is_rejected_and_returned() -> Result<(), Box<dyn std::error::Error>> {
        use std::fs::File;
        use std::os::fd::OwnedFd;

        let file = File::open("/dev/null")?;
        let lease = NativeFrameLease::new(
            NativeFrameDescriptor {
                frame_id: 4,
                stream_generation: 2,
                pts: Duration::from_millis(7),
                duration: Some(Duration::from_millis(33)),
                extent: FrameExtent::new(1, 1),
                format: PixelFormat::Rgba,
                color: lumina_video_native_frame::ColorMetadata::default(),
            },
            NativeMemory::DmaBuf(DmaBufMemory::new(
                vec![DmaBufObject {
                    fd: OwnedFd::from(file),
                    size: Some(32),
                }],
                vec![DmaBufMemoryPlane {
                    object: 0,
                    offset: 16,
                    size: Some(8),
                }],
                vec![DmaBufFormatPlane {
                    memory_plane: 0,
                    offset: 4,
                    stride: 4,
                    size: Some(4),
                }],
                Some(0x3432_5241),
                Some(0),
            )?),
            AcquireSync::None,
        )?;

        let error = classify_native_frame_lease(lease)
            .err()
            .ok_or("accepted DMABuf lease")?;
        assert!(matches!(
            &error,
            NativeFrameIngestionError::UnsupportedDmaBuf(_)
        ));
        let returned = error.into_lease();
        assert_eq!(returned.descriptor.frame_id, 4);
        assert_eq!(returned.descriptor.stream_generation, 2);
        assert_eq!(returned.descriptor.extent, FrameExtent::new(1, 1));

        let NativeMemory::DmaBuf(memory) = returned.memory else {
            return Err("expected DMABuf memory".into());
        };
        assert_eq!(memory.objects.len(), 1);
        assert_eq!(memory.memory_planes.len(), 1);
        assert_eq!(memory.format_planes.len(), 1);
        let object = memory.objects.first().ok_or("missing DMABuf object")?;
        assert_eq!(object.size, Some(32));
        let memory_plane = memory
            .memory_planes
            .first()
            .ok_or("missing DMABuf memory plane")?;
        assert_eq!(memory_plane.object, 0);
        assert_eq!(memory_plane.offset, 16);
        assert_eq!(memory_plane.size, Some(8));
        let plane = memory.format_planes.first().ok_or("missing DMABuf plane")?;
        assert_eq!(plane.memory_plane, 0);
        assert_eq!(plane.offset, 4);
        assert_eq!(plane.stride, 4);
        assert!(File::from(object.fd.try_clone()?).metadata().is_ok());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn borrowed_linux_surface_is_classified_as_unsupported(
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::fs::File;
        use std::os::fd::{FromRawFd, IntoRawFd};
        use std::sync::Arc;

        let raw_fd = File::open("/dev/null")?.into_raw_fd();
        let surface = unsafe {
            // SAFETY: raw_fd is a valid descriptor and the Arc owner outlives the surface.
            LinuxGpuSurface::new_single_plane(
                raw_fd,
                1,
                1,
                PixelFormat::Rgba,
                0,
                4,
                0,
                4,
                None,
                Arc::new(()),
            )
        };
        let frame = DecodedFrame::Linux(surface);
        let error = classify_legacy_frame(&frame)
            .err()
            .ok_or("accepted borrowed Linux GPU surface")?;
        assert_eq!(error, LegacyFrameIngestionError::UnsupportedNativeSurface);
        let surface = frame.as_linux_surface().ok_or("missing Linux surface")?;
        assert_eq!(surface.primary_fd(), raw_fd);
        let file = unsafe {
            // SAFETY: LinuxGpuSurface only borrows this raw descriptor; this closes the test fd.
            File::from_raw_fd(surface.primary_fd())
        };
        assert!(file.metadata().is_ok());
        Ok(())
    }
}
