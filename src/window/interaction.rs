//! Interaction state types.
//!
//! Groups state related to user interaction, mouse handling, search, and context menus.

use std::time::Instant;

use super::types::TabId;

/// Search match position in terminal
#[derive(Debug, Clone, Copy)]
pub struct SearchMatch {
    /// Line number (grid-relative: negative = history, 0+ = visible)
    pub line: i32,
    /// Starting column
    pub start_col: usize,
    /// Ending column (exclusive)
    pub end_col: usize,
}

/// Search state for find-in-terminal functionality
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// Whether search mode is active
    pub active: bool,
    /// Current search query
    pub query: String,
    /// All matches found
    pub matches: Vec<SearchMatch>,
    /// Index of current/focused match
    pub current_match: usize,
}

/// Context menu item
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuItem {
    Copy,
    Paste,
    SelectAll,
    Separator,
    /// Parent item that shows submenu on hover
    Themes,
    /// Individual theme (shown in submenu)
    Theme(String),
}

impl ContextMenuItem {
    /// Get the display label for this menu item
    pub fn label(&self) -> &str {
        match self {
            ContextMenuItem::Copy => "Copy",
            ContextMenuItem::Paste => "Paste",
            ContextMenuItem::SelectAll => "Select All",
            ContextMenuItem::Separator => "",
            ContextMenuItem::Themes => "Theme",
            ContextMenuItem::Theme(name) => name.as_str(),
        }
    }

    /// Get the keyboard shortcut hint (or arrow indicator for submenu)
    pub fn shortcut(&self) -> &'static str {
        #[cfg(target_os = "macos")]
        match self {
            ContextMenuItem::Copy => "Cmd+C",
            ContextMenuItem::Paste => "Cmd+V",
            ContextMenuItem::SelectAll => "Cmd+A",
            ContextMenuItem::Themes => "\u{25B6}", // Right-pointing triangle for submenu
            ContextMenuItem::Separator | ContextMenuItem::Theme(_) => "",
        }
        // The application chord on Linux/Windows is Ctrl+Shift, so plain
        // Ctrl chords (^C, ^A, ...) still reach the shell.
        #[cfg(not(target_os = "macos"))]
        match self {
            ContextMenuItem::Copy => "Ctrl+Shift+C",
            ContextMenuItem::Paste => "Ctrl+Shift+V",
            ContextMenuItem::SelectAll => "Ctrl+Shift+A",
            ContextMenuItem::Themes => "\u{25B6}", // Right-pointing triangle for submenu
            ContextMenuItem::Separator | ContextMenuItem::Theme(_) => "",
        }
    }

    /// Returns true if this is a submenu parent
    pub fn has_submenu(&self) -> bool {
        matches!(self, ContextMenuItem::Themes)
    }

    /// Returns true if this is a separator item
    pub fn is_separator(&self) -> bool {
        matches!(self, ContextMenuItem::Separator)
    }

    /// Returns true if this is a selectable (non-separator) item
    pub fn is_selectable(&self) -> bool {
        !self.is_separator()
    }

    /// Get the base edit menu items
    pub fn edit_items() -> Vec<ContextMenuItem> {
        vec![
            ContextMenuItem::Copy,
            ContextMenuItem::Paste,
            ContextMenuItem::SelectAll,
        ]
    }
}

/// Interaction state (cursor, mouse, selection, URLs)
///
/// Groups state related to user interaction and mouse handling.
#[derive(Default)]
pub struct InteractionState {
    /// Current cursor position in pixels
    pub cursor_position: (f32, f32),
    /// Last click time for double-click detection
    pub last_click_time: Option<Instant>,
    /// Last clicked tab for double-click detection
    pub last_click_tab: Option<TabId>,
    /// Whether mouse button is currently pressed
    pub mouse_pressed: bool,
    /// Click count for multi-click selection (1=single, 2=word, 3=line)
    pub selection_click_count: u8,
    /// Last selection click time for multi-click detection
    pub last_selection_click_time: Option<Instant>,
    /// Last selection click position (col, line) for multi-click detection
    pub last_selection_click_pos: Option<(usize, usize)>,
    /// Detected URLs in current viewport
    pub detected_urls: Vec<crate::input::DetectedUrl>,
    /// Index of currently hovered URL (for hover underline effect)
    pub hovered_url_index: Option<usize>,
    /// Detected (and existence-validated) file paths in current viewport
    pub detected_paths: Vec<crate::input::DetectedPath>,
    /// Index of currently hovered file path (for hover underline effect)
    pub hovered_path_index: Option<usize>,
    /// Caches path existence checks across frames (NFR-001)
    pub path_validator: crate::input::PathValidator,
}

// ---- Context menu layout constants (logical pixels; multiplied by scale) ----

/// Height of a selectable menu row.
pub const MENU_ITEM_HEIGHT: f32 = 24.0;
/// Height of a separator row.
pub const MENU_SEPARATOR_HEIGHT: f32 = 12.0;
/// Horizontal text inset.
pub const MENU_PADDING_X: f32 = 12.0;
/// Vertical inset above the first / below the last row.
pub const MENU_PADDING_Y: f32 = 6.0;
/// Main menu width.
pub const MENU_WIDTH: f32 = 160.0;
/// Theme submenu width (wider for theme names).
pub const SUBMENU_WIDTH: f32 = 200.0;
/// Border thickness.
pub const MENU_BORDER: f32 = 1.0;

/// Context menu metrics in physical pixels for a given scale factor.
///
/// Both hit testing and drawing derive every position from these, so the two
/// can never disagree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuMetrics {
    pub scale: f32,
    pub item_height: f32,
    pub separator_height: f32,
    pub padding_x: f32,
    pub padding_y: f32,
    pub menu_width: f32,
    pub submenu_width: f32,
    pub border: f32,
}

impl MenuMetrics {
    pub fn for_scale(scale: f32) -> Self {
        let scale = if scale > 0.0 { scale } else { 1.0 };
        Self {
            scale,
            item_height: MENU_ITEM_HEIGHT * scale,
            separator_height: MENU_SEPARATOR_HEIGHT * scale,
            padding_x: MENU_PADDING_X * scale,
            padding_y: MENU_PADDING_Y * scale,
            menu_width: MENU_WIDTH * scale,
            submenu_width: SUBMENU_WIDTH * scale,
            border: MENU_BORDER * scale,
        }
    }

    /// Row height for an item.
    fn row_height(&self, item: &ContextMenuItem) -> f32 {
        if item.is_separator() {
            self.separator_height
        } else {
            self.item_height
        }
    }

    /// Total menu height for a list of items.
    fn total_height(&self, items: &[ContextMenuItem]) -> f32 {
        self.padding_y * 2.0 + items.iter().map(|i| self.row_height(i)).sum::<f32>()
    }
}

/// Screen rectangle of one menu row (physical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MenuItemRect {
    /// Index into the menu's item list.
    pub index: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// True for separator rows (not selectable).
    pub is_separator: bool,
}

impl MenuItemRect {
    /// Half-open containment so adjacent rows never both claim a point.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    /// `(x, y, width, height)` of the row.
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        (self.x, self.y, self.width, self.height)
    }
}

/// Lay out `items` top-to-bottom starting at (`x`, `y`) with `width`.
fn layout_rows<'a>(
    items: &'a [ContextMenuItem],
    metrics: MenuMetrics,
    x: f32,
    y: f32,
    width: f32,
) -> impl Iterator<Item = MenuItemRect> + 'a {
    let mut next_y = y + metrics.padding_y;
    items.iter().enumerate().map(move |(index, item)| {
        let height = metrics.row_height(item);
        let rect = MenuItemRect {
            index,
            x,
            y: next_y,
            width,
            height,
            is_separator: item.is_separator(),
        };
        next_y += height;
        rect
    })
}

/// Context menu state
#[derive(Debug, Clone, Default)]
pub struct ContextMenu {
    /// Whether the context menu is visible
    pub visible: bool,
    /// Position of the menu (top-left corner)
    pub x: f32,
    pub y: f32,
    /// Currently hovered item index (from mouse)
    pub hovered_item: Option<usize>,
    /// Currently focused item index (from keyboard navigation)
    pub focused_item: Option<usize>,
    /// Menu dimensions (physical pixels; kept in sync with `menu_size()`)
    pub width: f32,
    pub height: f32,
    pub item_height: f32,
    /// Available themes for theme picker section. Set this via
    /// [`ContextMenu::set_themes`] (or assign it and call `show()`, which
    /// rebuilds the cached item lists).
    pub themes: Vec<String>,
    /// Currently active theme name
    pub current_theme: String,
    /// Whether the theme submenu is visible
    pub submenu_visible: bool,
    /// Submenu position (top-left corner)
    pub submenu_x: f32,
    pub submenu_y: f32,
    /// Submenu dimensions
    pub submenu_width: f32,
    pub submenu_height: f32,
    /// Hovered item in submenu
    pub submenu_hovered_item: Option<usize>,
    /// Keyboard-focused item in the submenu (`Some` means focus is inside it)
    pub submenu_focused_item: Option<usize>,
    /// Scale factor used for layout (0 = unset, treated as 1.0)
    pub scale: f32,
    /// Cached main-menu items (rebuilt by `show()` / `set_themes()`)
    items: Vec<ContextMenuItem>,
    /// Cached submenu items (rebuilt by `show()` / `set_themes()`)
    theme_items: Vec<ContextMenuItem>,
}

impl ContextMenu {
    // ---- Items ----

    /// Rebuild the cached item lists from `themes`.
    pub fn rebuild_items(&mut self) {
        self.items = ContextMenuItem::edit_items();
        if !self.themes.is_empty() {
            self.items.push(ContextMenuItem::Separator);
            self.items.push(ContextMenuItem::Themes);
        }
        self.theme_items = self
            .themes
            .iter()
            .map(|name| ContextMenuItem::Theme(name.clone()))
            .collect();
    }

    /// Replace the available themes and rebuild the cached items.
    pub fn set_themes(&mut self, themes: Vec<String>) {
        self.themes = themes;
        self.rebuild_items();
    }

    /// Set the scale factor used for layout.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        let m = self.metrics();
        self.item_height = m.item_height;
    }

    /// Layout metrics for the current scale.
    pub fn metrics(&self) -> MenuMetrics {
        MenuMetrics::for_scale(self.scale)
    }

    /// The main menu items (Themes shown as a parent item, not expanded)
    pub fn items(&self) -> &[ContextMenuItem] {
        &self.items
    }

    /// The theme submenu items
    pub fn theme_items(&self) -> &[ContextMenuItem] {
        &self.theme_items
    }

    // ---- Geometry ----

    /// (width, height) of the main menu in physical pixels.
    pub fn menu_size(&self) -> (f32, f32) {
        let m = self.metrics();
        (m.menu_width, m.total_height(&self.items))
    }

    /// (width, height) of the theme submenu in physical pixels.
    pub fn submenu_size(&self) -> (f32, f32) {
        let m = self.metrics();
        (m.submenu_width, m.total_height(&self.theme_items))
    }

    /// Row rectangles of the main menu, in order. Used by both hit testing
    /// and drawing so they always agree.
    pub fn item_rects(&self) -> impl Iterator<Item = MenuItemRect> + '_ {
        let m = self.metrics();
        layout_rows(&self.items, m, self.x, self.y, m.menu_width)
    }

    /// Row rectangles of the theme submenu, in order.
    pub fn submenu_item_rects(&self) -> impl Iterator<Item = MenuItemRect> + '_ {
        let m = self.metrics();
        layout_rows(
            &self.theme_items,
            m,
            self.submenu_x,
            self.submenu_y,
            m.submenu_width,
        )
    }

    // ---- Visibility ----

    /// Show the context menu at the given position
    pub fn show(&mut self, x: f32, y: f32) {
        self.rebuild_items();
        self.visible = true;
        self.x = x;
        self.y = y;
        let (w, h) = self.menu_size();
        self.width = w;
        self.height = h;
        self.item_height = self.metrics().item_height;
        self.hovered_item = None;
        self.focused_item = Some(0); // Focus first item for keyboard accessibility
        self.submenu_visible = false;
        self.submenu_hovered_item = None;
        self.submenu_focused_item = None;
    }

    /// Hide the context menu
    pub fn hide(&mut self) {
        self.visible = false;
        self.hovered_item = None;
        self.focused_item = None;
        self.submenu_visible = false;
        self.submenu_hovered_item = None;
        self.submenu_focused_item = None;
    }

    /// Recompute `submenu_visible`: shown while the hovered or focused item
    /// is Themes, while the pointer is inside the submenu, or while keyboard
    /// focus is inside it.
    fn sync_submenu_visibility(&mut self, pointer_in_submenu: bool) {
        let themes_idx = self.themes_item_index();
        let wanted = self.visible
            && themes_idx.is_some()
            && !self.theme_items.is_empty()
            && (self.hovered_item == themes_idx
                || self.focused_item == themes_idx
                || pointer_in_submenu
                || self.submenu_focused_item.is_some());
        self.submenu_visible = wanted;
        if !wanted {
            self.submenu_hovered_item = None;
            self.submenu_focused_item = None;
        }
    }

    // ---- Keyboard navigation ----

    fn step_focus(
        items: &[ContextMenuItem],
        current: Option<usize>,
        forward: bool,
    ) -> Option<usize> {
        let count = items.len();
        if count == 0 {
            return None;
        }
        let start = current.unwrap_or(if forward { count - 1 } else { 0 });
        for i in 1..=count {
            let idx = if forward {
                (start + i) % count
            } else {
                (start + count - i) % count
            };
            if items[idx].is_selectable() {
                return Some(idx);
            }
        }
        None
    }

    /// Move focus to the next selectable item (wraps around, skips separators).
    /// Operates inside the submenu when focus is there.
    pub fn focus_next(&mut self) {
        if !self.visible {
            return;
        }
        if let Some(cur) = self.submenu_focused_item {
            self.submenu_focused_item = Self::step_focus(&self.theme_items, Some(cur), true);
        } else if let Some(idx) = Self::step_focus(&self.items, self.focused_item, true) {
            self.focused_item = Some(idx);
        }
        // Clear hover when using keyboard
        self.hovered_item = None;
        self.submenu_hovered_item = None;
        self.sync_submenu_visibility(false);
    }

    /// Move focus to the previous selectable item (wraps around, skips separators).
    /// Operates inside the submenu when focus is there.
    pub fn focus_prev(&mut self) {
        if !self.visible {
            return;
        }
        if let Some(cur) = self.submenu_focused_item {
            self.submenu_focused_item = Self::step_focus(&self.theme_items, Some(cur), false);
        } else if let Some(idx) = Self::step_focus(&self.items, self.focused_item, false) {
            self.focused_item = Some(idx);
        }
        // Clear hover when using keyboard
        self.hovered_item = None;
        self.submenu_hovered_item = None;
        self.sync_submenu_visibility(false);
    }

    /// True while keyboard focus is inside the theme submenu.
    pub fn is_submenu_focused(&self) -> bool {
        self.submenu_focused_item.is_some()
    }

    /// Right arrow / Enter on the Themes item: move keyboard focus into the
    /// submenu (on the current theme if present). Returns true if handled.
    // TODO(keyboard.rs): wire to ArrowRight/ArrowLeft/Enter; unused until then.
    #[allow(dead_code)]
    pub fn enter_submenu(&mut self) -> bool {
        if !self.visible || self.submenu_focused_item.is_some() {
            return false;
        }
        let themes_idx = self.themes_item_index();
        if themes_idx.is_none() || self.focused_item != themes_idx || self.theme_items.is_empty() {
            return false;
        }
        let current = self
            .themes
            .iter()
            .position(|name| *name == self.current_theme)
            .unwrap_or(0);
        self.submenu_focused_item = Some(current);
        self.hovered_item = None;
        self.submenu_hovered_item = None;
        self.sync_submenu_visibility(false);
        true
    }

    /// Left arrow / Escape while inside the submenu: move focus back to the
    /// Themes item. Returns true if handled (false means "not in submenu";
    /// callers typically hide the menu on Escape in that case).
    // TODO(keyboard.rs): wire to ArrowRight/ArrowLeft/Enter; unused until then.
    #[allow(dead_code)]
    pub fn leave_submenu(&mut self) -> bool {
        if self.submenu_focused_item.is_none() {
            return false;
        }
        self.submenu_focused_item = None;
        self.submenu_hovered_item = None;
        self.focused_item = self.themes_item_index();
        self.sync_submenu_visibility(false);
        true
    }

    /// Enter key: returns the item to activate, or `None` after moving focus
    /// into the submenu (Enter on the Themes item opens it instead of
    /// activating it).
    // TODO(keyboard.rs): wire to ArrowRight/ArrowLeft/Enter; unused until then.
    #[allow(dead_code)]
    pub fn activate_focused(&mut self) -> Option<ContextMenuItem> {
        if self.enter_submenu() {
            return None;
        }
        self.get_focused_item()
    }

    /// Get the currently focused item (a theme when focus is in the submenu)
    pub fn get_focused_item(&self) -> Option<ContextMenuItem> {
        if let Some(idx) = self.submenu_focused_item {
            return self.theme_items.get(idx).cloned();
        }
        self.focused_item
            .and_then(|idx| self.items.get(idx).cloned())
    }

    // ---- Hit testing ----

    /// Check if a point is inside the main menu
    pub fn contains(&self, x: f32, y: f32) -> bool {
        if !self.visible {
            return false;
        }
        let (w, h) = self.menu_size();
        x >= self.x && x <= self.x + w && y >= self.y && y <= self.y + h
    }

    /// Whether a point is on the menu or its open submenu, including the
    /// padding and border around the rows where `item_at` finds nothing.
    /// A click there belongs to the menu, not to whatever is underneath.
    pub fn covers(&self, x: f32, y: f32) -> bool {
        self.contains(x, y) || self.contains_submenu(x, y)
    }

    /// Check if a point is inside the submenu
    pub fn contains_submenu(&self, x: f32, y: f32) -> bool {
        if !self.submenu_visible {
            return false;
        }
        let (w, h) = self.submenu_size();
        x >= self.submenu_x
            && x <= self.submenu_x + w
            && y >= self.submenu_y
            && y <= self.submenu_y + h
    }

    /// Index of the main-menu row (including separators) at the position.
    pub fn item_index_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains(x, y) {
            return None;
        }
        self.item_rects()
            .find(|r| r.contains(x, y))
            .map(|r| r.index)
    }

    /// Index of the submenu row at the position.
    pub fn submenu_item_index_at(&self, x: f32, y: f32) -> Option<usize> {
        if !self.contains_submenu(x, y) {
            return None;
        }
        self.submenu_item_rects()
            .find(|r| r.contains(x, y))
            .map(|r| r.index)
    }

    /// Get the menu item at the given position (main menu only).
    /// Separator rows return `Some(Separator)`; callers should treat that as
    /// a no-op click that keeps the menu open.
    pub fn item_at(&self, x: f32, y: f32) -> Option<ContextMenuItem> {
        self.item_index_at(x, y)
            .and_then(|idx| self.items.get(idx).cloned())
    }

    /// Get the submenu item at the given position
    pub fn submenu_item_at(&self, x: f32, y: f32) -> Option<ContextMenuItem> {
        self.submenu_item_index_at(x, y)
            .and_then(|idx| self.theme_items.get(idx).cloned())
    }

    /// Check if a theme name is the currently active theme
    pub fn is_current_theme(&self, name: &str) -> bool {
        self.current_theme == name
    }

    /// Update hover state based on mouse position
    pub fn update_hover(&mut self, x: f32, y: f32) {
        if !self.visible {
            self.hovered_item = None;
            self.submenu_hovered_item = None;
            return;
        }

        let in_submenu = self.contains_submenu(x, y);
        if in_submenu {
            self.submenu_hovered_item = self.submenu_item_index_at(x, y);
            // Keep the Themes item highlighted while browsing its submenu
            self.hovered_item = self.themes_item_index();
        } else {
            self.submenu_hovered_item = None;
            self.hovered_item = self
                .item_index_at(x, y)
                .filter(|&idx| self.items[idx].is_selectable());
        }
        self.sync_submenu_visibility(in_submenu);
    }

    /// Get the index of the Themes item in the main menu
    pub fn themes_item_index(&self) -> Option<usize> {
        self.items
            .iter()
            .position(|item| matches!(item, ContextMenuItem::Themes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu_with_themes(scale: f32) -> ContextMenu {
        let mut menu = ContextMenu {
            themes: vec!["alpha".into(), "beta".into(), "gamma".into()],
            current_theme: "beta".into(),
            ..Default::default()
        };
        menu.set_scale(scale);
        menu.show(100.0, 50.0);
        menu
    }

    #[test]
    fn items_are_cached_and_borrowed() {
        let menu = menu_with_themes(1.0);
        assert_eq!(menu.items().len(), 5); // Copy, Paste, SelectAll, Sep, Themes
        assert_eq!(menu.theme_items().len(), 3);
        assert_eq!(menu.items()[4].label(), "Theme");
        assert_eq!(menu.theme_items()[0].label(), "alpha");
        assert_eq!(menu.themes_item_index(), Some(4));

        let mut no_themes = ContextMenu::default();
        no_themes.show(0.0, 0.0);
        assert_eq!(no_themes.items().len(), 3);
        assert_eq!(no_themes.themes_item_index(), None);
    }

    /// Regression: the padded layout left a frame around the rows that is
    /// inside the menu but hits no item. Clicks there were treated as
    /// "outside", closing the menu and starting a selection underneath.
    #[test]
    fn menu_padding_is_covered_but_hits_no_item() {
        for scale in [1.0, 2.0] {
            let menu = menu_with_themes(scale);
            let m = menu.metrics();
            assert!(m.padding_y > 0.0);
            let (width, height) = menu.menu_size();
            let x = menu.x + width / 2.0;
            for y in [
                menu.y + m.padding_y / 2.0,
                menu.y + height - m.padding_y / 2.0,
            ] {
                assert!(menu.item_at(x, y).is_none(), "padding hit an item");
                assert!(menu.covers(x, y), "padding not covered at scale {scale}");
            }
            assert!(!menu.covers(menu.x - 1.0, menu.y));
            assert!(!menu.covers(x, menu.y + height + 1.0));
        }
        assert!(!ContextMenu::default().covers(0.0, 0.0), "hidden menu");
    }

    #[test]
    fn layout_and_hit_test_round_trip_for_every_row() {
        for scale in [1.0, 1.5, 2.0] {
            let mut menu = menu_with_themes(scale);
            let m = menu.metrics();
            let rects: Vec<MenuItemRect> = menu.item_rects().collect();
            assert_eq!(rects.len(), menu.items().len());

            // Rows start after the top padding and tile without gaps.
            assert_eq!(rects[0].y, menu.y + m.padding_y);
            for w in rects.windows(2) {
                assert_eq!(w[0].y + w[0].height, w[1].y);
            }
            let (_, height) = menu.menu_size();
            let last = rects.last().unwrap();
            assert!((last.y + last.height + m.padding_y - (menu.y + height)).abs() < 1e-3);

            for rect in &rects {
                let expect_sep = menu.items()[rect.index].is_separator();
                assert_eq!(rect.is_separator, expect_sep);
                assert_eq!(
                    rect.height,
                    if expect_sep {
                        m.separator_height
                    } else {
                        m.item_height
                    }
                );

                // Center, top edge and bottom edge (exclusive) all map back.
                let cx = rect.x + rect.width / 2.0;
                assert_eq!(
                    menu.item_index_at(cx, rect.y + rect.height / 2.0),
                    Some(rect.index)
                );
                assert_eq!(menu.item_index_at(cx, rect.y), Some(rect.index));
                assert_eq!(
                    menu.item_index_at(cx, rect.y + rect.height - 0.01),
                    Some(rect.index)
                );
                // Just above the row belongs to the previous row (or the padding).
                let above = menu.item_index_at(cx, rect.y - 0.01);
                if rect.index == 0 {
                    assert_eq!(above, None);
                } else {
                    assert_eq!(above, Some(rect.index - 1));
                }
                // item_at agrees with the item list.
                assert_eq!(
                    menu.item_at(cx, rect.y + 1.0).as_ref(),
                    Some(&menu.items()[rect.index])
                );
            }

            // The padding above the first row and below the last hits nothing.
            assert_eq!(menu.item_index_at(menu.x + 5.0, menu.y + 1.0), None);
            assert_eq!(
                menu.item_index_at(menu.x + 5.0, menu.y + height - 1.0),
                None
            );
            assert!(menu.contains(menu.x + 5.0, menu.y + 1.0));

            // Submenu round trip.
            menu.submenu_x = menu.x + m.menu_width;
            menu.submenu_y = rects[4].y;
            menu.submenu_visible = true;
            let sub: Vec<MenuItemRect> = menu.submenu_item_rects().collect();
            assert_eq!(sub.len(), 3);
            for rect in &sub {
                let cx = rect.x + rect.width / 2.0;
                let cy = rect.y + rect.height / 2.0;
                assert_eq!(menu.submenu_item_index_at(cx, cy), Some(rect.index));
                assert_eq!(
                    menu.submenu_item_at(cx, cy),
                    Some(ContextMenuItem::Theme(menu.themes[rect.index].clone()))
                );
                assert_eq!(menu.submenu_item_index_at(cx, rect.y), Some(rect.index));
            }
            let (_, sub_h) = menu.submenu_size();
            assert_eq!(sub_h, m.padding_y * 2.0 + 3.0 * m.item_height);
        }
    }

    #[test]
    fn separator_hover_does_not_highlight() {
        let mut menu = menu_with_themes(2.0);
        let sep = menu.item_rects().find(|r| r.is_separator).unwrap();
        menu.update_hover(sep.x + 5.0, sep.y + sep.height / 2.0);
        assert_eq!(menu.hovered_item, None);
        assert!(!menu.submenu_visible);
        // But the click still lands inside the menu (kept open by the caller).
        assert_eq!(
            menu.item_at(sep.x + 5.0, sep.y + 1.0),
            Some(ContextMenuItem::Separator)
        );
    }

    #[test]
    fn submenu_follows_hover_rules() {
        let mut menu = menu_with_themes(1.0);
        let m = menu.metrics();
        let rects: Vec<MenuItemRect> = menu.item_rects().collect();
        let themes = rects[4];
        let copy = rects[0];

        // Hover Themes: submenu opens.
        menu.update_hover(themes.x + 5.0, themes.y + 5.0);
        assert_eq!(menu.hovered_item, Some(4));
        assert!(menu.submenu_visible);

        // Renderer positions the submenu next to the Themes row.
        menu.submenu_x = menu.x + m.menu_width;
        menu.submenu_y = themes.y;

        // Move into the submenu: stays open, Themes stays highlighted.
        let sub: Vec<MenuItemRect> = menu.submenu_item_rects().collect();
        menu.update_hover(sub[1].x + 5.0, sub[1].y + 5.0);
        assert!(menu.submenu_visible);
        assert_eq!(menu.submenu_hovered_item, Some(1));
        assert_eq!(menu.hovered_item, Some(4));

        // Hover Copy: submenu closes.
        menu.update_hover(copy.x + 5.0, copy.y + 5.0);
        assert_eq!(menu.hovered_item, Some(0));
        assert!(!menu.submenu_visible);
        assert_eq!(menu.submenu_hovered_item, None);

        // Leave the menu entirely: nothing hovered, submenu stays closed.
        menu.update_hover(-10.0, -10.0);
        assert_eq!(menu.hovered_item, None);
        assert!(!menu.submenu_visible);
    }

    #[test]
    fn keyboard_navigation_reaches_submenu() {
        let mut menu = menu_with_themes(1.0);
        assert_eq!(menu.focused_item, Some(0));

        // Down x3 skips the separator and lands on Themes.
        menu.focus_next();
        menu.focus_next();
        menu.focus_next();
        assert_eq!(menu.focused_item, Some(4));
        assert!(menu.submenu_visible, "focused Themes shows the submenu");
        assert!(!menu.is_submenu_focused());

        // Enter on Themes moves focus in (on the current theme) without activating.
        assert_eq!(menu.activate_focused(), None);
        assert!(menu.is_submenu_focused());
        assert_eq!(menu.submenu_focused_item, Some(1)); // "beta" is current
        assert_eq!(
            menu.get_focused_item(),
            Some(ContextMenuItem::Theme("beta".into()))
        );

        // Up/Down move within the submenu and wrap.
        menu.focus_next();
        assert_eq!(menu.submenu_focused_item, Some(2));
        menu.focus_next();
        assert_eq!(menu.submenu_focused_item, Some(0));
        menu.focus_prev();
        assert_eq!(menu.submenu_focused_item, Some(2));
        assert!(menu.submenu_visible);

        // Enter inside the submenu selects the theme.
        assert_eq!(
            menu.activate_focused(),
            Some(ContextMenuItem::Theme("gamma".into()))
        );

        // Left leaves the submenu back to Themes; Left again is unhandled.
        assert!(menu.leave_submenu());
        assert!(!menu.is_submenu_focused());
        assert_eq!(menu.focused_item, Some(4));
        assert!(!menu.leave_submenu());

        // Right enters again; Right on a non-Themes item does nothing.
        assert!(menu.enter_submenu());
        assert!(menu.leave_submenu());
        menu.focus_next(); // wraps to Copy
        assert_eq!(menu.focused_item, Some(0));
        assert!(!menu.submenu_visible);
        assert!(!menu.enter_submenu());
        assert_eq!(menu.activate_focused(), Some(ContextMenuItem::Copy));

        // Hiding clears everything.
        menu.hide();
        assert!(!menu.visible);
        assert_eq!(menu.submenu_focused_item, None);
        assert_eq!(menu.focused_item, None);
    }
}
