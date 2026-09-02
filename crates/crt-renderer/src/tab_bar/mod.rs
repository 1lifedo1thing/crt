//! Tab Bar Rendering
//!
//! GPU-accelerated tab bar with theme support, separated into:
//! - state: Tab data and management (no GPU)
//! - layout: Positioning and hit testing (no GPU)
//!
//! Shapes are pushed into a `RectRenderer` via `render_shapes_to_rects`;
//! text and glow effects are rendered separately via the text pipeline,
//! triggered by CSS properties like `text-shadow`.
//!
//! Layout is recomputed eagerly by every mutating method on [`TabBar`], so
//! hit testing is always consistent with the current tab list (no window
//! between a mutation and the next redraw where indices can be stale).

mod layout;
mod state;

pub use layout::{TabLayout, TabRect};
pub use state::{EditState, Tab, TabBarState};

use crt_theme::TabTheme;
use unicode_width::UnicodeWidthChar;

/// Default tab font size in logical pixels (matches the tab glyph cache
/// created by the app: `12.0 * scale`, line height multiplier 1.3).
const DEFAULT_TAB_FONT_SIZE: f32 = 12.0;
const DEFAULT_TAB_LINE_HEIGHT_MULT: f32 = 1.3;
/// Fallback advance width as a fraction of font size when no metrics are set.
const DEFAULT_CELL_WIDTH_RATIO: f32 = 0.6;
/// Glyph used to mark a clipped title.
const ELLIPSIS: char = '\u{2026}';
/// Glyph used as the text cursor while editing a title.
const EDIT_CURSOR: char = '|';

/// Visual mode for the drag feedback indicator
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragMode {
    /// Reordering within the same window
    Reorder,
    /// Merging into this window from another
    Merge,
    /// Tab will detach into a new window
    Detach,
}

/// Visual feedback state for tab dragging.
///
/// Set before rendering to indicate what visual cues to show.
#[derive(Debug, Clone)]
pub struct DragFeedback {
    /// The tab being dragged (renders dimmed)
    pub dragged_tab_id: u64,
    /// Where to show the insertion caret (if any)
    pub insertion_index: Option<usize>,
    /// Cursor position for ghost tab rendering (window-local coords)
    pub ghost_position: Option<(f32, f32)>,
    /// Current drag mode (affects visual style)
    pub mode: DragMode,
}

/// Tab bar facade - combines state, layout, and rendering
///
/// Rendering architecture:
/// - `render_shapes_to_rects`: pushes all shapes (backgrounds, borders) into a
///   `RectRenderer`
/// - Shader pipeline: renders text with effects based on CSS properties
///   (e.g., text-shadow triggers glow shader pass)
pub struct TabBar {
    state: TabBarState,
    layout: TabLayout,
    theme: TabTheme,
    /// Active drag visual feedback (None when no drag in progress)
    drag_feedback: Option<DragFeedback>,
    /// Per-tab display text, parallel to `state.tabs()`: the title clipped to
    /// the space available in its rect, or the edit buffer (with cursor) for
    /// the tab being edited. Rebuilt on every relayout, never per frame.
    display_titles: Vec<String>,
    /// Tab font metrics in physical pixels: (cell advance width, line height).
    /// `None` falls back to the defaults derived from the scale factor.
    font_metrics: Option<(f32, f32)>,
    /// Bumped whenever anything that affects tab-title glyph placement changes
    /// (titles, active tab, edit state, layout, theme, scale, font metrics).
    titles_version: u64,
}

impl Default for TabBar {
    fn default() -> Self {
        Self::from_state(TabBarState::new())
    }
}

impl TabBar {
    /// Create a tab bar. The GPU parameters are no longer needed (the vello
    /// path was removed) and are ignored; kept for source compatibility.
    pub fn new(_device: &wgpu::Device, _format: wgpu::TextureFormat) -> Self {
        Self::default()
    }

    /// Create a tab bar with a specific initial tab ID (for global ID support).
    /// The GPU parameters are ignored; kept for source compatibility.
    pub fn with_initial_id(_device: &wgpu::Device, _format: wgpu::TextureFormat, id: u64) -> Self {
        Self::from_state(TabBarState::with_initial_id(id))
    }

    /// Create a tab bar from an existing state (no GPU needed).
    pub fn from_state(state: TabBarState) -> Self {
        let mut bar = Self {
            state,
            layout: TabLayout::new(),
            theme: TabTheme::default(),
            drag_feedback: None,
            display_titles: Vec::new(),
            font_metrics: None,
            titles_version: 0,
        };
        bar.set_theme(TabTheme::default());
        bar
    }

    /// Set drag visual feedback for this frame's rendering
    pub fn set_drag_feedback(&mut self, feedback: Option<DragFeedback>) {
        self.drag_feedback = feedback;
    }

    // ---- Versioning / font metrics ----

    /// Monotonic counter bumped whenever the tab-title glyph layout could have
    /// changed (titles, active tab, editing state, layout, theme, scale or
    /// font metrics). Callers can cache glyph buffers keyed on this value.
    pub fn titles_version(&self) -> u64 {
        self.titles_version
    }

    /// Set the metrics of the font used to draw tab titles, in physical
    /// pixels: the per-character advance (`cell_width`) and the line height.
    /// These drive title clipping and vertical centering. Call this whenever
    /// the tab glyph cache is (re)created.
    pub fn set_font_metrics(&mut self, cell_width: f32, line_height: f32) {
        let metrics = (cell_width.max(1.0), line_height.max(1.0));
        if self.font_metrics != Some(metrics) {
            self.font_metrics = Some(metrics);
            self.relayout();
        }
    }

    /// Advance width of one title character in physical pixels.
    fn font_cell_width(&self) -> f32 {
        self.font_metrics.map(|(w, _)| w).unwrap_or_else(|| {
            DEFAULT_TAB_FONT_SIZE * DEFAULT_CELL_WIDTH_RATIO * self.layout.scale_factor()
        })
    }

    /// Line height of the title font in physical pixels.
    fn font_line_height(&self) -> f32 {
        self.font_metrics.map(|(_, h)| h).unwrap_or_else(|| {
            DEFAULT_TAB_FONT_SIZE * DEFAULT_TAB_LINE_HEIGHT_MULT * self.layout.scale_factor()
        })
    }

    // ---- Theme ----

    /// Set the tab theme
    pub fn set_theme(&mut self, theme: TabTheme) {
        self.layout.set_bar_height(theme.bar.height);
        self.layout.set_content_padding(theme.bar.content_padding);
        self.theme = theme;
        self.relayout();
    }

    // ---- Layout delegation ----

    /// Set scale factor for HiDPI displays
    pub fn set_scale_factor(&mut self, scale_factor: f32) {
        self.layout.set_scale_factor(scale_factor);
        self.relayout();
    }

    /// Get current tab bar height (in logical pixels)
    pub fn height(&self) -> f32 {
        self.layout.height()
    }

    /// Get the content offset (x, y) in PHYSICAL pixels.
    /// Content starts below the tab bar (`height() * scale`) plus padding.
    pub fn content_offset(&self) -> (f32, f32) {
        self.layout.content_offset()
    }

    /// Update screen size (in physical pixels)
    pub fn resize(&mut self, width: f32, height: f32) {
        self.layout.resize(width, height);
        self.relayout();
    }

    /// Recompute rects and display titles from the current state, and bump
    /// the titles version. Called by every mutating method so hit testing and
    /// labels are never stale.
    fn relayout(&mut self) {
        self.layout.calculate_rects(&self.state, &self.theme);
        self.rebuild_display_titles();
        self.titles_version = self.titles_version.wrapping_add(1);
    }

    /// Number of character cells available for a title inside `rect`
    /// (rect width minus horizontal padding on both sides and the close button).
    fn title_capacity(&self, rect: &TabRect) -> usize {
        let s = self.layout.scale_factor();
        let padding_x = self.theme.tab.padding_x * s;
        let available = rect.width - padding_x * 2.0 - rect.close_width;
        if available <= 0.0 {
            0
        } else {
            (available / self.font_cell_width()).floor() as usize
        }
    }

    fn rebuild_display_titles(&mut self) {
        let edit = self.state.edit_state().clone();
        let mut titles = std::mem::take(&mut self.display_titles);
        titles.clear();
        for (rect, tab) in self.layout.tab_rects().iter().zip(self.state.tabs()) {
            let capacity = self.title_capacity(rect);
            let text = if edit.tab_id == Some(tab.id) {
                let mut with_cursor = String::with_capacity(edit.text.len() + 1);
                let mut inserted = false;
                for (i, c) in edit.text.chars().enumerate() {
                    if i == edit.cursor {
                        with_cursor.push(EDIT_CURSOR);
                        inserted = true;
                    }
                    with_cursor.push(c);
                }
                if !inserted {
                    with_cursor.push(EDIT_CURSOR);
                }
                clip_edit_text(&with_cursor, edit.cursor, capacity)
            } else {
                clip_title(&tab.title, capacity)
            };
            titles.push(text);
        }
        self.display_titles = titles;
    }

    // ---- State delegation ----

    /// Add a new tab with a caller-provided globally unique ID
    pub fn add_tab(&mut self, id: u64, title: impl Into<String>) {
        self.state.add_tab(id, title);
        self.relayout();
    }

    /// Move a tab from one index to another
    pub fn move_tab(&mut self, from: usize, to: usize) {
        self.state.move_tab(from, to);
        self.relayout();
    }

    /// Get the index of a tab by its ID
    pub fn tab_index(&self, id: u64) -> Option<usize> {
        self.state.tabs().iter().position(|t| t.id == id)
    }

    /// Remove a tab by ID and return it (for cross-window transfer)
    pub fn remove_tab(&mut self, id: u64) -> Option<Tab> {
        let result = self.state.remove_tab(id);
        if result.is_some() {
            self.relayout();
        }
        result
    }

    /// Insert a pre-existing tab at the end
    pub fn add_existing_tab(&mut self, tab: Tab) {
        self.state.add_existing_tab(tab);
        self.relayout();
    }

    /// Insert a pre-existing tab at a specific index
    pub fn insert_existing_tab(&mut self, tab: Tab, index: usize) {
        self.state.insert_existing_tab(tab, index);
        self.relayout();
    }

    /// Close a tab by ID
    pub fn close_tab(&mut self, id: u64) -> bool {
        let result = self.state.close_tab(id);
        if result {
            self.relayout();
        }
        result
    }

    /// Select a tab by ID
    pub fn select_tab(&mut self, id: u64) -> bool {
        let result = self.state.select_tab(id);
        if result {
            self.relayout();
        }
        result
    }

    /// Select tab by index (0-based)
    pub fn select_tab_index(&mut self, index: usize) -> bool {
        let result = self.state.select_tab_index(index);
        if result {
            self.relayout();
        }
        result
    }

    /// Select next tab
    pub fn next_tab(&mut self) {
        self.state.next_tab();
        self.relayout();
    }

    /// Select previous tab
    pub fn prev_tab(&mut self) {
        self.state.prev_tab();
        self.relayout();
    }

    /// Get active tab ID
    pub fn active_tab_id(&self) -> Option<u64> {
        self.state.active_tab_id()
    }

    /// Get tab rectangles for hit testing and drag computations
    pub fn tab_rects(&self) -> &[TabRect] {
        self.layout.tab_rects()
    }

    /// Get active tab rectangle (for focus indicator rendering)
    /// Returns (x, y, width, height) in physical pixels
    pub fn active_tab_rect(&self) -> Option<(f32, f32, f32, f32)> {
        let active_idx = self.state.active_tab_index();
        self.layout
            .tab_rects()
            .get(active_idx)
            .map(|r| (r.x, r.y, r.width, r.height))
    }

    /// Get number of tabs
    pub fn tab_count(&self) -> usize {
        self.state.tab_count()
    }

    /// Hit test - returns (tab_id, is_close_button) if hit
    pub fn hit_test(&self, x: f32, y: f32) -> Option<(u64, bool)> {
        let tabs = self.state.tabs();
        self.layout
            .hit_test(x, y)
            .and_then(|(idx, is_close)| tabs.get(idx).map(|t| (t.id, is_close)))
    }

    /// Update a tab's title by ID (from OSC escape sequences)
    pub fn set_tab_title(&mut self, id: u64, title: impl Into<String>) -> bool {
        let result = self.state.set_tab_title(id, title);
        if result {
            self.relayout();
        }
        result
    }

    /// Set a custom title for a tab (user-initiated)
    pub fn set_custom_tab_title(&mut self, id: u64, title: impl Into<String>) -> bool {
        let result = self.state.set_custom_tab_title(id, title);
        if result {
            self.relayout();
        }
        result
    }

    /// Clear custom title flag
    pub fn clear_custom_title(&mut self, id: u64) {
        self.state.clear_custom_title(id);
    }

    /// Check if a tab has a custom title
    pub fn has_custom_title(&self, id: u64) -> bool {
        self.state.has_custom_title(id)
    }

    /// Get a tab's title by ID
    pub fn get_tab_title(&self, id: u64) -> Option<&str> {
        self.state.get_tab_title(id)
    }

    // ---- Inline Editing ----

    /// Check if currently editing a tab
    pub fn is_editing(&self) -> bool {
        self.state.is_editing()
    }

    /// Get the tab ID being edited (if any)
    pub fn editing_tab_id(&self) -> Option<u64> {
        self.state.editing_tab_id()
    }

    /// Start editing a tab's title
    pub fn start_editing(&mut self, id: u64) -> bool {
        let result = self.state.start_editing(id);
        if result {
            self.relayout();
        }
        result
    }

    /// Cancel editing without saving
    pub fn cancel_editing(&mut self) {
        self.state.cancel_editing();
        self.relayout();
    }

    /// Confirm editing and save the new title
    pub fn confirm_editing(&mut self) -> bool {
        let result = self.state.confirm_editing();
        self.relayout();
        result
    }

    /// Handle a character input during editing
    pub fn edit_insert_char(&mut self, c: char) {
        self.state.edit_insert_char(c);
        self.relayout();
    }

    /// Handle backspace during editing
    pub fn edit_backspace(&mut self) {
        self.state.edit_backspace();
        self.relayout();
    }

    /// Handle delete during editing
    pub fn edit_delete(&mut self) {
        self.state.edit_delete();
        self.relayout();
    }

    /// Move cursor left during editing
    pub fn edit_cursor_left(&mut self) {
        self.state.edit_cursor_left();
        self.relayout();
    }

    /// Move cursor right during editing
    pub fn edit_cursor_right(&mut self) {
        self.state.edit_cursor_right();
        self.relayout();
    }

    /// Move cursor to start during editing
    pub fn edit_cursor_home(&mut self) {
        self.state.edit_cursor_home();
        self.relayout();
    }

    /// Move cursor to end during editing
    pub fn edit_cursor_end(&mut self) {
        self.state.edit_cursor_end();
        self.relayout();
    }

    // ---- Theme colors ----

    /// Get the foreground color for inactive tabs
    pub fn inactive_tab_color(&self) -> [f32; 4] {
        color_to_array(&self.theme.tab.foreground)
    }

    /// Get the foreground color for active tabs
    pub fn active_tab_color(&self) -> [f32; 4] {
        color_to_array(&self.theme.active.foreground)
    }

    /// Get text shadow for inactive tabs (if any)
    pub fn inactive_tab_text_shadow(&self) -> Option<(f32, [f32; 4])> {
        self.theme
            .tab
            .text_shadow
            .map(|s| (s.radius, color_to_array(&s.color)))
    }

    /// Get text shadow for active tabs (if any)
    pub fn active_tab_text_shadow(&self) -> Option<(f32, [f32; 4])> {
        self.theme
            .active
            .text_shadow
            .map(|s| (s.radius, color_to_array(&s.color)))
    }

    // ---- Rendering ----

    /// Iterate tab labels for text rendering: `(x, y, text, is_active, is_editing)`
    /// in physical pixels. `text` borrows from the tab bar and is already
    /// clipped to fit its tab (an ellipsis marks a clipped title; the edit
    /// buffer is scrolled so the cursor stays visible). Rendering should
    /// advance one `cell_width` per `char`.
    pub fn tab_labels(&self) -> impl Iterator<Item = (f32, f32, &str, bool, bool)> + '_ {
        let s = self.layout.scale_factor();
        let tab_padding_x = self.theme.tab.padding_x * s;
        let line_height = self.font_line_height();
        let active_idx = self.state.active_tab_index();
        let editing = self.state.edit_state().tab_id;

        self.layout
            .tab_rects()
            .iter()
            .zip(self.state.tabs())
            .enumerate()
            .map(move |(i, (rect, tab))| {
                let text_x = rect.x + tab_padding_x;
                let text_y = rect.y + (rect.height - line_height) / 2.0;
                let text = self.display_titles.get(i).map(String::as_str).unwrap_or("");
                (
                    text_x,
                    text_y,
                    text,
                    i == active_idx,
                    editing == Some(tab.id),
                )
            })
    }

    /// Get tab labels for text rendering (see [`TabBar::tab_labels`]).
    /// Returns `(x, y, text, is_active, is_editing)` with `text` borrowed
    /// from the tab bar (no per-frame string clones).
    pub fn get_tab_labels(&self) -> Vec<(f32, f32, &str, bool, bool)> {
        self.tab_labels().collect()
    }

    /// Hit test the "+" new-tab button.
    pub fn hit_test_new_tab_button(&self, x: f32, y: f32) -> bool {
        self.layout.hit_test_new_tab_button(x, y)
    }

    /// Position for the "+" glyph in the new-tab button, if it's visible.
    pub fn get_new_tab_button_label(&self) -> Option<(f32, f32)> {
        let cell_width = self.font_cell_width();
        let line_height = self.font_line_height();
        self.layout.new_tab_button_rect().map(|rect| {
            let x = rect.x + (rect.width - cell_width) / 2.0;
            let y = rect.y + (rect.height - line_height) / 2.0;
            (x, y)
        })
    }

    /// Close button positions for text rendering: (x, y) for the 'x' glyph of
    /// each tab, in the same order as `tab_labels()`.
    pub fn get_close_button_labels(&self) -> impl Iterator<Item = (f32, f32)> + '_ {
        let cell_width = self.font_cell_width();
        let line_height = self.font_line_height();

        self.layout.tab_rects().iter().map(move |rect| {
            // Center the 'x' character in the close button area
            let x = rect.close_x + (rect.close_width - cell_width) * 0.5;
            let y = rect.y + (rect.height - line_height) / 2.0;
            (x, y)
        })
    }

    /// Prepare the tab bar for rendering.
    ///
    /// Layout is already kept current by every mutating method, so this only
    /// catches a stale layout as a safety net. The GPU parameters are unused
    /// (the vello scene build was removed) and kept for source compatibility.
    pub fn prepare(&mut self, _device: &wgpu::Device, _queue: &wgpu::Queue) {
        if self.layout.is_dirty() {
            self.relayout();
        }
    }
}

/// Columns a character occupies when rendered one cell per `char` with a
/// monospace advance: at least one (the renderer always advances), two for
/// East Asian wide characters so they are not counted as fitting when they
/// visibly spill into the next cell.
fn char_cols(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(1).max(1)
}

/// Display width of `text` in cells.
fn text_cols(text: &str) -> usize {
    text.chars().map(char_cols).sum()
}

/// Clip `title` so it occupies at most `max_cols` cells, appending an
/// ellipsis when anything was removed.
fn clip_title(title: &str, max_cols: usize) -> String {
    if text_cols(title) <= max_cols {
        return title.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let budget = max_cols - char_cols(ELLIPSIS);
    let mut out = String::with_capacity(title.len());
    let mut used = 0;
    for c in title.chars() {
        let w = char_cols(c);
        if used + w > budget {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push(ELLIPSIS);
    out
}

/// Clip edit text (which already contains the cursor glyph at char index
/// `cursor`) to `max_cols` cells, scrolling so the cursor stays visible.
fn clip_edit_text(text: &str, cursor: usize, max_cols: usize) -> String {
    if text_cols(text) <= max_cols {
        return text.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len().saturating_sub(1));

    // Find the leftmost start such that start..=cursor fits.
    let mut start = cursor;
    let mut used = char_cols(chars[cursor]);
    while start > 0 && used + char_cols(chars[start - 1]) <= max_cols {
        start -= 1;
        used += char_cols(chars[start]);
    }

    // Then extend to the right while there's room.
    let mut end = cursor + 1;
    while end < chars.len() && used + char_cols(chars[end]) <= max_cols {
        used += char_cols(chars[end]);
        end += 1;
    }
    chars[start..end].iter().collect()
}

fn color_to_array(color: &crt_theme::Color) -> [f32; 4] {
    [color.r, color.g, color.b, color.a]
}

impl TabBar {
    /// Render tab bar shapes using RectRenderer (sharp corners, no Vello needed)
    pub fn render_shapes_to_rects(&self, rect_renderer: &mut crate::RectRenderer) {
        let s = self.layout.scale_factor();
        let bar_height = self.layout.height() * s;
        let (screen_width, _) = self.layout.screen_size();

        let tab_rects = self.layout.tab_rects();
        let active_tab = self.state.active_tab_index();

        // Tab bar background
        let bar_bg = color_to_array(&self.theme.bar.background);
        rect_renderer.push_rect(0.0, 0.0, screen_width, bar_height, bar_bg);

        // Bottom border
        let border_color = color_to_array(&self.theme.bar.border_color);
        rect_renderer.push_rect(0.0, bar_height - s, screen_width, s, border_color);

        // Determine which tab is being dragged (if any)
        let dragged_tab_id = self.drag_feedback.as_ref().map(|f| f.dragged_tab_id);

        // Draw individual tabs
        for (i, rect) in tab_rects.iter().enumerate() {
            let is_active = i == active_tab;
            let tab_id = self.state.tabs().get(i).map(|t| t.id);
            let is_dragged = dragged_tab_id.is_some() && tab_id == dragged_tab_id;

            let mut bg_color = if is_active {
                color_to_array(&self.theme.active.background)
            } else {
                color_to_array(&self.theme.tab.background)
            };

            // Dim the dragged tab
            if is_dragged {
                bg_color[3] *= 0.4; // Reduce alpha
            }

            // Tab background (sharp corners)
            rect_renderer.push_rect(rect.x, rect.y, rect.width, rect.height, bg_color);

            // Tab border (top and sides)
            let border = color_to_array(&self.theme.bar.border_color);
            // Top
            rect_renderer.push_rect(rect.x, rect.y, rect.width, s, border);
            // Left
            rect_renderer.push_rect(rect.x, rect.y, s, rect.height, border);
            // Right
            rect_renderer.push_rect(rect.x + rect.width - s, rect.y, s, rect.height, border);

            // Active tab accent line at bottom
            if is_active && !is_dragged {
                let accent = color_to_array(&self.theme.active.accent);
                let accent_height = 2.0 * s;
                rect_renderer.push_rect(
                    rect.x,
                    rect.y + rect.height - accent_height,
                    rect.width,
                    accent_height,
                    accent,
                );
            }
        }

        // Draw insertion caret during drag
        if let Some(ref feedback) = self.drag_feedback {
            if let Some(insert_idx) = feedback.insertion_index {
                let accent = color_to_array(&self.theme.active.accent);
                let caret_width = 2.0 * s;
                let padding = self.theme.bar.padding * s;
                let tab_height = bar_height - padding * 2.0;

                // Position caret at the insertion gap
                let caret_x = if insert_idx == 0 {
                    // Before first tab
                    tab_rects.first().map(|r| r.x - 2.0 * s).unwrap_or(padding)
                } else if insert_idx >= tab_rects.len() {
                    // After last tab
                    tab_rects.last().map(|r| r.x + r.width).unwrap_or(padding)
                } else {
                    // Between tabs: midpoint of the gap
                    let prev = &tab_rects[insert_idx - 1];
                    let next = &tab_rects[insert_idx];
                    (prev.x + prev.width + next.x) / 2.0 - caret_width / 2.0
                };

                rect_renderer.push_rect(caret_x, padding, caret_width, tab_height, accent);
            }

            // Ghost tab at cursor position
            if let Some((gx, gy)) = feedback.ghost_position {
                if let Some(first) = tab_rects.first() {
                    let ghost_width = first.width;
                    let ghost_height = first.height;
                    let mut ghost_bg = color_to_array(&self.theme.active.background);
                    ghost_bg[3] = 0.6; // Semi-transparent
                    rect_renderer.push_rect(
                        gx - ghost_width / 2.0,
                        gy - ghost_height / 2.0,
                        ghost_width,
                        ghost_height,
                        ghost_bg,
                    );

                    // Ghost tab border
                    let accent = color_to_array(&self.theme.active.accent);
                    let border_w = 1.0 * s;
                    let gx_off = gx - ghost_width / 2.0;
                    let gy_off = gy - ghost_height / 2.0;
                    rect_renderer.push_rect(gx_off, gy_off, ghost_width, border_w, accent);
                    rect_renderer.push_rect(
                        gx_off,
                        gy_off + ghost_height - border_w,
                        ghost_width,
                        border_w,
                        accent,
                    );
                    rect_renderer.push_rect(gx_off, gy_off, border_w, ghost_height, accent);
                    rect_renderer.push_rect(
                        gx_off + ghost_width - border_w,
                        gy_off,
                        border_w,
                        ghost_height,
                        accent,
                    );
                }
            }
        }

        // New-tab "+" button background (its glyph is drawn in render_tab_titles)
        if let Some(rect) = self.layout.new_tab_button_rect() {
            let bg = color_to_array(&self.theme.tab.background);
            rect_renderer.push_rect(rect.x, rect.y, rect.width, rect.height, bg);
            let border = color_to_array(&self.theme.bar.border_color);
            // Top / left / right border (bottom aligns with the bar border)
            rect_renderer.push_rect(rect.x, rect.y, rect.width, s, border);
            rect_renderer.push_rect(rect.x, rect.y, s, rect.height, border);
            rect_renderer.push_rect(rect.x + rect.width - s, rect.y, s, rect.height, border);
        }

        // Close buttons are rendered as text glyphs in render_tab_titles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar_with_tabs(n: usize) -> TabBar {
        let mut bar = TabBar::default();
        bar.resize(800.0, 600.0);
        for i in 1..n {
            bar.add_tab(i as u64, format!("Tab {i}"));
        }
        bar
    }

    #[test]
    fn hit_test_is_current_without_prepare() {
        let mut bar = bar_with_tabs(3);
        let last = *bar.tab_rects().last().unwrap();
        assert_eq!(bar.hit_test(last.x + 5.0, last.y + 5.0), Some((2, false)));

        // Remove a tab and hit-test immediately (no prepare/render in between):
        // must not panic and must report the tab now under the cursor.
        assert!(bar.close_tab(0));
        assert_eq!(bar.tab_count(), 2);
        assert_eq!(bar.tab_rects().len(), 2);
        let first = bar.tab_rects()[0];
        assert_eq!(bar.hit_test(first.x + 5.0, first.y + 5.0), Some((1, false)));
        // The old third-tab position is now the "+" button or empty space.
        assert_eq!(bar.hit_test(last.x + 5.0, last.y + 5.0), None);
        assert_eq!(bar.hit_test(-1.0, -1.0), None);
    }

    #[test]
    fn hit_test_after_remove_all_tabs_is_none() {
        let mut bar = TabBar::from_state(TabBarState::with_initial_id(9));
        bar.resize(800.0, 600.0);
        let rect = bar.tab_rects()[0];
        assert!(bar.remove_tab(9).is_some());
        assert_eq!(bar.hit_test(rect.x + 1.0, rect.y + 1.0), None);
        assert!(bar.tab_rects().is_empty());
        assert!(bar.active_tab_rect().is_none());
    }

    #[test]
    fn content_offset_is_physical_through_facade() {
        let mut bar = bar_with_tabs(1);
        bar.set_scale_factor(2.0);
        let (_, off) = bar.content_offset();
        let theme = TabTheme::default();
        assert_eq!(bar.height(), theme.bar.height);
        assert_eq!(off, (theme.bar.height + theme.bar.content_padding) * 2.0);
    }

    #[test]
    fn titles_version_bumps_on_relevant_changes() {
        let mut bar = bar_with_tabs(2);
        let v0 = bar.titles_version();
        bar.set_tab_title(1, "New title");
        let v1 = bar.titles_version();
        assert!(v1 > v0);
        bar.select_tab(1);
        let v2 = bar.titles_version();
        assert!(v2 > v1);
        assert!(bar.start_editing(1));
        let v3 = bar.titles_version();
        assert!(v3 > v2);
        bar.edit_insert_char('x');
        assert!(bar.titles_version() > v3);
        let v4 = bar.titles_version();
        bar.set_font_metrics(8.0, 16.0);
        assert!(bar.titles_version() > v4);
        let v5 = bar.titles_version();
        // Same metrics again: no change, no bump.
        bar.set_font_metrics(8.0, 16.0);
        assert_eq!(bar.titles_version(), v5);
        // Drag feedback doesn't affect titles.
        bar.set_drag_feedback(None);
        assert_eq!(bar.titles_version(), v5);
    }

    #[test]
    fn labels_are_borrowed_and_clipped_to_tab_width() {
        let mut bar = TabBar::default();
        bar.resize(400.0, 600.0);
        bar.set_font_metrics(8.0, 16.0);
        for i in 1..12 {
            bar.add_tab(i as u64, "A very long tab title that cannot fit");
        }
        let theme = TabTheme::default();
        let labels = bar.get_tab_labels();
        assert_eq!(labels.len(), 12);
        for ((_, _, text, _, _), rect) in labels.iter().zip(bar.tab_rects()) {
            let capacity =
                ((rect.width - theme.tab.padding_x * 2.0 - rect.close_width) / 8.0).floor();
            assert!(
                text_cols(text) as f32 <= capacity,
                "label {text:?} ({} cols) exceeds capacity {capacity}",
                text_cols(text)
            );
            assert!(
                text.ends_with(ELLIPSIS),
                "clipped label should end in ellipsis"
            );
        }
        // Short titles are left alone.
        let mut bar = bar_with_tabs(2);
        bar.set_font_metrics(8.0, 16.0);
        let labels = bar.get_tab_labels();
        assert_eq!(labels[0].2, "Terminal");
        assert_eq!(labels[1].2, "Tab 1");
        assert!(labels[0].3, "first tab is active");
        assert!(!labels[0].4);
    }

    #[test]
    fn clip_title_handles_wide_chars_and_zero_budget() {
        assert_eq!(clip_title("hello", 5), "hello");
        assert_eq!(clip_title("hello world", 5), "hell…");
        assert_eq!(clip_title("hello", 0), "");
        assert_eq!(clip_title("hello", 1), "…");
        // "日本語" is 6 cells wide; 5 cells fit two wide chars plus ellipsis.
        assert_eq!(clip_title("日本語", 5), "日本…");
        assert_eq!(clip_title("日本語", 6), "日本語");
    }

    #[test]
    fn edit_text_is_scrolled_to_keep_cursor_visible() {
        let mut bar = TabBar::default();
        bar.resize(800.0, 600.0);
        bar.set_font_metrics(8.0, 16.0);
        let theme = TabTheme::default();
        let rect = bar.tab_rects()[0];
        let capacity =
            ((rect.width - theme.tab.padding_x * 2.0 - rect.close_width) / 8.0).floor() as usize;

        assert!(bar.start_editing(0));
        for c in "abcdefghijklmnopqrstuvwxyz".chars() {
            bar.edit_insert_char(c);
        }
        let labels = bar.get_tab_labels();
        let (_, _, text, _, editing) = labels[0];
        assert!(editing);
        assert!(text.chars().count() <= capacity);
        assert!(
            text.ends_with('|'),
            "cursor at end must be visible: {text:?}"
        );

        // Move cursor home: the window scrolls to the start.
        bar.edit_cursor_home();
        let labels = bar.get_tab_labels();
        let text = labels[0].2;
        assert!(
            text.starts_with('|'),
            "cursor at start must be visible: {text:?}"
        );
        assert!(text.chars().count() <= capacity);

        // Direct check of the helper.
        // Text before the cursor is preferred (like an editor scrolled so the
        // caret sits at the right edge), then it extends right if room remains.
        assert_eq!(clip_edit_text("ab|cd", 2, 3), "ab|");
        assert_eq!(clip_edit_text("ab|cd", 2, 4), "ab|c");
        assert_eq!(clip_edit_text("|abcd", 0, 3), "|ab");
        assert_eq!(clip_edit_text("abcd|", 4, 3), "cd|");
        assert_eq!(clip_edit_text("ab|", 2, 10), "ab|");
    }

    #[test]
    fn close_and_plus_labels_use_font_metrics() {
        let mut bar = bar_with_tabs(2);
        bar.set_font_metrics(8.0, 16.0);
        let closes: Vec<(f32, f32)> = bar.get_close_button_labels().collect();
        assert_eq!(closes.len(), 2);
        for ((x, y), rect) in closes.iter().zip(bar.tab_rects()) {
            assert!(*x >= rect.close_x && *x + 8.0 <= rect.close_x + rect.close_width + 0.01);
            assert!(*y >= rect.y && *y + 16.0 <= rect.y + rect.height + 0.01);
        }
        let (px, py) = bar.get_new_tab_button_label().expect("plus button");
        let plus_rect = bar.tab_rects().last().unwrap();
        assert!(px > plus_rect.x + plus_rect.width);
        assert!(py >= plus_rect.y);
    }
}
