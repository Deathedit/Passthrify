//! GPU preview: uploads NV12 planes to two textures and converts to RGB in a fragment shader.

use std::sync::Arc;

use eframe::egui;
use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, RenderState};

use crate::capture::{Chroma, VideoFrame};

pub struct PreviewRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    tex: Option<Textures>,
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
        rs.renderer.write().callback_resources.insert(PreviewRenderer { pipeline, layout, sampler, tex: None });
    }

    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, f: &VideoFrame) {
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
                ],
            });
            self.tex = Some(Textures { width: w, height: h, chroma: f.chroma, y, uv, bind, seq: u64::MAX });
        }
        let t = self.tex.as_mut().unwrap();
        if t.seq == f.seq {
            return;
        }
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
        write(&t.y, f.y_plane(), w, h, w);
        write(&t.uv, f.uv_plane(), cw, ch, cw * 2);
    }
}

pub struct PreviewCallback {
    pub frame: Arc<VideoFrame>,
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
            r.upload(device, queue, &self.frame);
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
