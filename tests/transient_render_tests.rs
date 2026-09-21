//! Headless GPU tests for the per-pass buffer model.
//!
//! `wgpu::Queue::write_buffer` is not ordered against draws inside one
//! `submit`, so a buffer written twice in a frame makes every pass that
//! references it draw the last write. Overlays, dialogs and the context menu
//! therefore share one renderer per kind and draw through
//! `render_transient`, which bump-allocates each pass's data from a
//! `FrameArena`. These tests pin that contract with pixel assertions (no
//! golden images): several passes in ONE submit must each show their own
//! data, and a renderer drawn from its owned buffer must be unaffected by
//! transient use of a different renderer.
//!
//! ```sh
//! cargo test --test transient_render_tests
//! ```

use crt_renderer::headless::HeadlessRenderer;
use crt_renderer::{FrameArena, GlyphCache, GridRenderer, RectRenderer};

const WIDTH: u32 = 200;
const HEIGHT: u32 = 100;

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];
const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

static TEST_FONT: &[u8] = include_bytes!("../assets/fonts/MesloLGS-NF-Regular.ttf");

/// Same convention as `visual_tests.rs`: skip quietly without a GPU unless
/// `CRT_REQUIRE_GPU` says one is expected.
fn headless() -> Option<HeadlessRenderer> {
    let renderer = HeadlessRenderer::new(WIDTH, HEIGHT)
        .or_else(|_| HeadlessRenderer::with_options(WIDTH, HEIGHT, false))
        .ok();
    if renderer.is_none() {
        if std::env::var_os("CRT_REQUIRE_GPU").is_some() {
            panic!("CRT_REQUIRE_GPU is set but no GPU adapter is available");
        }
        eprintln!("Skipping: no GPU adapter");
    }
    renderer
}

fn begin_pass<'a>(
    encoder: &'a mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    clear: bool,
) -> wgpu::RenderPass<'a> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Transient Test Pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: if clear {
                    wgpu::LoadOp::Clear(wgpu::Color::BLACK)
                } else {
                    wgpu::LoadOp::Load
                },
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    })
}

fn pixel(frame: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * WIDTH + x) * 4) as usize;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

/// Number of pixels in the rectangle with any colour channel lit.
fn lit_pixels(frame: &[u8], x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
    let mut count = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let [r, g, b, _] = pixel(frame, x, y);
            if r.max(g).max(b) > 64 {
                count += 1;
            }
        }
    }
    count
}

fn is_red(p: [u8; 4]) -> bool {
    p[0] > 200 && p[1] < 50 && p[2] < 50
}
fn is_green(p: [u8; 4]) -> bool {
    p[0] < 50 && p[1] > 200 && p[2] < 50
}
fn is_blue(p: [u8; 4]) -> bool {
    p[0] < 50 && p[1] < 50 && p[2] > 200
}
fn is_black(p: [u8; 4]) -> bool {
    p[0] < 20 && p[1] < 20 && p[2] < 20
}

/// Three passes in one submit reuse ONE rect renderer with different
/// contents (as the overlays do). Each pass must draw its own rectangle.
#[test]
fn transient_rect_passes_in_one_submit_keep_their_own_data() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut rects = RectRenderer::new(device, headless.format());
    rects.update_screen_size(queue, WIDTH as f32, HEIGHT as f32);
    let mut arena = FrameArena::new(device, 0, "test arena");
    arena.begin(device);

    let mut encoder = device.create_command_encoder(&Default::default());
    for (i, (x, color)) in [(10.0, RED), (80.0, GREEN), (150.0, BLUE)]
        .into_iter()
        .enumerate()
    {
        rects.clear();
        rects.push_rect(x, 30.0, 40.0, 40.0, color);
        let mut pass = begin_pass(&mut encoder, headless.texture_view(), i == 0);
        rects.render_transient(queue, &mut pass, &mut arena);
    }
    queue.submit(std::iter::once(encoder.finish()));

    let frame = headless.capture_frame().expect("capture");
    assert!(is_red(pixel(&frame, 30, 50)), "first pass lost its rect");
    assert!(
        is_green(pixel(&frame, 100, 50)),
        "second pass lost its rect"
    );
    assert!(is_blue(pixel(&frame, 170, 50)), "third pass lost its rect");
    assert!(
        is_black(pixel(&frame, 65, 50)),
        "gap between rects is drawn"
    );
}

/// The same for text: two passes reuse one grid renderer (the role of
/// `overlay_text_renderer`) and both strings must reach the screen.
#[test]
fn transient_text_passes_in_one_submit_keep_their_own_data() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut glyphs = GlyphCache::new(device, TEST_FONT, 14.0).expect("glyph cache");
    glyphs.precache_ascii();
    glyphs.flush(queue);
    let mut text = GridRenderer::new(device, headless.format());
    text.set_glyph_cache(device, &glyphs);
    text.update_screen_size(queue, WIDTH as f32, HEIGHT as f32);
    let mut arena = FrameArena::new(device, 0, "test arena");
    arena.begin(device);

    let mut encoder = device.create_command_encoder(&Default::default());
    for (i, y) in [10.0_f32, 60.0].into_iter().enumerate() {
        text.clear();
        push_text(&mut text, &mut glyphs, "MMMM", 10.0, y);
        glyphs.flush(queue);
        let mut pass = begin_pass(&mut encoder, headless.texture_view(), i == 0);
        text.render_transient(queue, &mut pass, &mut arena);
    }
    queue.submit(std::iter::once(encoder.finish()));

    let frame = headless.capture_frame().expect("capture");
    let top = lit_pixels(&frame, 0, 0, WIDTH, 45);
    let bottom = lit_pixels(&frame, 0, 55, WIDTH, HEIGHT);
    assert!(top > 20, "first pass text missing ({top} lit pixels)");
    assert!(
        bottom > 20,
        "second pass text missing ({bottom} lit pixels)"
    );
}

/// Regression for the tab-title bug: a renderer drawn from its OWNED buffer
/// keeps its instances across frames without being rebuilt, while a separate
/// renderer is cleared and refilled for transient text every frame. Titles
/// must still be on screen in the second frame, and the transient text of
/// frame one must not linger into frame two.
#[test]
fn owned_text_survives_transient_use_of_a_separate_renderer() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut glyphs = GlyphCache::new(device, TEST_FONT, 14.0).expect("glyph cache");
    glyphs.precache_ascii();
    glyphs.flush(queue);

    let mut titles = GridRenderer::new(device, headless.format());
    titles.set_glyph_cache(device, &glyphs);
    titles.update_screen_size(queue, WIDTH as f32, HEIGHT as f32);
    let mut overlay = GridRenderer::new(device, headless.format());
    overlay.set_glyph_cache(device, &glyphs);
    overlay.update_screen_size(queue, WIDTH as f32, HEIGHT as f32);
    let mut arena = FrameArena::new(device, 0, "test arena");

    // Built once, like `build_tab_title_glyphs` on a titles_version change.
    push_text(&mut titles, &mut glyphs, "MMMM", 10.0, 10.0);
    glyphs.flush(queue);
    let title_instances = titles.instance_count();

    let mut frames = Vec::new();
    for overlay_visible in [true, false] {
        arena.begin(device);
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = begin_pass(&mut encoder, headless.texture_view(), true);
            titles.render(device, queue, &mut pass);
        }
        if overlay_visible {
            overlay.clear();
            push_text(&mut overlay, &mut glyphs, "MMMM", 10.0, 60.0);
            glyphs.flush(queue);
            let mut pass = begin_pass(&mut encoder, headless.texture_view(), false);
            overlay.render_transient(queue, &mut pass, &mut arena);
        }
        queue.submit(std::iter::once(encoder.finish()));
        frames.push(headless.capture_frame().expect("capture"));
    }

    assert_eq!(titles.instance_count(), title_instances);
    for (i, frame) in frames.iter().enumerate() {
        let top = lit_pixels(frame, 0, 0, WIDTH, 45);
        assert!(top > 20, "frame {i}: titles missing ({top} lit pixels)");
    }
    assert!(lit_pixels(&frames[0], 0, 55, WIDTH, HEIGHT) > 20);
    assert_eq!(
        lit_pixels(&frames[1], 0, 55, WIDTH, HEIGHT),
        0,
        "overlay text lingered after the overlay was dismissed"
    );
}

/// An overflowing frame skips the draw instead of reallocating under
/// earlier passes, and `begin()` grows the arena so the next frame fits.
#[test]
fn arena_overflow_skips_the_draw_and_grows_for_the_next_frame() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut arena = FrameArena::new(device, 0, "test arena");
    let capacity = arena.capacity();
    assert_eq!(capacity, FrameArena::MIN_CAPACITY);
    let too_big = vec![0u32; capacity as usize / 4 + 1];

    arena.begin(device);
    let small = arena.push(queue, &[1u32; 8]).expect("small push fits");
    assert_eq!(small.count, 8);
    assert!(!arena.overflowed());
    assert!(arena.push(queue, &too_big).is_none(), "overflow must skip");
    // The frame is incomplete: the caller has to schedule another one.
    assert!(arena.overflowed());
    // A failed push must not consume space or disturb later pushes.
    let after = arena.push(queue, &[2u32; 8]).expect("push after overflow");
    assert!(after.offset >= small.offset + small.len_bytes);
    assert_eq!(arena.capacity(), capacity, "must not grow mid-frame");
    assert!(arena.overflowed(), "a later fitting push must not hide it");

    arena.begin(device);
    assert!(
        arena.capacity() > capacity,
        "begin() grows after a shortfall"
    );
    assert_eq!(arena.used(), 0);
    assert!(!arena.overflowed());
    assert!(arena.push(queue, &too_big).is_some(), "fits after growing");
    assert!(!arena.overflowed());
}

/// Regression: only the single largest overage used to be recorded, so a
/// frame with several oversized pushes needed several (unscheduled) frames
/// to converge. One `begin()` must now make the whole frame fit.
#[test]
fn arena_grows_to_the_whole_frames_demand_in_one_step() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut arena = FrameArena::new(device, 0, "test arena");
    let big = vec![0u32; arena.capacity() as usize / 4 + 1];

    arena.begin(device);
    for _ in 0..3 {
        assert!(arena.push(queue, &big).is_none());
    }

    arena.begin(device);
    for i in 0..3 {
        assert!(arena.push(queue, &big).is_some(), "push {i} still skipped");
    }
    assert!(!arena.overflowed());
}

/// End to end: a draw skipped by an overflow appears on the retry frame.
#[test]
fn overflowed_draw_appears_on_the_next_frame() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut rects = RectRenderer::new(device, headless.format());
    rects.update_screen_size(queue, WIDTH as f32, HEIGHT as f32);
    let mut arena = FrameArena::new(device, 0, "test arena");
    // 32 bytes per rect: enough 1px rects to overflow the minimum arena.
    let count = FrameArena::MIN_CAPACITY as usize / 32 + 1;
    for i in 0..count {
        let x = (i % WIDTH as usize) as f32;
        rects.push_rect(x, 40.0, 1.0, 20.0, RED);
    }

    let mut frames = Vec::new();
    let mut overflowed = Vec::new();
    for _ in 0..2 {
        arena.begin(device);
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = begin_pass(&mut encoder, headless.texture_view(), true);
            rects.render_transient(queue, &mut pass, &mut arena);
        }
        queue.submit(std::iter::once(encoder.finish()));
        overflowed.push(arena.overflowed());
        frames.push(headless.capture_frame().expect("capture"));
    }

    assert_eq!(overflowed, [true, false]);
    assert!(is_black(pixel(&frames[0], 100, 50)), "overflowing draw ran");
    assert!(
        is_red(pixel(&frames[1], 100, 50)),
        "retry frame lost the draw"
    );
}

/// Slices handed out within a frame never overlap, and an empty push
/// allocates nothing.
#[test]
fn arena_slices_do_not_overlap_within_a_frame() {
    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let mut arena = FrameArena::new(device, 0, "test arena");
    arena.begin(device);
    assert!(arena.push::<u32>(queue, &[]).is_none());
    assert_eq!(arena.used(), 0);

    let mut end = 0;
    for len in [1usize, 3, 7, 32, 5] {
        let slice = arena.push(queue, &vec![0u32; len]).expect("fits");
        assert!(slice.offset >= end, "slice overlaps the previous one");
        assert_eq!(slice.len_bytes, len as u64 * 4);
        assert_eq!(slice.count, len as u32);
        end = slice.offset + slice.len_bytes;
    }
    assert_eq!(arena.used(), end);

    arena.begin(device);
    assert_eq!(arena.used(), 0, "begin() rewinds the cursor");
}

fn push_text(text: &mut GridRenderer, glyphs: &mut GlyphCache, s: &str, x: f32, y: f32) {
    let cell_width = glyphs.cell_width();
    for (i, ch) in s.chars().enumerate() {
        if let Some(glyph) = glyphs.position_char(ch, x + i as f32 * cell_width, y) {
            text.push_glyphs(&[glyph], WHITE);
        }
    }
}

/// A theme swap at an unchanged size must reach the GPU. The uniform upload
/// cache used to be keyed on the theme's `Arc` pointer, which a new theme can
/// inherit from a freed one when no frame is rendered between two reloads.
#[test]
fn background_uniforms_follow_a_theme_swap_at_the_same_size() {
    use crt_renderer::BackgroundPipeline;
    use crt_theme::{Color, LinearGradient, Theme};
    use std::sync::Arc;

    let Some(headless) = headless() else { return };
    let (device, queue) = (headless.device(), headless.queue());

    let solid = |color: Color| {
        let mut theme = Theme::minimal();
        theme.background = LinearGradient {
            top: color,
            bottom: color,
        };
        Arc::new(theme)
    };
    let render = |background: &mut BackgroundPipeline| {
        background.update_uniforms(queue, WIDTH as f32, HEIGHT as f32);
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut pass = begin_pass(&mut encoder, headless.texture_view(), true);
            background.render(&mut pass);
        }
        queue.submit(std::iter::once(encoder.finish()));
        headless.capture_frame().expect("capture")
    };

    let mut background = BackgroundPipeline::new(device, headless.format());
    background.set_theme(solid(Color::from_hex(0xff0000)));
    assert!(is_red(pixel(&render(&mut background), 100, 50)));

    // Two swaps with no frame in between, as when a theme file is saved
    // twice while the window is occluded.
    background.set_theme(solid(Color::from_hex(0x00ff00)));
    background.set_theme(solid(Color::from_hex(0x0000ff)));
    assert!(is_blue(pixel(&render(&mut background), 100, 50)));
}
