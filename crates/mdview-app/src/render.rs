// SPDX-License-Identifier: Apache-2.0
//! wgpu renderer: DX12/Vulkan/Metal via wgpu, glyphon for text, plus two
//! minimal instanced-quad pipelines (filled rects, textured rects) for
//! chrome, code backgrounds, and images.

use std::num::NonZeroU64;
use std::sync::Arc;

use wgpu::util::DeviceExt;

use crate::images::GpuImageSet;

pub const QUAD_SHADER: &str = r#"
struct Quad {
    pos: vec2<f32>,
    size: vec2<f32>,
    color: vec4<f32>,
};
@group(0) @binding(0) var<storage, read> quads: array<Quad>;
@group(0) @binding(1) var<uniform> viewport: vec2<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let q = quads[ii];
    let px = q.pos + c * q.size;
    var out: VsOut;
    out.pos = vec4<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0, 0.0, 1.0);
    out.color = q.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

pub const IMAGE_SHADER: &str = r#"
struct Instance {
    pos: vec2<f32>,
    size: vec2<f32>,
};
@group(0) @binding(0) var<storage, read> instances: array<Instance>;
@group(0) @binding(1) var<uniform> viewport: vec2<f32>;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    let corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corners[vi];
    let inst = instances[ii];
    let px = inst.pos + c * inst.size;
    var out: VsOut;
    out.pos = vec4<f32>(px.x / viewport.x * 2.0 - 1.0, 1.0 - px.y / viewport.y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(c.x, c.y);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Quad {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Quad {
    pub fn new(x: f32, y: f32, w: f32, h: f32, color: [u8; 3]) -> Quad {
        Quad {
            x,
            y,
            w,
            h,
            r: color[0] as f32 / 255.0,
            g: color[1] as f32 / 255.0,
            b: color[2] as f32 / 255.0,
            a: 1.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ImageInstance {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

#[allow(unsafe_code)]
fn as_bytes<T>(slice: &[T]) -> &[u8] {
    // SAFETY: Quad/ImageInstance are repr(C) POD (f32 fields only); reading
    // them as bytes is sound and they have no padding.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), std::mem::size_of_val(slice)) }
}

pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,

    pub viewport: glyphon::Viewport,
    pub atlas: glyphon::TextAtlas,
    pub text_renderer: glyphon::TextRenderer,
    pub swash_cache: glyphon::SwashCache,

    quad_pipeline: wgpu::RenderPipeline,
    quad_bind: wgpu::BindGroup,
    quad_buf: wgpu::Buffer,
    quad_capacity: u64,

    image_pipeline: wgpu::RenderPipeline,
    image_common_bind: wgpu::BindGroup,
    image_buf: wgpu::Buffer,
    image_capacity: u64,
    vp_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
}

fn storage_bgl_entry(binding: u32, min_size: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(min_size),
        },
        count: None,
    }
}

fn uniform_bgl_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(8),
        },
        count: None,
    }
}

impl Renderer {
    pub async fn new(window: Arc<winit::window::Window>) -> Renderer {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .expect("request adapter");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("mdview-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                memory_hints: wgpu::MemoryHints::Performance,
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .unwrap_or(wgpu::TextureFormat::Bgra8UnormSrgb);

        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        // glyphon text stack
        let glyphon_cache = glyphon::Cache::new(&device);
        let viewport = glyphon::Viewport::new(&device, &glyphon_cache);
        let mut atlas = glyphon::TextAtlas::new(&device, &queue, &glyphon_cache, format);
        let text_renderer = glyphon::TextRenderer::new(
            &mut atlas,
            &device,
            wgpu::MultisampleState::default(),
            None,
        );
        let swash_cache = glyphon::SwashCache::new();

        // Shared viewport uniform
        let vp_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("viewport"),
            contents: &[0u8; 8],
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        // Filled-quad pipeline
        let quad_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
        });
        let quad_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad-bgl"),
            entries: &[storage_bgl_entry(0, 32), uniform_bgl_entry(1)],
        });
        let quad_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad-pl"),
            bind_group_layouts: &[&quad_bgl],
            push_constant_ranges: &[],
        });
        let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad-pipeline"),
            layout: Some(&quad_layout),
            vertex: wgpu::VertexState {
                module: &quad_shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 32,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 16,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &quad_shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let quad_capacity = 256u64;
        let quad_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-instances"),
            size: quad_capacity * 32,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad-bind"),
            layout: &quad_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: quad_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: vp_buf.as_entire_binding(),
                },
            ],
        });

        // Textured-quad pipeline
        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });
        let image_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image-bgl"),
            entries: &[storage_bgl_entry(0, 16), uniform_bgl_entry(1)],
        });
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tex-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let image_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image-pl"),
            bind_group_layouts: &[&image_bgl, &tex_bgl],
            push_constant_ranges: &[],
        });
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image-pipeline"),
            layout: Some(&image_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &[wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x4,
                        offset: 0,
                        shader_location: 0,
                    }],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview: None,
            cache: None,
        });
        let image_capacity = 16u64;
        let image_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image-instances"),
            size: image_capacity * 16,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let image_common_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image-bind"),
            layout: &image_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: image_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: vp_buf.as_entire_binding(),
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tex-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Renderer {
            device,
            queue,
            surface,
            config,
            viewport,
            atlas,
            text_renderer,
            swash_cache,
            quad_pipeline,
            quad_bind,
            quad_buf,
            quad_capacity,
            image_pipeline,
            image_common_bind,
            image_buf,
            image_capacity,
            vp_buf,
            sampler,
        }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Upload a decoded image to a texture + bind group and register it.
    pub fn upload_image(
        &mut self,
        images: &mut GpuImageSet,
        block: u32,
        rgba: &image::RgbaImage,
        disp_w: f32,
        disp_h: f32,
    ) {
        let size = wgpu::Extent3d {
            width: rgba.width().max(1),
            height: rgba.height().max(1),
            depth_or_array_layers: 1,
        };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mdview-image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rgba.width() * 4),
                rows_per_image: Some(rgba.height()),
            },
            size,
        );
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bgl = self.image_pipeline.get_bind_group_layout(1);
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("img-bind"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let bytes = (rgba.width() as usize * rgba.height() as usize * 4).max(1);
        images.insert(block, view, bind, disp_w, disp_h, bytes);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        font_system: &mut glyphon::FontSystem,
        clear: wgpu::Color,
        quads: &[Quad],
        image_draws: &[(f32, f32, f32, f32, u32)],
        images: &mut GpuImageSet,
        areas: Vec<glyphon::TextArea>,
    ) {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                self.resize(self.config.width.max(1), self.config.height.max(1));
                return;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let vp: [f32; 2] = [self.config.width as f32, self.config.height as f32];
        let vp_bytes: Vec<u8> = [vp[0].to_ne_bytes(), vp[1].to_ne_bytes()].concat();
        self.queue.write_buffer(&self.vp_buf, 0, &vp_bytes);

        self.viewport.update(
            &self.queue,
            glyphon::Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        if !quads.is_empty() {
            let needed = quads.len() as u64 * 32;
            if needed > self.quad_capacity {
                self.quad_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("quad-instances"),
                    size: needed * 2,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.quad_capacity = needed * 2;
                let bgl = self.quad_pipeline.get_bind_group_layout(0);
                self.quad_bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("quad-bind"),
                    layout: &bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.quad_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: self.vp_buf.as_entire_binding(),
                        },
                    ],
                });
            }
            self.queue.write_buffer(&self.quad_buf, 0, as_bytes(quads));
        }

        if !image_draws.is_empty() {
            let instances: Vec<ImageInstance> = image_draws
                .iter()
                .map(|&(x, y, w, h, _)| ImageInstance { x, y, w, h })
                .collect();
            let needed = instances.len() as u64 * 16;
            if needed > self.image_capacity {
                self.image_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("image-instances"),
                    size: needed * 2,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.image_capacity = needed * 2;
                let bgl = self.image_pipeline.get_bind_group_layout(0);
                self.image_common_bind =
                    self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("image-bind"),
                        layout: &bgl,
                        entries: &[
                            wgpu::BindGroupEntry {
                                binding: 0,
                                resource: self.image_buf.as_entire_binding(),
                            },
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: self.vp_buf.as_entire_binding(),
                            },
                        ],
                    });
            }
            self.queue
                .write_buffer(&self.image_buf, 0, as_bytes(&instances));
        }

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            )
            .expect("glyphon prepare");

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("mdview"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mdview-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if !quads.is_empty() {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_bind_group(0, &self.quad_bind, &[]);
                pass.draw(0..6, 0..quads.len() as u32);
            }

            if !image_draws.is_empty() {
                pass.set_pipeline(&self.image_pipeline);
                pass.set_bind_group(0, &self.image_common_bind, &[]);
                for (i, &(_, _, _, _, block)) in image_draws.iter().enumerate() {
                    if let Some(img) = images.get(block) {
                        let bind = img.bind.clone();
                        images.touch(block);
                        pass.set_bind_group(1, &bind, &[]);
                        pass.draw(0..6, i as u32..(i as u32 + 1));
                    }
                }
            }

            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .expect("glyphon render");
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        self.atlas.trim();
    }
}

// Keep unused imports referenced (Mutex used by callers building areas).
