//! Application state and lifecycle management.
//!
//! Contains the `App` struct and core methods for managing windows,
//! GPU state, config, and theme resources.

mod effects;
mod handler;
mod initialization;
#[cfg(target_os = "macos")]
mod menu_actions;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{Config, ConfigPaths};
use crate::gpu::SharedGpuState;
use crate::input::drag::TabDragState;
use crate::render::{compute_shell_event_overrides, process_pty_updates};
use crate::theme_registry::ThemeRegistry;
use crate::watcher;
use crate::window::{OverrideEventType, WindowState};
use crt_core::{SpawnOptions, WakeFn};
use crt_renderer::{
    BackgroundImageState, SpriteAnimationState, SpriteConfig, SpriteMotion, SpritePosition,
};
use crt_theme::Theme;
use winit::event_loop::EventLoopProxy;
use winit::window::WindowId;

/// Why a background thread woke the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WakeReason {
    /// A PTY reader queued output
    Pty,
    /// The config watcher saw a file change
    Watcher,
}

/// Wakes the winit loop from other threads, coalescing bursts.
///
/// A flag per reason means a thousand small PTY reads produce one queued
/// user event rather than a thousand; the flag is cleared when the event is
/// handled, so the next read wakes us again.
pub(crate) struct Waker {
    proxy: EventLoopProxy<WakeReason>,
    pty_pending: AtomicBool,
    watcher_pending: AtomicBool,
}

impl Waker {
    fn new(proxy: EventLoopProxy<WakeReason>) -> Self {
        Self {
            proxy,
            pty_pending: AtomicBool::new(false),
            watcher_pending: AtomicBool::new(false),
        }
    }

    fn flag(&self, reason: WakeReason) -> &AtomicBool {
        match reason {
            WakeReason::Pty => &self.pty_pending,
            WakeReason::Watcher => &self.watcher_pending,
        }
    }

    /// Send a wake if one for this reason is not already queued.
    pub(crate) fn wake(&self, reason: WakeReason) {
        if !self.flag(reason).swap(true, Ordering::AcqRel) {
            let _ = self.proxy.send_event(reason);
        }
    }

    /// Called when the user event is handled; lets the next wake through.
    pub(crate) fn acknowledge(&self, reason: WakeReason) {
        self.flag(reason).store(false, Ordering::Release);
    }

    /// A `WakeFn` for threads that cannot see winit types.
    pub(crate) fn wake_fn(self: &Arc<Self>, reason: WakeReason) -> WakeFn {
        let waker = self.clone();
        Arc::new(move || waker.wake(reason))
    }
}

#[cfg(target_os = "macos")]
use muda::Menu;

#[cfg(target_os = "macos")]
use crate::menu::MenuIds;

// Font scale bounds
const MIN_FONT_SCALE: f32 = 0.5;
const MAX_FONT_SCALE: f32 = 3.0;
const FONT_SCALE_STEP: f32 = 0.1;

pub(crate) struct App {
    pub(crate) windows: HashMap<WindowId, WindowState>,
    pub(crate) shared_gpu: Option<SharedGpuState>,
    pub(crate) focused_window: Option<WindowId>,
    pub(crate) config: Config,
    /// Current theme (stored for event override access)
    pub(crate) theme: Arc<Theme>,
    /// Registry of available themes for runtime switching
    pub(crate) theme_registry: ThemeRegistry,
    pub(crate) modifiers: winit::event::Modifiers,
    pub(crate) pending_new_window: bool,
    pub(crate) config_watcher: Option<watcher::ConfigWatcher>,
    /// Wakes the event loop from PTY reader threads and the watcher
    pub(crate) waker: Arc<Waker>,
    /// Global tab ID counter — ensures IDs are unique across all windows
    pub(crate) next_tab_id: u64,
    /// Active tab drag state (lives on App for cross-window visibility)
    pub(crate) drag_state: Option<TabDragState>,
    /// Pending detach operation (deferred to about_to_wait for borrow safety)
    pub(crate) pending_detach: Option<crate::app::initialization::DetachPayload>,
    /// Pending merge operation (deferred to about_to_wait for borrow safety)
    pub(crate) pending_merge: Option<crate::app::initialization::MergePayload>,
    /// Window to close after tab extraction leaves it empty
    pub(crate) pending_close_empty: Option<WindowId>,
    /// Floating overlay window shown during tab drag (follows cursor across screen)
    pub(crate) drag_overlay: Option<std::sync::Arc<winit::window::Window>>,
    #[cfg(target_os = "macos")]
    pub(crate) menu: Option<Menu>,
    #[cfg(target_os = "macos")]
    pub(crate) menu_ids: Option<MenuIds>,
}

impl App {
    pub(crate) fn new(proxy: EventLoopProxy<WakeReason>) -> Self {
        let config = Config::load();
        let waker = Arc::new(Waker::new(proxy));
        let config_watcher = watcher::ConfigWatcher::new(Some(waker.wake_fn(WakeReason::Watcher)));

        // Initialize theme registry from themes directory
        let theme_registry = ConfigPaths::from_env_or_default()
            .map(|paths| ThemeRegistry::new(paths.themes_dir(), config.theme.name.clone()))
            .unwrap_or_else(|| {
                log::warn!("Could not determine config paths, using empty theme registry");
                ThemeRegistry::new(std::path::PathBuf::new(), config.theme.name.clone())
            });

        Self {
            windows: HashMap::new(),
            shared_gpu: None,
            focused_window: None,
            config,
            theme: Arc::new(Theme::default()), // Will be loaded properly in resumed()
            theme_registry,
            modifiers: winit::event::Modifiers::default(),
            pending_new_window: false,
            config_watcher,
            waker,
            next_tab_id: 0,
            drag_state: None,
            pending_detach: None,
            pending_merge: None,
            pending_close_empty: None,
            drag_overlay: None,
            #[cfg(target_os = "macos")]
            menu: None,
            #[cfg(target_os = "macos")]
            menu_ids: None,
        }
    }

    /// Allocate the next globally unique tab ID.
    pub(crate) fn next_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// Spawn options for a new shell in `cwd`, wired to wake the event loop.
    pub(crate) fn spawn_options(&self, cwd: Option<std::path::PathBuf>) -> SpawnOptions {
        SpawnOptions {
            shell: self.config.shell.program.clone(),
            cwd,
            semantic_prompts: self.config.shell.semantic_prompts,
            shell_assets_dir: Config::shell_assets_dir(),
            wake: Some(self.waker.wake_fn(WakeReason::Pty)),
        }
    }

    /// Drain PTY output for every tab of every window.
    ///
    /// Background tabs are parsed too, so their shells never block on a
    /// full output queue and their titles/bells are not lost. Only the
    /// active tab's changes mark the window's text layer dirty.
    pub(crate) fn drain_ptys(&mut self) {
        for state in self.windows.values_mut() {
            let active = state.gpu.tab_bar.active_tab_id();
            for (tab_id, shell) in state.shells.iter_mut() {
                let result = process_pty_updates(shell);
                if result.content_changed && Some(*tab_id) == active {
                    state.render.dirty = true;
                    state.text_rebuild.insert(*tab_id);
                }
                if let Some(title) = result.title_change {
                    state.gpu.tab_bar.set_tab_title(*tab_id, title);
                }
                if result.shell_events.is_empty() {
                    continue;
                }
                // A completed command may have changed the shell's directory
                shell.invalidate_cwd();
                let theme = state.gpu.effect_pipeline.theme();
                let overrides = compute_shell_event_overrides(&result.shell_events, theme);
                if overrides.bell_triggered {
                    state.ui.bell.trigger();
                }
                if overrides.clear_command_fail {
                    state
                        .ui
                        .overrides
                        .clear_event(OverrideEventType::CommandFail);
                }
                for (event_type, properties) in overrides.activations {
                    state.ui.overrides.add(event_type, properties);
                }
            }
        }
    }

    /// Create a small floating overlay window for drag feedback.
    ///
    /// The overlay follows the cursor during tab drag to provide visual feedback
    /// that escapes window boundaries.
    pub(crate) fn create_drag_overlay(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        title: &str,
        screen_x: i32,
        screen_y: i32,
    ) {
        use winit::window::Window;

        let attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(150u32, 28u32))
            .with_position(winit::dpi::PhysicalPosition::new(
                screen_x - 75,
                screen_y + 15,
            ))
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(winit::window::WindowLevel::AlwaysOnTop);

        // Prevent macOS from grouping this with terminal windows
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs = attrs.with_tabbing_identifier("crt-drag-overlay");
        }

        match event_loop.create_window(attrs) {
            Ok(window) => {
                // Set opacity for translucent effect
                window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
                self.drag_overlay = Some(std::sync::Arc::new(window));
                log::debug!("Created drag overlay window");
            }
            Err(e) => {
                log::warn!("Failed to create drag overlay: {}", e);
            }
        }
    }

    pub(crate) fn init_shared_gpu(&mut self) {
        if self.shared_gpu.is_none() {
            self.shared_gpu = Some(SharedGpuState::new());
        }
    }

    pub(crate) fn focused_window_mut(&mut self) -> Option<&mut WindowState> {
        self.focused_window.and_then(|id| self.windows.get_mut(&id))
    }

    /// Update CRT pipeline and textures for new theme
    pub(crate) fn update_crt_pipeline(
        state: &mut WindowState,
        shared: &SharedGpuState,
        theme: &Theme,
    ) {
        // Update CRT effect settings
        state.gpu.crt_pipeline.set_effect(theme.crt);

        // Create or destroy CRT texture based on whether effect is enabled
        if state.gpu.crt_pipeline.is_enabled() {
            if state.gpu.crt_texture.is_none() {
                log::info!("CRT effect enabled - creating intermediate texture");
                let width = state.gpu.config.width;
                let height = state.gpu.config.height;
                let format = state.gpu.config.format;
                let texture = shared
                    .texture_pool
                    .checkout(width, height, format)
                    .expect("Texture pool checkout failed");
                let bind_group = state
                    .gpu
                    .crt_pipeline
                    .create_bind_group(&shared.device, texture.view());
                state.gpu.crt_texture = Some(texture);
                state.gpu.crt_bind_group = Some(bind_group);
            }
        } else {
            // Disable CRT - release texture back to pool (dropped automatically)
            if state.gpu.crt_texture.take().is_some() {
                log::info!("CRT effect disabled - releasing texture");
            }
            state.gpu.crt_bind_group = None;
        }
    }

    /// Update background image state for new theme
    pub(crate) fn update_background_image(
        state: &mut WindowState,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        theme: &Theme,
    ) {
        // Clear existing background image
        state.gpu.background_image_state = None;
        state.gpu.background_image_bind_group = None;

        // Create new background image if theme has one
        if let Some(ref bg_image) = theme.background_image {
            match BackgroundImageState::new(device, queue, bg_image) {
                Ok(bg_state) => {
                    let bind_group = state.gpu.background_image_pipeline.create_bind_group(
                        device,
                        &bg_state.texture.view,
                        bg_state.texture.sampler(),
                    );
                    log::info!("Loaded background image: {:?}", bg_image.path);
                    state.gpu.background_image_state = Some(bg_state);
                    state.gpu.background_image_bind_group = Some(bind_group);
                }
                Err(e) => {
                    log::warn!("Failed to load background image: {}", e);
                }
            }
        }
    }

    /// Create sprite animation state from theme configuration
    pub(crate) fn create_sprite_state(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        theme: &Theme,
        format: wgpu::TextureFormat,
    ) -> Option<SpriteAnimationState> {
        let sprite = theme.sprite.as_ref()?;
        if !sprite.enabled {
            return None;
        }
        let path_str = sprite.path.as_ref()?;

        // Resolve path relative to theme base directory
        let path = std::path::PathBuf::from(path_str);
        let resolved_path = if let Some(ref base_dir) = sprite.base_dir {
            if path.is_relative() {
                base_dir.join(&path)
            } else {
                path.clone()
            }
        } else {
            path.clone()
        };

        log::info!("Creating sprite state from: {:?}", resolved_path);
        let base_dir = sprite.base_dir.clone().unwrap_or_else(|| {
            resolved_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
        });

        let config = SpriteConfig {
            path: resolved_path,
            frame_width: sprite.frame_width,
            frame_height: sprite.frame_height,
            columns: sprite.columns,
            rows: sprite.rows,
            frame_count: sprite.frame_count,
            fps: sprite.fps,
            scale: sprite.scale,
            opacity: sprite.opacity,
            position: SpritePosition::from_str(sprite.position.as_str()),
            motion: SpriteMotion::from_str(sprite.motion.as_str()),
            motion_speed: sprite.motion_speed,
            base_dir,
        };

        match SpriteAnimationState::new(device, queue, config, format) {
            Ok(state) => {
                log::info!("Loaded sprite animation");
                Some(state)
            }
            Err(e) => {
                log::warn!("Failed to load sprite animation: {}", e);
                None
            }
        }
    }

    pub(crate) fn close_window(&mut self, window_id: WindowId) {
        if let Some(ref shared) = self.shared_gpu {
            // Poll GPU to complete any pending work for this window before cleanup
            let _ = shared.device.poll(wgpu::PollType::Wait);

            // Explicitly cleanup GPU resources before dropping
            // This unconfigures the surface to release IOSurface buffers
            if let Some(state) = self.windows.get_mut(&window_id) {
                state.gpu.cleanup(&shared.device);
            }
        }

        // Now remove and drop the window state (triggers Drop impls)
        if self.windows.remove(&window_id).is_some() {
            log::info!(
                "Closed window {:?}, remaining: {}",
                window_id,
                self.windows.len()
            );

            // Poll again after Drop to ensure destroyed resources are freed
            if let Some(ref shared) = self.shared_gpu {
                let _ = shared.device.poll(wgpu::PollType::Wait);

                // Shrink texture pool to release excess pooled textures
                shared.texture_pool.shrink();
            }

            if self.focused_window == Some(window_id) {
                self.focused_window = self.windows.keys().next().copied();
            }
        }
    }

    /// Reload config from disk and apply changes
    pub(crate) fn reload_config(&mut self) {
        log::info!("Reloading config...");
        log::debug!(
            "Current theme: {}, font: {:?} @ {}pt",
            self.config.theme.name,
            self.config.font.family,
            self.config.font.size
        );
        let (new_config, config_error) = Config::load_with_error();

        // Show toast if there was a config error
        if let Some(error) = config_error
            && let Some(state) = self.focused_window_mut()
        {
            state.ui.toast.show(error, crate::window::ToastType::Error);
        }

        // Check if theme changed
        let theme_changed = new_config.theme.name != self.config.theme.name;
        log::debug!(
            "New theme: {}, theme_changed: {}",
            new_config.theme.name,
            theme_changed
        );

        self.config = new_config;

        // Reload theme if it changed
        if theme_changed {
            self.reload_theme();
        }

        // Apply other config changes to all windows
        log::debug!("Applying config to {} windows", self.windows.len());
        for state in self.windows.values_mut() {
            // Force redraw
            state.render.dirty = true;
            state.request_text_rebuild_all();
        }
    }

    /// Reload themes from disk and apply to all windows
    pub(crate) fn reload_theme(&mut self) {
        log::info!("Reloading themes from disk...");
        self.theme_registry.reload_all();
        self.reapply_registry_themes(None);
    }

    /// Reload the one theme file that changed and re-apply it to the
    /// windows using it (other themes are left untouched).
    pub(crate) fn reload_theme_file(&mut self, path: &std::path::Path) {
        let Some(name) = self.theme_registry.reload_path(path) else {
            return;
        };
        log::info!("Theme '{}' changed on disk", name);
        self.reapply_registry_themes(Some(&name));
    }

    /// Push the registry's current theme objects to windows. With `only`,
    /// just the windows using that theme are updated.
    fn reapply_registry_themes(&mut self, only: Option<&str>) {
        // Collect window updates to avoid borrow issues
        let window_themes: Vec<_> = self
            .windows
            .iter()
            .filter(|(_, state)| only.is_none_or(|n| state.theme_name == n))
            .map(|(id, state)| (*id, state.theme_name.clone()))
            .collect();

        for (window_id, theme_name) in window_themes {
            let theme = self
                .theme_registry
                .get_theme(&theme_name)
                .cloned()
                .unwrap_or_else(|| {
                    log::warn!(
                        "Theme '{}' not found after reload, using default",
                        theme_name
                    );
                    self.theme_registry.get_default_theme().1
                });

            if let Some(state) = self.windows.get_mut(&window_id) {
                apply_theme_to_window(state, self.shared_gpu.as_ref(), &theme_name, &theme);
                state.window.request_redraw();
                log::debug!("Theme '{}' reloaded for window {:?}", theme_name, window_id);
            }
        }

        // Update App.theme for backward compatibility (event overrides)
        let (_, default_theme) = self.theme_registry.get_default_theme();
        self.theme = default_theme;
    }

    /// Adjust the focused window's font scale by `delta`, reflowing the grid and
    /// showing the zoom indicator. Cross-platform (used by both the macOS menu
    /// and configurable keybindings).
    pub(crate) fn adjust_font_scale(&mut self, delta: f32) {
        use crt_core::Size;

        let base_font_size = self.config.font.size;
        let focused_id = match self.focused_window {
            Some(id) => id,
            None => return,
        };

        let shared = match self.shared_gpu.as_ref() {
            Some(s) => s,
            None => return,
        };

        let state = match self.windows.get_mut(&focused_id) {
            Some(s) => s,
            None => return,
        };

        let new_scale = (state.font_scale + delta).clamp(MIN_FONT_SCALE, MAX_FONT_SCALE);
        if (new_scale - state.font_scale).abs() > 0.001 {
            state.font_scale = new_scale;

            // Update glyph cache with new font size
            let new_font_size = base_font_size * new_scale * state.scale_factor;
            state
                .gpu
                .glyph_cache
                .set_font_size(&shared.queue, new_font_size);
            state.gpu.glyph_cache.precache_ascii();
            state.gpu.glyph_cache.flush(&shared.queue);

            // Update grid renderers with new glyph cache
            state
                .gpu
                .grid_renderer
                .set_glyph_cache(&shared.device, &state.gpu.glyph_cache);
            state
                .gpu
                .output_grid_renderer
                .set_glyph_cache(&shared.device, &state.gpu.glyph_cache);

            // Recalculate terminal grid size (like resize does)
            let cell_width = state.gpu.glyph_cache.cell_width();
            let line_height = state.gpu.glyph_cache.line_height();
            let tab_bar_height = state.gpu.tab_bar.height();

            let padding_physical = 20.0 * state.scale_factor;
            let tab_bar_physical = tab_bar_height * state.scale_factor;

            let content_width = (state.gpu.config.width as f32 - padding_physical).max(60.0);
            let content_height =
                (state.gpu.config.height as f32 - padding_physical - tab_bar_physical).max(40.0);

            let new_cols = ((content_width / cell_width) as usize).max(10);
            let new_rows = ((content_height / line_height) as usize).max(4);

            state.cols = new_cols;
            state.rows = new_rows;

            // Resize all shells to match new grid size
            for shell in state.shells.values_mut() {
                shell.resize(Size::new(new_cols, new_rows));
            }

            // Trigger zoom indicator
            state.ui.zoom_indicator.trigger(new_scale);

            // Force full redraw
            state.render.dirty = true;
            state.request_text_rebuild_all();
            state.window.request_redraw();
        }
    }

    /// Reset the focused window's font scale back to 100%.
    pub(crate) fn reset_font_scale(&mut self) {
        let Some(focused_id) = self.focused_window else {
            return;
        };
        if let Some(state) = self.windows.get(&focused_id) {
            let delta = 1.0 - state.font_scale;
            if delta.abs() > 0.001 {
                self.adjust_font_scale(delta);
            }
        }
    }

    /// Record a user's theme selection: update the in-memory config and persist
    /// it to `config.toml` so the choice survives a restart.
    ///
    /// Updating `config.theme.name` first means the config watcher sees the
    /// resulting file write as a no-op (the name already matches) rather than a
    /// theme change to re-apply.
    pub(crate) fn persist_theme_choice(&mut self, theme_name: &str) {
        self.config.theme.name = theme_name.to_string();
        Config::persist_theme(theme_name);
        #[cfg(target_os = "macos")]
        self.refresh_theme_checkmarks(theme_name);
    }

    /// Update the Theme menu's checkmarks to reflect the active theme.
    #[cfg(target_os = "macos")]
    pub(crate) fn refresh_theme_checkmarks(&self, current: &str) {
        if let Some(ids) = self.menu_ids.as_ref() {
            for (name, item) in &ids.theme_items {
                item.set_checked(name == current);
            }
        }
    }

    /// Open a new tab in the focused window, spawning a shell in the active
    /// tab's working directory and selecting the new tab. Shared by the
    /// keyboard shortcut, the macOS menu, and the tab bar "+" button.
    pub(crate) fn open_new_tab(&mut self) {
        let new_tab_id = self.next_tab_id();
        let cwd = self.focused_window_mut().and_then(|s| s.active_shell_cwd());
        let spawn_options = self.spawn_options(cwd);

        if let Some(state) = self.focused_window_mut() {
            let tab_num = state.gpu.tab_bar.tab_count() + 1;
            state
                .gpu
                .tab_bar
                .add_tab(new_tab_id, format!("Terminal {}", tab_num));
            state
                .gpu
                .tab_bar
                .select_tab_index(state.gpu.tab_bar.tab_count() - 1);
            state.create_shell_for_tab(new_tab_id, spawn_options);
            state.render.dirty = true;
            state.window.request_redraw();
        }
    }

    /// Open the user's config file in the system default editor, creating a
    /// starter file if none exists. Surfaces failures as a toast.
    pub(crate) fn open_config_file(&mut self) {
        let result = Config::ensure_config_file().and_then(|path| open::that(&path));
        if let Err(e) = result {
            log::warn!("Failed to open config file: {}", e);
            if let Some(state) = self.focused_window_mut() {
                state.ui.toast.show(
                    format!("Couldn't open config: {e}"),
                    crate::window::ToastType::Error,
                );
            }
        }
    }

    /// Toggle borderless fullscreen on the focused window.
    pub(crate) fn toggle_fullscreen_focused(&mut self) {
        let mut now_fullscreen = false;
        if let Some(state) = self.focused_window_mut() {
            let is_fullscreen = state.window.fullscreen().is_some();
            now_fullscreen = !is_fullscreen;
            state.window.set_fullscreen(if is_fullscreen {
                None
            } else {
                Some(winit::window::Fullscreen::Borderless(None))
            });
        }

        // Keep the menu item label in sync with the new state.
        #[cfg(not(target_os = "macos"))]
        let _ = now_fullscreen;
        #[cfg(target_os = "macos")]
        if let Some(ids) = self.menu_ids.as_ref() {
            ids.toggle_fullscreen_item.set_text(if now_fullscreen {
                "Exit Full Screen"
            } else {
                "Enter Full Screen"
            });
        }
    }
}

/// Apply a theme switch to a specific window state.
///
/// Shared helper that updates the window theme, effects, sprite, CRT pipeline,
/// background image, and context menu. Used by menu action, context menu,
/// and theme reload paths.
pub(crate) fn apply_theme_to_window(
    state: &mut WindowState,
    shared_gpu: Option<&SharedGpuState>,
    theme_name: &str,
    theme: &Arc<Theme>,
) {
    log::info!("Switching theme to: {}", theme_name);
    state.set_theme(theme_name, theme.clone());
    effects::configure_effects_from_theme(&mut state.gpu.effects_renderer, theme);
    if let Some(shared) = shared_gpu {
        state.gpu.sprite_state = App::create_sprite_state(
            &shared.device,
            &shared.queue,
            theme,
            state.gpu.config.format,
        );
        App::update_crt_pipeline(state, shared, theme);
        App::update_background_image(state, &shared.device, &shared.queue, theme);
    }
    state.ui.context_menu.current_theme = theme_name.to_string();
    state.request_text_rebuild_all();
}
