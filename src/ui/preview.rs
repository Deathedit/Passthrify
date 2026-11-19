//! GPU preview: uploads NV12 planes to two textures and converts to RGB in a fragment shader.

use std::sync::Arc;

use eframe::egui;
use arc_swap::ArcSwapOption;
use ash::vk;
use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, RenderState};
use wgpu::hal;

use crate::capture::{Chroma, VideoFrame};
use crate::color::ShaderParams;
use crate::gpu::{PreviewTarget, PREVIEW_SLOTS};

pub struct PreviewRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// `ColorParams` uniform (range + matrix of the current frames).
    params: wgpu::Buffer,
    params_set: Option<ShaderParams>,
    tex: Option<Textures>,
    /// The GPU frame buffer shared with CUDA, for the current capture shape.
    interop: Option<Interop>,
    /// Earlier buffers, freed once neither the capture thread nor the GPU uses them.
    retired: Vec<Interop>,
    /// The shape the shared buffer couldn't be created for (don't retry every frame).
    interop_failed: Option<(u32, u32, Chroma)>,
}

struct Textures {
    width: u32,
    height: u32,
    chroma: Chroma,
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind: wgpu::BindGroup,
    seq: u64,
}

impl PreviewRenderer {
    /// Registers the renderer in egui's callback resources.
    pub fn install(rs: &RenderState) {
        let device = &rs.device;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(include_str!("nv12.wgsl").into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let fs = if rs.target_format.is_srgb() { "fs_main_srgb" } else { "fs_main" };
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nv12"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(fs),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: rs.target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nv12"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nv12 color params"),
            size: std::mem::size_of::<ShaderParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        rs.renderer
            .write()
            .callback_resources
            .insert(PreviewRenderer {
            pipeline,
            layout,
            sampler,
            params,
            params_set: None,
            tex: None,
            interop: None,
            retired: Vec::new(),
            interop_failed: None,
        });
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &VideoFrame, shared: &ArcSwapOption<PreviewTarget>) {
        self.free_retired(device);
        let wanted = f.color.shader_params();
        if self.params_set != Some(wanted) {
            let bytes: Vec<u8> = wanted.levels.iter().chain(wanted.coefs.iter()).flat_map(|v| v.to_le_bytes()).collect();
            queue.write_buffer(&self.params, 0, &bytes);
            self.params_set = Some(wanted);
        }
        let (w, h) = (f.width, f.height);
        let (cw, ch) = f.chroma_size();
        if !self.tex.as_ref().is_some_and(|t| t.width == w && t.height == h && t.chroma == f.chroma) {
            let mk = |label, width, height, format| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            };
            let y = mk("y", w, h, wgpu::TextureFormat::R8Unorm);
            // Chroma texture at chroma resolution; normalized sampling upsamples it for free.
            let uv = mk("uv", cw, ch, wgpu::TextureFormat::Rg8Unorm);
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("nv12"),
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&y.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&uv.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 3, resource: self.params.as_entire_binding() },
                ],
            });
            self.tex = Some(Textures { width: w, height: h, chroma: f.chroma, y, uv, bind, seq: u64::MAX });
        }
        if self.tex.as_ref().is_some_and(|t| t.seq == f.seq) {
            return;
        }
        if self.copy_from_gpu(device, queue, f, shared) {
            return;
        }
        let t = self.tex.as_mut().unwrap();
        t.seq = f.seq;
        let write = |tex: &wgpu::Texture, data: &[u8], width: u32, height: u32, bpr: u32| {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(height) },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        };
        crate::perf::PREVIEW_UPLOAD.time(|| {
            write(&t.y, f.y_plane(), w, h, w);
            write(&t.uv, f.uv_plane(), cw, ch, cw * 2);
        });
    }

    /// A captured frame the capture thread already copied into the shared GPU buffer: copy it
    /// into the textures on the GPU. False when there's no such slot (the CPU upload follows).
    fn copy_from_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &VideoFrame, shared: &ArcSwapOption<PreviewTarget>) -> bool {
        // Only captured frames have a CUDA copy (generated frames come from the CPU).
        if f.gpu.get().is_none() || !PreviewTarget::supports(f.chroma) {
            return false;
        }
        let shape = (f.width, f.height, f.chroma);
        if self.interop.as_ref().map(Interop::shape) != Some(shape) {
            if self.interop_failed == Some(shape) {
                return false;
            }
            match Interop::create(device, f.width, f.height, f.chroma) {
                Ok(i) => {
                    tracing::info!("preview reads frames from the GPU ({}x{} {})", f.width, f.height, f.chroma.label());
                    shared.store(Some(i.target.clone()));
                    if let Some(old) = self.interop.replace(i) {
                        self.retired.push(old);
                    }
                }
                Err(e) => {
                    tracing::warn!("preview GPU buffer unavailable, uploading from the CPU: {e:#}");
                    self.interop_failed = Some(shape);
                    shared.store(None);
                    if let Some(old) = self.interop.take() {
                        self.retired.push(old);
                    }
                }
            }
            // The capture thread fills it from the next frame on.
            return false;
        }
        let (Some(i), Some(t)) = (self.interop.as_ref(), self.tex.as_mut()) else { return false };
        let Some(slot) = i.target.take(f.seq) else { return false };
        let started = std::time::Instant::now();
        let (yo, uvo) = i.target.plane_offsets(slot);
        let (cw, ch) = f.chroma_size();
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("preview from gpu") });
        let copy = |enc: &mut wgpu::CommandEncoder, tex: &wgpu::Texture, offset: u64, pitch: usize, w: u32, h: u32| {
            enc.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &i.buffer,
                    layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(pitch as u32), rows_per_image: Some(h) },
                },
                wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        copy(&mut enc, &t.y, yo, i.target.y_pitch, f.width, f.height);
        copy(&mut enc, &t.uv, uvo, i.target.uv_pitch, cw, ch);
        // Submitted on its own (before egui's frame), so the slot is released only once the
        // GPU has actually run this copy.
        queue.submit([enc.finish()]);
        let target = i.target.clone();
        queue.on_submitted_work_done(move || target.release(slot));
        t.seq = f.seq;
        crate::perf::PREVIEW_GPU.add(started.elapsed());
        true
    }

    /// Frees retired shared buffers that neither the capture thread nor the GPU still uses.
    fn free_retired(&mut self, device: &wgpu::Device) {
        if !self.retired.iter().any(|r| Arc::strong_count(&r.target) == 1) {
            return;
        }
        let (done, keep): (Vec<_>, Vec<_>) = self.retired.drain(..).partition(|r| Arc::strong_count(&r.target) == 1);
        self.retired = keep;
        let mut memory = Vec::new();
        for r in done {
            memory.push((r.device, r.memory));
            drop(r.target); // CUDA lets go of the memory
            drop(r.buffer); // wgpu destroys the VkBuffer once the GPU is done with it
        }
        let _ = device.poll(wgpu::Maintain::Wait);
        for (dev, mem) in memory {
            unsafe { dev.free_memory(mem, None) };
        }
    }
}

pub struct PreviewCallback {
    pub frame: Arc<VideoFrame>,
    /// Where the preview publishes its shared GPU buffer for the capture thread.
    pub target: Arc<ArcSwapOption<PreviewTarget>>,
}

impl CallbackTrait for PreviewCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        res: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(r) = res.get_mut::<PreviewRenderer>() {
            r.upload(device, queue, &self.frame, &self.target);
        }
        Vec::new()
    }

    fn paint(&self, _info: egui::PaintCallbackInfo, rp: &mut wgpu::RenderPass<'static>, res: &CallbackResources) {
        let Some(r) = res.get::<PreviewRenderer>() else { return };
        let Some(t) = &r.tex else { return };
        rp.set_pipeline(&r.pipeline);
        rp.set_bind_group(0, &t.bind, &[]);
        rp.draw(0..3, 0..1);
    }
}

/// Largest rect with the frame's aspect ratio centred in `avail`.
pub fn letterbox(avail: egui::Rect, w: u32, h: u32) -> egui::Rect {
    let aspect = w as f32 / h.max(1) as f32;
    let mut size = avail.size();
    if size.x / size.y > aspect {
        size.x = size.y * aspect;
    } else {
        size.y = size.x / aspect;
    }
    egui::Rect::from_center_size(avail.center(), size)
}

/// The preview's GPU frame buffer: Vulkan memory exported to CUDA (see `PreviewTarget`).
struct Interop {
    target: Arc<PreviewTarget>,
    buffer: wgpu::Buffer,
    device: ash::Device,
    memory: vk::DeviceMemory,
}

impl Interop {
    fn shape(&self) -> (u32, u32, Chroma) {
        (self.target.width, self.target.height, self.target.chroma)
    }

    /// A buffer of `PREVIEW_SLOTS` frames in exportable device memory, imported into CUDA and
    /// wrapped as a wgpu buffer. Needs wgpu on Vulkan with VK_KHR_external_memory_win32.
    fn create(device: &wgpu::Device, width: u32, height: u32, chroma: Chroma) -> anyhow::Result<Self> {
        use anyhow::{bail, Context};
        let (_, _, slot_size) = PreviewTarget::layout(width, height, chroma);
        let size = (slot_size * PREVIEW_SLOTS) as u64;
        let raw = unsafe {
            device.as_hal::<hal::api::Vulkan, _, _>(|d| {
                d.map(|d| {
                    (
                        d.raw_device().clone(),
                        d.raw_physical_device(),
                        d.shared_instance().raw_instance().clone(),
                        d.enabled_device_extensions().contains(&ash::khr::external_memory_win32::NAME),
                    )
                })
            })
        };
        let Some((dev, pdev, instance, has_ext)) = raw else { bail!("the preview isn't rendered with Vulkan") };
        if !has_ext {
            bail!("VK_KHR_external_memory_win32 isn't available");
        }
        let handle_type = vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32;
        unsafe {
            let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(handle_type);
            let info = vk::BufferCreateInfo::default()
                .size(size)
                .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .push_next(&mut external);
            let buffer = dev.create_buffer(&info, None).context("vkCreateBuffer")?;
            let req = dev.get_buffer_memory_requirements(buffer);
            let props = instance.get_physical_device_memory_properties(pdev);
            let Some(type_index) = (0..props.memory_type_count).find(|&i| {
                req.memory_type_bits & (1 << i) != 0
                    && props.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            }) else {
                dev.destroy_buffer(buffer, None);
                bail!("no device-local memory type for the preview buffer");
            };
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(handle_type);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(type_index)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            let memory = match dev.allocate_memory(&alloc, None) {
                Ok(m) => m,
                Err(e) => {
                    dev.destroy_buffer(buffer, None);
                    bail!("vkAllocateMemory: {e}");
                }
            };
            let cleanup = |e: anyhow::Error| {
                dev.destroy_buffer(buffer, None);
                dev.free_memory(memory, None);
                e
            };
            dev.bind_buffer_memory(buffer, memory, 0).map_err(|e| cleanup(anyhow::anyhow!("vkBindBufferMemory: {e}")))?;
            let win32 = ash::khr::external_memory_win32::Device::new(&instance, &dev);
            let handle = win32
                .get_memory_win32_handle(&vk::MemoryGetWin32HandleInfoKHR::default().memory(memory).handle_type(handle_type))
                .map_err(|e| cleanup(anyhow::anyhow!("vkGetMemoryWin32HandleKHR: {e}")))?;
            // CUDA keeps its own reference; the handle itself is ours to close.
            let target = PreviewTarget::import(handle as *mut std::ffi::c_void, req.size, width, height, chroma);
            let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(handle as _));
            let target = target.map_err(cleanup)?;
            let buffer = device.create_buffer_from_hal::<hal::api::Vulkan>(
                hal::vulkan::Device::buffer_from_raw(buffer),
                &wgpu::BufferDescriptor {
                    label: Some("preview shared frames"),
                    size,
                    usage: wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                },
            );
            Ok(Self { target: Arc::new(target), buffer, device: dev, memory })
        }
    }
}
