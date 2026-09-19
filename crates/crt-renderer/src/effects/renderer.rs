//! Effects renderer - manages and renders backdrop effects to texture
//!
//! The EffectsRenderer maintains a collection of backdrop effects,
//! updates their animation state each frame, and renders them to a
//! texture via the shared Vello renderer.
//!
//! Rendering is lazy: the vello scene is only re-encoded and re-rendered
//! when something changed (configuration, target size, or an enabled effect
//! that is actually animated). Static effects are rendered once and the
//! cached target texture is reused on subsequent frames.

use std::sync::{Arc, Mutex};

use vello::kurbo::Rect;
use vello::{AaConfig, RenderParams, Renderer, Scene, peniko};

use super::{BackdropEffect, EffectConfig};
use crate::shared_pipelines::SharedEffectsBlitPipeline;

/// Manages backdrop effects and renders them to a texture
///
/// # Usage
///
/// ```ignore
/// let effects_renderer = EffectsRenderer::new(device, vello_renderer, format);
///
/// // Configure from theme
/// effects_renderer.configure(&theme);
///
/// // Each frame:
/// effects_renderer.update(dt);
/// effects_renderer.render(device, queue, (width, height));
/// effects_renderer.composite(render_pass);
/// ```
pub struct EffectsRenderer {
    /// Collection of active effects
    effects: Vec<Box<dyn BackdropEffect>>,

    /// Shared Vello renderer (lazy-loaded, saves ~187MB when not used)
    vello_renderer: Arc<Mutex<Option<Renderer>>>,

    /// Vello scene built each frame
    scene: Scene,

    /// Render target texture
    target_texture: Option<wgpu::Texture>,

    /// Render target view
    target_view: Option<wgpu::TextureView>,

    /// Current target size
    target_size: (u32, u32),

    /// Total elapsed time in seconds.
    ///
    /// Stored as `f64`: an `f32` accumulator quantises to ~4 ms after
    /// roughly 18 hours and stops advancing entirely after ~6 days.
    time: f64,

    /// Shared blit pipeline objects (pipeline, bind group layout, sampler)
    shared_blit: Arc<SharedEffectsBlitPipeline>,

    /// Current bind group (recreated when texture changes)
    blit_bind_group: Option<wgpu::BindGroup>,

    /// Whether the scene must be re-rendered on the next `render()` call.
    ///
    /// Set by configuration changes, effect list changes, target recreation
    /// and by `update()` whenever any enabled effect is animated. Cleared
    /// after a successful vello render.
    needs_render: bool,
}

impl EffectsRenderer {
    /// Create a new effects renderer with shared pipeline objects
    pub fn new_with_shared(
        vello_renderer: Arc<Mutex<Option<Renderer>>>,
        shared_blit: &Arc<SharedEffectsBlitPipeline>,
    ) -> Self {
        Self {
            effects: Vec::new(),
            vello_renderer,
            scene: Scene::new(),
            target_texture: None,
            target_view: None,
            target_size: (0, 0),
            time: 0.0,
            shared_blit: shared_blit.clone(),
            blit_bind_group: None,
            needs_render: true,
        }
    }

    /// Create a new effects renderer with its own pipeline objects
    pub fn new(
        device: &wgpu::Device,
        vello_renderer: Arc<Mutex<Option<Renderer>>>,
        format: wgpu::TextureFormat,
    ) -> Self {
        let shared_blit = Arc::new(SharedEffectsBlitPipeline::new(device, format));
        Self::new_with_shared(vello_renderer, &shared_blit)
    }

    /// Add an effect to the renderer
    pub fn add_effect(&mut self, effect: Box<dyn BackdropEffect>) {
        self.effects.push(effect);
        self.needs_render = true;
    }

    /// Remove all effects
    pub fn clear_effects(&mut self) {
        self.effects.clear();
        self.needs_render = true;
    }

    /// Get a mutable reference to effects for configuration
    ///
    /// Any mutation through this reference is assumed to change the output,
    /// so the next `render()` re-renders the scene.
    pub fn effects_mut(&mut self) -> &mut Vec<Box<dyn BackdropEffect>> {
        self.needs_render = true;
        &mut self.effects
    }

    /// Configure all effects from theme config
    ///
    /// Each effect receives properties prefixed with its type.
    /// E.g., GridEffect receives properties like "grid-enabled", "grid-color".
    pub fn configure(&mut self, config: &EffectConfig) {
        for effect in &mut self.effects {
            // Extract properties for this effect type
            let prefix = format!("{}-", effect.effect_type());
            let mut effect_config = EffectConfig::new();

            for (key, value) in &config.properties {
                if let Some(suffix) = key.strip_prefix(&prefix) {
                    effect_config.insert(suffix.to_string(), value.clone());
                }
            }

            effect.configure(&effect_config);
            log::info!(
                "Configured effect '{}': enabled={}",
                effect.effect_type(),
                effect.is_enabled()
            );
        }

        self.needs_render = true;
    }

    /// Update all effects' animation state
    ///
    /// # Arguments
    /// * `dt` - Delta time since last frame in seconds
    pub fn update(&mut self, dt: f32) {
        let dt = dt as f64;
        self.time += dt;

        for effect in &mut self.effects {
            if effect.is_enabled() {
                effect.update(dt, self.time);
            }
        }

        if any_animated(&self.effects) {
            self.needs_render = true;
        }
    }

    /// Check if any effects are enabled
    pub fn has_enabled_effects(&self) -> bool {
        self.effects.iter().any(|e| e.is_enabled())
    }

    /// Check if any enabled effect changes over time
    pub fn has_animated_effects(&self) -> bool {
        any_animated(&self.effects)
    }

    /// Whether the backdrop needs continuous redraws.
    ///
    /// `true` when at least one enabled effect is animated, or when a
    /// pending change (configure/patch/resize) still has to be rendered.
    /// The event loop can sleep instead of scheduling frames when this is
    /// `false`.
    ///
    /// Only enabled effects count: `needs_render` is cleared by `render()`,
    /// which never runs while no effect is enabled, so without this guard a
    /// theme with no backdrop effects would keep the event loop redrawing
    /// forever.
    pub fn is_animating(&self) -> bool {
        self.has_enabled_effects() && (self.needs_render || any_animated(&self.effects))
    }

    /// Whether the next `render()` call will re-render the vello scene
    pub fn needs_render(&self) -> bool {
        self.needs_render
    }

    /// Force the next `render()` call to re-render the vello scene
    pub fn mark_dirty(&mut self) {
        self.needs_render = true;
    }

    /// Apply a temporary patch configuration to a specific effect type
    ///
    /// This allows overriding specific effect properties without reconfiguring
    /// the entire effects system.
    pub fn apply_effect_patch(&mut self, effect_type: &str, config: &EffectConfig) {
        for effect in &mut self.effects {
            if effect.effect_type() == effect_type {
                effect.configure(config);
                self.needs_render = true;
                log::debug!(
                    "Applied patch to effect '{}' with {} properties",
                    effect_type,
                    config.properties.len()
                );
                break;
            }
        }
    }

    /// Ensure render target is sized correctly
    fn ensure_target(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if self.target_size != (width, height) || self.target_texture.is_none() {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Effects Render Target"),
                size: wgpu::Extent3d {
                    width: width.max(1),
                    height: height.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::STORAGE_BINDING,
                view_formats: &[],
            });

            let view = texture.create_view(&Default::default());

            // Create bind group for blitting this texture
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Effects Blit Bind Group"),
                layout: &self.shared_blit.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.shared_blit.sampler),
                    },
                ],
            });

            // Release the old texture explicitly rather than waiting for GC
            if let Some(old) = self.target_texture.take() {
                old.destroy();
            }

            self.target_texture = Some(texture);
            self.target_view = Some(view);
            self.blit_bind_group = Some(bind_group);
            self.target_size = (width, height);
            // Fresh target has no content yet
            self.needs_render = true;
        }
    }

    /// Composite the effects texture onto the frame
    ///
    /// Call this after render() to draw the effects onto the frame.
    /// The render pass should already be started with the frame as target.
    pub fn composite<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        if let Some(bind_group) = &self.blit_bind_group {
            pass.set_pipeline(&self.shared_blit.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
    }

    /// Render all enabled effects to texture
    ///
    /// When nothing changed since the last successful render (no
    /// configuration change, no resize and no animated effect), the vello
    /// pass is skipped and the cached target is returned as-is.
    ///
    /// Returns the texture view for compositing, or None if no effects are enabled
    /// or the size is invalid.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        size: (u32, u32),
    ) -> Option<&wgpu::TextureView> {
        let (width, height) = size;

        // Skip if no effects enabled or invalid size
        if !self.has_enabled_effects() || width == 0 || height == 0 {
            return None;
        }

        // Log once when we first render
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("Effects render starting: {}x{}", width, height);
        }

        // Ensure target is sized (marks needs_render when recreated)
        self.ensure_target(device, width, height);

        // Nothing changed: reuse the cached target
        if !self.needs_render {
            return self.target_view.as_ref();
        }

        // Reset scene for new frame
        self.scene.reset();

        // Build scene from all enabled effects
        let bounds = Rect::new(0.0, 0.0, width as f64, height as f64);

        for effect in &self.effects {
            if effect.is_enabled() {
                effect.render(&mut self.scene, bounds);
            }
        }

        // Render scene to texture via shared Vello renderer
        let target_view = self.target_view.as_ref()?;

        let mut renderer_guard = self.vello_renderer.lock().ok()?;
        let renderer = renderer_guard.as_mut()?;

        let params = RenderParams {
            base_color: peniko::Color::TRANSPARENT,
            width,
            height,
            antialiasing_method: AaConfig::Area,
        };

        if let Err(e) = renderer.render_to_texture(device, queue, &self.scene, target_view, &params)
        {
            log::error!("Failed to render effects: {:?}", e);
            return None;
        }

        // Only clear the flag after a successful render so a failed frame is retried
        self.needs_render = false;

        // Log success once
        static LOGGED2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED2.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("Effects rendered to texture successfully");
        }

        self.target_view.as_ref()
    }

    /// Get the current render target texture view
    pub fn texture_view(&self) -> Option<&wgpu::TextureView> {
        self.target_view.as_ref()
    }

    /// Get current target size
    pub fn target_size(&self) -> (u32, u32) {
        self.target_size
    }

    /// Get total elapsed time in seconds
    pub fn elapsed_time(&self) -> f64 {
        self.time
    }

    /// Reset elapsed time
    pub fn reset_time(&mut self) {
        self.time = 0.0;
        self.needs_render = true;
    }
}

/// True when at least one enabled effect reports itself as animated
fn any_animated(effects: &[Box<dyn BackdropEffect>]) -> bool {
    effects.iter().any(|e| e.is_enabled() && e.is_animated())
}

impl Drop for EffectsRenderer {
    fn drop(&mut self) {
        // Destroy render target texture to release GPU memory
        if let Some(ref texture) = self.target_texture {
            texture.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal effect for exercising the dirty-tracking logic without a GPU
    struct StubEffect {
        enabled: bool,
        animated: bool,
    }

    impl BackdropEffect for StubEffect {
        fn effect_type(&self) -> &'static str {
            "stub"
        }
        fn update(&mut self, _dt: f64, _time: f64) {}
        fn render(&self, _scene: &mut Scene, _bounds: Rect) {}
        fn configure(&mut self, _config: &EffectConfig) {}
        fn is_enabled(&self) -> bool {
            self.enabled
        }
        fn is_animated(&self) -> bool {
            self.animated
        }
    }

    fn stub(enabled: bool, animated: bool) -> Box<dyn BackdropEffect> {
        Box::new(StubEffect { enabled, animated })
    }

    #[test]
    fn any_animated_requires_enabled_and_animated() {
        assert!(!any_animated(&[]));
        assert!(!any_animated(&[stub(true, false)]));
        assert!(!any_animated(&[stub(false, true)]));
        assert!(any_animated(&[stub(true, true)]));
        assert!(any_animated(&[stub(true, false), stub(true, true)]));
    }

    #[test]
    fn default_is_animated_is_true() {
        struct DefaultStub;
        impl BackdropEffect for DefaultStub {
            fn effect_type(&self) -> &'static str {
                "d"
            }
            fn update(&mut self, _dt: f64, _time: f64) {}
            fn render(&self, _scene: &mut Scene, _bounds: Rect) {}
            fn configure(&mut self, _config: &EffectConfig) {}
            fn is_enabled(&self) -> bool {
                true
            }
        }
        assert!(DefaultStub.is_animated());
    }
}
