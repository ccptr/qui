//! The Vulkan renderer: one pipeline drawing instanced quads (rounded
//! rectangles, shadows, glyphs and images) from a host-visible instance
//! buffer, an R8 glyph atlas and an RGBA image atlas, FIFO presentation and
//! two frames in flight.
//!
//! The surface can be detached and attached again ([`Gfx::detach`],
//! [`Gfx::attach`]), so a window that is hidden by destroying it keeps the
//! device (fast to show again) but holds no swapchain images.
//!
//! Frames are only drawn when the UI output changed (see `App`), so an idle
//! window costs nothing here.

use crate::image::{IMAGE_ATLAS, Images};
use crate::text::{ATLAS, Text};
use ash::vk;
use std::ffi::c_char;

const FRAMES: usize = 2;

/// One quad, as the vertex shader reads it (per-instance attributes).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Quad {
    /// x, y, w, h in physical pixels.
    pub rect: [f32; 4],
    /// Atlas u0, v0, u1, v1 (glyphs only).
    pub uv: [f32; 4],
    /// RGBA8, little-endian (`0xAABBGGRR`).
    pub color: u32,
    /// Corner radius, border width (0: filled) or blur radius (shadows),
    /// mode (0: rect, 1: glyph, 2: image, 3: shadow).
    pub params: [f32; 3],
}

pub const MODE_RECT: f32 = 0.0;
pub const MODE_GLYPH: f32 = 1.0;
pub const MODE_IMAGE: f32 = 2.0;
pub const MODE_SHADOW: f32 = 3.0;

/// A run of quads drawn with one scissor rectangle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Batch {
    pub clip: [i32; 4],
    pub first: u32,
    pub count: u32,
}

struct Frame {
    cmd: vk::CommandBuffer,
    acquired: vk::Semaphore,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut u8,
    capacity: usize,
}

pub struct Gfx {
    _entry: ash::Entry,
    instance: ash::Instance,
    surface_fn: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    pdev: vk::PhysicalDevice,
    memory_props: vk::PhysicalDeviceMemoryProperties,
    device: ash::Device,
    queue: vk::Queue,
    swapchain_fn: ash::khr::swapchain::Device,
    swapchain: vk::SwapchainKHR,
    format: vk::Format,
    pub extent: vk::Extent2D,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    /// Signalled when rendering into swapchain image `i` finished; one per
    /// image so a semaphore is never reused while its present is pending.
    rendered: Vec<vk::Semaphore>,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    atlas: vk::Image,
    atlas_memory: vk::DeviceMemory,
    atlas_view: vk::ImageView,
    linear: vk::Sampler,
    images: vk::Image,
    images_memory: vk::DeviceMemory,
    images_view: vk::ImageView,
    /// Staging for image uploads, grown on demand.
    image_staging: Option<(vk::Buffer, vk::DeviceMemory, *mut u8, usize)>,
    family: u32,
    transparent: bool,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_mapped: *mut u8,
    cmd_pool: vk::CommandPool,
    frames: Vec<Frame>,
    frame: usize,
    /// The swapchain must be rebuilt before the next frame.
    stale: bool,
    /// Use MAILBOX where the surface offers it (see [`Gfx::set_mailbox`]).
    want_mailbox: bool,
    present_mode: vk::PresentModeKHR,
    /// The kind of window the instance makes surfaces for.
    native: Native,
    pub device_name: String,
}

fn err(what: &str) -> impl Fn(vk::Result) -> String + '_ {
    move |e| format!("{what}: {e:?}")
}

impl Gfx {
    /// Renders into a Wayland surface. With `transparent`, the swapchain
    /// uses premultiplied alpha, so what isn't drawn shows what's below.
    ///
    /// # Safety
    /// `display` and `surface` must be live `wl_display*` / `wl_surface*`
    /// that outlive the swapchain (until [`Gfx::detach`] or drop).
    pub unsafe fn wayland(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
        size: (u32, u32),
        transparent: bool,
    ) -> Result<Gfx, String> {
        unsafe { Self::new(Native::Wayland { display, surface }, size, transparent) }
    }

    /// Renders into a native window (see [`Gfx::wayland`]). The instance
    /// is made for that kind of window: later windows must be the same kind.
    ///
    /// # Safety
    /// The handles must be live and outlive the swapchain (until
    /// [`Gfx::detach`] or drop).
    pub unsafe fn new(native: Native, size: (u32, u32), transparent: bool) -> Result<Gfx, String> {
        native.check()?;
        unsafe { Self::create(native, size, transparent) }
    }

    /// Destroys the swapchain and the Vulkan surface, keeping the device.
    pub fn detach(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.destroy_swapchain_resources();
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_fn.destroy_swapchain(self.swapchain, None);
                self.swapchain = vk::SwapchainKHR::null();
            }
            if self.surface != vk::SurfaceKHR::null() {
                self.surface_fn.destroy_surface(self.surface, None);
                self.surface = vk::SurfaceKHR::null();
            }
        }
        self.stale = true;
    }

    /// Renders into a new Wayland surface after [`Gfx::detach`].
    ///
    /// # Safety
    /// As for [`Gfx::wayland`].
    pub unsafe fn attach(
        &mut self,
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
        size: (u32, u32),
    ) -> Result<(), String> {
        unsafe { self.attach_native(Native::Wayland { display, surface }, size) }
    }

    /// Renders into a new native window after [`Gfx::detach`]; it must be
    /// the kind the device was made for.
    ///
    /// # Safety
    /// As for [`Gfx::new`].
    pub unsafe fn attach_native(&mut self, native: Native, size: (u32, u32)) -> Result<(), String> {
        native.check()?;
        if std::mem::discriminant(&native) != std::mem::discriminant(&self.native) {
            return Err("The Vulkan device was made for another kind of window".into());
        }
        self.detach();
        let surface = unsafe { create_surface(&self._entry, &self.instance, native) }?;
        let ok = unsafe { self.surface_fn.get_physical_device_surface_support(self.pdev, self.family, surface) }
            .unwrap_or(false);
        if !ok {
            unsafe { self.surface_fn.destroy_surface(surface, None) };
            return Err("The Vulkan device can't present to the new surface".into());
        }
        self.surface = surface;
        self.extent = vk::Extent2D { width: size.0, height: size.1 };
        self.stale = true;
        Ok(())
    }

    unsafe fn create(native: Native, size: (u32, u32), transparent: bool) -> Result<Gfx, String> {
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("Vulkan is not available ({e})"))?;

        let extensions: Vec<*const c_char> = vec![ash::khr::surface::NAME.as_ptr(), native.extension().as_ptr()];
        let flags = vk::InstanceCreateFlags::empty();
        let mut layers: Vec<*const c_char> = Vec::new();
        if std::env::var_os("RLS_VK_VALIDATE").is_some() {
            layers.push(c"VK_LAYER_KHRONOS_validation".as_ptr());
        }
        let app = vk::ApplicationInfo::default()
            .application_name(c"qui")
            .api_version(vk::API_VERSION_1_0);
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .flags(flags)
                    .application_info(&app)
                    .enabled_extension_names(&extensions)
                    .enabled_layer_names(&layers),
                None,
            )
        }
        .map_err(err("vkCreateInstance"))?;

        let surface = unsafe { create_surface(&entry, &instance, native) }?;
        let surface_fn = ash::khr::surface::Instance::new(&entry, &instance);

        // Prefer the integrated GPU: a few hundred quads never need more,
        // and it does not wake a discrete card.
        let pdevs = unsafe { instance.enumerate_physical_devices() }
            .map_err(err("vkEnumeratePhysicalDevices"))?;
        let mut best: Option<(u32, vk::PhysicalDevice, u32)> = None;
        for pdev in pdevs {
            let props = unsafe { instance.get_physical_device_properties(pdev) };
            let families =
                unsafe { instance.get_physical_device_queue_family_properties(pdev) };
            let family = families.iter().enumerate().position(|(i, f)| {
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
                    && unsafe {
                        surface_fn.get_physical_device_surface_support(pdev, i as u32, surface)
                    }
                    .unwrap_or(false)
            });
            let Some(family) = family else { continue };
            let rank = match props.device_type {
                vk::PhysicalDeviceType::INTEGRATED_GPU => 3,
                vk::PhysicalDeviceType::DISCRETE_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            if best.is_none_or(|(r, _, _)| rank > r) {
                best = Some((rank, pdev, family as u32));
            }
        }
        let (_, pdev, family) = best.ok_or("No Vulkan device can present to this window")?;
        let props = unsafe { instance.get_physical_device_properties(pdev) };
        let device_name = props
            .device_name_as_c_str()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let memory_props = unsafe { instance.get_physical_device_memory_properties(pdev) };

        let dev_extensions = [ash::khr::swapchain::NAME.as_ptr()];
        let priorities = [1.0f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let device = unsafe {
            instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queue_info)
                    .enabled_extension_names(&dev_extensions),
                None,
            )
        }
        .map_err(err("vkCreateDevice"))?;
        let queue = unsafe { device.get_device_queue(family, 0) };
        let swapchain_fn = ash::khr::swapchain::Device::new(&instance, &device);

        let formats = unsafe { surface_fn.get_physical_device_surface_formats(pdev, surface) }
            .map_err(err("surface formats"))?;
        // A UNORM target, so theme colors are written as given.
        let format = formats
            .iter()
            .find(|f| {
                matches!(f.format, vk::Format::B8G8R8A8_UNORM | vk::Format::R8G8B8A8_UNORM)
                    && f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR
            })
            .or(formats.first())
            .copied()
            .ok_or("The surface has no formats")?;

        let render_pass = {
            let attachment = [vk::AttachmentDescription::default()
                .format(format.format)
                .samples(vk::SampleCountFlags::TYPE_1)
                .load_op(vk::AttachmentLoadOp::CLEAR)
                .store_op(vk::AttachmentStoreOp::STORE)
                .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
                .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
            let color = [vk::AttachmentReference::default()
                .attachment(0)
                .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
            let subpass = [vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color)];
            // The acquire semaphore is waited on at color output; make the
            // layout transition wait for it too.
            let dependency = [vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
            unsafe {
                device.create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachment)
                        .subpasses(&subpass)
                        .dependencies(&dependency),
                    None,
                )
            }
            .map_err(err("render pass"))?
        };

        // Atlas texture, sampler, descriptor.
        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }
        .map_err(err("sampler"))?;
        let atlas = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
                    .extent(vk::Extent3D {
                        width: ATLAS,
                        height: ATLAS,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(err("atlas image"))?;
        let requirements = unsafe { device.get_image_memory_requirements(atlas) };
        let atlas_memory = allocate(
            &device,
            &memory_props,
            requirements,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        unsafe { device.bind_image_memory(atlas, atlas_memory, 0) }.map_err(err("bind atlas"))?;
        let atlas_view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(atlas)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8_UNORM)
                    .subresource_range(color_range()),
                None,
            )
        }
        .map_err(err("atlas view"))?;
        let (staging, staging_memory, staging_mapped) =
            host_buffer(&device, &memory_props, (ATLAS * ATLAS) as u64, vk::BufferUsageFlags::TRANSFER_SRC)?;
        let linear = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }
        .map_err(err("sampler"))?;
        let images = unsafe {
            device.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D { width: IMAGE_ATLAS, height: IMAGE_ATLAS, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )
        }
        .map_err(err("image atlas"))?;
        let requirements = unsafe { device.get_image_memory_requirements(images) };
        let images_memory =
            allocate(&device, &memory_props, requirements, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        unsafe { device.bind_image_memory(images, images_memory, 0) }.map_err(err("bind image atlas"))?;
        let images_view = unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(images)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .subresource_range(color_range()),
                None,
            )
        }
        .map_err(err("image atlas view"))?;

        let bindings = [0, 1].map(|binding| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(binding)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        });
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }
        .map_err(err("set layout"))?;
        let pool_sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
            descriptor_count: 2,
        }];
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .map_err(err("descriptor pool"))?;
        let set_layouts = [set_layout];
        let set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&set_layouts),
            )
        }
        .map_err(err("descriptor set"))?[0];
        let image_info = [vk::DescriptorImageInfo {
            sampler,
            image_view: atlas_view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }];
        let images_info = [vk::DescriptorImageInfo {
            sampler: linear,
            image_view: images_view,
            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        }];
        unsafe {
            device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&image_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&images_info),
                ],
                &[],
            )
        };

        let (layout, pipeline) = create_pipeline(&device, render_pass, set_layout)?;

        let cmd_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(family),
                None,
            )
        }
        .map_err(err("command pool"))?;
        let cmds = unsafe {
            device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(FRAMES as u32),
            )
        }
        .map_err(err("command buffers"))?;
        let mut frames = Vec::new();
        for cmd in cmds {
            let capacity = 64 * 1024;
            let (buffer, memory, mapped) = host_buffer(
                &device,
                &memory_props,
                capacity as u64,
                vk::BufferUsageFlags::VERTEX_BUFFER,
            )?;
            frames.push(Frame {
                cmd,
                acquired: unsafe {
                    device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
                }
                .map_err(err("semaphore"))?,
                fence: unsafe {
                    device.create_fence(
                        &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                        None,
                    )
                }
                .map_err(err("fence"))?,
                buffer,
                memory,
                mapped,
                capacity,
            });
        }

        let mut gfx = Gfx {
            _entry: entry,
            instance,
            surface_fn,
            surface,
            pdev,
            memory_props,
            device,
            queue,
            swapchain_fn,
            swapchain: vk::SwapchainKHR::null(),
            format: format.format,
            extent: vk::Extent2D {
                width: size.0,
                height: size.1,
            },
            views: Vec::new(),
            framebuffers: Vec::new(),
            rendered: Vec::new(),
            render_pass,
            layout,
            pipeline,
            set_layout,
            pool,
            set,
            sampler,
            atlas,
            atlas_memory,
            atlas_view,
            linear,
            images,
            images_memory,
            images_view,
            image_staging: None,
            family,
            transparent,
            staging,
            staging_memory,
            staging_mapped,
            cmd_pool,
            frames,
            frame: 0,
            stale: true,
            want_mailbox: false,
            present_mode: vk::PresentModeKHR::FIFO,
            native,
            device_name,
        };
        gfx.clear_atlas()?;
        Ok(gfx)
    }

    /// Transitions the atlas into its sampled layout, cleared to zero.
    fn clear_atlas(&mut self) -> Result<(), String> {
        let d = &self.device;
        let cmd = self.frames[0].cmd;
        unsafe {
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin"))?;
            barrier(
                d,
                cmd,
                self.atlas,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            );
            d.cmd_clear_color_image(
                cmd,
                self.atlas,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue::default(),
                &[color_range()],
            );
            barrier(
                d,
                cmd,
                self.atlas,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            barrier(d, cmd, self.images, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
            d.cmd_clear_color_image(
                cmd,
                self.images,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue::default(),
                &[color_range()],
            );
            barrier(
                d,
                cmd,
                self.images,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            );
            d.end_command_buffer(cmd).map_err(err("end"))?;
            let cmds = [cmd];
            d.reset_fences(&[self.frames[0].fence]).map_err(err("fence"))?;
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                self.frames[0].fence,
            )
            .map_err(err("submit"))?;
        }
        Ok(())
    }

    /// Marks the swapchain for rebuilding at the window's new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        let extent = vk::Extent2D { width, height };
        if extent != self.extent {
            self.extent = extent;
            self.stale = true;
        }
    }

    /// Prefers MAILBOX presentation (from the next swapchain on). Presents
    /// then never wait for the compositor, which matters for a surface
    /// drawn while unmapped: under FIFO, the driver may wait for a frame
    /// callback that an unmapped surface never gets.
    pub fn set_mailbox(&mut self, on: bool) {
        if self.want_mailbox != on {
            self.want_mailbox = on;
            self.stale = true;
        }
    }

    /// Creates the swapchain now if it is due (normally the next draw
    /// does), without presenting anything.
    pub fn ensure_swapchain(&mut self) -> Result<(), String> {
        if self.stale && self.rebuild()? {
            self.warm()?;
        }
        Ok(())
    }

    /// Submits an empty command buffer and waits for it. A driver may
    /// set up a new swapchain's memory on its first submit (several
    /// milliseconds for full-window images); this does it ahead of the
    /// first frame.
    fn warm(&mut self) -> Result<(), String> {
        let d = self.device.clone();
        let fi = self.frame;
        let (cmd, fence) = (self.frames[fi].cmd, self.frames[fi].fence);
        unsafe {
            d.wait_for_fences(&[fence], true, u64::MAX).map_err(err("wait"))?;
            d.reset_fences(&[fence]).map_err(err("reset fence"))?;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()).map_err(err("reset cmd"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin"))?;
            d.end_command_buffer(cmd).map_err(err("end"))?;
            let cmds = [cmd];
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], fence)
                .map_err(err("vkQueueSubmit"))?;
            d.wait_for_fences(&[fence], true, u64::MAX).map_err(err("wait"))?;
        }
        Ok(())
    }

    /// Whether the current swapchain presents with MAILBOX.
    pub fn is_mailbox(&self) -> bool {
        self.swapchain != vk::SwapchainKHR::null() && !self.stale && self.present_mode == vk::PresentModeKHR::MAILBOX
    }

    fn rebuild(&mut self) -> Result<bool, String> {
        if self.surface == vk::SurfaceKHR::null() {
            return Ok(false);
        }
        let d = &self.device;
        unsafe { d.device_wait_idle() }.map_err(err("wait idle"))?;
        let caps = unsafe {
            self.surface_fn
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }
        .map_err(err("surface capabilities"))?;
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: self
                    .extent
                    .width
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: self
                    .extent
                    .height
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        if extent.width == 0 || extent.height == 0 {
            return Ok(false); // Minimized.
        }
        // The minimum suffices: the UI renders rarely and never waits on
        // a backlog of frames. Each extra image is a full-window buffer.
        let mut count = caps.min_image_count.max(2);
        if caps.max_image_count > 0 {
            count = count.min(caps.max_image_count);
        }
        // Transparent windows want premultiplied alpha (what the
        // blending below produces); opaque ones don't care.
        let alpha_order = if self.transparent {
            [
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::INHERIT,
                vk::CompositeAlphaFlagsKHR::OPAQUE,
                vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            ]
        } else {
            [
                vk::CompositeAlphaFlagsKHR::OPAQUE,
                vk::CompositeAlphaFlagsKHR::INHERIT,
                vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED,
                vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED,
            ]
        };
        let alpha = alpha_order
            .into_iter()
        .find(|a| caps.supported_composite_alpha.contains(*a))
        .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        let mailbox = self.want_mailbox
            && unsafe { self.surface_fn.get_physical_device_surface_present_modes(self.pdev, self.surface) }
                .is_ok_and(|m| m.contains(&vk::PresentModeKHR::MAILBOX));
        let present_mode = if mailbox { vk::PresentModeKHR::MAILBOX } else { vk::PresentModeKHR::FIFO };
        if mailbox {
            // One image on screen, one queued, one to draw into.
            count = count.max(3);
            if caps.max_image_count > 0 {
                count = count.min(caps.max_image_count);
            }
        }
        let old = self.swapchain;
        let swapchain = unsafe {
            self.swapchain_fn.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(self.surface)
                    .min_image_count(count)
                    .image_format(self.format)
                    .image_color_space(vk::ColorSpaceKHR::SRGB_NONLINEAR)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(caps.current_transform)
                    .composite_alpha(alpha)
                    // FIFO (unless MAILBOX is wanted): always available,
                    // never renders faster than the display, and lets the
                    // GPU idle in between.
                    .present_mode(present_mode)
                    .clipped(true)
                    .old_swapchain(old),
                None,
            )
        }
        .map_err(err("vkCreateSwapchainKHR"))?;
        self.destroy_swapchain_resources();
        if old != vk::SwapchainKHR::null() {
            unsafe { self.swapchain_fn.destroy_swapchain(old, None) };
        }
        self.swapchain = swapchain;
        self.extent = extent;
        self.present_mode = present_mode;

        let d = &self.device;
        let images = unsafe { self.swapchain_fn.get_swapchain_images(swapchain) }
            .map_err(err("swapchain images"))?;
        for image in images {
            let view = unsafe {
                d.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.format)
                        .subresource_range(color_range()),
                    None,
                )
            }
            .map_err(err("image view"))?;
            let attachments = [view];
            let framebuffer = unsafe {
                d.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }
            .map_err(err("framebuffer"))?;
            self.views.push(view);
            self.framebuffers.push(framebuffer);
            self.rendered.push(
                unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
                    .map_err(err("semaphore"))?,
            );
        }
        self.stale = false;
        Ok(true)
    }

    fn destroy_swapchain_resources(&mut self) {
        let d = &self.device;
        unsafe {
            for framebuffer in self.framebuffers.drain(..) {
                d.destroy_framebuffer(framebuffer, None);
            }
            for view in self.views.drain(..) {
                d.destroy_image_view(view, None);
            }
            for semaphore in self.rendered.drain(..) {
                d.destroy_semaphore(semaphore, None);
            }
        }
    }

    /// Draws one frame. Returns `Ok(false)` when nothing could be drawn
    /// (minimized, or the swapchain went out of date: draw again).
    pub fn draw(
        &mut self,
        quads: &[Quad],
        batches: &[Batch],
        clear: [f32; 4],
        text: &mut Text,
        images: &mut Images,
    ) -> Result<bool, String> {
        if self.stale && !self.rebuild()? {
            return Ok(false);
        }
        let d = self.device.clone();
        let fi = self.frame;
        let fence = self.frames[fi].fence;
        unsafe { d.wait_for_fences(&[fence], true, u64::MAX) }.map_err(err("wait"))?;

        let acquired = self.frames[fi].acquired;
        let index = match unsafe {
            self.swapchain_fn
                .acquire_next_image(self.swapchain, u64::MAX, acquired, vk::Fence::null())
        } {
            Ok((index, _suboptimal)) => index,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.stale = true;
                return Ok(false);
            }
            Err(e) => return Err(format!("vkAcquireNextImageKHR: {e:?}")),
        };

        // Instance data.
        let bytes = std::mem::size_of_val(quads);
        if bytes > self.frames[fi].capacity {
            self.grow(fi, bytes.next_power_of_two())?;
        }
        let frame = &self.frames[fi];
        unsafe {
            std::ptr::copy_nonoverlapping(quads.as_ptr().cast::<u8>(), frame.mapped, bytes);
        }

        let cmd = frame.cmd;
        unsafe {
            d.reset_fences(&[fence]).map_err(err("reset fence"))?;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(err("reset cmd"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(err("begin"))?;
        }

        if let Some([x0, y0, x1, y1]) = text.dirty.take() {
            // The staging buffer is shared by both frames: let the other
            // frame's copy finish first. Atlas updates are rare (new glyphs).
            let fences: Vec<vk::Fence> = self
                .frames
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != fi)
                .map(|(_, f)| f.fence)
                .collect();
            unsafe { d.wait_for_fences(&fences, true, u64::MAX) }.map_err(err("wait"))?;
            for y in y0..y1 {
                let start = (y * ATLAS + x0) as usize;
                let end = (y * ATLAS + x1) as usize;
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        text.pixels[start..end].as_ptr(),
                        self.staging_mapped.add(start),
                        end - start,
                    );
                }
            }
            unsafe {
                barrier(
                    &d,
                    cmd,
                    self.atlas,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                );
                d.cmd_copy_buffer_to_image(
                    cmd,
                    self.staging,
                    self.atlas,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy {
                        buffer_offset: (y0 * ATLAS + x0) as u64,
                        buffer_row_length: ATLAS,
                        buffer_image_height: 0,
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_offset: vk::Offset3D {
                            x: x0 as i32,
                            y: y0 as i32,
                            z: 0,
                        },
                        image_extent: vk::Extent3D {
                            width: x1 - x0,
                            height: y1 - y0,
                            depth: 1,
                        },
                    }],
                );
                barrier(
                    &d,
                    cmd,
                    self.atlas,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                );
            }
        }

        if !images.pending.is_empty() {
            unsafe { self.upload_images(fi, cmd, images) }?;
        }

        let clear_values = [vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
        }];
        let extent = self.extent;
        unsafe {
            d.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(self.framebuffers[index as usize])
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D::default(),
                        extent,
                    })
                    .clear_values(&clear_values),
                vk::SubpassContents::INLINE,
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            d.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: extent.width as f32,
                    height: extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.layout,
                0,
                &[self.set],
                &[],
            );
            d.cmd_bind_vertex_buffers(cmd, 0, &[self.frames[fi].buffer], &[0]);
            let screen = [extent.width as f32, extent.height as f32];
            d.cmd_push_constants(
                cmd,
                self.layout,
                vk::ShaderStageFlags::VERTEX,
                0,
                std::slice::from_raw_parts(screen.as_ptr().cast::<u8>(), 8),
            );
            for batch in batches {
                if batch.count == 0 {
                    continue;
                }
                let [x0, y0, x1, y1] = batch.clip;
                let x0 = x0.clamp(0, extent.width as i32);
                let y0 = y0.clamp(0, extent.height as i32);
                let x1 = x1.clamp(x0, extent.width as i32);
                let y1 = y1.clamp(y0, extent.height as i32);
                if x1 == x0 || y1 == y0 {
                    continue;
                }
                d.cmd_set_scissor(
                    cmd,
                    0,
                    &[vk::Rect2D {
                        offset: vk::Offset2D { x: x0, y: y0 },
                        extent: vk::Extent2D {
                            width: (x1 - x0) as u32,
                            height: (y1 - y0) as u32,
                        },
                    }],
                );
                d.cmd_draw(cmd, 6, batch.count, 0, batch.first);
            }
            d.cmd_end_render_pass(cmd);
            d.end_command_buffer(cmd).map_err(err("end"))?;

            let waits = [acquired];
            let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
            let cmds = [cmd];
            let signals = [self.rendered[index as usize]];
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&cmds)
                    .signal_semaphores(&signals)],
                fence,
            )
            .map_err(err("vkQueueSubmit"))?;

            let swapchains = [self.swapchain];
            let indices = [index];
            match self.swapchain_fn.queue_present(
                self.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&signals)
                    .swapchains(&swapchains)
                    .image_indices(&indices),
            ) {
                Ok(false) => {}
                Ok(true) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.stale = true,
                Err(e) => return Err(format!("vkQueuePresentKHR: {e:?}")),
            }
        }
        self.frame = (fi + 1) % FRAMES;
        Ok(true)
    }

    /// Records copies of the pending image-atlas rectangles.
    unsafe fn upload_images(&mut self, fi: usize, cmd: vk::CommandBuffer, images: &mut Images) -> Result<(), String> {
        let d = self.device.clone();
        let total: usize = images.pending.iter().map(|u| u.data.len()).sum();
        // The staging buffer is shared by both frames.
        let fences: Vec<vk::Fence> =
            self.frames.iter().enumerate().filter(|(i, _)| *i != fi).map(|(_, f)| f.fence).collect();
        unsafe { d.wait_for_fences(&fences, true, u64::MAX) }.map_err(err("wait"))?;
        if self.image_staging.as_ref().is_none_or(|s| s.3 < total) {
            if let Some((buffer, memory, _, _)) = self.image_staging.take() {
                unsafe {
                    d.destroy_buffer(buffer, None);
                    d.free_memory(memory, None);
                }
            }
            let size = total.next_power_of_two().max(256 << 10);
            let (buffer, memory, mapped) =
                host_buffer(&d, &self.memory_props, size as u64, vk::BufferUsageFlags::TRANSFER_SRC)?;
            self.image_staging = Some((buffer, memory, mapped, size));
        }
        let (buffer, _, mapped, _) = *self.image_staging.as_ref().unwrap();
        let mut regions = Vec::with_capacity(images.pending.len());
        let mut offset = 0usize;
        for u in images.pending.drain(..) {
            unsafe { std::ptr::copy_nonoverlapping(u.data.as_ptr(), mapped.add(offset), u.data.len()) };
            regions.push(vk::BufferImageCopy {
                buffer_offset: offset as u64,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: u.x as i32, y: u.y as i32, z: 0 },
                image_extent: vk::Extent3D { width: u.w, height: u.h, depth: 1 },
            });
            // Offsets of texel copies must be multiples of 4 bytes: RGBA8 is.
            offset += u.data.len();
        }
        unsafe {
            barrier(&d, cmd, self.images, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
            d.cmd_copy_buffer_to_image(cmd, buffer, self.images, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &regions);
            barrier(&d, cmd, self.images, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        }
        Ok(())
    }

    fn grow(&mut self, fi: usize, capacity: usize) -> Result<(), String> {
        let d = &self.device;
        let frame = &mut self.frames[fi];
        // This frame's fence was waited on: its buffer is idle.
        unsafe {
            d.destroy_buffer(frame.buffer, None);
            d.free_memory(frame.memory, None);
        }
        let (buffer, memory, mapped) = host_buffer(
            d,
            &self.memory_props,
            capacity as u64,
            vk::BufferUsageFlags::VERTEX_BUFFER,
        )?;
        frame.buffer = buffer;
        frame.memory = memory;
        frame.mapped = mapped;
        frame.capacity = capacity;
        Ok(())
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            let d = &self.device;
            let _ = d.device_wait_idle();
            self.destroy_swapchain_resources();
            let d = &self.device;
            if self.swapchain != vk::SwapchainKHR::null() {
                self.swapchain_fn.destroy_swapchain(self.swapchain, None);
            }
            for frame in &self.frames {
                d.destroy_semaphore(frame.acquired, None);
                d.destroy_fence(frame.fence, None);
                d.destroy_buffer(frame.buffer, None);
                d.free_memory(frame.memory, None);
            }
            d.destroy_command_pool(self.cmd_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_sampler(self.linear, None);
            d.destroy_image_view(self.images_view, None);
            d.destroy_image(self.images, None);
            d.free_memory(self.images_memory, None);
            if let Some((buffer, memory, _, _)) = self.image_staging.take() {
                d.destroy_buffer(buffer, None);
                d.free_memory(memory, None);
            }
            d.destroy_image_view(self.atlas_view, None);
            d.destroy_image(self.atlas, None);
            d.free_memory(self.atlas_memory, None);
            d.destroy_buffer(self.staging, None);
            d.free_memory(self.staging_memory, None);
            d.destroy_render_pass(self.render_pass, None);
            d.destroy_device(None);
            if self.surface != vk::SurfaceKHR::null() {
                self.surface_fn.destroy_surface(self.surface, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

unsafe fn barrier(
    d: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let (src_access, src_stage) = match from {
        vk::ImageLayout::TRANSFER_DST_OPTIMAL => {
            (vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TRANSFER)
        }
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => {
            (vk::AccessFlags::SHADER_READ, vk::PipelineStageFlags::FRAGMENT_SHADER)
        }
        _ => (vk::AccessFlags::empty(), vk::PipelineStageFlags::TOP_OF_PIPE),
    };
    let (dst_access, dst_stage) = match to {
        vk::ImageLayout::TRANSFER_DST_OPTIMAL => {
            (vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TRANSFER)
        }
        _ => (vk::AccessFlags::SHADER_READ, vk::PipelineStageFlags::FRAGMENT_SHADER),
    };
    unsafe {
        d.cmd_pipeline_barrier(
            cmd,
            src_stage,
            dst_stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[vk::ImageMemoryBarrier::default()
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .old_layout(from)
                .new_layout(to)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(color_range())],
        );
    }
}

fn allocate(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    requirements: vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> Result<vk::DeviceMemory, String> {
    let index = (0..props.memory_type_count)
        .find(|&i| {
            requirements.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize].property_flags.contains(flags)
        })
        .ok_or("No suitable GPU memory type")?;
    unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(index),
            None,
        )
    }
    .map_err(err("vkAllocateMemory"))
}

/// A persistently mapped, host-coherent buffer.
fn host_buffer(
    device: &ash::Device,
    props: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory, *mut u8), String> {
    let buffer = unsafe {
        device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(size)
                .usage(usage)
                .sharing_mode(vk::SharingMode::EXCLUSIVE),
            None,
        )
    }
    .map_err(err("vkCreateBuffer"))?;
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    let memory = allocate(
        device,
        props,
        requirements,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    unsafe { device.bind_buffer_memory(buffer, memory, 0) }.map_err(err("bind buffer"))?;
    let mapped = unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
        .map_err(err("vkMapMemory"))?;
    Ok((buffer, memory, mapped.cast()))
}

fn create_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
) -> Result<(vk::PipelineLayout, vk::Pipeline), String> {
    let load = |bytes: &[u8]| -> Result<vk::ShaderModule, String> {
        let code = ash::util::read_spv(&mut std::io::Cursor::new(bytes))
            .map_err(|e| format!("SPIR-V: {e}"))?;
        unsafe {
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
        }
        .map_err(err("shader module"))
    };
    let vert = load(include_bytes!("../shaders/ui.vert.spv"))?;
    let frag = load(include_bytes!("../shaders/ui.frag.spv"))?;

    let push = [vk::PushConstantRange {
        stage_flags: vk::ShaderStageFlags::VERTEX,
        offset: 0,
        size: 8,
    }];
    let set_layouts = [set_layout];
    let layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&set_layouts)
                .push_constant_ranges(&push),
            None,
        )
    }
    .map_err(err("pipeline layout"))?;

    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(c"main"),
    ];
    let bindings = [vk::VertexInputBindingDescription {
        binding: 0,
        stride: std::mem::size_of::<Quad>() as u32,
        input_rate: vk::VertexInputRate::INSTANCE,
    }];
    let attribute = |location, format, offset| vk::VertexInputAttributeDescription {
        location,
        binding: 0,
        format,
        offset,
    };
    let attributes = [
        attribute(0, vk::Format::R32G32B32A32_SFLOAT, 0),
        attribute(1, vk::Format::R32G32B32A32_SFLOAT, 16),
        attribute(2, vk::Format::R8G8B8A8_UNORM, 32),
        attribute(3, vk::Format::R32G32B32_SFLOAT, 36),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
        .vertex_binding_descriptions(&bindings)
        .vertex_attribute_descriptions(&attributes);
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
        .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .color_blend_op(vk::BlendOp::ADD)
        .src_alpha_blend_factor(vk::BlendFactor::ONE)
        .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
        .alpha_blend_op(vk::BlendOp::ADD)
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0)];
    let pipeline = unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None)
    }
    .map_err(|(_, e)| format!("pipeline: {e:?}"))?[0];
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    Ok((layout, pipeline))
}

/// A native window to render into.
#[derive(Clone, Copy, Debug)]
pub enum Native {
    /// A `wl_display*` and a `wl_surface*`.
    Wayland { display: *mut std::ffi::c_void, surface: *mut std::ffi::c_void },
    /// An `xcb_connection_t*` and an X11 window.
    Xcb { connection: *mut std::ffi::c_void, window: u32 },
}

impl Native {
    fn check(&self) -> Result<(), String> {
        match *self {
            Native::Wayland { display, surface } if display.is_null() || surface.is_null() => {
                Err("null Wayland handle".into())
            }
            Native::Xcb { connection, window } if connection.is_null() || window == 0 => {
                Err("null X11 handle".into())
            }
            _ => Ok(()),
        }
    }

    /// The instance extension that makes surfaces for this kind of window.
    fn extension(&self) -> &'static std::ffi::CStr {
        match self {
            Native::Wayland { .. } => ash::khr::wayland_surface::NAME,
            Native::Xcb { .. } => ash::khr::xcb_surface::NAME,
        }
    }
}

/// Creates the `VkSurfaceKHR` for a native window.
unsafe fn create_surface(entry: &ash::Entry, instance: &ash::Instance, native: Native) -> Result<vk::SurfaceKHR, String> {
    match native {
        Native::Wayland { display, surface } => unsafe {
            ash::khr::wayland_surface::Instance::new(entry, instance).create_wayland_surface(
                &vk::WaylandSurfaceCreateInfoKHR::default().display(display.cast()).surface(surface.cast()),
                None,
            )
        }
        .map_err(err("vkCreateWaylandSurfaceKHR")),
        Native::Xcb { connection, window } => unsafe {
            ash::khr::xcb_surface::Instance::new(entry, instance).create_xcb_surface(
                &vk::XcbSurfaceCreateInfoKHR::default().connection(connection.cast()).window(window),
                None,
            )
        }
        .map_err(err("vkCreateXcbSurfaceKHR")),
    }
}
