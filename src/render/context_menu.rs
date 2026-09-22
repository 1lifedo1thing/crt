//! Context menu rendering
//!
//! Renders the right-click context menu with nested theme submenu.
//!
//! All row positions come from `ContextMenu::item_rects()` /
//! `submenu_item_rects()` — the same functions hit testing uses — so what is
//! drawn is exactly what a click selects.

use crate::gpu::SharedGpuState;
use crate::window::{ContextMenuItem, WindowState};
use crt_renderer::{GlyphCache, GridRenderer, PositionedGlyph};

/// Render context menu overlay
pub fn render(
    state: &mut WindowState,
    shared: &SharedGpuState,
    encoder: &mut wgpu::CommandEncoder,
    frame_view: &wgpu::TextureView,
) {
    let scale = state.scale_factor;
    let screen_width = state.gpu.config.width as f32;
    let screen_height = state.gpu.config.height as f32;

    // ---- Layout (mutates menu position/size; hit testing reads the same) ----
    {
        let menu = &mut state.ui.context_menu;
        menu.set_scale(scale);
        if menu.items().is_empty() {
            menu.rebuild_items();
        }
        let m = menu.metrics();
        let (menu_width, menu_height) = menu.menu_size();

        // Keep menu within screen bounds
        let mut menu_x = menu.x;
        let mut menu_y = menu.y;
        if menu_x + menu_width > screen_width {
            menu_x = screen_width - menu_width - 4.0;
        }
        if menu_x < 4.0 {
            menu_x = 4.0;
        }
        if menu_height > screen_height - 8.0 {
            menu_y = 4.0;
        } else if menu_y + menu_height > screen_height - 4.0 {
            menu_y = screen_height - menu_height - 4.0;
        }
        if menu_y < 4.0 {
            menu_y = 4.0;
        }
        menu.x = menu_x;
        menu.y = menu_y;
        menu.width = menu_width;
        menu.height = menu_height;
        menu.item_height = m.item_height;

        // Submenu: to the right of the main menu, aligned with the Themes row.
        let (submenu_width, submenu_height) = menu.submenu_size();
        let themes_row_y = menu
            .themes_item_index()
            .and_then(|idx| menu.item_rects().nth(idx))
            .map(|r| r.y)
            .unwrap_or(menu_y);
        let mut submenu_x = menu_x + menu_width - m.border;
        let mut submenu_y = themes_row_y - m.padding_y;
        if submenu_x + submenu_width > screen_width - 4.0 {
            // Show submenu on the left side instead
            submenu_x = menu_x - submenu_width + m.border;
        }
        if submenu_x < 4.0 {
            submenu_x = 4.0;
        }
        if submenu_y + submenu_height > screen_height - 4.0 {
            submenu_y = screen_height - submenu_height - 4.0;
        }
        if submenu_y < 4.0 {
            submenu_y = 4.0;
        }
        menu.submenu_x = submenu_x;
        menu.submenu_y = submenu_y;
        menu.submenu_width = submenu_width;
        menu.submenu_height = submenu_height;
    }

    let menu = &state.ui.context_menu;
    let m = menu.metrics();
    let items = menu.items();
    let theme_items = menu.theme_items();
    let show_submenu = menu.submenu_visible && !theme_items.is_empty();
    let (menu_x, menu_y, menu_width, menu_height) = (menu.x, menu.y, menu.width, menu.height);
    let (submenu_x, submenu_y, submenu_width, submenu_height) = (
        menu.submenu_x,
        menu.submenu_y,
        menu.submenu_width,
        menu.submenu_height,
    );
    let border_thickness = m.border;

    // Colors from theme
    let ui_style = &state.gpu.effect_pipeline.theme().ui;
    let bg_color = ui_style.context_menu.background.to_array();
    let border_color = ui_style.context_menu.border_color.to_array();
    let hover_color = ui_style.hover.background.to_array();
    let focus_color = ui_style.focus.ring_color.to_array();
    let text_color = ui_style.context_menu.text_color.to_array();
    let shortcut_color = ui_style.context_menu.shortcut_color.to_array();

    // ---- Shapes ----
    let rects = &mut state.gpu.rect_renderer;
    rects.clear();
    rects.update_screen_size(&shared.queue, screen_width, screen_height);

    // Menu background + border
    rects.push_rect(menu_x, menu_y, menu_width, menu_height, bg_color);
    push_menu_border(
        rects,
        menu_x,
        menu_y,
        menu_width,
        menu_height,
        border_thickness,
        border_color,
    );

    if show_submenu {
        rects.push_rect(
            submenu_x,
            submenu_y,
            submenu_width,
            submenu_height,
            bg_color,
        );
        push_menu_border(
            rects,
            submenu_x,
            submenu_y,
            submenu_width,
            submenu_height,
            border_thickness,
            border_color,
        );

        // Submenu hover highlight / focus ring
        for rect in menu.submenu_item_rects() {
            if menu.submenu_hovered_item == Some(rect.index) {
                push_row_highlight(rects, rect.bounds(), border_thickness, hover_color);
            }
            if menu.submenu_focused_item == Some(rect.index) {
                push_focus_ring(rects, rect.bounds(), border_thickness, scale, focus_color);
            }
        }
    }

    for rect in menu.item_rects() {
        if rect.is_separator {
            // Separator: hairline centred in its row
            let sep_y = rect.y + rect.height / 2.0;
            let sep_inset = m.padding_x / 2.0;
            rects.push_rect(
                rect.x + sep_inset,
                sep_y,
                rect.width - sep_inset * 2.0,
                1.0 * scale,
                border_color,
            );
            continue;
        }
        if menu.hovered_item == Some(rect.index) {
            push_row_highlight(rects, rect.bounds(), border_thickness, hover_color);
        }
        // Keyboard focus ring (only on the main menu while focus is there)
        if menu.focused_item == Some(rect.index) && !menu.is_submenu_focused() {
            push_focus_ring(rects, rect.bounds(), border_thickness, scale, focus_color);
        }
    }

    // Render background pass
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Context Menu Background Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        state
            .gpu
            .rect_renderer
            .render_transient(&shared.queue, &mut pass, &mut state.gpu.arena);
    }

    // ---- Text ----
    let glyphs = &mut state.gpu.tab_glyph_cache;
    let text = &mut state.gpu.overlay_text_renderer;
    text.clear();

    let cell_width = glyphs.cell_width();
    let text_offset_y = (m.item_height - glyphs.line_height()) / 2.0;
    let mut glyph_buffer: Vec<PositionedGlyph> = Vec::with_capacity(32);

    // Main menu items: label left-aligned, shortcut/arrow right-aligned
    for rect in menu.item_rects() {
        let item = &items[rect.index];
        if item.is_separator() {
            continue;
        }
        let item_y = rect.y + text_offset_y;
        push_text(
            glyphs,
            text,
            &mut glyph_buffer,
            item.label(),
            rect.x + m.padding_x,
            item_y,
            text_color,
        );

        let shortcut = item.shortcut();
        if !shortcut.is_empty() {
            let shortcut_width = shortcut.chars().count() as f32 * cell_width;
            let shortcut_x = rect.x + rect.width - m.padding_x - shortcut_width;
            push_text(
                glyphs,
                text,
                &mut glyph_buffer,
                shortcut,
                shortcut_x,
                item_y,
                shortcut_color,
            );
        }
    }

    // Submenu items: optional checkmark, then the theme name (indented so
    // checked and unchecked names align)
    if show_submenu {
        let label_indent = m.padding_x + 2.0 * cell_width;
        for rect in menu.submenu_item_rects() {
            let item = &theme_items[rect.index];
            let item_y = rect.y + text_offset_y;
            if let ContextMenuItem::Theme(name) = item
                && menu.is_current_theme(name)
            {
                push_text(
                    glyphs,
                    text,
                    &mut glyph_buffer,
                    "\u{2713}",
                    rect.x + m.padding_x,
                    item_y,
                    text_color,
                );
            }
            push_text(
                glyphs,
                text,
                &mut glyph_buffer,
                item.label(),
                rect.x + label_indent,
                item_y,
                text_color,
            );
        }
    }

    glyphs.flush(&shared.queue);

    // Render text pass
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Context Menu Text Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        state.gpu.overlay_text_renderer.render_transient(
            &shared.queue,
            &mut pass,
            &mut state.gpu.arena,
        );
    }
}

/// Position `text` one cell per char starting at (`x`, `y`) and push it.
fn push_text(
    glyphs: &mut GlyphCache,
    renderer: &mut GridRenderer,
    buffer: &mut Vec<PositionedGlyph>,
    text: &str,
    x: f32,
    y: f32,
    color: [f32; 4],
) {
    buffer.clear();
    let cell_width = glyphs.cell_width();
    let mut char_x = x;
    for c in text.chars() {
        if let Some(glyph) = glyphs.position_char(c, char_x, y) {
            buffer.push(glyph);
        }
        char_x += cell_width;
    }
    renderer.push_glyphs(buffer, color);
}

/// A row rectangle as `(x, y, width, height)`.
type Row = (f32, f32, f32, f32);

/// Hover highlight filling a row, inset by the border.
fn push_row_highlight(
    renderer: &mut crt_renderer::RectRenderer,
    (x, y, width, height): Row,
    border: f32,
    color: [f32; 4],
) {
    renderer.push_rect(x + border, y, width - border * 2.0, height, color);
}

/// Keyboard focus ring drawn as four bars just inside a row.
fn push_focus_ring(
    renderer: &mut crt_renderer::RectRenderer,
    (x, y, width, height): Row,
    border: f32,
    scale: f32,
    color: [f32; 4],
) {
    let focus_border = 2.0 * scale;
    let inset = border + 2.0 * scale;
    let ring_x = x + inset;
    let ring_w = width - inset * 2.0;
    renderer.push_rect(ring_x, y, ring_w, focus_border, color);
    renderer.push_rect(
        ring_x,
        y + height - focus_border,
        ring_w,
        focus_border,
        color,
    );
    renderer.push_rect(ring_x, y, focus_border, height, color);
    renderer.push_rect(
        x + width - inset - focus_border,
        y,
        focus_border,
        height,
        color,
    );
}

/// Helper to push menu border rectangles
fn push_menu_border(
    renderer: &mut crt_renderer::RectRenderer,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    thickness: f32,
    color: [f32; 4],
) {
    // Top
    renderer.push_rect(x, y, width, thickness, color);
    // Bottom
    renderer.push_rect(x, y + height - thickness, width, thickness, color);
    // Left
    renderer.push_rect(x, y, thickness, height, color);
    // Right
    renderer.push_rect(x + width - thickness, y, thickness, height, color);
}
