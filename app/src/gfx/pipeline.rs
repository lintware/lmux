//! Drawing a pane's [`Frame`] with wgpu inside egui's render pass.
//!
//! The pipelines and the two glyph atlases are created once
//! ([`install`]); each pane keeps its own uniform, background and instance
//! buffers, grown as needed and dropped a few seconds after the pane was
//! last drawn. A paint is three draws: the pane's background, the cell
//! backgrounds, then every glyph quad (`cell.wgsl`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use eframe::egui_wgpu::{self, wgpu, CallbackResources, CallbackTrait, ScreenDescriptor};

use super::frame::{CellText, Frame};
use super::glyphs::SharedAtlases;

/// Buffers of a pane not drawn for this long are freed.
const IDLE: Duration = Duration::from_secs(5);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod, Zeroable)]
struct Uniforms {
    origin: [f32; 2],
    screen: [f32; 2],
    cell: [f32; 2],
    pad: [f32; 2],
    grid: u32,
    min_contrast: f32,
    cursor_pos: u32,
    cursor_color: u32,
    bg_color: u32,
    bools: u32,
    _p: [u32; 2],
}

struct AtlasTexture {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    size: u32,
    uploaded: u64,
}

struct PaneGpu {
    uniforms: wgpu::Buffer,
    bg: wgpu::Buffer,
    bg_cap: u64,
    text: wgpu::Buffer,
    text_cap: u64,
    bind_group: wgpu::BindGroup,
    text_count: u32,
    used: Instant,
}

/// The renderer's GPU state, kept in egui's callback resources.
pub struct TermRenderer {
    bg_color: wgpu::RenderPipeline,
    cell_bg: wgpu::RenderPipeline,
    cell_text: wgpu::RenderPipeline,
    pane_layout: wgpu::BindGroupLayout,
    atlas_layout: wgpu::BindGroupLayout,
    gray: AtlasTexture,
    color: AtlasTexture,
    atlas_group: wgpu::BindGroup,
    panes: HashMap<u64, PaneGpu>,
}

/// Set up the pipelines on egui's renderer. Called once from the app.
pub fn install(rs: &egui_wgpu::RenderState) {
    let r = TermRenderer::new(&rs.device, rs.target_format);
    rs.renderer.write().callback_resources.insert(r);
}

impl TermRenderer {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> TermRenderer {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lmux_cell"),
            source: wgpu::ShaderSource::Wgsl(include_str!("cell.wgsl").into()),
        });
        let vis = wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT;
        let pane_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lmux_pane"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: vis,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: vis,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let tex = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lmux_atlas"),
            entries: &[tex(0), tex(1)],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lmux_cells"),
            bind_group_layouts: &[Some(&pane_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let premultiplied = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let pipeline = |label, vs, fs, blend: Option<wgpu::BlendState>, buffers: &[Option<wgpu::VertexBufferLayout>], topology| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    buffers,
                    compilation_options: Default::default(),
                },
                primitive: wgpu::PrimitiveState { topology, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState { format, blend, write_mask: wgpu::ColorWrites::ALL })],
                    compilation_options: Default::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let text_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<CellText>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Uint32x2,
                1 => Uint32x2,
                2 => Sint16x2,
                3 => Uint16x2,
                4 => Uint32,
                5 => Uint32,
            ],
        };
        let list = wgpu::PrimitiveTopology::TriangleList;
        let bg_color = pipeline("lmux_bg_color", "full_screen_vertex", "bg_color_fragment", None, &[], list);
        let cell_bg =
            pipeline("lmux_cell_bg", "full_screen_vertex", "cell_bg_fragment", Some(premultiplied), &[], list);
        let cell_text = pipeline(
            "lmux_cell_text",
            "cell_text_vertex",
            "cell_text_fragment",
            Some(premultiplied),
            &[Some(text_layout)],
            wgpu::PrimitiveTopology::TriangleStrip,
        );

        let gray = atlas_texture(device, 512, wgpu::TextureFormat::R8Unorm);
        let color = atlas_texture(device, 512, wgpu::TextureFormat::Bgra8Unorm);
        let atlas_group = atlas_bind_group(device, &atlas_layout, &gray, &color);
        TermRenderer {
            bg_color,
            cell_bg,
            cell_text,
            pane_layout,
            atlas_layout,
            gray,
            color,
            atlas_group,
            panes: HashMap::new(),
        }
    }

    fn sync_atlases(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, atlases: &SharedAtlases) {
        let Ok(a) = atlases.lock() else { return };
        let mut rebind = false;
        for (gpu, cpu, format) in [
            (&mut self.gray, &a.gray, wgpu::TextureFormat::R8Unorm),
            (&mut self.color, &a.color, wgpu::TextureFormat::Bgra8Unorm),
        ] {
            if gpu.uploaded == cpu.modified && gpu.size == cpu.size {
                continue;
            }
            if gpu.size != cpu.size {
                *gpu = atlas_texture(device, cpu.size, format);
                rebind = true;
            }
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &gpu.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &cpu.data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(cpu.size * cpu.depth),
                    rows_per_image: Some(cpu.size),
                },
                wgpu::Extent3d { width: cpu.size, height: cpu.size, depth_or_array_layers: 1 },
            );
            gpu.uploaded = cpu.modified;
        }
        if rebind {
            self.atlas_group = atlas_bind_group(device, &self.atlas_layout, &self.gray, &self.color);
        }
    }
}

fn atlas_texture(device: &wgpu::Device, size: u32, format: wgpu::TextureFormat) -> AtlasTexture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lmux_atlas"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    AtlasTexture { texture, view, size, uploaded: u64::MAX }
}

fn atlas_bind_group(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    gray: &AtlasTexture,
    color: &AtlasTexture,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("lmux_atlas"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&gray.view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&color.view) },
        ],
    })
}

fn buffer(device: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16),
        usage: usage | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One pane's draw, placed in egui's paint list.
pub struct TermPaint {
    /// Identifies the pane's GPU buffers between frames.
    pub key: u64,
    pub frame: Arc<Frame>,
    pub atlases: SharedAtlases,
    /// The pane in framebuffer pixels: x, y, width, height, inside the
    /// window (the shaders map positions against this size).
    pub viewport: [u32; 4],
    /// Cell size and the grid's left/top offset inside the pane, in pixels.
    pub cell: [f32; 2],
    pub pad: [f32; 2],
}

impl CallbackTrait for TermPaint {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(r) = resources.get_mut::<TermRenderer>() else { return Vec::new() };
        r.sync_atlases(device, queue, &self.atlases);

        let now = Instant::now();
        r.panes.retain(|_, p| now.duration_since(p.used) < IDLE);
        let f = &self.frame;
        let bg_bytes = (f.bg_cells.len().max(1) * 4) as u64;
        let text_bytes = (f.text.len().max(1) * std::mem::size_of::<CellText>()) as u64;
        let needs_new = r.panes.get(&self.key).is_none_or(|p| p.bg_cap < bg_bytes || p.text_cap < text_bytes);
        if needs_new {
            // Grow with headroom so a resize does not reallocate every frame.
            let bg_cap = bg_bytes.next_power_of_two();
            let text_cap = text_bytes.next_power_of_two();
            let uniforms = buffer(device, "lmux_uniforms", std::mem::size_of::<Uniforms>() as u64, wgpu::BufferUsages::UNIFORM);
            let bg = buffer(device, "lmux_bg_cells", bg_cap, wgpu::BufferUsages::STORAGE);
            let text = buffer(device, "lmux_text", text_cap, wgpu::BufferUsages::VERTEX);
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("lmux_pane"),
                layout: &r.pane_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: uniforms.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: bg.as_entire_binding() },
                ],
            });
            r.panes.insert(self.key, PaneGpu { uniforms, bg, bg_cap, text, text_cap, bind_group, text_count: 0, used: now });
        }
        let Some(p) = r.panes.get_mut(&self.key) else { return Vec::new() };
        p.used = now;
        let u = Uniforms {
            origin: [self.viewport[0] as f32, self.viewport[1] as f32],
            screen: [self.viewport[2] as f32, self.viewport[3] as f32],
            cell: self.cell,
            pad: self.pad,
            grid: f.cols as u32 | (f.rows as u32) << 16,
            min_contrast: f.min_contrast,
            cursor_pos: f.cursor_pos,
            cursor_color: f.cursor_color,
            bg_color: f.bg_color,
            bools: if f.cursor_wide { 1 } else { 0 },
            _p: [0; 2],
        };
        queue.write_buffer(&p.uniforms, 0, bytemuck::bytes_of(&u));
        if !f.bg_cells.is_empty() {
            queue.write_buffer(&p.bg, 0, bytemuck::cast_slice(&f.bg_cells));
        }
        if !f.text.is_empty() {
            queue.write_buffer(&p.text, 0, bytemuck::cast_slice(&f.text));
        }
        p.text_count = f.text.len() as u32;
        Vec::new()
    }

    fn paint(&self, info: egui::PaintCallbackInfo, pass: &mut wgpu::RenderPass<'static>, resources: &CallbackResources) {
        let Some(r) = resources.get::<TermRenderer>() else { return };
        let Some(p) = r.panes.get(&self.key) else { return };
        let [x, y, w, h] = self.viewport;
        if w == 0 || h == 0 {
            return;
        }
        // The UI keeps the viewport inside the window; a stale one from a
        // resize mid-frame is skipped rather than rejected by wgpu.
        let screen = info.screen_size_px;
        if x + w > screen[0] || y + h > screen[1] {
            return;
        }
        pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
        pass.set_bind_group(0, &p.bind_group, &[]);
        pass.set_bind_group(1, &r.atlas_group, &[]);
        pass.set_pipeline(&r.bg_color);
        pass.draw(0..3, 0..1);
        pass.set_pipeline(&r.cell_bg);
        pass.draw(0..3, 0..1);
        if p.text_count > 0 {
            pass.set_pipeline(&r.cell_text);
            pass.set_vertex_buffer(0, p.text.slice(..));
            pass.draw(0..4, 0..p.text_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shaders parse and validate, and the uniform block matches the
    /// Rust struct (naga's layout of `Uniforms` is 64 bytes).
    #[test]
    fn cell_shader_is_valid_wgsl() {
        use wgpu::naga;
        let module = naga::front::wgsl::parse_str(include_str!("cell.wgsl")).expect("parses");
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::empty())
            .validate(&module)
            .expect("validates");
        let ty = module.types.iter().find(|(_, t)| t.name.as_deref() == Some("Uniforms")).expect("Uniforms").1;
        assert_eq!(ty.inner.size(module.to_ctx()) as usize, std::mem::size_of::<Uniforms>());
        assert_eq!(std::mem::size_of::<CellText>(), 32);
    }
}
