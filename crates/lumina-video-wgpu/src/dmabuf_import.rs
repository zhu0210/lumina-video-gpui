//! Linux external DMABuf import for one native NV12 texture.
//!
//! This seam accepts an exported [`AcquireSync::SyncFile`] without waiting on
//! it. The normal renderer submission consumes that producer-write snapshot
//! before sampling the externally owned image.

use crate::zero_copy::linux::{
    find_memory_type_index, EXT_EXTERNAL_MEMORY_DMA_BUF, EXT_IMAGE_DRM_FORMAT_MODIFIER,
    KHR_EXTERNAL_MEMORY_FD,
};
use ash::vk;
use lumina_video_native_frame::video::PixelFormat;
use lumina_video_native_frame::{
    render_decision, AcquireSync, ColorRenderDecision, DmaBufMemory, NativeFrameLease, NativeMemory,
};
use std::fmt;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

const DRM_FORMAT_NV12: u32 = 0x3231_564e;
const PLANE_COUNT: usize = 2;
// ponytail: keep the capability query allocation-free; reject pathological
// drivers instead of adding a per-frame heap allocation for this tiny list.
const MAX_DRM_MODIFIER_PROPERTIES: usize = 64;

/// One NV12 plane's object-local layout after resolving both descriptor layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Nv12ImportPlane {
    /// Index into the source [`DmaBufMemory::objects`] vector.
    object: usize,
    /// Effective offset within that object.
    offset: u64,
    /// Bytes between adjacent rows.
    stride: u32,
    /// Bytes occupied by this plane.
    size: u64,
}

/// Vulkan memory binding shape selected by the source DMABuf objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Nv12ImportBinding {
    /// Both format planes use one unique DMABuf object and one Vulkan memory bind.
    Shared { object: usize },
    /// Each format plane uses a distinct DMABuf object and gets its own bind.
    Disjoint { objects: [usize; 2] },
}

impl Nv12ImportBinding {
    fn is_disjoint(self) -> bool {
        matches!(self, Self::Disjoint { .. })
    }
}

/// Pure, allocation-free description of an NV12 DMABuf import.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Nv12ImportPlan {
    /// Exact DRM fourcc supplied by the producer.
    drm_fourcc: u32,
    /// Exact DRM format modifier supplied by the producer; zero is linear.
    modifier: u64,
    /// Plane 0 (Y) and plane 1 (UV) object-local layouts.
    planes: [Nv12ImportPlane; 2],
    /// Shared or disjoint Vulkan memory binding selected from the plane objects.
    binding: Nv12ImportBinding,
}

impl Nv12ImportPlan {
    /// Resolves a validated native-frame DMABuf description without touching FDs.
    fn from_memory(memory: &DmaBufMemory) -> Result<Self, Nv12ImportPlanError> {
        memory
            .validate()
            .map_err(|error| Nv12ImportPlanError::InvalidReferences(error.to_string()))?;

        let drm_fourcc = memory
            .drm_fourcc
            .ok_or(Nv12ImportPlanError::MissingDrmFourcc)?;
        if drm_fourcc != DRM_FORMAT_NV12 {
            return Err(Nv12ImportPlanError::UnsupportedDrmFourcc(drm_fourcc));
        }
        let modifier = memory
            .modifier
            .ok_or(Nv12ImportPlanError::MissingModifier)?;
        if memory.format_planes.len() != PLANE_COUNT {
            return Err(Nv12ImportPlanError::PlaneCount {
                actual: memory.format_planes.len(),
            });
        }

        let mut planes = [
            Nv12ImportPlane {
                object: 0,
                offset: 0,
                stride: 0,
                size: 0,
            },
            Nv12ImportPlane {
                object: 0,
                offset: 0,
                stride: 0,
                size: 0,
            },
        ];

        for (index, format_plane) in memory.format_planes.iter().enumerate() {
            let Some(memory_plane) = memory.memory_planes.get(format_plane.memory_plane) else {
                return Err(Nv12ImportPlanError::InvalidReferences(format!(
                    "format plane {index} references missing memory plane {}",
                    format_plane.memory_plane
                )));
            };
            let Some(object) = memory.objects.get(memory_plane.object) else {
                return Err(Nv12ImportPlanError::InvalidReferences(format!(
                    "memory plane {} references missing object {}",
                    format_plane.memory_plane, memory_plane.object
                )));
            };
            let Some(size) = format_plane.size else {
                return Err(Nv12ImportPlanError::MissingPlaneSize { plane: index });
            };
            if format_plane.stride == 0 {
                return Err(Nv12ImportPlanError::ZeroStride { plane: index });
            }
            if size == 0 {
                return Err(Nv12ImportPlanError::ZeroPlaneSize { plane: index });
            }
            if let Some(memory_size) = memory_plane.size {
                let Some(end) = format_plane.offset.checked_add(size) else {
                    return Err(Nv12ImportPlanError::PlaneOffsetOverflow { plane: index });
                };
                if end > memory_size {
                    return Err(Nv12ImportPlanError::PlaneOutsideMemoryView { plane: index });
                }
            }
            let Some(offset) = memory_plane.offset.checked_add(format_plane.offset) else {
                return Err(Nv12ImportPlanError::PlaneOffsetOverflow { plane: index });
            };
            if let Some(object_size) = object.size {
                let Some(end) = offset.checked_add(size) else {
                    return Err(Nv12ImportPlanError::PlaneOffsetOverflow { plane: index });
                };
                if end > object_size {
                    return Err(Nv12ImportPlanError::PlaneOutsideObject { plane: index });
                }
            }
            let Some(plane) = planes.get_mut(index) else {
                return Err(Nv12ImportPlanError::PlaneCount {
                    actual: memory.format_planes.len(),
                });
            };
            *plane = Nv12ImportPlane {
                object: memory_plane.object,
                offset,
                stride: format_plane.stride,
                size,
            };
        }

        for (memory_plane, _) in memory.memory_planes.iter().enumerate() {
            if !memory
                .format_planes
                .iter()
                .any(|plane| plane.memory_plane == memory_plane)
            {
                return Err(Nv12ImportPlanError::UnusedMemoryPlane { memory_plane });
            }
        }
        for (object, _) in memory.objects.iter().enumerate() {
            if !planes.iter().any(|plane| plane.object == object) {
                return Err(Nv12ImportPlanError::UnusedObject { object });
            }
        }

        let [plane0, plane1] = planes;
        let binding = if plane0.object == plane1.object {
            Nv12ImportBinding::Shared {
                object: plane0.object,
            }
        } else {
            Nv12ImportBinding::Disjoint {
                objects: [plane0.object, plane1.object],
            }
        };

        Ok(Self {
            drm_fourcc,
            modifier,
            planes,
            binding,
        })
    }
}

/// Rejection from the pure NV12 plan builder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Nv12ImportPlanError {
    InvalidReferences(String),
    MissingDrmFourcc,
    UnsupportedDrmFourcc(u32),
    MissingModifier,
    PlaneCount { actual: usize },
    MissingPlaneSize { plane: usize },
    ZeroStride { plane: usize },
    ZeroPlaneSize { plane: usize },
    PlaneOffsetOverflow { plane: usize },
    PlaneOutsideMemoryView { plane: usize },
    PlaneOutsideObject { plane: usize },
    UnusedMemoryPlane { memory_plane: usize },
    UnusedObject { object: usize },
}

impl fmt::Display for Nv12ImportPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReferences(reason) => write!(f, "invalid DMABuf references: {reason}"),
            Self::MissingDrmFourcc => write!(f, "DMABuf DRM fourcc is missing"),
            Self::UnsupportedDrmFourcc(fourcc) => {
                write!(f, "DMABuf DRM fourcc 0x{fourcc:08x} is not NV12")
            }
            Self::MissingModifier => write!(f, "DMABuf DRM modifier is missing"),
            Self::PlaneCount { actual } => write!(f, "NV12 requires 2 planes, got {actual}"),
            Self::MissingPlaneSize { plane } => {
                write!(f, "NV12 plane {plane} does not have an exact size")
            }
            Self::ZeroStride { plane } => write!(f, "NV12 plane {plane} has a zero stride"),
            Self::ZeroPlaneSize { plane } => write!(f, "NV12 plane {plane} has a zero size"),
            Self::PlaneOffsetOverflow { plane } => {
                write!(f, "NV12 plane {plane} offset overflowed")
            }
            Self::PlaneOutsideMemoryView { plane } => {
                write!(f, "NV12 plane {plane} exceeds its memory view")
            }
            Self::PlaneOutsideObject { plane } => {
                write!(f, "NV12 plane {plane} exceeds its DMABuf object")
            }
            Self::UnusedMemoryPlane { memory_plane } => {
                write!(
                    f,
                    "NV12 layout leaves memory plane {memory_plane} unreferenced"
                )
            }
            Self::UnusedObject { object } => {
                write!(f, "NV12 layout leaves DMABuf object {object} unreferenced")
            }
        }
    }
}

impl std::error::Error for Nv12ImportPlanError {}

/// One imported multiplanar NV12 texture and the renderer acquire fence.
#[derive(Debug)]
pub struct ImportedNv12Texture {
    pub texture: Arc<wgpu::Texture>,
    pub sync_file: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub color_transform: [[f32; 4]; 4],
    /// Encoded transfer function applied by the renderer after the YUV matrix.
    pub color_transfer: lumina_video_native_frame::ColorTransfer,
}

/// Failure from [`import_external_dmabuf_nv12`]. Every variant retains the source lease.
#[derive(Debug)]
pub enum Nv12ImportError {
    UnsupportedAcquireSync(NativeFrameLease),
    UnsupportedFrame(NativeFrameLease),
    InvalidLayout {
        lease: NativeFrameLease,
        reason: String,
    },
    ImportFailed {
        lease: NativeFrameLease,
        reason: String,
    },
}

impl Nv12ImportError {
    /// Returns the unchanged source lease for a caller-owned fallback or retry.
    pub fn into_lease(self) -> NativeFrameLease {
        match self {
            Self::UnsupportedAcquireSync(lease)
            | Self::UnsupportedFrame(lease)
            | Self::InvalidLayout { lease, .. }
            | Self::ImportFailed { lease, .. } => lease,
        }
    }
}

impl fmt::Display for Nv12ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedAcquireSync(_) => write!(
                f,
                "external DMABuf NV12 import requires AcquireSync::SyncFile"
            ),
            Self::UnsupportedFrame(_) => {
                write!(f, "external DMABuf NV12 import requires NV12 DMABuf")
            }
            Self::InvalidLayout { reason, .. } => {
                write!(f, "invalid NV12 DMABuf layout: {reason}")
            }
            Self::ImportFailed { reason, .. } => write!(f, "NV12 DMABuf import failed: {reason}"),
        }
    }
}

impl std::error::Error for Nv12ImportError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ValidatedTextureWrap {
    usage: wgpu::TextureUses,
    initial_state: wgpu::TextureUses,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalPreflightError {
    UnsupportedAcquireSync,
    UnsupportedFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExplicitPlaneLayout {
    offset: u64,
    row_pitch: u64,
    // Vulkan requires this field to be zero for an explicit DRM modifier
    // layout. The source plane size remains in `Nv12ImportPlan` for bounds
    // validation; it is deliberately not passed to VkImageCreateInfo.
    size: u64,
}

fn explicit_modifier_plane_layouts(plan: &Nv12ImportPlan) -> [ExplicitPlaneLayout; PLANE_COUNT] {
    plan.planes.map(|plane| ExplicitPlaneLayout {
        offset: plane.offset,
        row_pitch: u64::from(plane.stride),
        size: 0,
    })
}

fn nv12_memory_plane_aspects() -> [vk::ImageAspectFlags; PLANE_COUNT] {
    [
        vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
        vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
    ]
}

fn validate_external_preflight(
    acquire: &AcquireSync,
    format: PixelFormat,
    width: u32,
    height: u32,
) -> Result<(), ExternalPreflightError> {
    if !matches!(acquire, AcquireSync::SyncFile(_)) {
        return Err(ExternalPreflightError::UnsupportedAcquireSync);
    }
    if format != PixelFormat::Nv12
        || width == 0
        || height == 0
        || !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
    {
        return Err(ExternalPreflightError::UnsupportedFrame);
    }
    Ok(())
}

fn validated_texture_wrap() -> ValidatedTextureWrap {
    // These are tracker states, not Vulkan `VkImageCreateInfo::initialLayout`.
    // The latter remains UNDEFINED below solely because Vulkan requires a legal
    // creation-time value; no transition is issued by this importer.
    ValidatedTextureWrap {
        usage: wgpu::TextureUses::RESOURCE,
        initial_state: wgpu::TextureUses::RESOURCE,
    }
}

/// Imports an externally owned NV12 DMABuf as one native wgpu NV12 texture.
///
/// The producer-write snapshot in [`AcquireSync::SyncFile`] is handed to the
/// renderer; this function never waits for producer completion. The imported
/// image is externally owned in `GENERAL` through `FOREIGN`, while wgpu tracks
/// it as [`wgpu::TextureUses::RESOURCE`] until the normal renderer acquire.
///
/// # Safety
///
/// The caller must guarantee that the producer has handed the image to foreign
/// external ownership in actual Vulkan layout `GENERAL`, and that the
/// [`AcquireSync::SyncFile`] snapshots unfinished producer writes. The normal
/// renderer submission consumes that fence before sampling; this importer does
/// not wait or submit work. The lease's descriptor and FD payload must remain
/// stable and truthful for the duration of the call.
#[allow(clippy::result_large_err)]
pub unsafe fn import_external_dmabuf_nv12(
    lease: NativeFrameLease,
    device: &wgpu::Device,
) -> Result<ImportedNv12Texture, Nv12ImportError> {
    match validate_external_preflight(
        &lease.acquire,
        lease.descriptor.format,
        lease.descriptor.extent.width,
        lease.descriptor.extent.height,
    ) {
        Ok(()) => {}
        Err(ExternalPreflightError::UnsupportedAcquireSync) => {
            return Err(Nv12ImportError::UnsupportedAcquireSync(lease));
        }
        Err(ExternalPreflightError::UnsupportedFrame) => {
            return Err(Nv12ImportError::UnsupportedFrame(lease));
        }
    }
    let wrap = validated_texture_wrap();
    let plan = match &lease.memory {
        NativeMemory::DmaBuf(memory) => match Nv12ImportPlan::from_memory(memory) {
            Ok(plan) => plan,
            Err(error) => {
                return Err(Nv12ImportError::InvalidLayout {
                    lease,
                    reason: error.to_string(),
                });
            }
        },
        NativeMemory::Cpu(_) => return Err(Nv12ImportError::UnsupportedFrame(lease)),
    };
    if !device
        .features()
        .contains(wgpu::Features::TEXTURE_FORMAT_NV12)
    {
        return Err(Nv12ImportError::ImportFailed {
            lease,
            reason: "wgpu TEXTURE_FORMAT_NV12 feature is unavailable".to_string(),
        });
    }

    // SAFETY: The HAL guard is borrowed only for this synchronous import. All
    // Vulkan handles handed to wgpu are created from this device and remain
    // alive through the HAL texture drop callback.
    let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return Err(Nv12ImportError::ImportFailed {
            lease,
            reason: "wgpu device is not using Vulkan".to_string(),
        });
    };

    let extensions = hal_device.enabled_device_extensions();
    if !extensions.contains(&KHR_EXTERNAL_MEMORY_FD)
        || !extensions.contains(&EXT_EXTERNAL_MEMORY_DMA_BUF)
        || !extensions.contains(&EXT_IMAGE_DRM_FORMAT_MODIFIER)
    {
        return Err(Nv12ImportError::ImportFailed {
            lease,
            reason: "required Vulkan DMABuf/modifier extensions are unavailable".to_string(),
        });
    }

    import_vulkan_nv12(device, hal_device, lease, plan, wrap)
}

struct ImageGuard {
    device: ash::Device,
    image: Option<vk::Image>,
}

impl ImageGuard {
    fn new(device: ash::Device, image: vk::Image) -> Self {
        Self {
            device,
            image: Some(image),
        }
    }

    fn destroy_now(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        // SAFETY: This guard owns the image and no Vulkan operation is using it
        // after the synchronous import path has returned.
        unsafe { self.device.destroy_image(image, None) };
    }
}

impl Drop for ImageGuard {
    fn drop(&mut self) {
        self.destroy_now();
    }
}

struct MemoryGuard {
    device: ash::Device,
    memory: Option<vk::DeviceMemory>,
}

struct ImportResources {
    image: ImageGuard,
    memories: [Option<MemoryGuard>; PLANE_COUNT],
}

impl Drop for ImportResources {
    fn drop(&mut self) {
        self.image.destroy_now();
        // Struct fields are dropped after this method returns, so every
        // VkDeviceMemory is freed after its image has been destroyed.
    }
}

impl MemoryGuard {
    fn new(device: ash::Device, memory: vk::DeviceMemory) -> Self {
        Self {
            device,
            memory: Some(memory),
        }
    }

    fn handle(&self) -> Option<vk::DeviceMemory> {
        self.memory
    }
}

impl Drop for MemoryGuard {
    fn drop(&mut self) {
        let Some(memory) = self.memory.take() else {
            return;
        };
        // SAFETY: This guard owns the imported VkDeviceMemory and frees it only
        // after its image has been destroyed by the callback/order above.
        unsafe { self.device.free_memory(memory, None) };
    }
}

fn duplicate_fd(fd: &OwnedFd) -> Result<OwnedFd, String> {
    // SAFETY: `dup` receives a live descriptor borrowed from the source lease.
    let duplicated = unsafe { libc::dup(fd.as_raw_fd()) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: A non-negative `dup` result is a fresh descriptor owned by this
    // value and is closed exactly once by `OwnedFd` unless Vulkan takes it.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn duplicate_object_fd(memory: &DmaBufMemory, object: usize) -> Result<OwnedFd, String> {
    let Some(source_object) = memory.objects.get(object) else {
        return Err(format!("DMABuf object {object} is missing"));
    };
    duplicate_fd(&source_object.fd)
}

fn allocate_imported_memory(
    raw_device: &ash::Device,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    requirements: vk::MemoryRequirements,
    fd: OwnedFd,
) -> Result<MemoryGuard, String> {
    let external_memory_fd = ash::khr::external_memory_fd::Device::new(instance, raw_device);
    let mut fd_properties = vk::MemoryFdPropertiesKHR::default();
    // SAFETY: The duplicated descriptor remains owned by this function while
    // Vulkan queries the memory types that can import it.
    unsafe {
        external_memory_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            fd.as_raw_fd(),
            &mut fd_properties,
        )
    }
    .map_err(|error| format!("vkGetMemoryFdPropertiesKHR failed: {error:?}"))?;
    let type_bits = requirements.memory_type_bits & fd_properties.memory_type_bits;
    let memory_type_index = find_memory_type_index(
        instance,
        physical_device,
        type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        find_memory_type_index(
            instance,
            physical_device,
            type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    })
    .ok_or_else(|| "no compatible Vulkan memory type for imported DMABuf".to_string())?;
    let raw_fd = fd.as_raw_fd();
    let mut import_info = vk::ImportMemoryFdInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
        .fd(raw_fd);
    let allocate_info = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut import_info);
    // SAFETY: `allocate_info` references live stack-owned pNext data and the FD
    // remains owned here until Vulkan reports successful ownership transfer.
    let memory = unsafe { raw_device.allocate_memory(&allocate_info, None) }
        .map_err(|error| format!("vkAllocateMemory failed: {error:?}"))?;
    let _ = fd.into_raw_fd();
    Ok(MemoryGuard::new(raw_device.clone(), memory))
}

fn validate_modifier_support(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    modifier: u64,
    disjoint: bool,
) -> Result<(), String> {
    let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut format_properties = vk::FormatProperties2::default().push_next(&mut modifier_list);
    // SAFETY: `physical_device` belongs to `instance`; both query structures
    // are initialized and live for the duration of this synchronous call.
    unsafe {
        instance.get_physical_device_format_properties2(
            physical_device,
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            &mut format_properties,
        );
    }
    let modifier_count = modifier_list.drm_format_modifier_count as usize;
    if modifier_count == 0 {
        return Err("Vulkan advertises no DRM modifier properties for NV12".to_string());
    }

    if modifier_count > MAX_DRM_MODIFIER_PROPERTIES {
        return Err(format!(
            "Vulkan advertises {modifier_count} NV12 DRM modifiers; importer supports at most {MAX_DRM_MODIFIER_PROPERTIES}"
        ));
    }
    let mut modifier_properties =
        [vk::DrmFormatModifierPropertiesEXT::default(); MAX_DRM_MODIFIER_PROPERTIES];
    let returned_count = {
        let Some(properties_slice) = modifier_properties.get_mut(..modifier_count) else {
            return Err("Vulkan DRM modifier property count cannot be represented".to_string());
        };
        let mut modifier_list = vk::DrmFormatModifierPropertiesListEXT::default()
            .drm_format_modifier_properties(properties_slice);
        let mut format_properties = vk::FormatProperties2::default().push_next(&mut modifier_list);
        // SAFETY: The property slice is sized from Vulkan's first count query
        // and is borrowed by the pNext chain only for this second query.
        unsafe {
            instance.get_physical_device_format_properties2(
                physical_device,
                vk::Format::G8_B8R8_2PLANE_420_UNORM,
                &mut format_properties,
            );
        }
        (modifier_list.drm_format_modifier_count as usize).min(modifier_count)
    };
    let Some(properties) = modifier_properties
        .get(..returned_count)
        .and_then(|properties| {
            properties
                .iter()
                .find(|properties| properties.drm_format_modifier == modifier)
        })
    else {
        return Err(format!(
            "Vulkan does not advertise DRM modifier 0x{modifier:016x} for NV12"
        ));
    };
    if properties.drm_format_modifier_plane_count != PLANE_COUNT as u32 {
        return Err(format!(
            "DRM modifier 0x{modifier:016x} advertises {} planes; NV12 import requires {PLANE_COUNT}",
            properties.drm_format_modifier_plane_count
        ));
    }
    if !properties
        .drm_format_modifier_tiling_features
        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
    {
        return Err(format!(
            "DRM modifier 0x{modifier:016x} does not support sampled NV12 images"
        ));
    }
    if disjoint
        && !properties
            .drm_format_modifier_tiling_features
            .contains(vk::FormatFeatureFlags::DISJOINT)
    {
        return Err(format!(
            "DRM modifier 0x{modifier:016x} does not support disjoint NV12 images"
        ));
    }
    Ok(())
}

fn image_create_flags(binding: Nv12ImportBinding) -> vk::ImageCreateFlags {
    let mut flags = vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE;
    if binding.is_disjoint() {
        flags |= vk::ImageCreateFlags::DISJOINT;
    }
    flags
}

#[allow(clippy::result_large_err)]
fn import_vulkan_nv12(
    device: &wgpu::Device,
    hal_device: impl std::ops::Deref<Target = wgpu::hal::vulkan::Device>,
    lease: NativeFrameLease,
    plan: Nv12ImportPlan,
    wrap: ValidatedTextureWrap,
) -> Result<ImportedNv12Texture, Nv12ImportError> {
    let NativeFrameLease {
        descriptor,
        memory,
        acquire,
    } = lease;
    let width = descriptor.extent.width;
    let height = descriptor.extent.height;
    let NativeMemory::DmaBuf(memory) = memory else {
        return Err(Nv12ImportError::UnsupportedFrame(NativeFrameLease {
            descriptor,
            memory,
            acquire,
        }));
    };
    let explicit_layouts = explicit_modifier_plane_layouts(&plan);
    let plane_layouts = explicit_layouts.map(|layout| {
        vk::SubresourceLayout::default()
            .offset(layout.offset)
            .size(layout.size)
            .row_pitch(layout.row_pitch)
    });
    let mut external_memory_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(plan.modifier)
        .plane_layouts(&plane_layouts);
    let image_create_info = vk::ImageCreateInfo::default()
        .flags(image_create_flags(plan.binding))
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        // The producer's actual externally owned image is GENERAL/FOREIGN at
        // this seam. Vulkan image creation permits UNDEFINED here as a
        // creation-only value; it is not the wgpu tracker state, which is
        // passed explicitly to create_texture_from_hal, and no transition is
        // submitted by this importer.
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_memory_info)
        .push_next(&mut modifier_info);
    let raw_device = hal_device.raw_device();
    let raw_instance = hal_device.shared_instance().raw_instance();
    let physical_device = hal_device.raw_physical_device();
    if let Err(reason) = validate_modifier_support(
        raw_instance,
        physical_device,
        plan.modifier,
        plan.binding.is_disjoint(),
    ) {
        return Err(Nv12ImportError::ImportFailed {
            lease: NativeFrameLease {
                descriptor,
                memory: NativeMemory::DmaBuf(memory),
                acquire,
            },
            reason,
        });
    }
    // SAFETY: The create info is fully initialized with valid DRM modifier and
    // external-memory pNext chains; the device belongs to the wgpu HAL guard.
    let image = match unsafe { raw_device.create_image(&image_create_info, None) } {
        Ok(image) => image,
        Err(error) => {
            return Err(Nv12ImportError::ImportFailed {
                lease: NativeFrameLease {
                    descriptor,
                    memory: NativeMemory::DmaBuf(memory),
                    acquire,
                },
                reason: format!("vkCreateImage failed: {error:?}"),
            });
        }
    };
    let mut resources = ImportResources {
        image: ImageGuard::new(raw_device.clone(), image),
        memories: [None, None],
    };

    let bind_result = (|| -> Result<(), String> {
        match plan.binding {
            Nv12ImportBinding::Shared { object } => {
                // SAFETY: The image belongs to this device and was just created.
                let requirements = unsafe { raw_device.get_image_memory_requirements(image) };
                let fd = duplicate_object_fd(&memory, object)?;
                let memory_guard = allocate_imported_memory(
                    raw_device,
                    raw_instance,
                    physical_device,
                    requirements,
                    fd,
                )?;
                let Some(memory_slot) = resources.memories.get_mut(0) else {
                    return Err("shared memory guard slot is unavailable".to_string());
                };
                *memory_slot = Some(memory_guard);
                let Some(memory_handle) = resources
                    .memories
                    .first()
                    .and_then(Option::as_ref)
                    .and_then(MemoryGuard::handle)
                else {
                    return Err("shared memory guard lost its allocation".to_string());
                };
                // SAFETY: The image and imported memory were created on this device;
                // the shared NV12 image requires exactly one ordinary bind.
                if let Err(error) = unsafe { raw_device.bind_image_memory(image, memory_handle, 0) }
                {
                    return Err(format!("vkBindImageMemory failed: {error:?}"));
                }
                Ok(())
            }
            Nv12ImportBinding::Disjoint { objects } => {
                let [object0, object1] = objects;
                let [memory_plane0_aspect, memory_plane1_aspect] = nv12_memory_plane_aspects();
                let requirements = [
                    plane_memory_requirements(raw_device, image, memory_plane0_aspect),
                    plane_memory_requirements(raw_device, image, memory_plane1_aspect),
                ];
                let [requirements0, requirements1] = requirements;
                let fd0 = duplicate_object_fd(&memory, object0)?;
                let fd1 = duplicate_object_fd(&memory, object1)?;
                let memory0 = allocate_imported_memory(
                    raw_device,
                    raw_instance,
                    physical_device,
                    requirements0,
                    fd0,
                )?;
                let Some(memory_slot0) = resources.memories.get_mut(0) else {
                    return Err("plane 0 memory guard slot is unavailable".to_string());
                };
                *memory_slot0 = Some(memory0);
                let memory1 = allocate_imported_memory(
                    raw_device,
                    raw_instance,
                    physical_device,
                    requirements1,
                    fd1,
                )?;
                let Some(memory_slot1) = resources.memories.get_mut(1) else {
                    return Err("plane 1 memory guard slot is unavailable".to_string());
                };
                *memory_slot1 = Some(memory1);
                let Some(memory_handle0) = resources
                    .memories
                    .first()
                    .and_then(Option::as_ref)
                    .and_then(MemoryGuard::handle)
                else {
                    return Err("plane 0 memory guard lost its allocation".to_string());
                };
                let Some(memory_handle1) = resources
                    .memories
                    .get(1)
                    .and_then(Option::as_ref)
                    .and_then(MemoryGuard::handle)
                else {
                    return Err("plane 1 memory guard lost its allocation".to_string());
                };
                let mut plane0_info =
                    vk::BindImagePlaneMemoryInfo::default().plane_aspect(memory_plane0_aspect);
                let mut plane1_info =
                    vk::BindImagePlaneMemoryInfo::default().plane_aspect(memory_plane1_aspect);
                let bind0 = vk::BindImageMemoryInfo::default()
                    .image(image)
                    .memory(memory_handle0)
                    .memory_offset(0)
                    .push_next(&mut plane0_info);
                let bind1 = vk::BindImageMemoryInfo::default()
                    .image(image)
                    .memory(memory_handle1)
                    .memory_offset(0)
                    .push_next(&mut plane1_info);
                // SAFETY: The image was created with DISJOINT and each bind carries
                // the matching plane aspect and memory allocation.
                if let Err(error) = unsafe { raw_device.bind_image_memory2(&[bind0, bind1]) } {
                    return Err(format!("vkBindImageMemory2 failed: {error:?}"));
                }
                Ok(())
            }
        }
    })();
    if let Err(reason) = bind_result {
        return Err(Nv12ImportError::ImportFailed {
            lease: NativeFrameLease {
                descriptor,
                memory: NativeMemory::DmaBuf(memory),
                acquire,
            },
            reason,
        });
    }

    let color_transform = match render_decision(descriptor.color) {
        ColorRenderDecision::Gpu(transform) => transform,
        _ => {
            return Err(Nv12ImportError::ImportFailed {
                lease: NativeFrameLease {
                    descriptor,
                    memory: NativeMemory::DmaBuf(memory),
                    acquire,
                },
                reason: "NV12 external import requires supported SDR color metadata".into(),
            });
        }
    };
    let sync_file = match acquire {
        AcquireSync::SyncFile(sync_file) => sync_file,
        AcquireSync::None => {
            return Err(Nv12ImportError::UnsupportedAcquireSync(NativeFrameLease {
                descriptor,
                memory: NativeMemory::DmaBuf(memory),
                acquire: AcquireSync::None,
            }));
        }
    };
    let lease = NativeFrameLease {
        descriptor,
        memory: NativeMemory::DmaBuf(memory),
        // The original producer fence is moved to ImportedNv12Texture for the
        // renderer boundary; the HAL callback retains the full backing lease
        // but must not retain a redundant second fence descriptor.
        acquire: AcquireSync::None,
    };
    let hal_descriptor = wgpu::hal::TextureDescriptor {
        label: Some("external DMABuf NV12"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::NV12,
        usage: wrap.usage,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    let drop_callback = Box::new(move || {
        drop(resources);
        drop(lease);
    });
    // SAFETY: The Vulkan image, descriptor, and callback satisfy the HAL
    // ownership contract; the callback retains the source lease and imported
    // memory until wgpu releases its final texture reference.
    let hal_texture = unsafe {
        hal_device.texture_from_raw(
            image,
            &hal_descriptor,
            Some(drop_callback),
            wgpu::hal::vulkan::TextureMemory::External,
        )
    };
    let texture_descriptor = wgpu::TextureDescriptor {
        label: Some("external DMABuf NV12"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::NV12,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    // SAFETY: `hal_texture` was created from this device and its descriptor;
    // the validated initialized state is passed verbatim to the forked hook.
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
            hal_texture,
            &texture_descriptor,
            wrap.initial_state,
            true, // Producer pixels are initialized; lazy clearing would destroy them.
        )
    };
    Ok(ImportedNv12Texture {
        texture: Arc::new(texture),
        sync_file,
        width,
        height,
        color_transform,
        color_transfer: descriptor.color.transfer,
    })
}

fn plane_memory_requirements(
    raw_device: &ash::Device,
    image: vk::Image,
    aspect: vk::ImageAspectFlags,
) -> vk::MemoryRequirements {
    let mut plane_info = vk::ImagePlaneMemoryRequirementsInfo::default().plane_aspect(aspect);
    let info = vk::ImageMemoryRequirementsInfo2::default()
        .image(image)
        .push_next(&mut plane_info);
    let mut requirements = vk::MemoryRequirements2::default();
    // SAFETY: The image is disjoint and the plane aspect identifies one valid
    // image plane for this memory-requirements query.
    unsafe { raw_device.get_image_memory_requirements2(&info, &mut requirements) };
    requirements.memory_requirements
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumina_video_native_frame::{DmaBufFormatPlane, DmaBufMemoryPlane, DmaBufObject};
    use std::fs::File;

    fn object(size: u64) -> Result<DmaBufObject, std::io::Error> {
        Ok(DmaBufObject {
            fd: OwnedFd::from(File::open("/dev/null")?),
            size: Some(size),
        })
    }

    fn shared_memory(modifier: u64) -> Result<DmaBufMemory, Box<dyn std::error::Error>> {
        Ok(DmaBufMemory::new(
            vec![object(4096)?],
            vec![DmaBufMemoryPlane {
                object: 0,
                offset: 64,
                size: Some(2048),
            }],
            vec![
                DmaBufFormatPlane {
                    memory_plane: 0,
                    offset: 16,
                    stride: 256,
                    size: Some(1024),
                },
                DmaBufFormatPlane {
                    memory_plane: 0,
                    offset: 1056,
                    stride: 256,
                    size: Some(512),
                },
            ],
            Some(DRM_FORMAT_NV12),
            Some(modifier),
        )?)
    }

    #[test]
    fn shared_plan_resolves_exact_object_offsets_for_linear_and_tiled_modifiers(
    ) -> Result<(), Box<dyn std::error::Error>> {
        for modifier in [0, 0x0102_0304_0506_0708] {
            let plan = Nv12ImportPlan::from_memory(&shared_memory(modifier)?)?;
            assert_eq!(plan.drm_fourcc, DRM_FORMAT_NV12);
            assert_eq!(plan.modifier, modifier);
            assert_eq!(
                plan.planes,
                [
                    Nv12ImportPlane {
                        object: 0,
                        offset: 80,
                        stride: 256,
                        size: 1024,
                    },
                    Nv12ImportPlane {
                        object: 0,
                        offset: 1120,
                        stride: 256,
                        size: 512,
                    },
                ]
            );
            assert_eq!(plan.binding, Nv12ImportBinding::Shared { object: 0 });
            assert_eq!(
                explicit_modifier_plane_layouts(&plan),
                [
                    ExplicitPlaneLayout {
                        offset: 80,
                        row_pitch: 256,
                        size: 0,
                    },
                    ExplicitPlaneLayout {
                        offset: 1120,
                        row_pitch: 256,
                        size: 0,
                    },
                ]
            );
        }
        Ok(())
    }

    #[test]
    fn drm_modifier_disjoint_binds_use_memory_plane_aspects() {
        assert_eq!(
            nv12_memory_plane_aspects(),
            [
                vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
            ]
        );
    }

    #[test]
    fn disjoint_plan_keeps_each_plane_object_and_layout() -> Result<(), Box<dyn std::error::Error>>
    {
        let memory = DmaBufMemory::new(
            vec![object(2048)?, object(1024)?],
            vec![
                DmaBufMemoryPlane {
                    object: 0,
                    offset: 32,
                    size: Some(1024),
                },
                DmaBufMemoryPlane {
                    object: 1,
                    offset: 48,
                    size: Some(512),
                },
            ],
            vec![
                DmaBufFormatPlane {
                    memory_plane: 0,
                    offset: 8,
                    stride: 128,
                    size: Some(512),
                },
                DmaBufFormatPlane {
                    memory_plane: 1,
                    offset: 16,
                    stride: 128,
                    size: Some(256),
                },
            ],
            Some(DRM_FORMAT_NV12),
            Some(0),
        )?;
        let plan = Nv12ImportPlan::from_memory(&memory)?;
        assert_eq!(plan.planes.first().map(|plane| plane.offset), Some(40));
        assert_eq!(plan.planes.get(1).map(|plane| plane.offset), Some(64));
        assert_eq!(
            plan.binding,
            Nv12ImportBinding::Disjoint { objects: [0, 1] }
        );
        assert_eq!(
            explicit_modifier_plane_layouts(&plan),
            [
                ExplicitPlaneLayout {
                    offset: 40,
                    row_pitch: 128,
                    size: 0,
                },
                ExplicitPlaneLayout {
                    offset: 64,
                    row_pitch: 128,
                    size: 0,
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn image_flags_keep_mutable_nv12_views_and_conditional_disjoint() {
        let base = vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE;
        assert_eq!(
            image_create_flags(Nv12ImportBinding::Shared { object: 0 }),
            base
        );
        assert_eq!(
            image_create_flags(Nv12ImportBinding::Disjoint { objects: [0, 1] }),
            base | vk::ImageCreateFlags::DISJOINT
        );
    }

    #[test]
    fn ambiguous_layouts_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let mut memory = shared_memory(0)?;
        memory.drm_fourcc = None;
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::MissingDrmFourcc)
        ));

        let mut memory = shared_memory(0)?;
        memory.modifier = None;
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::MissingModifier)
        ));

        let mut memory = shared_memory(0)?;
        memory.drm_fourcc = Some(0x3432_5241);
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::UnsupportedDrmFourcc(0x3432_5241))
        ));

        let mut memory = shared_memory(0)?;
        if let Some(plane) = memory.format_planes.get_mut(1) {
            plane.size = None;
        }
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::MissingPlaneSize { plane: 1 })
        ));

        let mut memory = shared_memory(0)?;
        memory.objects.push(object(32)?);
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::UnusedObject { object: 1 })
        ));

        let mut memory = shared_memory(0)?;
        memory.memory_planes.push(DmaBufMemoryPlane {
            object: 0,
            offset: 0,
            size: Some(1),
        });
        assert!(matches!(
            Nv12ImportPlan::from_memory(&memory),
            Err(Nv12ImportPlanError::UnusedMemoryPlane { memory_plane: 1 })
        ));
        Ok(())
    }

    #[test]
    fn texture_wrap_uses_resource_for_usage_and_tracker_state() {
        let wrap = validated_texture_wrap();
        assert_eq!(wrap.usage, wgpu::TextureUses::RESOURCE);
        assert_eq!(wrap.initial_state, wgpu::TextureUses::RESOURCE);
    }

    #[test]
    fn external_preflight_rejects_missing_fences_zero_and_odd_extents(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let sync_file = AcquireSync::SyncFile(OwnedFd::from(File::open("/dev/null")?));
        assert_eq!(
            validate_external_preflight(&sync_file, PixelFormat::Nv12, 2, 2),
            Ok(())
        );
        assert_eq!(
            validate_external_preflight(&sync_file, PixelFormat::Nv12, 0, 2),
            Err(ExternalPreflightError::UnsupportedFrame)
        );
        assert_eq!(
            validate_external_preflight(&AcquireSync::None, PixelFormat::Nv12, 2, 2),
            Err(ExternalPreflightError::UnsupportedAcquireSync)
        );
        assert_eq!(
            validate_external_preflight(&sync_file, PixelFormat::Nv12, 3, 2),
            Err(ExternalPreflightError::UnsupportedFrame)
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a genuine exported DMA-BUF FD and Vulkan NV12-capable device; no safe fixture is available"]
    fn hardware_import_requires_genuine_exported_dmabuf_fixture() {
        // /dev/null is deliberately not used here: this test must be supplied
        // with a real exporter fixture before it can exercise Vulkan import.
    }
}
