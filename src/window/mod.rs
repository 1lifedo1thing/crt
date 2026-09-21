//! Window state management
//!
//! Per-window state including shells, GPU resources, and interaction state.

mod interaction;
mod overrides;
mod render;
mod types;
mod ui;

// Re-export all public types for backward compatibility
pub use interaction::{ContextMenu, ContextMenuItem, InteractionState, SearchMatch};
#[cfg_attr(not(test), allow(unused_imports))]
pub use overrides::{ActiveOverride, OverrideEventType, OverrideState};
pub use render::{
    CursorInfo, DecorationKind, FrameDemand, RenderContext, RenderLayout, RenderState,
    TextBufferUpdateResult, frame_deadline, prepare_render_cells,
};
pub use types::{EffectId, TabId};
pub use ui::{BellState, ToastType, UiState};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crt_core::{ShellTerminal, Size, SpawnOptions};
use crt_renderer::GlyphStyle;
use crt_theme::Theme;
use winit::window::Window;

use crate::gpu::{SharedGpuState, WindowGpuState};
use crate::input::{detect_paths_in_line, detect_urls_in_line, merge_wrapped_urls};

/// Per-window state containing window handle, GPU state, shells, and interaction state
pub struct WindowState {
    pub window: Arc<Window>,
    pub gpu: WindowGpuState,
    // Map of tab_id -> shell (each window has its own tabs)
    pub shells: HashMap<TabId, ShellTerminal>,
    /// Tabs whose text layer must be rebuilt on their next frame regardless
    /// of terminal damage (tab switch, hover/search/theme changes).
    pub text_rebuild: HashSet<TabId>,
    // Window-specific sizing
    pub cols: usize,
    pub rows: usize,
    pub scale_factor: f32,
    // User font scale multiplier (1.0 = default)
    pub font_scale: f32,
    // Rendering state (dirty, frame_count, occluded, focused, cached)
    pub render: RenderState,
    // Interaction state (cursor, mouse, selection, URLs)
    pub interaction: InteractionState,
    // UI overlay state (search, bell, context menu)
    pub ui: UiState,
    // Custom window title (None = use default "CRT Terminal")
    pub custom_title: Option<String>,
    // Per-window theme (shared with the render pipelines)
    pub theme: Arc<Theme>,
    pub theme_name: String,
}

/// Minimum interval between frames of the focused window (~60 fps)
pub const FOCUSED_FRAME_INTERVAL: Duration = Duration::from_micros(16_666);
/// Minimum interval between frames of unfocused windows (10 fps)
pub const UNFOCUSED_FRAME_INTERVAL: Duration = Duration::from_millis(100);

impl WindowState {
    /// Whether something on screen changes continuously (needs a frame every
    /// interval regardless of terminal content).
    pub fn is_animating(&self) -> bool {
        let gpu = &self.gpu;
        gpu.effects_renderer.is_animating()
            || gpu.sprite_state.is_some()
            || gpu
                .background_image_state
                .as_ref()
                .is_some_and(|bg| bg.image.is_animated())
            || gpu.crt_pipeline.is_animated()
            || self.ui.bell.is_active()
            || self.ui.overrides.has_active()
            || self.ui.zoom_indicator.is_visible()
            || self.ui.copy_indicator.is_visible()
            || self.ui.toast.is_visible()
    }

    /// When this window next needs a frame, if ever.
    ///
    /// `None` means the loop may sleep until an event arrives. A deadline in
    /// the past means "redraw now" (subject to the per-focus frame cap).
    pub fn next_frame_deadline(&self, focused: bool) -> Option<Instant> {
        let vello = &self.gpu.terminal_vello;
        let cursor_blinks = focused
            && vello.blink_enabled()
            && self.render.cached.cursor.is_some_and(|c| c.visible);
        // The tab bar versions every mutation and each presented frame
        // records the version it drew, so a change made without marking the
        // window dirty (a background tab's OSC title, a rename confirmed by
        // clicking away) still gets its frame.
        let tab_bar_stale = self.gpu.tab_titles_version != Some(self.gpu.tab_bar.titles_version());
        frame_deadline(&FrameDemand {
            occluded: self.render.occluded,
            dirty: self.render.dirty || tab_bar_stale,
            animating: self.is_animating(),
            settling: self.render.settling,
            last_frame_at: self.render.last_frame_at,
            interval: if focused {
                FOCUSED_FRAME_INTERVAL
            } else {
                UNFOCUSED_FRAME_INTERVAL
            },
            next_blink_toggle: cursor_blinks.then(|| vello.next_blink_toggle()),
        })
    }

    /// Set the theme for this window, updating all GPU resources
    pub fn set_theme(&mut self, name: &str, theme: Arc<Theme>) {
        self.theme_name = name.to_string();
        self.theme = theme.clone();

        // Update effect pipeline with new theme
        self.gpu.effect_pipeline.set_theme(theme.clone());

        // Update tab bar theme
        self.gpu.tab_bar.set_theme(theme.tabs);

        // Update cursor colors
        self.gpu.terminal_vello.set_cursor_color([
            theme.cursor_color.r,
            theme.cursor_color.g,
            theme.cursor_color.b,
            theme.cursor_color.a,
        ]);
        self.gpu
            .terminal_vello
            .set_cursor_glow(theme.cursor_glow.map(|g| {
                (
                    [g.color.r, g.color.g, g.color.b, g.color.a],
                    g.radius,
                    g.intensity,
                )
            }));

        // Mark window as needing redraw
        self.render.dirty = true;
    }

    /// Force the active tab's text layer to be rebuilt on the next frame
    /// (hover underline, search highlight, theme change, ...).
    pub fn invalidate_text(&mut self) {
        if let Some(tab_id) = self.gpu.tab_bar.active_tab_id() {
            self.text_rebuild.insert(tab_id);
        }
        self.render.dirty = true;
    }

    /// Force every tab's text layer to be rebuilt (font, theme or DPI change).
    pub fn request_text_rebuild_all(&mut self) {
        self.text_rebuild.extend(self.shells.keys().copied());
        self.render.dirty = true;
    }

    /// Update text buffer for this window's active shell
    ///
    /// Invalidation comes from alacritty's damage tracking (cursor moves,
    /// selection, attribute changes, scrolling, resizes) plus an explicit
    /// "force" flag set by UI state changes. Returns cursor position and
    /// decorations if the text layer was rebuilt, `None` otherwise.
    pub fn update_text_buffer(
        &mut self,
        shared_gpu: &SharedGpuState,
    ) -> Option<TextBufferUpdateResult> {
        let tab_id = self.gpu.tab_bar.active_tab_id()?;

        let forced = self.text_rebuild.remove(&tab_id);
        let damage = self.shells.get_mut(&tab_id)?.terminal_mut().take_damage();
        if damage.is_none() && !forced {
            return None;
        }

        let shell = self.shells.get(&tab_id)?;
        let terminal = shell.terminal();

        // Get content offset (excluding tab bar)
        let (offset_x, offset_y) = self.gpu.tab_bar.content_offset();

        let content = terminal.renderable_content();
        self.gpu.grid_renderer.clear();
        self.gpu.output_grid_renderer.clear();

        let cell_width = self.gpu.glyph_cache.cell_width();
        let line_height = self.gpu.glyph_cache.line_height();
        let padding = 10.0 * self.scale_factor;

        // Get display offset to convert grid lines to viewport lines
        let display_offset = terminal.display_offset() as i32;

        // Cursor info
        let cursor = content.cursor;
        let cursor_point = cursor.point;
        // Check cursor visibility via TermMode::SHOW_CURSOR (CSI ?25h/l)
        let cursor_visible = terminal.cursor_mode_visible();

        // Compute cursor position (adjust for scroll offset)
        let cursor_viewport_line = cursor_point.line.0 + display_offset;
        let cursor_x = offset_x + padding + (cursor_point.column.0 as f32 * cell_width);
        let cursor_y = offset_y + padding + (cursor_viewport_line as f32 * line_height);

        // Single pass: collect cells AND build line text for URL detection.
        // Reuse cached collections to avoid per-update allocations
        // (line_texts keeps its keys so the String buffers are reused).
        for s in self.render.cached.line_texts.values_mut() {
            s.clear();
        }
        self.render.cached.collected_cells.clear();

        for cell in content.display_iter {
            let viewport_line = cell.point.line.0 + display_offset;
            self.render
                .cached
                .line_texts
                .entry(viewport_line)
                .or_default()
                .push(cell.c);

            self.render
                .cached
                .collected_cells
                .push(render::CollectedCell {
                    col: cell.point.column.0,
                    grid_line: cell.point.line.0,
                    c: cell.c,
                    flags: cell.flags,
                    fg: cell.fg,
                    bg: cell.bg,
                });
        }

        // Detect URLs before rendering so we can underline them with text color
        self.interaction.detected_urls.clear();
        for (viewport_line, line_text) in &self.render.cached.line_texts {
            let urls = detect_urls_in_line(line_text, *viewport_line as usize);
            self.interaction.detected_urls.extend(urls);
        }
        // Merge URLs that wrap across multiple lines
        merge_wrapped_urls(
            &mut self.interaction.detected_urls,
            &self.render.cached.line_texts,
            self.cols,
        );

        // Detect file paths the same way, then validate them against the
        // filesystem so only existing paths become clickable. Resolution uses
        // the shell's current working directory (cached in the shell) and
        // $HOME; existence checks are cached across frames (PathValidator).
        let path_cwd = self.active_shell_cwd();
        let path_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        self.interaction.detected_paths.clear();
        for (viewport_line, line_text) in &self.render.cached.line_texts {
            let paths = detect_paths_in_line(line_text, *viewport_line as usize);
            self.interaction.detected_paths.extend(paths);
        }
        let interaction = &mut self.interaction;
        interaction.path_validator.begin_pass(path_cwd, path_home);
        interaction
            .path_validator
            .validate_all(&mut interaction.detected_paths);
        interaction.detected_paths.retain(|p| p.exists);

        // Prepare render data using pure function (no GPU calls)
        let has_semantic_zones = terminal.has_semantic_zones();
        let line_zone = |grid_line: i32| terminal.get_line_zone(grid_line);
        let theme = self.gpu.effect_pipeline.theme();
        let ctx = RenderContext {
            layout: RenderLayout {
                offset_x,
                offset_y,
                padding,
                cell_width,
                line_height,
            },
            display_offset,
            cursor_viewport_line,
            palette: &theme.palette,
            default_fg: theme.foreground.to_array(),
            default_bg: theme.background.bottom.to_array(),
            hovered_url_index: self.interaction.hovered_url_index,
            detected_urls: &self.interaction.detected_urls,
            hovered_path_index: self.interaction.hovered_path_index,
            detected_paths: &self.interaction.detected_paths,
            search_active: self.ui.search.active,
            search_matches: &self.ui.search.matches,
            current_match: self.ui.search.current_match,
            highlight_style: if self.ui.search.active {
                Some(&theme.highlight)
            } else {
                None
            },
            has_semantic_zones,
            get_line_zone: &line_zone,
        };

        let (prepared_cells, decorations) =
            prepare_render_cells(&self.render.cached.collected_cells, &ctx);

        // Push glyph instances (rasterisation is cached in the glyph atlas)
        for cell in &prepared_cells {
            let style = GlyphStyle::new(cell.bold, cell.italic);
            if let Some(glyph) =
                self.gpu
                    .glyph_cache
                    .position_char_styled(cell.character, cell.x, cell.y, style)
            {
                if cell.use_glow {
                    self.gpu.grid_renderer.push_glyph(&glyph, cell.fg_color);
                } else {
                    self.gpu
                        .output_grid_renderer
                        .push_glyph(&glyph, cell.fg_color);
                }
            }
        }

        self.gpu.glyph_cache.flush(&shared_gpu.queue);

        Some(TextBufferUpdateResult {
            cursor: CursorInfo {
                x: cursor_x,
                y: cursor_y,
                cell_width,
                cell_height: line_height,
                visible: cursor_visible,
                shape: cursor.shape,
            },
            decorations,
        })
    }

    /// Create a shell for a new tab with spawn options
    pub fn create_shell_for_tab(&mut self, tab_id: u64, options: SpawnOptions) {
        let size = Size::new(self.cols, self.rows);
        log::info!(
            "Spawning shell for tab {} with semantic_prompts={}",
            tab_id,
            options.semantic_prompts
        );
        let result = ShellTerminal::with_options(size, options);

        match result {
            Ok(shell) => {
                log::info!("Shell spawned for tab {}", tab_id);
                self.shells.insert(tab_id, shell);
                self.text_rebuild.insert(tab_id);
            }
            Err(e) => {
                log::error!("Failed to spawn shell for tab {}: {}", tab_id, e);
            }
        }
    }

    /// Get the current working directory of the active tab's shell
    pub fn active_shell_cwd(&self) -> Option<std::path::PathBuf> {
        let tab_id = self.gpu.tab_bar.active_tab_id()?;
        let shell = self.shells.get(&tab_id)?;
        shell.working_directory()
    }

    /// Remove shell for a closed tab
    pub fn remove_shell_for_tab(&mut self, tab_id: u64) {
        self.shells.remove(&tab_id);
        self.text_rebuild.remove(&tab_id);
        log::info!("Removed shell for tab {}", tab_id);
    }

    /// Force redraw of active tab (alias of `invalidate_text`)
    pub fn force_active_tab_redraw(&mut self) {
        self.invalidate_text();
    }
}
