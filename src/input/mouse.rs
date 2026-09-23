//! Mouse input handling
//!
//! Extracts mouse event handling logic from main.rs for better modularity.
//!
//! Pure decision functions (no side effects, fully testable):
//! - `screen_to_grid_position` — pixel-to-cell coordinate conversion
//! - `determine_click_target` — decides what a mouse click hits
//! - `compute_click_count` — multi-click detection (single/double/triple)
//! - `normalize_scroll_delta` — converts pixel scroll delta to line count
//! - `accumulate_scroll` — turns fractional (trackpad) deltas into whole lines

use std::cell::Cell;
use std::time::{Duration, Instant};

use crt_core::Scroll;
use winit::event::{ElementState, Modifiers, MouseButton, MouseScrollDelta};
use winit::keyboard::ModifiersState;

use crate::window::{ContextMenuItem, WindowState};

use super::{
    DetectedPath, DetectedUrl, MOUSE_BUTTON_LEFT, MOUSE_BUTTON_MIDDLE, MOUSE_BUTTON_RIGHT,
    find_path_at_position, find_path_index_at_position, find_url_at_position,
    find_url_index_at_position, get_clipboard_content, get_terminal_selection_text,
    handle_alternate_scroll, handle_tab_click, handle_terminal_mouse_button,
    handle_terminal_mouse_move, handle_terminal_scroll, open_file, open_url, paste_to_terminal,
    set_clipboard_content, set_modifiers,
};

// ── Pure decision functions (no side effects) ──────────────────────────────

/// Layout parameters needed for coordinate conversion
#[derive(Debug, Clone, Copy)]
pub struct GridLayout {
    /// X offset of the content area (e.g., tab bar offset)
    pub content_offset_x: f32,
    /// Y offset of the content area
    pub content_offset_y: f32,
    /// Padding around the terminal content
    pub padding: f32,
    /// Width of a single character cell
    pub cell_width: f32,
    /// Height of a single line
    pub line_height: f32,
    /// Maximum number of columns
    pub max_cols: usize,
    /// Maximum number of rows
    pub max_rows: usize,
}

/// Convert screen pixel coordinates to terminal grid position (col, line).
///
/// Returns `None` if the position is outside the terminal content area
/// (negative relative coordinates after subtracting offsets and padding).
/// Clamps to grid bounds when inside the content area.
pub fn screen_to_grid_position(x: f32, y: f32, layout: &GridLayout) -> Option<(usize, usize)> {
    let rel_x = x - layout.content_offset_x - layout.padding;
    let rel_y = y - layout.content_offset_y - layout.padding;

    if rel_x < 0.0 || rel_y < 0.0 {
        return None;
    }

    let col = (rel_x / layout.cell_width) as usize;
    let line = (rel_y / layout.line_height) as usize;

    let col = col.min(layout.max_cols.saturating_sub(1));
    let line = line.min(layout.max_rows.saturating_sub(1));

    Some((col, line))
}

/// Convert screen pixel coordinates to the *nearest* terminal grid position.
///
/// Like [`screen_to_grid_position`] but never returns `None`: positions
/// above/left of the content area clamp to column/row 0, and positions
/// beyond it clamp to the last column/row. Used for button releases and
/// drags, so applications never see a button stuck down because the pointer
/// left the grid.
pub fn screen_to_grid_position_clamped(x: f32, y: f32, layout: &GridLayout) -> (usize, usize) {
    let rel_x = (x - layout.content_offset_x - layout.padding).max(0.0);
    let rel_y = (y - layout.content_offset_y - layout.padding).max(0.0);

    let col = (rel_x / layout.cell_width) as usize;
    let line = (rel_y / layout.line_height) as usize;

    (
        col.min(layout.max_cols.saturating_sub(1)),
        line.min(layout.max_rows.saturating_sub(1)),
    )
}

/// What a mouse click targets
#[allow(dead_code)] // pure decision model, exercised by tests
#[derive(Debug, Clone, PartialEq)]
pub enum MouseClickTarget {
    /// Cmd+click on a URL at grid position (col, line)
    OpenUrl { col: usize, line: usize },
    /// Cmd+click on a file path at grid position (col, line)
    OpenFile { col: usize, line: usize },
    /// Left-click on a context menu submenu item
    ContextSubmenuItem,
    /// Left-click on a context menu main item
    ContextMenuItem,
    /// Left-click on a context menu item that has a submenu (no-op click)
    ContextSubmenuParent,
    /// Click outside the context menu (dismisses it)
    DismissContextMenu,
    /// Right-click while context menu is open (repositions it)
    MoveContextMenu,
    /// Right-click to open context menu
    ShowContextMenu,
    /// Left-click in terminal/tab area (normal click handling)
    Terminal { button: u8 },
    /// Mouse button release
    Release,
    /// Unhandled button or state
    None,
}

/// Determine what a mouse click targets, without performing any side effects.
///
/// This is the pure decision function for `handle_mouse_input`. The caller
/// is responsible for executing the appropriate side effects based on the result.
#[allow(dead_code)] // pure decision model, exercised by tests
pub fn determine_click_target(
    button: MouseButton,
    button_state: ElementState,
    cmd_pressed: bool,
    context_menu_visible: bool,
    has_grid_position: bool,
) -> MouseClickTarget {
    // Cmd+click to open URL
    if cmd_pressed && button == MouseButton::Left && button_state == ElementState::Pressed {
        // Caller must check if there's actually a URL at the position
        if has_grid_position {
            return MouseClickTarget::OpenUrl { col: 0, line: 0 };
        }
    }

    // Context menu interactions
    if context_menu_visible {
        match (button, button_state) {
            (MouseButton::Left, ElementState::Pressed) => {
                // Caller determines which item was hit and fills in the variant
                // This returns a hint that context menu should be checked
                return MouseClickTarget::ContextMenuItem;
            }
            (MouseButton::Right, ElementState::Pressed) => {
                return MouseClickTarget::MoveContextMenu;
            }
            _ => {}
        }
    }

    // Right-click shows context menu
    if button == MouseButton::Right && button_state == ElementState::Pressed {
        return MouseClickTarget::ShowContextMenu;
    }

    // Map winit button to terminal button code
    let mouse_button = match button {
        MouseButton::Left => Some(MOUSE_BUTTON_LEFT),
        MouseButton::Middle => Some(MOUSE_BUTTON_MIDDLE),
        MouseButton::Right => Some(MOUSE_BUTTON_RIGHT),
        _ => None,
    };

    match (mouse_button, button_state) {
        (Some(btn), ElementState::Pressed) => MouseClickTarget::Terminal { button: btn },
        (Some(_), ElementState::Released) => MouseClickTarget::Release,
        _ => MouseClickTarget::None,
    }
}

/// Which detected link a Cmd+click cell targets.
#[derive(Debug)]
pub enum ClickLink<'a> {
    /// A URL at this cell.
    Url(&'a DetectedUrl),
    /// A file path at this cell.
    Path(&'a DetectedPath),
    /// No link at this cell.
    None,
}

/// Decide which detected link a `(col, line)` cell targets.
///
/// URLs take precedence over file paths when both overlap a cell, so a
/// `file://` link (handled as a URL) or an `http(s)` link is never shadowed by
/// path detection. Pure: no side effects.
pub fn resolve_click_link<'a>(
    urls: &'a [DetectedUrl],
    paths: &'a [DetectedPath],
    col: usize,
    line: usize,
) -> ClickLink<'a> {
    if let Some(url) = find_url_at_position(urls, col, line) {
        ClickLink::Url(url)
    } else if let Some(path) = find_path_at_position(paths, col, line) {
        ClickLink::Path(path)
    } else {
        ClickLink::None
    }
}

/// Compute click count for multi-click detection (single, double, triple).
///
/// Returns 1 for single click, 2 for double, 3 for triple. Cycles back to 1
/// after triple click. Resets to 1 if the click is too far from the previous
/// click or too much time has elapsed.
pub fn compute_click_count(
    now: Instant,
    last_click_time: Option<Instant>,
    last_click_pos: Option<(usize, usize)>,
    current_pos: (usize, usize),
    previous_count: usize,
    threshold: Duration,
    max_distance: usize,
) -> usize {
    if let (Some(last_time), Some((last_col, last_line))) = (last_click_time, last_click_pos) {
        let time_ok = now.duration_since(last_time) < threshold;
        let pos_ok = current_pos.0.abs_diff(last_col) <= max_distance
            && current_pos.1.abs_diff(last_line) <= max_distance;

        if time_ok && pos_ok {
            (previous_count % 3) + 1
        } else {
            1
        }
    } else {
        1
    }
}

/// Normalize a pixel scroll delta to a (vertical) line count.
///
/// For `LineDelta`, returns the Y component directly.
/// For `PixelDelta`, divides by line height to convert pixels to lines.
#[allow(dead_code)] // vertical-only convenience, exercised by tests
pub fn normalize_scroll_delta(delta: &MouseScrollDelta, line_height: f32) -> f32 {
    normalize_scroll_delta_xy(delta, line_height).1
}

/// Normalize a scroll delta to `(horizontal, vertical)` line counts.
///
/// Horizontal pixel deltas are divided by the same line height (there is no
/// better unit for a wheel-left/right tick).
pub fn normalize_scroll_delta_xy(delta: &MouseScrollDelta, line_height: f32) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (*x, *y),
        MouseScrollDelta::PixelDelta(pos) => (
            (pos.x / line_height as f64) as f32,
            (pos.y / line_height as f64) as f32,
        ),
    }
}

/// Fold a fractional scroll delta into a running accumulator and return the
/// whole lines it now amounts to.
///
/// Trackpads deliver many sub-line deltas per gesture; truncating each one to
/// an integer would drop them all. The accumulator carries the remainder
/// across events, and resets when the scroll direction flips so a reversal
/// doesn't have to "pay back" the leftover first.
///
/// Returns `(new_accumulator, whole_lines)`.
pub fn accumulate_scroll(accumulator: f32, delta: f32) -> (f32, i32) {
    let accumulator =
        if delta != 0.0 && accumulator != 0.0 && delta.signum() != accumulator.signum() {
            0.0
        } else {
            accumulator
        };
    let total = accumulator + delta;
    let lines = total.trunc();
    (total - lines, lines as i32)
}

thread_local! {
    /// Sub-line scroll remainders `(x, y)`. One focused window receives wheel
    /// events at a time, so a single accumulator suffices; `InteractionState`
    /// is owned by another module and can't grow a field here.
    static SCROLL_ACCUMULATOR: Cell<(f32, f32)> = const { Cell::new((0.0, 0.0)) };
}

/// Accumulate a `(x, y)` delta and return the whole `(lines_x, lines_y)`.
fn accumulate_scroll_xy(dx: f32, dy: f32) -> (i32, i32) {
    SCROLL_ACCUMULATOR.with(|acc| {
        let (ax, ay) = acc.get();
        let (ax, lines_x) = accumulate_scroll(ax, dx);
        let (ay, lines_y) = accumulate_scroll(ay, dy);
        acc.set((ax, ay));
        (lines_x, lines_y)
    })
}

// ── Side-effectful handler functions ───────────────────────────────────────

/// Handle cursor moved event with explicit modifier state (used for xterm
/// modifier bits in motion reports and for Shift bypassing mouse reporting).
pub fn handle_cursor_moved_with_modifiers(
    state: &mut WindowState,
    x: f32,
    y: f32,
    mods: ModifiersState,
) {
    state.interaction.cursor_position = (x, y);

    // Update context menu hover state
    if state.ui.context_menu.visible {
        let menu = &state.ui.context_menu;
        let old_hover = (
            menu.hovered_item,
            menu.submenu_hovered_item,
            menu.submenu_visible,
        );
        state.ui.context_menu.update_hover(x, y);
        let menu = &state.ui.context_menu;
        if old_hover
            != (
                menu.hovered_item,
                menu.submenu_hovered_item,
                menu.submenu_visible,
            )
        {
            state.render.dirty = true;
            state.window.request_redraw();
        }
    }

    // Update selection if dragging
    handle_terminal_mouse_move(state, x, y, mods);

    // Check for URL / file-path hover and update underline state
    let layout = grid_layout_from_state(state);
    let grid_pos = screen_to_grid_position(x, y, &layout);
    let new_hovered_url = grid_pos.and_then(|(col, line)| {
        find_url_index_at_position(&state.interaction.detected_urls, col, line)
    });
    // URLs take precedence: only consider a path hover when no URL is hovered,
    // so overlapping detections never produce a double underline.
    let new_hovered_path = if new_hovered_url.is_some() {
        None
    } else {
        grid_pos.and_then(|(col, line)| {
            find_path_index_at_position(&state.interaction.detected_paths, col, line)
        })
    };

    // Redraw if either hover state changed
    if new_hovered_url != state.interaction.hovered_url_index
        || new_hovered_path != state.interaction.hovered_path_index
    {
        state.interaction.hovered_url_index = new_hovered_url;
        state.interaction.hovered_path_index = new_hovered_path;
        // Force content re-render to update decorations
        state.force_active_tab_redraw();
    }
}

/// Handle mouse button event
///
/// Returns true if the event was fully handled.
pub fn handle_mouse_input(
    state: &mut WindowState,
    button: MouseButton,
    button_state: ElementState,
    modifiers: &Modifiers,
    open_file_command: Option<&str>,
) -> bool {
    let (x, y) = state.interaction.cursor_position;
    set_modifiers(modifiers);
    let mods = modifiers.state();

    // Check for Cmd+click (Super on macOS, Ctrl on Linux) to open URLs
    #[cfg(target_os = "macos")]
    let cmd_pressed = mods.super_key();
    #[cfg(not(target_os = "macos"))]
    let cmd_pressed = mods.control_key();

    if cmd_pressed && button == MouseButton::Left && button_state == ElementState::Pressed {
        let layout = grid_layout_from_state(state);
        if let Some((col, line)) = screen_to_grid_position(x, y, &layout) {
            // Resolve to owned data first so we don't hold a borrow of `state`
            // across the `active_shell_cwd()` call below.
            let link = match resolve_click_link(
                &state.interaction.detected_urls,
                &state.interaction.detected_paths,
                col,
                line,
            ) {
                ClickLink::Url(url) => Some((Some(url.url.clone()), None)),
                ClickLink::Path(path) => Some((
                    None,
                    Some((path.path.clone(), path.target_line, path.target_col)),
                )),
                ClickLink::None => None,
            };
            if let Some((url, path)) = link {
                if let Some(url) = url {
                    log::info!("Opening URL: {}", url);
                    open_url(&url);
                    return true;
                }
                if let Some((token, target_line, target_col)) = path {
                    // Re-resolve against the same context the detector used so a
                    // relative token becomes an absolute path before opening.
                    let cwd = state.active_shell_cwd();
                    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
                    if let Some(resolved) =
                        crate::input::resolve_path(&token, cwd.as_deref(), home.as_deref())
                    {
                        log::info!("Opening file: {}", resolved.display());
                        open_file(&resolved, target_line, target_col, open_file_command);
                        return true;
                    }
                }
            }
        }
    }

    // Handle context menu interactions first
    if state.ui.context_menu.visible {
        match (button, button_state) {
            (MouseButton::Left, ElementState::Pressed) => {
                // Check submenu first
                if let Some(item) = state.ui.context_menu.submenu_item_at(x, y) {
                    handle_context_menu_action(state, item);
                    state.ui.context_menu.hide();
                    state.render.dirty = true;
                    state.window.request_redraw();
                    return true;
                }
                // Check main menu
                if let Some(item) = state.ui.context_menu.item_at(x, y) {
                    // Separators are inert; Themes opens its submenu on hover
                    if item.is_separator() {
                        return true;
                    }
                    if !item.has_submenu() {
                        handle_context_menu_action(state, item);
                        state.ui.context_menu.hide();
                        state.render.dirty = true;
                        state.window.request_redraw();
                        return true;
                    }
                    // Clicking on Themes just keeps it highlighted
                    return true;
                }
                // The padding and border around the rows are still the menu
                if state.ui.context_menu.covers(x, y) {
                    return true;
                }
                // Clicking outside the menu dismisses it
                state.ui.context_menu.hide();
                state.render.dirty = true;
                state.window.request_redraw();
                // Fall through to normal click handling
            }
            (MouseButton::Right, ElementState::Pressed) => {
                // Right-click while menu is open moves the menu
                state.ui.context_menu.show(x, y);
                state.render.dirty = true;
                state.window.request_redraw();
                return true;
            }
            _ => {}
        }
    }

    // Right-click shows context menu
    if button == MouseButton::Right && button_state == ElementState::Pressed {
        state.ui.context_menu.show(x, y);
        state.render.dirty = true;
        state.window.request_redraw();
        return true;
    }

    let mouse_button = match button {
        MouseButton::Left => Some(MOUSE_BUTTON_LEFT),
        MouseButton::Middle => Some(MOUSE_BUTTON_MIDDLE),
        MouseButton::Right => Some(MOUSE_BUTTON_RIGHT),
        _ => None,
    };

    if let Some(btn) = mouse_button {
        let now = Instant::now();
        match button_state {
            ElementState::Pressed => {
                // Try terminal (mouse reporting or selection) first, then tab bar
                if !handle_terminal_mouse_button(state, x, y, now, btn, true, mods)
                    && btn == MOUSE_BUTTON_LEFT
                {
                    handle_tab_click(state, x, y, now);
                }
            }
            ElementState::Released => {
                // Button-aware release (SGR reports the real button), clamped
                // to the grid if the pointer left the content area.
                handle_terminal_mouse_button(state, x, y, now, btn, false, mods);
            }
        }
    }

    false
}

/// Handle mouse wheel event with explicit modifier state.
///
/// Order of precedence:
/// 1. Mouse reporting (unless Shift is held): wheel buttons 64/65 (vertical)
///    and 66/67 (horizontal) with xterm modifier bits, one report per line.
/// 2. Alternate scroll: in the alternate screen, vertical lines become
///    Up/Down cursor keys.
/// 3. Local scrollback.
///
/// Fractional (trackpad) deltas are accumulated across events so slow
/// scrolling still moves.
pub fn handle_mouse_wheel_with_modifiers(
    state: &mut WindowState,
    delta: MouseScrollDelta,
    mods: ModifiersState,
) {
    let (x, y) = state.interaction.cursor_position;
    let line_height = state.gpu.glyph_cache.line_height();
    let (delta_x, delta_y) = normalize_scroll_delta_xy(&delta, line_height);
    let (lines_x, lines_y) = accumulate_scroll_xy(delta_x, delta_y);
    if lines_x == 0 && lines_y == 0 {
        return;
    }

    // Mouse reporting (Shift bypasses it so the user can always scroll back)
    if handle_terminal_scroll(state, x, y, lines_x, lines_y, mods) {
        return;
    }

    if lines_y == 0 {
        return; // horizontal-only wheel has no local meaning
    }

    // Alternate screen: translate to cursor keys (DECSET 1007)
    if !mods.shift_key() && handle_alternate_scroll(state, lines_y) {
        return;
    }

    // Fall back to local scrollback
    let tab_id = state.gpu.tab_bar.active_tab_id();
    if let Some(tab_id) = tab_id
        && let Some(shell) = state.shells.get_mut(&tab_id)
    {
        shell.scroll(Scroll::Delta(lines_y));
        state.render.dirty = true;
        state.text_rebuild.insert(tab_id);
        state.window.request_redraw();
    }
}

/// Build a `GridLayout` from the current window state.
///
/// Extracts the layout parameters needed by pure coordinate-conversion functions.
fn grid_layout_from_state(state: &WindowState) -> GridLayout {
    let (content_offset_x, content_offset_y) = state.gpu.tab_bar.content_offset();
    GridLayout {
        content_offset_x,
        content_offset_y,
        padding: 10.0 * state.scale_factor,
        cell_width: state.gpu.glyph_cache.cell_width(),
        line_height: state.gpu.glyph_cache.line_height(),
        max_cols: state.cols,
        max_rows: state.rows,
    }
}

/// Handle context menu action (copy, paste, select all)
pub(super) fn handle_context_menu_action(state: &mut WindowState, item: ContextMenuItem) {
    match item {
        ContextMenuItem::Copy => {
            if let Some(text) = get_terminal_selection_text(state) {
                set_clipboard_content(&text);
                state.ui.copy_indicator.trigger();
            }
        }
        ContextMenuItem::Paste => {
            if let Some(content) = get_clipboard_content() {
                paste_to_terminal(state, &content);
            }
        }
        ContextMenuItem::SelectAll => {
            // Select all visible content
            if let Some(tab_id) = state.gpu.tab_bar.active_tab_id()
                && let Some(shell) = state.shells.get_mut(&tab_id)
            {
                use crt_core::{Column, Line, Point, SelectionType};
                let terminal = shell.terminal_mut();
                let screen_lines = terminal.screen_lines();
                let columns = terminal.columns();

                // Start selection at top-left
                terminal.start_selection(
                    Point {
                        line: Line(0),
                        column: Column(0),
                    },
                    SelectionType::Simple,
                );
                // Extend to bottom-right
                terminal.update_selection(Point {
                    line: Line(screen_lines as i32 - 1),
                    column: Column(columns - 1),
                });
                state.render.dirty = true;
            }
        }
        ContextMenuItem::Separator | ContextMenuItem::Themes => {
            // Separator and Themes parent items are not clickable
        }
        ContextMenuItem::Theme(name) => {
            // Store pending theme change for main loop to process
            state.ui.pending_theme = Some(name);
        }
        ContextMenuItem::Update(_) => {
            // Checking needs the waker and config, which live on App.
            state.ui.pending_update_check = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_layout() -> GridLayout {
        GridLayout {
            content_offset_x: 0.0,
            content_offset_y: 30.0, // tab bar height
            padding: 10.0,
            cell_width: 8.0,
            line_height: 16.0,
            max_cols: 80,
            max_rows: 24,
        }
    }

    // ── screen_to_grid_position tests ──────────────────────────────────

    #[test]
    fn grid_position_at_origin() {
        let layout = test_layout();
        // First cell: x just past offset+padding, y just past offset+padding
        let pos = screen_to_grid_position(10.5, 40.5, &layout);
        assert_eq!(pos, Some((0, 0)));
    }

    #[test]
    fn grid_position_mid_screen() {
        let layout = test_layout();
        // x = 0 + 10 + 8*10 = 90, y = 30 + 10 + 16*5 = 120
        let pos = screen_to_grid_position(90.0, 120.0, &layout);
        assert_eq!(pos, Some((10, 5)));
    }

    #[test]
    fn grid_position_outside_returns_none() {
        let layout = test_layout();
        // x is in the padding zone (before content starts)
        assert_eq!(screen_to_grid_position(5.0, 50.0, &layout), None);
        // y is above the content area
        assert_eq!(screen_to_grid_position(50.0, 25.0, &layout), None);
    }

    #[test]
    fn grid_position_clamps_to_bounds() {
        let layout = test_layout();
        // Very far right — should clamp to max_cols - 1
        let pos = screen_to_grid_position(10000.0, 50.0, &layout);
        assert_eq!(pos, Some((79, 0)));
        // Very far down — should clamp to max_rows - 1
        let pos = screen_to_grid_position(50.0, 10000.0, &layout);
        assert_eq!(pos, Some((5, 23)));
    }

    // ── compute_click_count tests ──────────────────────────────────────

    #[test]
    fn click_count_single_on_first_click() {
        let now = Instant::now();
        let count = compute_click_count(
            now,
            None, // no previous click
            None,
            (5, 5),
            0,
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn click_count_double_on_rapid_same_position() {
        let first = Instant::now();
        let second = first + Duration::from_millis(200);
        let count = compute_click_count(
            second,
            Some(first),
            Some((5, 5)),
            (5, 5),
            1, // previous count was 1
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 2);
    }

    #[test]
    fn click_count_triple_then_wraps() {
        let first = Instant::now();
        let second = first + Duration::from_millis(200);
        // Third click with previous count 2 → should give 3
        let count = compute_click_count(
            second,
            Some(first),
            Some((5, 5)),
            (5, 5),
            2,
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 3);

        // Fourth click with previous count 3 → wraps back to 1
        let third = second + Duration::from_millis(200);
        let count = compute_click_count(
            third,
            Some(second),
            Some((5, 5)),
            (5, 5),
            3,
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn click_count_resets_on_timeout() {
        let first = Instant::now();
        let second = first + Duration::from_millis(500); // beyond 400ms threshold
        let count = compute_click_count(
            second,
            Some(first),
            Some((5, 5)),
            (5, 5),
            1,
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn click_count_resets_on_distance() {
        let first = Instant::now();
        let second = first + Duration::from_millis(200);
        let count = compute_click_count(
            second,
            Some(first),
            Some((5, 5)),
            (10, 10), // far from previous
            1,
            Duration::from_millis(400),
            1,
        );
        assert_eq!(count, 1);
    }

    // ── resolve_click_link tests ───────────────────────────────────────

    fn url_at(start: usize, end: usize) -> DetectedUrl {
        DetectedUrl {
            url: "https://example.com".to_string(),
            start_col: start,
            end_col: end,
            line: 0,
            end_line: 0,
        }
    }

    fn path_at(start: usize, end: usize) -> DetectedPath {
        DetectedPath {
            path: "src/main.rs".to_string(),
            target_line: None,
            target_col: None,
            exists: true,
            start_col: start,
            end_col: end,
            line: 0,
            end_line: 0,
        }
    }

    #[test]
    fn click_link_picks_url() {
        let urls = vec![url_at(0, 5)];
        let paths = vec![];
        assert!(matches!(
            resolve_click_link(&urls, &paths, 2, 0),
            ClickLink::Url(_)
        ));
    }

    #[test]
    fn click_link_picks_path() {
        let urls = vec![];
        let paths = vec![path_at(0, 5)];
        assert!(matches!(
            resolve_click_link(&urls, &paths, 2, 0),
            ClickLink::Path(_)
        ));
    }

    #[test]
    fn click_link_url_takes_precedence_on_overlap() {
        // A URL and a path overlap the same cell → URL wins.
        let urls = vec![url_at(0, 10)];
        let paths = vec![path_at(0, 10)];
        assert!(matches!(
            resolve_click_link(&urls, &paths, 3, 0),
            ClickLink::Url(_)
        ));
    }

    #[test]
    fn click_link_none_when_empty() {
        assert!(matches!(
            resolve_click_link(&[], &[], 3, 0),
            ClickLink::None
        ));
    }

    // ── determine_click_target tests ───────────────────────────────────

    #[test]
    fn click_target_cmd_click_url() {
        let target = determine_click_target(
            MouseButton::Left,
            ElementState::Pressed,
            true, // cmd pressed
            false,
            true, // has grid position
        );
        assert!(matches!(target, MouseClickTarget::OpenUrl { .. }));
    }

    #[test]
    fn click_target_right_click_shows_menu() {
        let target = determine_click_target(
            MouseButton::Right,
            ElementState::Pressed,
            false,
            false, // no context menu visible
            true,
        );
        assert_eq!(target, MouseClickTarget::ShowContextMenu);
    }

    #[test]
    fn click_target_right_click_moves_menu_when_visible() {
        let target = determine_click_target(
            MouseButton::Right,
            ElementState::Pressed,
            false,
            true, // context menu visible
            true,
        );
        assert_eq!(target, MouseClickTarget::MoveContextMenu);
    }

    #[test]
    fn click_target_left_press_terminal() {
        let target =
            determine_click_target(MouseButton::Left, ElementState::Pressed, false, false, true);
        assert_eq!(
            target,
            MouseClickTarget::Terminal {
                button: MOUSE_BUTTON_LEFT
            }
        );
    }

    #[test]
    fn click_target_release() {
        let target = determine_click_target(
            MouseButton::Left,
            ElementState::Released,
            false,
            false,
            true,
        );
        assert_eq!(target, MouseClickTarget::Release);
    }

    // ── normalize_scroll_delta tests ───────────────────────────────────

    #[test]
    fn scroll_delta_line_passthrough() {
        let delta = MouseScrollDelta::LineDelta(0.0, 3.0);
        assert_eq!(normalize_scroll_delta(&delta, 16.0), 3.0);
    }

    #[test]
    fn scroll_delta_pixel_converts_to_lines() {
        let delta = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 32.0));
        let result = normalize_scroll_delta(&delta, 16.0);
        assert!((result - 2.0).abs() < 0.01);
    }

    #[test]
    fn scroll_delta_xy_reports_horizontal() {
        let delta = MouseScrollDelta::LineDelta(-2.0, 0.0);
        assert_eq!(normalize_scroll_delta_xy(&delta, 16.0), (-2.0, 0.0));
        let delta = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(48.0, 16.0));
        let (x, y) = normalize_scroll_delta_xy(&delta, 16.0);
        assert!((x - 3.0).abs() < 0.01);
        assert!((y - 1.0).abs() < 0.01);
    }

    // ── accumulate_scroll tests (B15 trackpad) ─────────────────────────

    #[test]
    fn accumulate_scroll_sums_sub_line_deltas() {
        // Four quarter-line ticks add up to one line.
        let (acc, lines) = accumulate_scroll(0.0, 0.25);
        assert_eq!(lines, 0);
        let (acc, lines) = accumulate_scroll(acc, 0.25);
        assert_eq!(lines, 0);
        let (acc, lines) = accumulate_scroll(acc, 0.25);
        assert_eq!(lines, 0);
        let (acc, lines) = accumulate_scroll(acc, 0.25);
        assert_eq!(lines, 1);
        assert!(acc.abs() < 1e-6);
    }

    #[test]
    fn accumulate_scroll_keeps_remainder_and_sign() {
        let (acc, lines) = accumulate_scroll(0.0, -2.5);
        assert_eq!(lines, -2);
        assert!((acc + 0.5).abs() < 1e-6);
        let (_, lines) = accumulate_scroll(acc, -0.5);
        assert_eq!(lines, -1);
    }

    #[test]
    fn accumulate_scroll_resets_on_direction_change() {
        let (acc, _) = accumulate_scroll(0.0, 0.9);
        // Reversing direction discards the pending 0.9 rather than netting.
        let (acc, lines) = accumulate_scroll(acc, -0.5);
        assert_eq!(lines, 0);
        assert!((acc + 0.5).abs() < 1e-6);
    }

    #[test]
    fn accumulate_scroll_whole_lines_pass_through() {
        assert_eq!(accumulate_scroll(0.0, 3.0), (0.0, 3));
        assert_eq!(accumulate_scroll(0.0, 0.0), (0.0, 0));
    }

    // ── screen_to_grid_position_clamped tests ─────────────────────────

    #[test]
    fn grid_position_clamped_snaps_outside_to_edges() {
        let layout = test_layout();
        // Left of / above the content area → (0, 0)
        assert_eq!(
            screen_to_grid_position_clamped(-50.0, -50.0, &layout),
            (0, 0)
        );
        assert_eq!(screen_to_grid_position_clamped(5.0, 25.0, &layout), (0, 0));
        // Far right / below → last cell
        assert_eq!(
            screen_to_grid_position_clamped(10000.0, 10000.0, &layout),
            (79, 23)
        );
        // Inside agrees with the non-clamped conversion
        assert_eq!(
            screen_to_grid_position_clamped(90.0, 120.0, &layout),
            (10, 5)
        );
    }
}
