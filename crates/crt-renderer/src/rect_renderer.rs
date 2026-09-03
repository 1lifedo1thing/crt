//! Rect renderer for solid color rectangles using instanced quads
//!
//! Renders colored rectangles for cell backgrounds, the tab bar and overlays.
//! All rectangles of one pass render in a single draw call.
//!
//! Two upload paths exist:
//! - `render()` uses a buffer owned by this renderer and only re-uploads when
//!   the instance list changed. Use it for content that persists across
//!   frames (cell backgrounds).
//! - `render_transient()` bump-allocates from a [`FrameArena`], so several
//!   passes in one frame can share one renderer without overwriting each
//!   other's data. Use it for overlays and per-pass UI.

use std::sync::Arc;

use crate::frame_arena::{FrameArena, OwnedInstanceBuffer};
use crate::shared_pipelines::SharedRectPipeline;
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// Per-instance data for a rectangle
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct RectInstance {
    /// Screen position (top-left of rect)
    pub pos: [f32; 2],
    /// Rect size in pixels
    pub size: [f32; 2],
    /// RGBA color
    pub color: [f32; 4],
}

/// Global uniforms for the rect shader
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Globals {
    screen_size: [f32; 2],
    _pad: [f32; 2],
}

/// Rect renderer using instanced quads
pub struct RectRenderer {
    /// Shared pipeline objects (pipeline, bind group layout).
    shared: Arc<SharedRectPipeline>,
    globals_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    /// Pending instances to render
    instances: Vec<RectInstance>,
    /// Instances changed since the last owned upload
    dirty: bool,
    owned: OwnedInstanceBuffer,
    /// Cached screen size to avoid redundant uniform updates
    cached_screen_size: (f32, f32),
}

impl RectRenderer {
    /// Create a rect renderer using shared pipeline objects.
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedRectPipeline>) -> Self {
        let (globals_buffer, bind_group) =
            Self::create_per_window_resources(device, &shared.bind_group_layout);

        Self {
            shared: shared.clone(),
            globals_buffer,
            bind_group,
            instances: Vec::new(),
            dirty: false,
            owned: OwnedInstanceBuffer::new("Rect Instance Buffer"),
            cached_screen_size: (0.0, 0.0),
        }
    }

    /// Create a rect renderer with its own pipeline objects.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedRectPipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    fn create_per_window_resources(
        device: &wgpu::Device,
        bind_group_layout: &wgpu::BindGroupLayout,
    ) -> (wgpu::Buffer, wgpu::BindGroup) {
        let globals = Globals {
            screen_size: [1.0, 1.0],
            _pad: [0.0, 0.0],
        };

        let globals_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Rect Globals Buffer"),
            contents: bytemuck::cast_slice(&[globals]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Rect Bind Group"),
            layout: bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buffer.as_entire_binding(),
            }],
        });

        (globals_buffer, bind_group)
    }

    /// Clear pending instances
    pub fn clear(&mut self) {
        if !self.instances.is_empty() {
            self.dirty = true;
        }
        self.instances.clear();
    }

    /// Add a rectangle
    pub fn push_rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [f32; 4]) {
        self.instances.push(RectInstance {
            pos: [x, y],
            size: [width, height],
            color,
        });
        self.dirty = true;
    }

    /// Get the number of pending instances
    pub fn instance_count(&self) -> usize {
        self.instances.len()
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
            _pad: [0.0, 0.0],
        };
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::cast_slice(&[globals]));
    }

    fn bind(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
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
        self.bind(render_pass);
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
        let Some(slice) = arena.push(queue, &self.instances) else {
            return;
        };
        self.bind(render_pass);
        render_pass.set_vertex_buffer(0, arena.slice(slice));
        render_pass.draw(0..4, 0..slice.count);
    }
}

impl Drop for RectRenderer {
    fn drop(&mut self) {
        // Destroy globals buffer to release GPU memory immediately
        self.globals_buffer.destroy();
    }
}
