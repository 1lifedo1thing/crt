//! CRT Renderer - GPU-accelerated text and effect rendering
//!
//! This crate provides a two-layer rendering architecture:
//! - Background layer: gradient + animated grid + optional background image (runs every frame)
//! - Text overlay: rendered only when content changes, composited on top
//!
//! This separation allows smooth 60fps animation while only re-rendering
//! text when it actually changes.

#![allow(clippy::collapsible_if)]
#![allow(clippy::manual_strip)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::wrong_self_convention)]

pub mod background_image;
pub mod effects;
pub mod frame_arena;
pub mod glyph_cache;
pub mod golden;
pub mod grid_renderer;
pub mod headless;
pub mod mock;
pub mod rect_renderer;
pub mod shaders;
pub mod shared_pipelines;
pub mod sprite_renderer;
pub mod tab_bar;
pub mod terminal_vello;
pub mod traits;

pub use background_image::{BackgroundImageState, BackgroundTexture, ImageFrame, LoadedImage};
pub use effects::{
    BackdropEffect, EffectConfig, EffectsRenderer, GridEffect, MatrixEffect, MotionBehavior,
    ParticleEffect, Position, RainEffect, ShapeEffect, SpriteEffect, StarfieldEffect,
};
pub use golden::{ComparisonResult, assert_visual_match, compare_images, compare_with_golden, golden_path};
pub use headless::{HeadlessError, HeadlessRenderer};
pub use glyph_cache::{
    CachedGlyph, FontData, FontVariants, GlyphCache, GlyphKey, GlyphStyle, PositionedGlyph,
};
pub use frame_arena::{ArenaSlice, FrameArena};
pub use grid_renderer::GridRenderer;
pub use shared_pipelines::SharedPipelines;
pub use mock::{MockRenderer, RenderCall};
pub use rect_renderer::RectRenderer;
pub use sprite_renderer::{
    OriginalSpriteValues, SpriteAnimationState, SpriteConfig, SpriteMotion, SpriteOverlayState,
    SpritePosition, SpriteRenderer, SpriteSheet, SpriteTexture,
};
pub use tab_bar::{
    DragFeedback, DragMode, EditState, Tab, TabBar, TabBarState, TabLayout, TabRect,
    VelloTabBarRenderer,
};
pub use terminal_vello::{CursorShape, CursorState, TerminalVelloRenderer};
pub use traits::{
    BackdropRenderer, CellContent, Color, ContextMenuItem, CursorInfo,
    CursorShape as TraitCursorShape, GridPosition, Rect, SearchHighlight, SelectionRange,
    TabRenderInfo, TextRenderer, UiRenderer,
};

use std::sync::Arc;

use bytemuck::cast_slice;
use crt_theme::Theme;
use shared_pipelines::{
    GLOW_BLUR_FORMAT, SharedBackgroundImagePipeline, SharedBackgroundPipeline,
    SharedCompositePipeline, SharedCrtPipeline,
};
use wgpu::util::DeviceExt;

/// Background pipeline - renders gradient + animated grid
pub struct BackgroundPipeline {
    shared: Arc<SharedBackgroundPipeline>,
    uniform_buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    theme: Arc<Theme>,
    /// (width, height, theme pointer) of the last uniform write
    uploaded: Option<(f32, f32, *const Theme)>,
}

impl BackgroundPipeline {
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedBackgroundPipeline>) -> Self {
        let theme = Arc::new(Theme::default());
        let uniforms = theme.to_uniforms(1.0, 1.0, 0.0);

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Background Uniform Buffer"),
            contents: cast_slice(&[uniforms]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Background Bind Group"),
            layout: &shared.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        Self {
            shared: shared.clone(),
            uniform_buffer,
            bind_group,
            theme,
            uploaded: None,
        }
    }

    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedBackgroundPipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn theme_arc(&self) -> &Arc<Theme> {
        &self.theme
    }

    /// Write the theme uniforms if the size or theme changed since the last
    /// write (the shader does not read `time`).
    pub fn update_uniforms(&mut self, queue: &wgpu::Queue, width: f32, height: f32) {
        let key = (width, height, Arc::as_ptr(&self.theme));
        if self.uploaded == Some(key) {
            return;
        }
        let uniforms = self.theme.to_uniforms(width, height, 0.0);
        queue.write_buffer(&self.uniform_buffer, 0, cast_slice(&[uniforms]));
        self.uploaded = Some(key);
    }

    pub fn render<'a>(&'a self, render_pass: &mut wgpu::RenderPass<'a>) {
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw(0..4, 0..1);
    }
}

/// Uniform buffer for background image shader
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BackgroundImageUniforms {
    /// UV transform: (scale_x, scale_y, offset_x, offset_y)
    pub uv_transform: [f32; 4],
    /// Opacity (0-1)
    pub opacity: f32,
    /// Padding for alignment
    pub _pad: [f32; 3],
}

/// Background image pipeline - renders textured background with sizing/positioning
pub struct BackgroundImagePipeline {
    shared: Arc<SharedBackgroundImagePipeline>,
    uniform_buffer: wgpu::Buffer,
}

impl BackgroundImagePipeline {
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedBackgroundImagePipeline>) -> Self {
        let uniforms = BackgroundImageUniforms {
            uv_transform: [1.0, 1.0, 0.0, 0.0],
            opacity: 1.0,
            _pad: [0.0; 3],
        };

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Background Image Uniform Buffer"),
            contents: cast_slice(&[uniforms]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            shared: shared.clone(),
            uniform_buffer,
        }
    }

    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedBackgroundImagePipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Background Image Bind Group"),
            layout: &self.shared.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.shared.sampler),
                },
            ],
        })
    }

    pub fn update_uniforms(&self, queue: &wgpu::Queue, uv_transform: [f32; 4], opacity: f32) {
        let uniforms = BackgroundImageUniforms {
            uv_transform,
            opacity,
            _pad: [0.0; 3],
        };
        queue.write_buffer(&self.uniform_buffer, 0, cast_slice(&[uniforms]));
    }

    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        bind_group: &'a wgpu::BindGroup,
    ) {
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..4, 0..1);
    }
}

/// Composite pipeline - blends text onto screen with glow
pub struct CompositePipeline {
    shared: Arc<SharedCompositePipeline>,
    uniform_buffer: wgpu::Buffer,
    theme: Arc<Theme>,
    /// (width, height, theme pointer) of the last uniform write
    uploaded: Option<(f32, f32, *const Theme)>,
    /// Horizontally-blurred alpha target (sized to the frame)
    blur: Option<GlowBlurTarget>,
}

/// Intermediate texture for the separable glow blur.
struct GlowBlurTarget {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    size: (u32, u32),
}

impl Drop for GlowBlurTarget {
    fn drop(&mut self) {
        self.texture.destroy();
    }
}

/// Pixel-space scissor for the glow passes: the rows that hold glyphs,
/// expanded by the blur radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlowRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl GlowRegion {
    /// Region covering `bounds` (min_x, min_y, max_x, max_y) grown by
    /// `radius` pixels and clamped to `width` x `height`. `None` if empty.
    pub fn from_bounds(bounds: [f32; 4], radius: f32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        let grow = radius.max(0.0) + 1.0;
        let x0 = (bounds[0] - grow).floor().max(0.0) as u32;
        let y0 = (bounds[1] - grow).floor().max(0.0) as u32;
        let x1 = ((bounds[2] + grow).ceil().max(0.0) as u32).min(width);
        let y1 = ((bounds[3] + grow).ceil().max(0.0) as u32).min(height);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Self {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

impl CompositePipeline {
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedCompositePipeline>) -> Self {
        let theme = Arc::new(Theme::default());
        let uniforms = theme.to_uniforms(1.0, 1.0, 0.0);

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Composite Uniform Buffer"),
            contents: cast_slice(&[uniforms]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            shared: shared.clone(),
            uniform_buffer,
            theme,
            uploaded: None,
            blur: None,
        }
    }

    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedCompositePipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    /// Ensure the blur target matches the frame size. Returns true when it
    /// was (re)created, in which case the horizontal blur must be re-run.
    pub fn ensure_blur_target(&mut self, device: &wgpu::Device, width: u32, height: u32) -> bool {
        let size = (width.max(1), height.max(1));
        if self.blur.as_ref().is_some_and(|b| b.size == size) {
            return false;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Glow Blur Texture"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: GLOW_BLUR_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Glow Blur Bind Group"),
            layout: &self.shared.blur_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.shared.sampler),
                },
            ],
        });
        self.blur = Some(GlowBlurTarget {
            texture,
            bind_group,
            size,
        });
        true
    }

    /// Glow radius in pixels the shader will sample over (0 when no glow).
    pub fn glow_radius(&self) -> f32 {
        match self.theme.text_shadow {
            Some(shadow) if shadow.intensity > 0.0 => shadow.radius.min(50.0),
            _ => 0.0,
        }
    }

    /// Run the horizontal blur into the blur texture (call when the text
    /// texture changed). `region` limits the work to the rows with glyphs.
    pub fn render_hblur(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        text_bind_group: &wgpu::BindGroup,
        region: GlowRegion,
    ) {
        let Some(blur) = &self.blur else {
            return;
        };
        let view = blur.texture.create_view(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Glow HBlur Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_scissor_rect(region.x, region.y, region.width, region.height);
        pass.set_pipeline(&self.shared.hblur_pipeline);
        pass.set_bind_group(0, text_bind_group, &[]);
        pass.draw(0..4, 0..1);
    }

    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.theme = theme;
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        text_texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Composite Bind Group"),
            layout: &self.shared.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(text_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.shared.sampler),
                },
            ],
        })
    }

    /// Write the theme uniforms if the size or theme changed since the last
    /// write (the shader does not read `time`).
    pub fn update_uniforms(&mut self, queue: &wgpu::Queue, width: f32, height: f32) {
        let key = (width, height, Arc::as_ptr(&self.theme));
        if self.uploaded == Some(key) {
            return;
        }
        let uniforms = self.theme.to_uniforms(width, height, 0.0);
        queue.write_buffer(&self.uniform_buffer, 0, cast_slice(&[uniforms]));
        self.uploaded = Some(key);
    }

    /// Vertical blur + text composite. `region`, when given, scissors the
    /// pass to the rows holding glyphs.
    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        bind_group: &'a wgpu::BindGroup,
        region: Option<GlowRegion>,
    ) {
        let Some(blur) = &self.blur else {
            return;
        };
        if let Some(r) = region {
            render_pass.set_scissor_rect(r.x, r.y, r.width, r.height);
        }
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_bind_group(1, &blur.bind_group, &[]);
        render_pass.draw(0..4, 0..1);
    }
}

// Keep the old EffectPipeline for backwards compatibility during transition
pub struct EffectPipeline {
    pub background: BackgroundPipeline,
    pub composite: CompositePipeline,
}

impl EffectPipeline {
    pub fn new_with_shared(
        device: &wgpu::Device,
        background_shared: &Arc<SharedBackgroundPipeline>,
        composite_shared: &Arc<SharedCompositePipeline>,
    ) -> Self {
        Self {
            background: BackgroundPipeline::new_with_shared(device, background_shared),
            composite: CompositePipeline::new_with_shared(device, composite_shared),
        }
    }

    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self {
            background: BackgroundPipeline::new(device, target_format),
            composite: CompositePipeline::new(device, target_format),
        }
    }

    pub fn set_theme(&mut self, theme: Arc<Theme>) {
        self.background.set_theme(theme.clone());
        self.composite.set_theme(theme);
    }

    pub fn theme(&self) -> &Theme {
        self.background.theme()
    }

    pub fn theme_arc(&self) -> &Arc<Theme> {
        self.background.theme_arc()
    }

    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        text_texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        self.composite.create_bind_group(device, text_texture_view)
    }

    pub fn update_uniforms(&mut self, queue: &wgpu::Queue, width: f32, height: f32) {
        self.background.update_uniforms(queue, width, height);
        self.composite.update_uniforms(queue, width, height);
    }

}

/// Reference height for resolution-independent CRT effects (1080p baseline)
const CRT_REFERENCE_HEIGHT: f32 = 1080.0;

/// Uniform buffer for CRT post-processing shader
/// Must match CrtParams struct in crt.wgsl (64 bytes, 16-byte aligned)
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CrtUniforms {
    pub screen_size: [f32; 2],     // 8 bytes
    pub time: f32,                 // 4 bytes
    pub scanline_intensity: f32,   // 4 bytes = 16 bytes
    pub scanline_frequency: f32,   // 4 bytes
    pub curvature: f32,            // 4 bytes
    pub vignette: f32,             // 4 bytes
    pub chromatic_aberration: f32, // 4 bytes = 32 bytes
    pub bloom: f32,                // 4 bytes
    pub flicker: f32,              // 4 bytes
    pub reference_height: f32,     // 4 bytes - baseline resolution for scaling
    pub _pad: [f32; 5],            // 20 bytes = 64 bytes total
}

/// CRT post-processing pipeline - applies scanlines, curvature, vignette
pub struct CrtPipeline {
    shared: Arc<SharedCrtPipeline>,
    uniform_buffer: wgpu::Buffer,
    start_time: std::time::Instant,
    enabled: bool,
    params: crt_theme::CrtEffect,
}

impl CrtPipeline {
    pub fn new_with_shared(device: &wgpu::Device, shared: &Arc<SharedCrtPipeline>) -> Self {
        let uniforms = CrtUniforms {
            screen_size: [1.0, 1.0],
            time: 0.0,
            scanline_intensity: 0.0,
            scanline_frequency: 2.0,
            curvature: 0.0,
            vignette: 0.0,
            chromatic_aberration: 0.0,
            bloom: 0.0,
            flicker: 0.0,
            reference_height: CRT_REFERENCE_HEIGHT,
            _pad: [0.0; 5],
        };

        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("CRT Uniform Buffer"),
            contents: cast_slice(&[uniforms]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        Self {
            shared: shared.clone(),
            uniform_buffer,
            start_time: std::time::Instant::now(),
            enabled: false,
            params: crt_theme::CrtEffect::default(),
        }
    }

    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shared = Arc::new(SharedCrtPipeline::new(device, target_format));
        Self::new_with_shared(device, &shared)
    }

    /// Set CRT effect from theme
    pub fn set_effect(&mut self, effect: Option<crt_theme::CrtEffect>) {
        if let Some(crt) = effect {
            self.enabled = crt.enabled;
            self.params = crt;
        } else {
            self.enabled = false;
        }
    }

    /// Check if CRT effect is enabled
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn create_bind_group(
        &self,
        device: &wgpu::Device,
        input_texture_view: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("CRT Bind Group"),
            layout: &self.shared.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniform_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(input_texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.shared.sampler),
                },
            ],
        })
    }

    pub fn update_uniforms(&self, queue: &wgpu::Queue, width: f32, height: f32) {
        let time = self.start_time.elapsed().as_secs_f32();
        let uniforms = CrtUniforms {
            screen_size: [width, height],
            time,
            scanline_intensity: self.params.scanline_intensity,
            scanline_frequency: self.params.scanline_frequency,
            curvature: self.params.curvature,
            vignette: self.params.vignette,
            chromatic_aberration: self.params.chromatic_aberration,
            bloom: self.params.bloom,
            flicker: self.params.flicker,
            reference_height: CRT_REFERENCE_HEIGHT,
            _pad: [0.0; 5],
        };
        queue.write_buffer(&self.uniform_buffer, 0, cast_slice(&[uniforms]));
    }

    pub fn render<'a>(
        &'a self,
        render_pass: &mut wgpu::RenderPass<'a>,
        bind_group: &'a wgpu::BindGroup,
    ) {
        render_pass.set_pipeline(&self.shared.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..4, 0..1);
    }
}

// Drop implementations to release GPU resources immediately on window close

impl Drop for BackgroundPipeline {
    fn drop(&mut self) {
        self.uniform_buffer.destroy();
    }
}

impl Drop for CompositePipeline {
    fn drop(&mut self) {
        self.uniform_buffer.destroy();
    }
}

impl Drop for BackgroundImagePipeline {
    fn drop(&mut self) {
        self.uniform_buffer.destroy();
    }
}

impl Drop for CrtPipeline {
    fn drop(&mut self) {
        self.uniform_buffer.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shaders::builtin;

    #[test]
    fn test_shaders_compile() {
        assert!(builtin::BACKGROUND.contains("vs_main"));
        assert!(builtin::BACKGROUND.contains("fs_main"));
        assert!(builtin::COMPOSITE.contains("vs_main"));
        assert!(builtin::COMPOSITE.contains("fs_main"));
    }
}
