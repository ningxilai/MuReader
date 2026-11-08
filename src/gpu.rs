// SPDX-License-Identifier: AGPL-3.0-only
//! wgpu compositor.
//!
//! MuPDF still rasterizes pages on the worker thread. This module uploads the
//! finished bitmaps as textures and draws them, together with the outline,
//! command menu, prompt, highlights, and selection, as textured quads.
//!
//! The surface and textures deliberately use non-sRGB formats when the
//! surface supports them. MuReader's existing overlay math blends the stored
//! sRGB bytes directly; using non-sRGB targets keeps that arithmetic and the
//! visual result unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::document::PageBitmap;

const SOLID_TEXTURE: u64 = 0;

const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOutput {
    var out: VertexOutput;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.uv = uv;
    out.color = color;
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSampleLevel(tex, samp, in.uv, 0.0) * in.color;
}
"#;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }

    fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }

    fn is_empty(self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

#[derive(Clone, Copy)]
enum Command {
    Texture {
        id: u64,
        dst: Rect,
        clip: Rect,
    },
    Solid {
        dst: Rect,
        color: [f32; 4],
        clip: Rect,
    },
}

/// A frame's draw commands in painter's order. Bitmaps are uploaded
/// separately with [`GpuRenderer::upload`].
#[derive(Default)]
pub struct DrawList {
    commands: Vec<Command>,
}

impl DrawList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn texture(&mut self, id: u64, dst: Rect, clip: Rect) {
        if !dst.is_empty() && !clip.is_empty() {
            self.commands.push(Command::Texture { id, dst, clip });
        }
    }

    pub fn solid(&mut self, dst: Rect, color: [f32; 4], clip: Rect) {
        if !dst.is_empty() && !clip.is_empty() && color[3] > 0.0 {
            self.commands.push(Command::Solid { dst, color, clip });
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

struct TextureTile {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    bind_group: wgpu::BindGroup,
    /// Keep the texture alive; the bind group only owns a view.
    _texture: wgpu::Texture,
}

struct GpuTexture {
    revision: u64,
    width: u32,
    height: u32,
    tiles: Vec<TextureTile>,
}

struct Batch<'a> {
    start: u32,
    end: u32,
    bind_group: &'a wgpu::BindGroup,
    scissor: (u32, u32, u32, u32),
}

pub struct GpuRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    textures: HashMap<u64, GpuTexture>,
    max_texture: u32,
    srgb: bool,
}

impl GpuRenderer {
    pub fn new(window: Arc<Window>, width: u32, height: u32) -> Result<Self, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(window.clone()),
        ));
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("wgpu surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("wgpu adapter: {e}"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("MuReader"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| format!("wgpu device: {e}"))?;

        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| *format == wgpu::TextureFormat::Bgra8Unorm)
            .or_else(|| {
                capabilities
                    .formats
                    .iter()
                    .copied()
                    .find(|format| !format.is_srgb())
            })
            .or_else(|| capabilities.formats.first().copied())
            .ok_or_else(|| "wgpu surface has no formats".to_string())?;

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("MuReader sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("MuReader texture layout"),
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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("MuReader shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("MuReader pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("MuReader pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4
                    ],
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let white = PageBitmap {
            width: 1,
            height: 1,
            pixels: vec![0x00ff_ffff],
        };
        let mut renderer = Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            textures: HashMap::new(),
            max_texture: adapter.limits().max_texture_dimension_2d.max(1),
            srgb: format.is_srgb(),
        };
        renderer.textures.insert(
            SOLID_TEXTURE,
            renderer.new_texture(1, &white, renderer.max_texture),
        );
        Ok(renderer)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
    }

    /// Upload or replace a bitmap. `revision` is an opaque counter owned by
    /// the caller; an unchanged revision skips the upload.
    pub fn upload(&mut self, id: u64, revision: u64, bitmap: &PageBitmap) {
        if bitmap.width == 0 || bitmap.height == 0 {
            return;
        }
        if let Some(current) = self.textures.get(&id) {
            if current.revision == revision
                && current.width == bitmap.width
                && current.height == bitmap.height
            {
                return;
            }
        }
        let texture = self.new_texture(revision, bitmap, self.max_texture);
        self.textures.insert(id, texture);
    }

    fn new_texture(&self, revision: u64, bitmap: &PageBitmap, max_texture: u32) -> GpuTexture {
        let tile_size = max_texture.max(1);
        let mut tiles = Vec::new();
        let mut y = 0;
        while y < bitmap.height {
            let tile_h = (bitmap.height - y).min(tile_size);
            let mut x = 0;
            while x < bitmap.width {
                let tile_w = (bitmap.width - x).min(tile_size);
                let mut data = Vec::with_capacity(tile_w as usize * tile_h as usize * 4);
                for row in 0..tile_h {
                    let start = (y + row) as usize * bitmap.width as usize + x as usize;
                    for &pixel in &bitmap.pixels[start..start + tile_w as usize] {
                        data.push((pixel >> 16) as u8);
                        data.push((pixel >> 8) as u8);
                        data.push(pixel as u8);
                        data.push(0xff);
                    }
                }

                let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("MuReader bitmap"),
                    size: wgpu::Extent3d {
                        width: tile_w,
                        height: tile_h,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: if self.srgb {
                        wgpu::TextureFormat::Rgba8UnormSrgb
                    } else {
                        wgpu::TextureFormat::Rgba8Unorm
                    },
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(tile_w * 4),
                        rows_per_image: Some(tile_h),
                    },
                    wgpu::Extent3d {
                        width: tile_w,
                        height: tile_h,
                        depth_or_array_layers: 1,
                    },
                );
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("MuReader bitmap"),
                    layout: &self.bind_group_layout,
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
                tiles.push(TextureTile {
                    x,
                    y,
                    w: tile_w,
                    h: tile_h,
                    bind_group,
                    _texture: texture,
                });
                x += tile_w;
            }
            y += tile_h;
        }
        GpuTexture {
            revision,
            width: bitmap.width,
            height: bitmap.height,
            tiles,
        }
    }

    /// Draw one frame. Returns whether a frame was actually presented; a
    /// timeout or an occluded surface leaves the window's contents unchanged.
    pub fn render(&self, clear: [f32; 4], list: &DrawList) -> Result<bool, String> {
        let clear = self.solid_color(clear);
        let clear = wgpu::Color {
            r: clear[0] as f64,
            g: clear[1] as f64,
            b: clear[2] as f64,
            a: clear[3] as f64,
        };
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err("wgpu surface validation error".to_string());
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let (width, height) = (self.config.width, self.config.height);
        let mut vertices: Vec<Vertex> = Vec::new();
        let mut batches: Vec<Batch<'_>> = Vec::new();
        for command in &list.commands {
            match *command {
                Command::Texture { id, dst, clip } => {
                    if let Some(texture) = self.textures.get(&id) {
                        self.append_texture(
                            texture,
                            dst,
                            clip,
                            [1.0, 1.0, 1.0, 1.0],
                            (width, height),
                            &mut vertices,
                            &mut batches,
                        );
                    }
                }
                Command::Solid { dst, color, clip } => {
                    if let Some(texture) = self.textures.get(&SOLID_TEXTURE) {
                        let color = self.solid_color(color);
                        self.append_texture(
                            texture,
                            dst,
                            clip,
                            color,
                            (width, height),
                            &mut vertices,
                            &mut batches,
                        );
                    }
                }
            }
        }

        let buffer = if vertices.is_empty() {
            None
        } else {
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("MuReader vertices"),
                size: std::mem::size_of_val(vertices.as_slice()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue
                .write_buffer(&buffer, 0, bytemuck::cast_slice(&vertices));
            Some(buffer)
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("MuReader frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("MuReader frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(buffer) = buffer.as_ref() {
                pass.set_pipeline(&self.pipeline);
                pass.set_vertex_buffer(0, buffer.slice(..));
                for batch in &batches {
                    let (x, y, w, h) = batch.scissor;
                    pass.set_scissor_rect(x, y, w, h);
                    pass.set_bind_group(0, batch.bind_group, &[]);
                    pass.draw(batch.start..batch.end, 0..1);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(true)
    }

    /// Solid colors arrive as sRGB fractions because that is what the CPU
    /// compositor used. Convert only when the surface itself expects linear
    /// values.
    fn solid_color(&self, color: [f32; 4]) -> [f32; 4] {
        if self.srgb {
            [
                srgb_to_linear(color[0]),
                srgb_to_linear(color[1]),
                srgb_to_linear(color[2]),
                color[3],
            ]
        } else {
            color
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_texture<'a>(
        &'a self,
        texture: &'a GpuTexture,
        dst: Rect,
        clip: Rect,
        color: [f32; 4],
        (viewport_w, viewport_h): (u32, u32),
        vertices: &mut Vec<Vertex>,
        batches: &mut Vec<Batch<'a>>,
    ) {
        let clip_x0 = clip.x.max(0);
        let clip_y0 = clip.y.max(0);
        let clip_x1 = clip.right().min(viewport_w as i32);
        let clip_y1 = clip.bottom().min(viewport_h as i32);
        if clip_x0 >= clip_x1 || clip_y0 >= clip_y1 {
            return;
        }
        let scissor = (
            clip_x0 as u32,
            clip_y0 as u32,
            (clip_x1 - clip_x0) as u32,
            (clip_y1 - clip_y0) as u32,
        );

        for tile in &texture.tiles {
            let x0 = dst.x + (tile.x as i64 * dst.w as i64 / texture.width as i64) as i32;
            let x1 =
                dst.x + ((tile.x + tile.w) as i64 * dst.w as i64 / texture.width as i64) as i32;
            let y0 = dst.y + (tile.y as i64 * dst.h as i64 / texture.height as i64) as i32;
            let y1 =
                dst.y + ((tile.y + tile.h) as i64 * dst.h as i64 / texture.height as i64) as i32;
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            let left = x0 as f32 / viewport_w as f32 * 2.0 - 1.0;
            let right = x1 as f32 / viewport_w as f32 * 2.0 - 1.0;
            let top = 1.0 - y0 as f32 / viewport_h as f32 * 2.0;
            let bottom = 1.0 - y1 as f32 / viewport_h as f32 * 2.0;

            let start = vertices.len() as u32;
            let mut push = |position: [f32; 2], uv: [f32; 2]| {
                vertices.push(Vertex {
                    position,
                    uv,
                    color,
                });
            };
            push([left, top], [0.0, 0.0]);
            push([right, top], [1.0, 0.0]);
            push([left, bottom], [0.0, 1.0]);
            push([right, top], [1.0, 0.0]);
            push([right, bottom], [1.0, 1.0]);
            push([left, bottom], [0.0, 1.0]);

            let can_merge = batches.last().is_some_and(|batch| {
                std::ptr::eq(batch.bind_group, &tile.bind_group) && batch.scissor == scissor
            });
            if can_merge {
                if let Some(batch) = batches.last_mut() {
                    batch.end = vertices.len() as u32;
                }
            } else {
                batches.push(Batch {
                    start,
                    end: vertices.len() as u32,
                    bind_group: &tile.bind_group,
                    scissor,
                });
            }
        }
    }
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// The CPU compositor's `0x00RRGGBB` color as normalized sRGB fractions.
pub fn rgb(value: u32, alpha: f32) -> [f32; 4] {
    [
        ((value >> 16) & 0xff) as f32 / 255.0,
        ((value >> 8) & 0xff) as f32 / 255.0,
        (value & 0xff) as f32 / 255.0,
        alpha,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_edges_saturate() {
        let rect = Rect::new(i32::MAX - 1, 0, 10, 10);
        assert_eq!(rect.right(), i32::MAX);
    }

    #[test]
    fn rgb_converts_channels_in_srgb_order() {
        assert_eq!(
            rgb(0x0012_3456, 0.5),
            [18.0 / 255.0, 52.0 / 255.0, 86.0 / 255.0, 0.5]
        );
    }
}
