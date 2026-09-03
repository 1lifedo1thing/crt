//! Dialog rendering
//!
//! Renders input dialog overlays: search bar and window rename.

use crate::gpu::SharedGpuState;
use crate::window::WindowState;

/// Render search bar overlay
pub fn render_search_bar(
    state: &mut WindowState,
    shared: &mut SharedGpuState,
    encoder: &mut wgpu::CommandEncoder,
    frame_view: &wgpu::TextureView,
) {
    // content_offset() is already in physical pixels
    let (_, content_offset_y) = state.gpu.tab_bar.content_offset();
    let s = state.scale_factor;

    // Theme colors for search bar
    let ui_style = &state.gpu.effect_pipeline.theme().ui;
    let bg_color = ui_style.search_bar.background.to_array();
    let focus_glow_color = ui_style.focus.glow_color.to_array();
    let focus_border_color = ui_style.focus.ring_color.to_array();

    // Calculate search bar dimensions (same as vello version)
    let bar_width = 300.0 * s;
    let bar_height = 32.0 * s;
    let margin = 20.0 * s;
    let padding = 8.0 * s;
    let border_width = ui_style.focus.ring_thickness * s;
    let glow_size = ui_style.focus.glow_size * s;

    let bar_x = state.gpu.config.width as f32 - bar_width - margin;
    let bar_y = content_offset_y + margin;

    // Render search bar using RectRenderer (direct, no intermediate texture)
    state.gpu.rect_renderer.clear();
    state.gpu.rect_renderer.update_screen_size(
        &shared.queue,
        state.gpu.config.width as f32,
        state.gpu.config.height as f32,
    );

    // Outer glow (focus indicator) - slightly larger than the bar
    state.gpu.rect_renderer.push_rect(
        bar_x - glow_size,
        bar_y - glow_size,
        bar_width + glow_size * 2.0,
        bar_height + glow_size * 2.0,
        focus_glow_color,
    );

    // Focus border rect (bright blue)
    state
        .gpu
        .rect_renderer
        .push_rect(bar_x, bar_y, bar_width, bar_height, focus_border_color);

    // Inner background rect
    state.gpu.rect_renderer.push_rect(
        bar_x + border_width,
        bar_y + border_width,
        bar_width - border_width * 2.0,
        bar_height - border_width * 2.0,
        bg_color,
    );

    // Render search bar background directly to frame
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Search Bar Background Pass"),
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

    // Calculate text position
    let text_x = bar_x + border_width + padding;
    let text_y = bar_y + border_width + padding;
    let text_height = bar_height - border_width * 2.0 - padding * 2.0;

    // Render search text using tab glyph cache
    state.gpu.tab_title_renderer.clear();

    // Build display text: query with cursor + match count, truncated (from the
    // left, so the cursor end stays visible) to what fits inside the box.
    let cell_width = state.gpu.tab_glyph_cache.cell_width();
    let max_chars = max_chars_in_box(bar_width, border_width + padding, cell_width);
    let query = &state.ui.search.query;
    let match_count = state.ui.search.matches.len();
    let current_match = state.ui.search.current_match + 1; // 1-indexed for display

    let display_text = if query.is_empty() {
        fit_prefix("Find...", max_chars)
    } else {
        let suffix = if match_count > 0 {
            format!("| ({}/{})", current_match, match_count)
        } else {
            "| (no matches)".to_string()
        };
        fit_input_line("", query, &suffix, max_chars)
    };

    // Render text - get fresh reference to ui_style for text colors
    let ui_style = &state.gpu.effect_pipeline.theme().ui;
    let text_color = if query.is_empty() {
        ui_style.search_bar.placeholder_color.to_array()
    } else if match_count > 0 {
        ui_style.search_bar.text_color.to_array()
    } else {
        ui_style.search_bar.no_match_color.to_array()
    };

    let mut glyphs = Vec::new();
    let mut char_x = text_x;
    // Centre on the real glyph line height (the tab glyph cache is 12px * scale
    // with a 1.3 line height, not 14px).
    let font_height = state.gpu.tab_glyph_cache.line_height();
    let text_baseline_y = text_y + (text_height - font_height) / 2.0;

    for c in display_text.chars() {
        if let Some(glyph) = state
            .gpu
            .tab_glyph_cache
            .position_char(c, char_x, text_baseline_y)
        {
            glyphs.push(glyph);
        }
        char_x += state.gpu.tab_glyph_cache.cell_width();
    }

    state
        .gpu
        .tab_title_renderer
        .push_glyphs(&glyphs, text_color);
    state.gpu.tab_glyph_cache.flush(&shared.queue);

    // Render text pass
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Search Bar Text Render Pass"),
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
        .tab_title_renderer
        .render_transient(&shared.queue, &mut pass, &mut state.gpu.arena);
}

/// Render window rename input bar overlay
pub fn render_window_rename(
    state: &mut WindowState,
    shared: &mut SharedGpuState,
    encoder: &mut wgpu::CommandEncoder,
    frame_view: &wgpu::TextureView,
) {
    // content_offset() is already in physical pixels
    let (_, content_offset_y) = state.gpu.tab_bar.content_offset();
    let s = state.scale_factor;

    // Theme colors for rename bar
    let ui_style = &state.gpu.effect_pipeline.theme().ui;
    let bg_color = ui_style.rename_bar.background.to_array();
    let focus_glow_color = ui_style.focus.glow_color.to_array();
    let focus_border_color = ui_style.focus.ring_color.to_array();

    // Calculate rename bar dimensions (wider than search bar, centered)
    let bar_width = 400.0 * s;
    let bar_height = 36.0 * s;
    let margin = 40.0 * s;
    let padding = 10.0 * s;
    let border_width = ui_style.focus.ring_thickness * s;
    let glow_size = ui_style.focus.glow_size * s;

    // Center horizontally
    let bar_x = (state.gpu.config.width as f32 - bar_width) / 2.0;
    let bar_y = content_offset_y + margin;

    // Render rename bar using RectRenderer
    state.gpu.rect_renderer.clear();
    state.gpu.rect_renderer.update_screen_size(
        &shared.queue,
        state.gpu.config.width as f32,
        state.gpu.config.height as f32,
    );

    // Outer glow (focus indicator)
    state.gpu.rect_renderer.push_rect(
        bar_x - glow_size,
        bar_y - glow_size,
        bar_width + glow_size * 2.0,
        bar_height + glow_size * 2.0,
        focus_glow_color,
    );

    // Focus border rect (bright blue)
    state
        .gpu
        .rect_renderer
        .push_rect(bar_x, bar_y, bar_width, bar_height, focus_border_color);

    // Inner background rect
    state.gpu.rect_renderer.push_rect(
        bar_x + border_width,
        bar_y + border_width,
        bar_width - border_width * 2.0,
        bar_height - border_width * 2.0,
        bg_color,
    );

    // Render rename bar background directly to frame
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Window Rename Bar Background Pass"),
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

    // Calculate text position
    let text_x = bar_x + border_width + padding;
    let text_y = bar_y + border_width + padding;
    let text_height = bar_height - border_width * 2.0 - padding * 2.0;

    // Render rename text using tab glyph cache
    state.gpu.tab_title_renderer.clear();

    // Build display text: "Rename: " + input + cursor, truncated so it fits.
    let cell_width = state.gpu.tab_glyph_cache.cell_width();
    let max_chars = max_chars_in_box(bar_width, border_width + padding, cell_width);
    let input = &state.ui.window_rename.input;
    let display_text = fit_input_line("Rename: ", input, "|", max_chars);

    // Render text - get fresh reference to ui_style for text colors
    let ui_style = &state.gpu.effect_pipeline.theme().ui;
    let label_color = ui_style.rename_bar.label_color.to_array();
    let input_color = ui_style.rename_bar.text_color.to_array();

    let mut glyphs = Vec::new();
    let mut char_x = text_x;
    // Centre on the real glyph line height (the tab glyph cache is 12px * scale
    // with a 1.3 line height, not 14px).
    let font_height = state.gpu.tab_glyph_cache.line_height();
    let text_baseline_y = text_y + (text_height - font_height) / 2.0;
    let label_len = "Rename: ".chars().count().min(display_text.chars().count());

    for (idx, c) in display_text.chars().enumerate() {
        if let Some(glyph) = state
            .gpu
            .tab_glyph_cache
            .position_char(c, char_x, text_baseline_y)
        {
            glyphs.push(glyph);
        }

        // Render label part first, then input part
        if label_len > 0 && idx == label_len - 1 {
            // Push label glyphs
            state
                .gpu
                .tab_title_renderer
                .push_glyphs(&glyphs, label_color);
            glyphs.clear();
        }

        char_x += state.gpu.tab_glyph_cache.cell_width();
    }

    // Push remaining input glyphs
    if !glyphs.is_empty() {
        state
            .gpu
            .tab_title_renderer
            .push_glyphs(&glyphs, input_color);
    }

    state.gpu.tab_glyph_cache.flush(&shared.queue);

    // Render text pass
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Window Rename Bar Text Render Pass"),
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
        .tab_title_renderer
        .render_transient(&shared.queue, &mut pass, &mut state.gpu.arena);
}

/// Number of whole character cells that fit inside a box of `box_width`
/// physical pixels with `inset` pixels of border+padding on each side.
fn max_chars_in_box(box_width: f32, inset: f32, cell_width: f32) -> usize {
    let usable = box_width - inset * 2.0;
    if usable <= 0.0 || cell_width <= 0.0 {
        0
    } else {
        (usable / cell_width).floor() as usize
    }
}

/// Take at most `max_chars` characters from the start of `text`.
fn fit_prefix(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Compose `prefix + input + suffix` so the result is at most `max_chars`
/// characters. The input is trimmed from the left (its tail, where the cursor
/// is, stays visible); if even prefix+suffix don't fit they are cut from the
/// right.
fn fit_input_line(prefix: &str, input: &str, suffix: &str, max_chars: usize) -> String {
    let fixed = prefix.chars().count() + suffix.chars().count();
    if fixed > max_chars {
        return fit_prefix(&format!("{prefix}{suffix}"), max_chars);
    }
    let available = max_chars - fixed;
    let input_len = input.chars().count();
    let skip = input_len.saturating_sub(available);
    let mut out = String::with_capacity(prefix.len() + input.len() + suffix.len());
    out.push_str(prefix);
    out.extend(input.chars().skip(skip));
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_chars_in_box_floors_and_clamps() {
        assert_eq!(max_chars_in_box(300.0, 10.0, 7.0), 40); // 280 / 7
        assert_eq!(max_chars_in_box(300.0, 10.0, 7.5), 37); // floor(37.33)
        assert_eq!(max_chars_in_box(10.0, 10.0, 7.0), 0);
        assert_eq!(max_chars_in_box(300.0, 10.0, 0.0), 0);
    }

    #[test]
    fn fit_input_line_keeps_tail_and_fixed_parts() {
        assert_eq!(fit_input_line("Rename: ", "abc", "|", 40), "Rename: abc|");
        // 8 + 1 fixed, room for 3 input chars: keep the last three.
        assert_eq!(
            fit_input_line("Rename: ", "abcdef", "|", 12),
            "Rename: def|"
        );
        // Search-style suffix.
        assert_eq!(fit_input_line("", "hello", "| (1/2)", 9), "lo| (1/2)");
        // Fixed parts alone overflow: cut from the right.
        assert_eq!(fit_input_line("Rename: ", "x", "|", 4), "Rena");
        assert_eq!(fit_input_line("", "", "|", 0), "");
        // Multi-byte input counts characters, not bytes.
        assert_eq!(fit_input_line("", "héllo", "|", 4), "llo|");
    }
}
