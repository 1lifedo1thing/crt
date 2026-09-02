//! Grid renderer for terminal text using instanced quads
//!
//! Renders glyphs as instanced quads, sampling from a glyph atlas.
//! Each glyph is one instance with position, UV coords, and color.
//! All text of one pass renders in a single draw call.
//!
//! `render()` uploads to a renderer-owned buffer only when the instance list
//! changed; `render_transient()` bump-allocates from a [`FrameArena`] so one
//! renderer can serve several passes in a frame (see `frame_arena.rs`).

use std::sync::Arc;

use crate::frame_arena::{FrameArena, OwnedInstanceBuffer};
use crate::glyph_cache::PositionedGlyph;
use crate::shared_pipelines::SharedGridPipeline;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// Per-instance data for a glyph
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct GlyphInstance {
    /// Screen position (top-left of glyph)
    pub pos: [f32; 2],
    /// UV min (atlas coordinates)
    pub uv_min: [f32; 2],
    /// UV max (atlas coordinates)
    pub uv_max: [f32; 2],
    /// Glyph size in pixels
    pub size: [f32; 2],
    /// RGBA color
    pub color: [f32; 4],
}

impl GlyphInstance {
    pub fn from_positioned(glyph: &PositionedGlyph, color: [f32; 4]) -> Self {
        Self {
            pos: [glyph.x, glyph.y],
            uv_min: glyph.uv_min,
            uv_max: glyph.uv_max,
            size: [glyph.width, glyph.height],
            color,
        }
    }
}

/// Global uniforms for the grid shader
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Globals {
    screen_size: [f32; 2],
    atlas_size: [f32; 2],
}

/// Grid renderer using instanced quads
///
/// Pipeline objects (pipeline, bind group layout, sampler) can be shared across
/// windows via `new_with_shared()` to avoid duplicating Metal shader caches.
pub struct GridRenderer {
    /// Shared pipeline objects (pipeline, bind group layout, sampler).
    /// Multiple GridRenderers can share the same Arc to avoid duplicate
    /// Metal shader compilation (~5-15 MB per pipeline on macOS).
    shared: Arc<SharedGridPipeline>,
    globals_buffer: wgpu::Buffer,
    bind_group: Option<wgpu::BindGroup>,
    /// Pending instances to render
    instances: Vec<GlyphInstance>,
    /// Instances changed since the last owned upload
    dirty: bool,
    owned: OwnedInstanceBuffer,
    /// Pixel bounds of pending instances: (min_x, min_y, max_x, max_y)
    bounds: Option<[f32; 4]>,
    /// Cached screen size to avoid redundant uniform updates
    cached_screen_size: (f32, f32),
}

impl GridRenderer {
    /// Create a grid renderer using shared pipeline objects.
    ///
    /// This avoids duplicating the compiled Metal pipeline and shader caches
    /// across multiple windows, saving ~5-15 MB per additional window.
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedGridPipeline>) -> Self {
        let globals_buffer = Self::create_globals_buffer(device);

        Self {
            shared: shared.clone(),
            globals_buffer,
            bind_group: None,
            instances: Vec::new(),
            dirty: false,
            owned: OwnedInstanceBuffer::new("Grid Instance Buffer"),
            bounds: None,
            cached_screen_size: (0.0, 0.0),
        }
    }

    /// Create a grid renderer with its own pipeline objects.
    ///
    /// Prefer `new_with_shared()` when creating multiple renderers to avoid
    /// duplicating GPU pipeline state.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedGridPipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    fn create_globals_buffer(device: &wgpu::Device) -> wgpu::Buffer {
        let globals = Globals {
            screen_size: [1.0, 1.0],
            atlas_size: [1024.0, 1024.0],
        };
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Grid Globals Buffer"),
            contents: bytemuck::cast_slice(&[globals]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        })
    }

    /// Update the bind group with a new glyph cache atlas
    pub fn set_glyph_cache(
        &mut self,
        device: &wgpu::Device,
        glyph_cache: &crate::glyph_cache::GlyphCache,
    ) {
        self.bind_group = Some(device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Grid Bind Group"),
            layout: &self.shared.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&glyph_cache.atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.shared.sampler),
                },
            ],
        }));
    }

    /// Clear pending instances
    pub fn clear(&mut self) {
        if !self.instances.is_empty() {
            self.dirty = true;
        }
        self.instances.clear();
        self.bounds = None;
    }

    fn extend_bounds(&mut self, g: &PositionedGlyph) {
        let b = self.bounds.get_or_insert([g.x, g.y, g.x, g.y]);
        b[0] = b[0].min(g.x);
        b[1] = b[1].min(g.y);
        b[2] = b[2].max(g.x + g.width);
        b[3] = b[3].max(g.y + g.height);
    }

    /// Add positioned glyphs from layout
    pub fn push_glyphs(&mut self, glyphs: &[PositionedGlyph], color: [f32; 4]) {
        for g in glyphs {
            self.extend_bounds(g);
            self.instances.push(GlyphInstance::from_positioned(g, color));
        }
        self.dirty |= !glyphs.is_empty();
    }

    /// Add a single positioned glyph
    pub fn push_glyph(&mut self, glyph: &PositionedGlyph, color: [f32; 4]) {
        self.extend_bounds(glyph);
        self.instances.push(GlyphInstance::from_positioned(glyph, color));
        self.dirty = true;
    }

    /// Pixel bounds of the pending glyphs as (min_x, min_y, max_x, max_y),
    /// or `None` when there are no glyphs.
    pub fn bounds(&self) -> Option<[f32; 4]> {
        self.bounds
    }

    /// Update screen size uniform (only writes if size changed)
    pub fn update_screen_size(&mut self, queue: &wgpu::Queue, width: f32, height: f32) {
        // Skip if size hasn't changed
        if self.cached_screen_size == (width, height) {
            return;
        }
        self.cached_screen_size = (width, height);

        let globals = Globals {
            screen_size: [width, height],
            atlas_size: [1024.0, 1024.0],
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::cast_slice(&[globals]));
    }

    fn bind(&self, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(bind_group) = &self.bind_group else {
            return false;
        };
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        true
    }

    /// Render from the renderer-owned buffer, uploading only when the
    /// instance list changed since the last upload.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        render_pass: &mut wgpu::RenderPass<'_>,
    ) {
        if self.instances.is_empty() {
            return;
        }
        if self.dirty {
            self.owned.upload(device, queue, &self.instances);
            self.dirty = false;
        }
        let Some(buffer) = self.owned.buffer() else {
            return;
        };
        if !self.bind(render_pass) {
            return;
        }
        render_pass.set_vertex_buffer(0, buffer.slice(..));
        // Draw 4 vertices per instance (triangle strip quad)
        render_pass.draw(0..4, 0..self.instances.len() as u32);
    }

    /// Render from a per-frame arena. Safe to call several times per frame
    /// from different passes with different instance lists.
    pub fn render_transient(
        &self,
        queue: &wgpu::Queue,
        render_pass: &mut wgpu::RenderPass<'_>,
        arena: &mut FrameArena,
    ) {
        if self.bind_group.is_none() {
            return;
        }
        let Some(slice) = arena.push(queue, &self.instances) else {
            return;
        };
        self.bind(render_pass);
        render_pass.set_vertex_buffer(0, arena.slice(slice));
        render_pass.draw(0..4, 0..slice.count);
    }

    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }
}

impl Drop for GridRenderer {
    fn drop(&mut self) {
        // Destroy globals buffer to release GPU memory immediately
        self.globals_buffer.destroy();
    }
}
