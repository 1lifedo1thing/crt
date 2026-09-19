//! GPU state management
//!
//! Shared and per-window GPU resources for wgpu rendering.
//!
//! ## Module Structure
//! - `texture_pool` - Render target texture pooling
//!
//! Vertex data uses `crt_renderer::FrameArena` (per-frame bump allocation for
//! transient passes) and renderer-owned buffers for persistent content.

mod texture_pool;

#[allow(unused_imports)]
pub use texture_pool::TexturePoolStats;
pub use texture_pool::{PooledTexture, TexturePool};

use std::sync::{Arc, Mutex};

use crt_renderer::{
    BackgroundImagePipeline, BackgroundImageState, CrtPipeline, EffectPipeline, EffectsRenderer,
    FrameArena, GlyphCache, GridRenderer, RectRenderer, SharedPipelines, SpriteAnimationState,
    TabBar, TerminalVelloRenderer,
};

/// Shared GPU resources across all windows
pub struct SharedGpuState {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: Arc<wgpu::Device>,
    pub queue: wgpu::Queue,
    /// Shared Vello renderer - lazy loaded only when CSS effects need it
    /// (rounded corners, gradients, shadows, backdrop effects, etc.)
    /// Wrapped in Arc<Mutex> for sharing with EffectsRenderer
    pub vello_renderer: Arc<Mutex<Option<vello::Renderer>>>,
    /// Texture pool for reusing render target textures (fully integrated)
    pub texture_pool: TexturePool,
    /// Shared render pipelines across all windows (created lazily on first window)
    pub shared_pipelines: Option<SharedPipelines>,
}

impl SharedGpuState {
    /// Initialize shared GPU resources
    pub fn new() -> Self {
        log::debug!("Initializing shared GPU state");
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());

        // Request adapter without a surface first (we'll create surfaces per-window)
        let adapter = pollster::block_on(async {
            instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::default(),
                    compatible_surface: None,
                    force_fallback_adapter: false,
                })
                .await
                .expect("Failed to find suitable GPU adapter")
        });

        log::debug!(
            "GPU adapter: {:?} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );

        let (device, queue) = pollster::block_on(async {
            adapter
                .request_device(&wgpu::DeviceDescriptor::default())
                .await
                .expect("Failed to create device")
        });

        log::debug!("GPU device created successfully");

        // Wrap device in Arc for sharing with buffer pool
        let device = Arc::new(device);

        // Vello renderer is lazy-loaded only when CSS effects need it
        // (rounded corners, gradients, shadows, backdrop effects, complex paths)
        let vello_renderer = Arc::new(Mutex::new(None));

        // Create texture pool for reusing render target textures
        // Max 2 textures per bucket since textures are large (~8MB+ each)
        let texture_pool = TexturePool::new(device.clone(), 2);

        Self {
            instance,
            adapter,
            device,
            queue,
            vello_renderer,
            texture_pool,
            shared_pipelines: None,
        }
    }

    /// Ensure shared render pipelines are initialized for the given surface format.
    ///
    /// Called on first window creation when the surface format is known.
    /// Subsequent calls are no-ops.
    pub fn ensure_shared_pipelines(&mut self, target_format: wgpu::TextureFormat) {
        if self.shared_pipelines.is_none() {
            log::info!(
                "Creating shared render pipelines for format {:?}",
                target_format
            );
            self.shared_pipelines = Some(SharedPipelines::new(&self.device, target_format));
        }
    }

    /// Ensure the Vello renderer is initialized (lazy initialization)
    ///
    /// Call this when you need advanced CSS effects like rounded corners,
    /// gradients, backdrop effects, or complex paths. The renderer is
    /// cached after first creation.
    pub fn ensure_vello_renderer(&self) {
        let mut guard = match self.vello_renderer.lock() {
            Ok(g) => g,
            Err(e) => {
                log::error!(
                    "Vello renderer lock poisoned, skipping initialization: {}",
                    e
                );
                return;
            }
        };
        if guard.is_none() {
            log::info!("Lazy-loading Vello renderer for advanced CSS/backdrop effects");
            *guard = Some(
                vello::Renderer::new(
                    &self.device,
                    vello::RendererOptions {
                        pipeline_cache: None,
                        ..Default::default()
                    },
                )
                .expect("Failed to create Vello renderer"),
            );
        }
    }

    /// Get a clone of the shared Vello renderer Arc for passing to EffectsRenderer
    pub fn vello_renderer_arc(&self) -> Arc<Mutex<Option<vello::Renderer>>> {
        self.vello_renderer.clone()
    }
}

/// Per-window GPU state (surface tied to specific window)
pub struct WindowGpuState {
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,

    // Text rendering with swash glyph cache (scales with zoom)
    pub glyph_cache: GlyphCache,
    // Grid renderer for cursor line (rendered with glow effect)
    pub grid_renderer: GridRenderer,
    // Grid renderer for output text (rendered flat, no glow)
    pub output_grid_renderer: GridRenderer,

    // Fixed-size glyph cache for tab titles (doesn't scale with zoom)
    pub tab_glyph_cache: GlyphCache,
    // Renderer for tab titles (owned buffer; its instance list persists
    // across frames and is rebuilt only when the tab bar's titles_version
    // changes). Nothing else may clear or push into it.
    pub tab_title_renderer: GridRenderer,
    /// `TabBar::titles_version()` the title glyphs were last built for
    pub tab_titles_version: Option<u64>,
    /// Instance count right after the last title build; a mismatch at draw
    /// time means another pass has used `tab_title_renderer` as scratch space.
    pub tab_titles_instances: usize,
    // Renderer for transient UI text (search/rename bars, context menu,
    // zoom/copy indicators, toasts). Shares the tab glyph cache; every pass
    // that uses it clears it, fills it and draws from the frame arena.
    pub overlay_text_renderer: GridRenderer,

    // Per-frame bump allocator for transient vertex data (overlays, dialogs, tab bar)
    pub arena: FrameArena,

    // Effect pipeline
    pub effect_pipeline: EffectPipeline,

    // Backdrop effects renderer (grid, starfield, particles, etc.)
    pub effects_renderer: EffectsRenderer,

    // Tab bar
    pub tab_bar: TabBar,

    // Terminal vello renderer for cursor and selection
    pub terminal_vello: TerminalVelloRenderer,

    // Rect renderer for cell backgrounds (owned buffer, re-uploaded on content change)
    pub cell_bg_renderer: RectRenderer,

    // Rect renderer for tab bar shapes and transient UI (arena-backed)
    pub rect_renderer: RectRenderer,

    // Separate rect renderer for overlays (cursor, selection, underlines)
    // to avoid buffer conflicts with tab bar rendering
    pub overlay_rect_renderer: RectRenderer,

    // Background image rendering (optional)
    pub background_image_pipeline: BackgroundImagePipeline,
    pub background_image_state: Option<BackgroundImageState>,
    pub background_image_bind_group: Option<wgpu::BindGroup>,

    // Sprite animation rendering (optional, bypasses vello for memory efficiency)
    pub sprite_state: Option<SpriteAnimationState>,

    // Intermediate text texture for glow effect (pooled for memory reuse)
    // Text is rendered here first, then composited with Gaussian blur
    pub text_texture: PooledTexture,
    pub composite_bind_group: wgpu::BindGroup,

    // CRT post-processing (optional - scanlines, curvature, vignette)
    pub crt_pipeline: CrtPipeline,
    // Intermediate texture for CRT post-processing (pooled for memory reuse)
    // When CRT is enabled, everything renders here first, then CRT effect outputs to surface
    pub crt_texture: Option<PooledTexture>,
    pub crt_bind_group: Option<wgpu::BindGroup>,
}

impl WindowGpuState {
    /// Explicitly release GPU resources before dropping.
    ///
    /// Call this before removing WindowState to ensure proper cleanup.
    /// This unconfigures the surface which releases swap chain buffers (IOSurface on macOS).
    /// Note: PooledTextures (text_texture, crt_texture) are automatically returned
    /// to the pool when dropped, so we don't explicitly destroy them here.
    pub fn cleanup(&mut self, device: &wgpu::Device) {
        log::debug!("WindowGpuState cleanup - releasing GPU resources");

        // Note: text_texture and crt_texture are PooledTextures that will be
        // returned to the pool automatically when dropped. We don't destroy them
        // manually - the pool handles cleanup and potential reuse.

        // Unconfigure surface to release swap chain buffers (IOSurface on macOS)
        // This signals to the Metal driver that we're done with this surface
        self.surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: self.config.format,
                width: 1,
                height: 1,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: self.config.alpha_mode,
                view_formats: vec![],
                desired_maximum_frame_latency: 1,
            },
        );

        // Poll to process the unconfigure
        let _ = device.poll(wgpu::PollType::Wait);
    }
}

impl Drop for WindowGpuState {
    fn drop(&mut self) {
        log::debug!("Dropping WindowGpuState");
        // Note: Most cleanup happens in cleanup() which should be called first.
        // Surface is cleaned up automatically on drop in wgpu 26+
        // Other resources (buffers, bind groups, pipelines) in sub-components
        // are cleaned up by their own Drop implementations
    }
}
