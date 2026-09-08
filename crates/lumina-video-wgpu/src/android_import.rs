//! Android AHardwareBuffer import and GPU conversion, recorded off the UI thread.
//! The renderer submits `commands` before sampling `texture`; this module never
//! submits to a Vulkan queue or waits for GPU completion.

use ash::vk;
use lumina_video_native_frame::android_video::AndroidVideoFrame;
use std::sync::Arc;

use crate::zero_copy::ZeroCopyError;

pub struct PreparedAndroidFrame {
    pub commands: Vec<wgpu::CommandBuffer>,
    pub texture: Arc<wgpu::Texture>,
    pub width: u32,
    pub height: u32,
}

#[derive(Default)]
pub struct AndroidFrameImporter {
    pipeline: Option<Arc<ConversionPipeline>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ConversionKey {
    format: vk::Format,
    external_format: u64,
    model: vk::SamplerYcbcrModelConversion,
    range: vk::SamplerYcbcrRange,
    x: vk::ChromaLocation,
    y: vk::ChromaLocation,
    components: [vk::ComponentSwizzle; 4],
}

struct ConversionPipeline {
    device: ash::Device,
    key: ConversionKey,
    conversion: vk::SamplerYcbcrConversion,
    sampler: vk::Sampler,
    descriptor_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    render_pass: vk::RenderPass,
    pipeline: vk::Pipeline,
}

impl Drop for ConversionPipeline {
    fn drop(&mut self) {
        // SAFETY: every frame retains this pipeline until GPU-tracked destruction;
        // these handles belong to this device and are destroyed in dependency order.
        unsafe {
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.descriptor_layout, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device
                .destroy_sampler_ycbcr_conversion(self.conversion, None);
            self.device.destroy_render_pass(self.render_pass, None);
        }
    }
}

struct FrameResources {
    pipeline: Arc<ConversionPipeline>,
    // The Java Image remains acquired, preventing ImageReader/MediaCodec pool reuse.
    _producer: Arc<AndroidVideoFrame>,
    input: vk::Image,
    input_memory: vk::DeviceMemory,
    input_view: vk::ImageView,
    output: vk::Image,
    output_memory: vk::DeviceMemory,
    output_view: vk::ImageView,
    descriptor_pool: vk::DescriptorPool,
    framebuffer: vk::Framebuffer,
}

impl Drop for FrameResources {
    fn drop(&mut self) {
        // SAFETY: before submission this object exclusively owns all resources;
        // afterwards the output texture's HAL drop callback runs only after GPU
        // use completes. Views/framebuffer die before images and backing memory.
        unsafe {
            let device = &self.pipeline.device;
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_descriptor_pool(self.descriptor_pool, None);
            device.destroy_image_view(self.input_view, None);
            device.destroy_image_view(self.output_view, None);
            device.destroy_image(self.input, None);
            device.destroy_image(self.output, None);
            device.free_memory(self.input_memory, None);
            device.free_memory(self.output_memory, None);
        }
    }
}

fn failure(error: impl std::fmt::Display) -> ZeroCopyError {
    ZeroCopyError::ImportFailed(error.to_string())
}

fn memory_type(bits: u32) -> Result<u32, ZeroCopyError> {
    if bits == 0 {
        return Err(failure("no compatible Vulkan memory type"));
    }
    Ok(bits.trailing_zeros())
}

impl AndroidFrameImporter {
    /// Prepare a producer-ready frame for the matching GPUI Vulkan device.
    ///
    /// No pixels are mapped or copied on the CPU. A single cached conversion
    /// pipeline is reused while the producer's format/color contract is stable.
    ///
    /// # Safety
    /// The frame must come from the native ImageReader bridge with its original
    /// handle/extent, an acquired Image lease, and a completed producer fence.
    /// The renderer must submit the returned commands before sampling its texture.
    pub unsafe fn prepare(
        &mut self,
        frame: Arc<AndroidVideoFrame>,
        device: &wgpu::Device,
    ) -> Result<PreparedAndroidFrame, ZeroCopyError> {
        if !frame.owns_producer_image()
            || frame.buffer.is_null()
            || frame.width == 0
            || frame.height == 0
            || frame.width > device.limits().max_texture_dimension_2d
            || frame.height > device.limits().max_texture_dimension_2d
            || frame.fence_fd >= 0
        {
            return Err(failure("expected a valid producer-ready Android frame"));
        }
        // SAFETY: `device` remains alive through the HAL borrow. The native bridge
        // retains the Image and has completed its bounded producer-fence wait.
        unsafe { self.prepare_vulkan(frame, device) }
    }

    unsafe fn prepare_vulkan(
        &mut self,
        frame: Arc<AndroidVideoFrame>,
        device: &wgpu::Device,
    ) -> Result<PreparedAndroidFrame, ZeroCopyError> {
        // SAFETY: this is a live wgpu device, and no raw queue operations occur.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }
            .ok_or_else(|| failure("Android native frames require Vulkan"))?;
        for extension in [
            ash::android::external_memory_android_hardware_buffer::NAME,
            ash::ext::queue_family_foreign::NAME,
        ] {
            if !hal.enabled_device_extensions().contains(&extension) {
                return Err(failure(format!(
                    "required extension unavailable: {extension:?}"
                )));
            }
        }
        let raw = hal.raw_device();
        let instance = hal.shared_instance().raw_instance();
        let android =
            ash::android::external_memory_android_hardware_buffer::Device::new(instance, raw);
        let mut format = vk::AndroidHardwareBufferFormatPropertiesANDROID::default();
        let (allocation_size, memory_bits) = {
            let mut properties =
                vk::AndroidHardwareBufferPropertiesANDROID::default().push_next(&mut format);
            // SAFETY: the owned AHB is live and the output chain has valid storage.
            unsafe {
                android.get_android_hardware_buffer_properties(frame.buffer.cast(), &mut properties)
            }
            .map_err(failure)?;
            (properties.allocation_size, properties.memory_type_bits)
        };
        if !format
            .format_features
            .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
        {
            return Err(failure("AHardwareBuffer cannot be sampled"));
        }
        // AHB's reported format features and suggested conversion describe its
        // external format, even when a Vulkan-equivalent format is also present.
        // Always use that external format so RGBA/vendor formats are not falsely
        // subjected to ordinary VkFormat YCbCr feature requirements.
        let key = ConversionKey {
            format: vk::Format::UNDEFINED,
            external_format: format.external_format,
            model: format.suggested_ycbcr_model,
            range: format.suggested_ycbcr_range,
            x: format.suggested_x_chroma_offset,
            y: format.suggested_y_chroma_offset,
            components: [
                format.sampler_ycbcr_conversion_components.r,
                format.sampler_ycbcr_conversion_components.g,
                format.sampler_ycbcr_conversion_components.b,
                format.sampler_ycbcr_conversion_components.a,
            ],
        };
        if key.format == vk::Format::UNDEFINED && key.external_format == 0 {
            return Err(failure("AHardwareBuffer has no importable format"));
        }
        if self
            .pipeline
            .as_ref()
            .is_none_or(|pipeline| pipeline.key != key)
        {
            // SAFETY: GPUI enabled samplerYcbcrConversion on this device. The key
            // comes from this device's AHB properties rather than guessed metadata.
            self.pipeline = Some(Arc::new(unsafe {
                ConversionPipeline::new(raw.clone(), key)
            }?));
        }
        let pipeline = self
            .pipeline
            .as_ref()
            .cloned()
            .ok_or_else(|| failure("missing conversion pipeline"))?;
        let width = frame.width;
        let height = frame.height;
        let mut resources = FrameResources {
            pipeline,
            _producer: frame,
            input: vk::Image::null(),
            input_memory: vk::DeviceMemory::null(),
            input_view: vk::ImageView::null(),
            output: vk::Image::null(),
            output_memory: vk::DeviceMemory::null(),
            output_view: vk::ImageView::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            framebuffer: vk::Framebuffer::null(),
        };
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let mut external_memory = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::ANDROID_HARDWARE_BUFFER_ANDROID);
        let mut external_format =
            vk::ExternalFormatANDROID::default().external_format(key.external_format);
        let mut image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(key.format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_memory);
        if key.external_format != 0 {
            image_info = image_info.push_next(&mut external_format);
        }
        // SAFETY: creation descriptors match the queried AHB; resources is the
        // exclusive owner and cleans partial allocations on every failure.
        unsafe {
            resources.input = raw.create_image(&image_info, None).map_err(failure)?;
            let mut import = vk::ImportAndroidHardwareBufferInfoANDROID::default()
                .buffer(resources._producer.buffer.cast());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(resources.input);
            let allocation = vk::MemoryAllocateInfo::default()
                .allocation_size(allocation_size)
                .memory_type_index(memory_type(memory_bits)?)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            resources.input_memory = raw.allocate_memory(&allocation, None).map_err(failure)?;
            raw.bind_image_memory(resources.input, resources.input_memory, 0)
                .map_err(failure)?;
            let mut conversion =
                vk::SamplerYcbcrConversionInfo::default().conversion(resources.pipeline.conversion);
            let view = vk::ImageViewCreateInfo::default()
                .image(resources.input)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(key.format)
                .subresource_range(color_range())
                .push_next(&mut conversion);
            resources.input_view = raw.create_image_view(&view, None).map_err(failure)?;

            let output_info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            resources.output = raw.create_image(&output_info, None).map_err(failure)?;
            let requirements = raw.get_image_memory_requirements(resources.output);
            let allocation = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type(requirements.memory_type_bits)?);
            resources.output_memory = raw.allocate_memory(&allocation, None).map_err(failure)?;
            raw.bind_image_memory(resources.output, resources.output_memory, 0)
                .map_err(failure)?;
            let view = vk::ImageViewCreateInfo::default()
                .image(resources.output)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .subresource_range(color_range());
            resources.output_view = raw.create_image_view(&view, None).map_err(failure)?;
            // The spec bounds combined YCbCr descriptors by the number of planes;
            // four covers external formats with alpha without underallocating.
            let pool_size = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 4,
            }];
            resources.descriptor_pool = raw
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&pool_size),
                    None,
                )
                .map_err(failure)?;
            let layouts = [resources.pipeline.descriptor_layout];
            let descriptor_set = raw
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(resources.descriptor_pool)
                        .set_layouts(&layouts),
                )
                .map_err(failure)?
                .into_iter()
                .next()
                .ok_or_else(|| failure("no descriptor set returned"))?;
            let image = [vk::DescriptorImageInfo::default()
                .image_view(resources.input_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            raw.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(descriptor_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&image)],
                &[],
            );
            let attachments = [resources.output_view];
            resources.framebuffer = raw
                .create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(resources.pipeline.render_pass)
                        .attachments(&attachments)
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )
                .map_err(failure)?;
            let resources = Arc::new(resources);
            let output = resources.output;
            let descriptor = wgpu::hal::TextureDescriptor {
                label: Some("Android converted frame"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COLOR_TARGET,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            let retained_resources = Arc::clone(&resources);
            let texture = hal.texture_from_raw(
                output,
                &descriptor,
                Some(Box::new(move || drop(retained_resources))),
                wgpu::hal::vulkan::TextureMemory::External,
            );
            let texture = device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(
                texture,
                &wgpu::TextureDescriptor {
                    label: Some("Android converted frame"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING
                        | wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
                false,
            );
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Android AHB conversion"),
            });
            // The fresh VkImage has undefined contents. Initialize through wgpu so its
            // initialization tracker will not clear the completed raw conversion later.
            // This GPU clear also retains the output and its producer lease on submission.
            let output_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            {
                let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Android output initialization"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &output_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            // Register the output in wgpu's tracker so the conversion submission
            // itself holds its HAL callback/producer lease, even if no draw uses it.
            encoder.transition_resources(
                std::iter::empty(),
                std::iter::once(wgpu::TextureTransition {
                    texture: &texture,
                    selector: None,
                    state: wgpu::TextureUses::RESOURCE,
                }),
            );
            let initialize = encoder.finish();
            // wgpu 30 does not permit mixing wgpu and raw encoding APIs on one
            // encoder. Keep initialization first in the same renderer submission.
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("Android raw AHB conversion"),
            });
            // SAFETY: raw commands are recorded exclusively into this wgpu-owned
            // encoder. It is neither ended nor submitted here. Resource lifetimes
            // are transferred to the output texture's GPU-tracked drop callback.
            encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|encoder| {
                let encoder = encoder.ok_or_else(|| failure("missing Vulkan command encoder"))?;
                resources.record(
                    encoder.raw_handle(),
                    hal.queue_family_index(),
                    descriptor_set,
                    width,
                    height,
                );
                Ok::<_, ZeroCopyError>(())
            })?;

            let texture = Arc::new(texture);
            // Raw encoding bypasses wgpu's resource tracker. Retain the texture's
            // HAL callback (and native Image lease) through this buffer's GPU work,
            // including when the frame is retired immediately after submission.
            let retained_texture = Arc::clone(&texture);
            encoder.on_submitted_work_done(move || drop(retained_texture));
            let commands = vec![initialize, encoder.finish()];
            Ok(PreparedAndroidFrame {
                commands,
                texture,
                width,
                height,
            })
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

impl FrameResources {
    unsafe fn record(
        &self,
        command: vk::CommandBuffer,
        family: u32,
        descriptor: vk::DescriptorSet,
        width: u32,
        height: u32,
    ) {
        let raw = &self.pipeline.device;
        let acquire = vk::ImageMemoryBarrier::default()
            .image(self.input)
            .subresource_range(color_range())
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(family);
        // SAFETY: this is an active, exclusively borrowed wgpu Vulkan command
        // buffer; all resources and the producer Image outlive its submission.
        unsafe {
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            let area = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent: vk::Extent2D { width, height },
            };
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue { float32: [0.0; 4] },
            }];
            raw.cmd_begin_render_pass(
                command,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.pipeline.render_pass)
                    .framebuffer(self.framebuffer)
                    .render_area(area)
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            raw.cmd_bind_pipeline(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.pipeline,
            );
            raw.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline.pipeline_layout,
                0,
                &[descriptor],
                &[],
            );
            raw.cmd_set_viewport(
                command,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            raw.cmd_set_scissor(command, 0, &[area]);
            raw.cmd_draw(command, 3, 1, 0, 0);
            raw.cmd_end_render_pass(command);
            let release = vk::ImageMemoryBarrier::default()
                .image(self.input)
                .subresource_range(color_range())
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT);
            raw.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
        }
    }
}

impl ConversionPipeline {
    unsafe fn new(device: ash::Device, key: ConversionKey) -> Result<Self, ZeroCopyError> {
        let mut state = Self {
            device,
            key,
            conversion: vk::SamplerYcbcrConversion::null(),
            sampler: vk::Sampler::null(),
            descriptor_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            render_pass: vk::RenderPass::null(),
            pipeline: vk::Pipeline::null(),
        };
        // SAFETY: the device enabled samplerYcbcrConversion, key is from its AHB
        // properties, and state owns all partially initialized handles on errors.
        unsafe {
            let mut external =
                vk::ExternalFormatANDROID::default().external_format(key.external_format);
            let [r, g, b, a] = key.components;
            let mut info = vk::SamplerYcbcrConversionCreateInfo::default()
                .format(key.format)
                .ycbcr_model(key.model)
                .ycbcr_range(key.range)
                .components(vk::ComponentMapping { r, g, b, a })
                .x_chroma_offset(key.x)
                .y_chroma_offset(key.y)
                .chroma_filter(vk::Filter::NEAREST);
            if key.external_format != 0 {
                info = info.push_next(&mut external);
            }
            state.conversion = state
                .device
                .create_sampler_ycbcr_conversion(&info, None)
                .map_err(failure)?;
            let mut conversion =
                vk::SamplerYcbcrConversionInfo::default().conversion(state.conversion);
            state.sampler = state
                .device
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::NEAREST)
                        .min_filter(vk::Filter::NEAREST)
                        .mipmap_mode(vk::SamplerMipmapMode::NEAREST)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .max_lod(0.0)
                        .push_next(&mut conversion),
                    None,
                )
                .map_err(failure)?;
            let samplers = [state.sampler];
            let binding = [vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&samplers)];
            state.descriptor_layout = state
                .device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binding),
                    None,
                )
                .map_err(failure)?;
            state.pipeline_layout = state
                .device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[state.descriptor_layout]),
                    None,
                )
                .map_err(failure)?;
            let attachment = [vk::AttachmentDescription::default()
                .format(vk::Format::R8G8B8A8_UNORM)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let color = [vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color)];
            let dependencies = [
                // The wgpu initialization pass and RESOURCE transition precede this
                // conversion on the same queue. Preserve their execution dependency.
                vk::SubpassDependency::default()
                    .src_subpass(vk::SUBPASS_EXTERNAL)
                    .dst_subpass(0)
                    .src_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
                    .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                    .src_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
                    .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
                vk::SubpassDependency::default()
                    .src_subpass(0)
                    .dst_subpass(vk::SUBPASS_EXTERNAL)
                    .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                    .dst_stage_mask(vk::PipelineStageFlags::FRAGMENT_SHADER)
                    .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ),
            ];
            state.render_pass = state
                .device
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachment)
                        .subpasses(&subpass)
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(failure)?;
            let vertex = shader(&state.device, include_bytes!("shaders/blit.vert.spv"))?;
            let fragment =
                match shader(&state.device, include_bytes!("shaders/ycbcr_blit.frag.spv")) {
                    Ok(shader) => shader,
                    Err(error) => {
                        state.device.destroy_shader_module(vertex, None);
                        return Err(error);
                    }
                };
            let stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vertex)
                    .name(c"main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(fragment)
                    .name(c"main"),
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(vk::CullModeFlags::NONE)
                .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let blend = [vk::PipelineColorBlendAttachmentState::default()
                .color_write_mask(vk::ColorComponentFlags::RGBA)];
            let color_blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
            let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic_state =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .color_blend_state(&color_blend)
                .dynamic_state(&dynamic_state)
                .layout(state.pipeline_layout)
                .render_pass(state.render_pass);
            let result =
                state
                    .device
                    .create_graphics_pipelines(vk::PipelineCache::null(), &[info], None);
            state.device.destroy_shader_module(fragment, None);
            state.device.destroy_shader_module(vertex, None);
            match result {
                Ok(pipelines) => {
                    state.pipeline = pipelines
                        .into_iter()
                        .next()
                        .ok_or_else(|| failure("no conversion pipeline returned"))?
                }
                Err((pipelines, error)) => {
                    for pipeline in pipelines {
                        state.device.destroy_pipeline(pipeline, None);
                    }
                    return Err(failure(error));
                }
            }
        }
        Ok(state)
    }
}

fn shader(device: &ash::Device, bytes: &[u8]) -> Result<vk::ShaderModule, ZeroCopyError> {
    let code = ash::util::read_spv(&mut std::io::Cursor::new(bytes)).map_err(failure)?;
    // SAFETY: shader assets are validated SPIR-V; Vulkan copies the input words.
    unsafe { device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None) }
        .map_err(failure)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_type_rejects_empty_masks_and_selects_a_supported_bit() {
        assert!(memory_type(0).is_err());
        assert_eq!(memory_type(0b10100).ok(), Some(2));
    }
}
