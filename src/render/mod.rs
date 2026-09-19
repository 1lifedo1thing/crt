//! Rendering logic
//!
//! Multi-pass rendering pipeline for terminal content and effects.

mod context_menu;
mod dialogs;
mod overlays;
mod selection;

use std::time::Instant;

use crate::gpu::SharedGpuState;
use crate::profiling::{self, FrameTiming, GridSnapshot};
use crate::window::{DecorationKind, EffectId, OverrideEventType, WindowState};
use crt_core::{ShellEvent, ShellTerminal};
use crt_renderer::EffectConfig;
use crt_theme::{EventOverride, Theme, ToEffectConfig};

/// Result of processing PTY updates from the active shell
pub struct PtyUpdateResult {
    /// Whether terminal content changed (needs re-render)
    pub content_changed: bool,
    /// Shell events collected (bell, command success/fail)
    pub shell_events: Vec<ShellEvent>,
    /// Title change from the shell, if any
    pub title_change: Option<String>,
}

/// Process PTY output and collect shell events from a shell terminal.
///
/// This is a pure extraction — no state mutation beyond the shell itself.
/// The caller is responsible for applying the result to window state.
pub fn process_pty_updates(shell: &mut ShellTerminal) -> PtyUpdateResult {
    let content_changed = shell.process_pty_output();
    let (shell_events, title_change) = shell.take_shell_events();
    PtyUpdateResult {
        content_changed,
        shell_events,
        title_change,
    }
}

/// An effect patch action computed by `compute_effect_patches`.
pub enum EffectPatchAction {
    /// Apply an override patch to the named effect and mark it as patched
    Apply {
        effect_id: EffectId,
        config: EffectConfig,
    },
    /// Restore the named effect to its base theme config and clear the patched flag
    Restore {
        effect_id: EffectId,
        config: EffectConfig,
    },
}

/// Compute which effect patches need to be applied or restored.
///
/// Pure function — reads override state and theme, returns a list of actions.
/// The caller applies each action to the effects renderer and updates patched tracking.
pub fn compute_effect_patches(
    overrides: &crate::window::OverrideState,
    theme: &Theme,
) -> Vec<EffectPatchAction> {
    let mut actions = Vec::new();

    // Helper macro to reduce repetition across the 6 effect types
    macro_rules! check_effect {
        ($get_patch:ident, $id:expr, $theme_field:ident) => {
            if let Some(patch) = overrides.$get_patch() {
                if !overrides.is_patched($id) {
                    actions.push(EffectPatchAction::Apply {
                        effect_id: $id,
                        config: to_effect_config(patch),
                    });
                }
            } else if overrides.is_patched($id) {
                if let Some(ref base) = theme.$theme_field {
                    actions.push(EffectPatchAction::Restore {
                        effect_id: $id,
                        config: to_effect_config(base),
                    });
                }
            }
        };
    }

    check_effect!(get_starfield_patch, EffectId::Starfield, starfield);
    check_effect!(get_particle_patch, EffectId::Particles, particles);
    check_effect!(get_grid_patch, EffectId::Grid, grid);
    check_effect!(get_rain_patch, EffectId::Rain, rain);
    check_effect!(get_matrix_patch, EffectId::Matrix, matrix);
    check_effect!(get_shape_patch, EffectId::Shape, shape);

    actions
}

/// Result of computing shell event overrides from theme configuration
pub struct ShellEventOverrides {
    /// Override activations to apply (event_type, properties)
    pub activations: Vec<(OverrideEventType, EventOverride)>,
    /// Whether a bell event was present (triggers bell flash)
    pub bell_triggered: bool,
    /// Whether to clear the command fail override (command success clears it)
    pub clear_command_fail: bool,
}

/// Compute theme override activations from shell events.
///
/// Pure function — maps shell events to theme overrides without mutating any state.
pub fn compute_shell_event_overrides(
    shell_events: &[ShellEvent],
    theme: &Theme,
) -> ShellEventOverrides {
    let mut activations = Vec::new();
    let mut bell_triggered = false;
    let mut clear_command_fail = false;

    for event in shell_events {
        log::debug!("Processing shell event: {:?}", event);

        if matches!(event, ShellEvent::Bell) {
            log::info!("Bell triggered - activating flash");
            bell_triggered = true;
        }

        if matches!(event, ShellEvent::CommandSuccess) {
            log::info!("Command success - clearing fail override");
            clear_command_fail = true;
        }

        if matches!(event, ShellEvent::CommandFail(_)) {
            log::info!("Command failed - checking for theme override");
        }

        let override_opt = match event {
            ShellEvent::Bell => theme.on_bell.clone(),
            ShellEvent::CommandSuccess => theme.on_command_success.clone(),
            ShellEvent::CommandFail(_) => theme.on_command_fail.clone(),
        };

        if let Some(properties) = override_opt {
            log::info!(
                "Applying theme override for {:?} (duration: {}ms, cursor_color: {:?})",
                event,
                properties.duration_ms,
                properties.cursor_color
            );
            let event_type = OverrideEventType::from(*event);
            activations.push((event_type, properties));
        } else {
            log::debug!("No theme override defined for {:?}", event);
        }
    }

    ShellEventOverrides {
        activations,
        bell_triggered,
        clear_command_fail,
    }
}

/// Convert a ToEffectConfig implementor to an EffectConfig
fn to_effect_config<T: ToEffectConfig>(source: &T) -> EffectConfig {
    let mut config = EffectConfig::new();
    for (key, value) in source.to_config_pairs() {
        config.insert(key, value);
    }
    config
}

/// Render a single frame for a window
pub fn render_frame(state: &mut WindowState, shared: &mut SharedGpuState) {
    // Skip rendering for fully occluded windows (covered by other windows)
    if state.render.occluded {
        return;
    }

    let frame_start = Instant::now();
    let mut timing = FrameTiming::default();
    state.render.frame_count = state.render.frame_count.saturating_add(1);

    // Real elapsed time since the previous frame drives animations; clamped
    // so a pause (occlusion, sleep) does not fast-forward effects.
    let dt = frame_start
        .duration_since(state.render.last_frame_at)
        .as_secs_f32()
        .clamp(0.0, 0.1);
    state.render.last_frame_at = frame_start;

    // Log every 300 frames (~5 seconds at 60fps) or on frame 1
    if state.render.frame_count == 1 || state.render.frame_count.is_multiple_of(300) {
        log::debug!(
            "Frame {} starting (dirty={}, effects_enabled={})",
            state.render.frame_count,
            state.render.dirty,
            state.gpu.effects_renderer.has_enabled_effects()
        );
    }

    let active_tab_id = state.gpu.tab_bar.active_tab_id();

    // Update backdrop effects animation
    let update_start = Instant::now();
    state.gpu.effects_renderer.update(dt);
    timing.effects_us = update_start.elapsed().as_micros() as u64;

    // Expire finished overrides
    state.ui.overrides.update();

    // Compute and apply effect patches from override state
    let theme = state.gpu.effect_pipeline.theme();
    let patch_actions = compute_effect_patches(&state.ui.overrides, theme);
    for action in patch_actions {
        match action {
            EffectPatchAction::Apply { effect_id, config } => {
                state
                    .gpu
                    .effects_renderer
                    .apply_effect_patch(effect_id.as_str(), &config);
                state.ui.overrides.set_patched(effect_id);
            }
            EffectPatchAction::Restore { effect_id, config } => {
                state
                    .gpu
                    .effects_renderer
                    .apply_effect_patch(effect_id.as_str(), &config);
                state.ui.overrides.clear_patched(effect_id);
                log::debug!("Restored {} to base theme", effect_id.as_str());
            }
        }
    }

    // Update text buffer and get cursor/decoration info
    let text_update_start = Instant::now();
    let update_result = if state.render.dirty {
        state.render.dirty = false;
        state.update_text_buffer(shared)
    } else {
        None
    };
    timing.update_us = text_update_start.elapsed().as_micros() as u64;

    // Render backdrop effects to their intermediate texture (if any effects are enabled)
    // This must happen before we create our command encoder since Vello submits its own commands
    // NOTE: Memory stability depends on frame throttling in main.rs (see about_to_wait)
    let effects_render_start = Instant::now();
    let effects_rendered = if state.gpu.effects_renderer.has_enabled_effects() {
        // Ensure Vello renderer is initialized
        shared.ensure_vello_renderer();

        // Render effects to intermediate texture
        state
            .gpu
            .effects_renderer
            .render(
                &shared.device,
                &shared.queue,
                (state.gpu.config.width, state.gpu.config.height),
            )
            .is_some()
    } else {
        false
    };
    timing.effects_us += effects_render_start.elapsed().as_micros() as u64;

    // Render
    let frame = match state.gpu.surface.get_current_texture() {
        Ok(f) => f,
        Err(e) => {
            log::warn!("Failed to get surface texture: {:?}", e);
            // The text layer above was rebuilt for a frame that will never be
            // presented. The loop only redraws on demand, so put the window
            // back in the state it was in before this frame: dirty, with the
            // active tab's text layer to rebuild. `about_to_wait` then
            // schedules the retry at the normal frame cap (and not at all
            // while the window is occluded), so a surface that keeps failing
            // cannot spin the loop. A stale or lost swapchain is reconfigured
            // first.
            if matches!(e, wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) {
                state
                    .gpu
                    .surface
                    .configure(&shared.device, &state.gpu.config);
            }
            state.invalidate_text();
            return;
        }
    };
    let frame_view = frame.texture.create_view(&Default::default());

    // Determine render target: CRT intermediate texture if enabled, otherwise surface directly
    let crt_enabled = state.gpu.crt_pipeline.is_enabled();
    // Take ownership of CRT view to avoid borrow conflicts
    let crt_view_clone = state.gpu.crt_texture.as_ref().map(|pooled| {
        // Create a new view from the texture each frame (cheap operation)
        pooled.texture().create_view(&Default::default())
    });
    let render_target: &wgpu::TextureView = if crt_enabled {
        crt_view_clone.as_ref().unwrap_or(&frame_view)
    } else {
        &frame_view
    };

    let render_start = Instant::now();
    // Per-frame transient vertex allocations start fresh (grows if last frame overflowed)
    state.gpu.arena.begin(&shared.device);
    let mut encoder = shared.device.create_command_encoder(&Default::default());

    // Update effect uniforms
    state.gpu.effect_pipeline.update_uniforms(
        &shared.queue,
        state.gpu.config.width as f32,
        state.gpu.config.height as f32,
    );

    // Pass 1: Render background gradient
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Background Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        state.gpu.effect_pipeline.background.render(&mut pass);
    }

    // Pass 1.25: Composite backdrop effects (grid, etc.) if rendered
    if effects_rendered {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Backdrop Effects Composite Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
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

        state.gpu.effects_renderer.composite(&mut pass);
    }

    // Pass 1.3: Render sprite animation (if configured, uses raw wgpu to bypass vello memory issues)
    if let Some(ref mut sprite_state) = state.gpu.sprite_state {
        let width = state.gpu.config.width as f32;
        let height = state.gpu.config.height as f32;

        // Log once to confirm sprite is rendering
        static LOGGED_SPRITE: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !LOGGED_SPRITE.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("Rendering sprite: {}x{}", width, height);
        }

        // Apply sprite patch from active overrides (if any)
        if let Some(patch) = state.ui.overrides.get_sprite_patch() {
            if !state.ui.overrides.is_patched(EffectId::Sprite) {
                // Use apply_patch_with_device if path change is needed
                if crt_renderer::SpriteAnimationState::needs_device_for_patch(patch) {
                    sprite_state.apply_patch_with_device(patch, &shared.device, &shared.queue);
                } else {
                    sprite_state.apply_patch(patch);
                }
                state.ui.overrides.set_patched(EffectId::Sprite);
            }
        } else if state.ui.overrides.is_patched(EffectId::Sprite) {
            // No active override, restore original values (with device for texture restore)
            sprite_state.restore_with_device(&shared.device, &shared.queue);
            state.ui.overrides.clear_patched(EffectId::Sprite);
        }

        // Update animation state
        sprite_state.update(dt, width, height);

        // Render sprite
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Sprite Animation Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
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

        sprite_state.render(&mut pass, &shared.queue, width, height);
    }

    // Pass 1.5: Render background image (if configured)
    if let (Some(bg_state), Some(bind_group)) = (
        &mut state.gpu.background_image_state,
        &state.gpu.background_image_bind_group,
    ) {
        // Advance the animation if this is an animated GIF
        bg_state.update(&shared.queue);

        // Update uniforms with UV transform and opacity
        let uv_transform = bg_state.calculate_uv_transform(
            state.gpu.config.width as f32,
            state.gpu.config.height as f32,
        );
        state.gpu.background_image_pipeline.update_uniforms(
            &shared.queue,
            uv_transform,
            bg_state.opacity(),
            bg_state.repeat_flags(),
        );

        // Render background image
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Background Image Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
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
            .background_image_pipeline
            .render(&mut pass, bind_group);
    }

    // Pass 2: Update cursor position if content changed
    if let Some(ref result) = update_result {
        state.gpu.terminal_vello.set_cursor(
            result.cursor.x,
            result.cursor.y,
            result.cursor.cell_width,
            result.cursor.cell_height,
            true, // visible
        );
        // Reset blink when cursor moves (makes cursor visible immediately)
        state.gpu.terminal_vello.reset_blink();
    }

    // Update cursor blink state
    state.gpu.terminal_vello.update_blink();

    // Update cached decorations when content changes
    let content_changed = update_result.is_some();
    if let Some(mut result) = update_result {
        // Use take() to avoid cloning the decorations vector
        state.render.cached.decorations = std::mem::take(&mut result.decorations);
        state.render.cached.cursor = Some(result.cursor);
    }

    // Pass 3: Render cell backgrounds via RectRenderer (before text)
    // The instance list is rebuilt only when the decorations changed; the
    // renderer-owned buffer is re-uploaded only then.
    {
        if content_changed {
            state.gpu.cell_bg_renderer.clear();
            for decoration in &state.render.cached.decorations {
                if decoration.kind == DecorationKind::Background {
                    state.gpu.cell_bg_renderer.push_rect(
                        decoration.x,
                        decoration.y,
                        decoration.cell_width,
                        decoration.cell_height,
                        decoration.color,
                    );
                }
            }
        }
        if state.gpu.cell_bg_renderer.instance_count() > 0 {
            state.gpu.cell_bg_renderer.update_screen_size(
                &shared.queue,
                state.gpu.config.width as f32,
                state.gpu.config.height as f32,
            );
            // Render backgrounds directly to frame
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Background Rect Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: render_target,
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
                .cell_bg_renderer
                .render(&shared.device, &shared.queue, &mut pass);
        }
    }

    // Pass 3.5: Render output text directly to frame (flat, no glow)
    // This is all terminal text EXCEPT the cursor line
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Output Text Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
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
            .output_grid_renderer
            .render(&shared.device, &shared.queue, &mut pass);
    }

    // Pass 4: cursor-line text -> text texture, then horizontal glow blur.
    // Both only run when the text changed (or the blur target was resized);
    // the vertical blur + composite in pass 4.5 runs every frame because the
    // background beneath it is redrawn every frame. All glow work is
    // scissored to the rows that actually hold glyphs.
    let blur_recreated = state.gpu.effect_pipeline.composite.ensure_blur_target(
        &shared.device,
        state.gpu.config.width,
        state.gpu.config.height,
    );
    let glow_region = state.gpu.grid_renderer.bounds().and_then(|b| {
        crt_renderer::GlowRegion::from_bounds(
            b,
            state.gpu.effect_pipeline.composite.glow_radius(),
            state.gpu.config.width,
            state.gpu.config.height,
        )
    });
    if content_changed || blur_recreated {
        // Clear the text texture, then render the cursor-line text into it
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Terminal Text Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: state.gpu.text_texture.view(),
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
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
            .grid_renderer
            .render(&shared.device, &shared.queue, &mut pass);
        drop(pass);

        if let Some(region) = glow_region {
            state.gpu.effect_pipeline.composite.render_hblur(
                &mut encoder,
                &state.gpu.composite_bind_group,
                region,
            );
        }
    }

    // Pass 4.5: Composite text texture onto frame with the vertical glow blur
    if let Some(region) = glow_region {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Text Composite Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: render_target,
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

        state.gpu.effect_pipeline.composite.render(
            &mut pass,
            &state.gpu.composite_bind_group,
            Some(region),
        );
    }

    // Pass 5: Render cursor, selection, underlines, strikethroughs via RectRenderer
    // (Direct rendering without intermediate texture saves ~8MB)
    {
        // Get selection and display_offset from active terminal (if any)
        let (selection, display_offset) = active_tab_id
            .and_then(|id| state.shells.get(&id))
            .map(|shell| {
                let content = shell.terminal().renderable_content();
                (content.selection, shell.terminal().display_offset() as i32)
            })
            .unwrap_or((None, 0));

        // Collect all overlay rectangles (uses separate renderer to avoid buffer conflicts with tab bar)
        state.gpu.overlay_rect_renderer.clear();
        state.gpu.overlay_rect_renderer.update_screen_size(
            &shared.queue,
            state.gpu.config.width as f32,
            state.gpu.config.height as f32,
        );

        // Add selection rectangles
        if let Some(selection) = selection {
            selection::render_selection_rects(state, &selection, display_offset);
        }

        // Add underlines and strikethroughs from cached decorations
        for decoration in &state.render.cached.decorations {
            match decoration.kind {
                DecorationKind::Background => {} // Already rendered in Pass 3
                DecorationKind::Underline => {
                    // Underline: thin rect near bottom of cell
                    let underline_y = decoration.y + decoration.cell_height - 3.0;
                    state.gpu.overlay_rect_renderer.push_rect(
                        decoration.x,
                        underline_y,
                        decoration.cell_width,
                        1.5,
                        decoration.color,
                    );
                }
                DecorationKind::Strikethrough => {
                    // Strikethrough: positioned at center of x-height using font metrics
                    let strike_y = decoration.y + state.gpu.glyph_cache.strikethrough_offset();
                    state.gpu.overlay_rect_renderer.push_rect(
                        decoration.x,
                        strike_y,
                        decoration.cell_width,
                        1.5,
                        decoration.color,
                    );
                }
            }
        }

        // Add cursor (if visible after blink check AND terminal hasn't hidden it)
        if state.gpu.terminal_vello.cursor_visible()
            && let Some(cursor) = &state.render.cached.cursor
        {
            // Only render if terminal hasn't hidden the cursor (e.g., TUI apps hide it)
            if cursor.visible {
                // Check for active theme override cursor color first
                let cursor_color =
                    if let Some(override_color) = state.ui.overrides.get_cursor_color() {
                        log::debug!(
                            "Using override cursor color: rgba({}, {}, {}, {})",
                            override_color.r,
                            override_color.g,
                            override_color.b,
                            override_color.a
                        );
                        [
                            override_color.r,
                            override_color.g,
                            override_color.b,
                            override_color.a,
                        ]
                    } else {
                        state.gpu.terminal_vello.cursor_color()
                    };

                // Use configured cursor shape as default, but allow terminal/apps to override
                // via escape sequences (DECSCUSR). If the terminal explicitly requests a
                // non-Block shape, use that; otherwise use the user's configured preference.
                // Theme overrides take highest priority.
                let configured_shape = state.gpu.terminal_vello.cursor_shape();
                let terminal_shape = cursor.shape;
                let override_shape = state.ui.overrides.get_cursor_shape();

                // Determine effective cursor shape:
                // - Theme override takes priority (for event effects)
                // - If terminal explicitly set non-Block shape, use it
                // - If terminal shape is Hidden, honor that
                // - Otherwise use user's configured shape
                let cursor_rect = if terminal_shape == crt_core::CursorShape::Hidden {
                    None
                } else if let Some(override_shape) = override_shape {
                    // Theme override takes priority
                    match override_shape {
                        crt_theme::CursorShape::Block => {
                            Some((cursor.x, cursor.y, cursor.cell_width, cursor.cell_height))
                        }
                        crt_theme::CursorShape::Bar => {
                            Some((cursor.x, cursor.y, 2.0, cursor.cell_height))
                        }
                        crt_theme::CursorShape::Underline => Some((
                            cursor.x,
                            cursor.y + cursor.cell_height - 2.0,
                            cursor.cell_width,
                            2.0,
                        )),
                    }
                } else if terminal_shape != crt_core::CursorShape::Block {
                    // Terminal explicitly requested a non-default shape
                    match terminal_shape {
                        crt_core::CursorShape::Beam => {
                            Some((cursor.x, cursor.y, 2.0, cursor.cell_height))
                        }
                        crt_core::CursorShape::Underline => Some((
                            cursor.x,
                            cursor.y + cursor.cell_height - 2.0,
                            cursor.cell_width,
                            2.0,
                        )),
                        crt_core::CursorShape::HollowBlock => {
                            // Hollow block renders as outline only (handled specially below)
                            Some((cursor.x, cursor.y, cursor.cell_width, cursor.cell_height))
                        }
                        _ => Some((cursor.x, cursor.y, cursor.cell_width, cursor.cell_height)),
                    }
                } else {
                    // Use user's configured shape
                    match configured_shape {
                        crt_renderer::CursorShape::Block => {
                            Some((cursor.x, cursor.y, cursor.cell_width, cursor.cell_height))
                        }
                        crt_renderer::CursorShape::Bar => {
                            Some((cursor.x, cursor.y, 2.0, cursor.cell_height))
                        }
                        crt_renderer::CursorShape::Underline => Some((
                            cursor.x,
                            cursor.y + cursor.cell_height - 2.0,
                            cursor.cell_width,
                            2.0,
                        )),
                    }
                };

                if let Some((rect_x, rect_y, rect_w, rect_h)) = cursor_rect {
                    log::debug!(
                        "Cursor rect: x={}, y={}, w={}, h={}, color={:?}",
                        rect_x,
                        rect_y,
                        rect_w,
                        rect_h,
                        cursor_color
                    );
                    // Render cursor glow effect (layered rectangles with decreasing opacity)
                    if let Some((glow_color, radius, intensity)) =
                        state.gpu.terminal_vello.cursor_glow()
                    {
                        let layers = 5;
                        for i in (1..=layers).rev() {
                            let layer_factor = i as f32 / layers as f32;
                            let expand = radius * layer_factor;
                            let alpha = glow_color[3] * intensity * (1.0 - layer_factor) * 0.3;

                            state.gpu.overlay_rect_renderer.push_rect(
                                rect_x - expand,
                                rect_y - expand,
                                rect_w + expand * 2.0,
                                rect_h + expand * 2.0,
                                [glow_color[0], glow_color[1], glow_color[2], alpha],
                            );
                        }
                    }

                    // Render cursor
                    let is_hollow = terminal_shape == crt_core::CursorShape::HollowBlock;
                    if is_hollow {
                        // Hollow block: render as 4 edge rectangles (outline)
                        let border = 2.0;
                        // Top edge
                        state.gpu.overlay_rect_renderer.push_rect(
                            rect_x,
                            rect_y,
                            rect_w,
                            border,
                            cursor_color,
                        );
                        // Bottom edge
                        state.gpu.overlay_rect_renderer.push_rect(
                            rect_x,
                            rect_y + rect_h - border,
                            rect_w,
                            border,
                            cursor_color,
                        );
                        // Left edge (excluding corners already covered)
                        state.gpu.overlay_rect_renderer.push_rect(
                            rect_x,
                            rect_y + border,
                            border,
                            rect_h - border * 2.0,
                            cursor_color,
                        );
                        // Right edge (excluding corners already covered)
                        state.gpu.overlay_rect_renderer.push_rect(
                            rect_x + rect_w - border,
                            rect_y + border,
                            border,
                            rect_h - border * 2.0,
                            cursor_color,
                        );
                    } else {
                        // Solid cursor (block, bar, underline)
                        state.gpu.overlay_rect_renderer.push_rect(
                            rect_x,
                            rect_y,
                            rect_w,
                            rect_h,
                            cursor_color,
                        );
                    }
                }
            }
        }

        // Render all overlay rects directly to frame
        if state.gpu.overlay_rect_renderer.instance_count() > 0 {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Overlay Rect Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: render_target,
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

            state.gpu.overlay_rect_renderer.render_transient(
                &shared.queue,
                &mut pass,
                &mut state.gpu.arena,
            );
        }
    }

    // Pass 6: Render tab bar shapes via RectRenderer (no Vello needed)
    {
        // Recalculate layout if dirty
        state.gpu.tab_bar.prepare(&shared.device, &shared.queue);

        // Render tab bar shapes directly via RectRenderer
        state.gpu.rect_renderer.clear();
        state.gpu.rect_renderer.update_screen_size(
            &shared.queue,
            state.gpu.config.width as f32,
            state.gpu.config.height as f32,
        );
        state
            .gpu
            .tab_bar
            .render_shapes_to_rects(&mut state.gpu.rect_renderer);

        // Add focus ring around active tab (accessibility indicator)
        if let Some((tab_x, tab_y, tab_w, tab_h)) = state.gpu.tab_bar.active_tab_rect() {
            let s = state.scale_factor;
            let ui_style = &state.gpu.effect_pipeline.theme().ui;
            let focus_thickness = ui_style.focus.ring_thickness * s;
            let focus_color = ui_style.focus.ring_color.to_array();

            // Draw focus ring as 4 rectangles (top, bottom, left, right)
            // Top edge
            state
                .gpu
                .rect_renderer
                .push_rect(tab_x, tab_y, tab_w, focus_thickness, focus_color);
            // Bottom edge
            state.gpu.rect_renderer.push_rect(
                tab_x,
                tab_y + tab_h - focus_thickness,
                tab_w,
                focus_thickness,
                focus_color,
            );
            // Left edge
            state
                .gpu
                .rect_renderer
                .push_rect(tab_x, tab_y, focus_thickness, tab_h, focus_color);
            // Right edge
            state.gpu.rect_renderer.push_rect(
                tab_x + tab_w - focus_thickness,
                tab_y,
                focus_thickness,
                tab_h,
                focus_color,
            );
        }

        if state.gpu.rect_renderer.instance_count() > 0 {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Tab Bar Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: render_target,
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

            state.gpu.rect_renderer.render_transient(
                &shared.queue,
                &mut pass,
                &mut state.gpu.arena,
            );
        }
    }

    // Pass 7: Render tab title text with glow
    render_tab_titles(state, shared, &mut encoder, render_target);

    // Pass 8: Render search bar overlay (if search is active)
    if state.ui.search.active {
        dialogs::render_search_bar(state, shared, &mut encoder, render_target);
    }

    // Pass 8.5: Render window rename bar overlay (if rename is active)
    if state.ui.window_rename.active {
        dialogs::render_window_rename(state, shared, &mut encoder, render_target);
    }

    // Pass 9: Render bell flash overlay (if active via CSS theme)
    if let Some((color, intensity)) = state.ui.overrides.get_effective_flash()
        && intensity > 0.0
    {
        overlays::render_bell_flash(state, shared, &mut encoder, render_target, color, intensity);
    }

    // Pass 10: Render context menu (if visible)
    if state.ui.context_menu.visible {
        context_menu::render(state, shared, &mut encoder, render_target);
    }

    // Pass 11: Render zoom indicator (if recently changed)
    if state.ui.zoom_indicator.is_visible() {
        overlays::render_zoom_indicator(state, shared, &mut encoder, render_target);
    }

    // Pass 12: Render copy indicator (brief "Copied!" feedback)
    if state.ui.copy_indicator.is_visible() {
        overlays::render_copy_indicator(state, shared, &mut encoder, render_target);
    }

    // Pass 13: Render toast notifications (errors, warnings)
    if state.ui.toast.is_visible() {
        overlays::render_toast(state, shared, &mut encoder, render_target);
    }

    // Final Pass: Apply CRT post-processing (if enabled)
    if crt_enabled && let Some(bind_group) = &state.gpu.crt_bind_group {
        // Update CRT uniforms
        state.gpu.crt_pipeline.update_uniforms(
            &shared.queue,
            state.gpu.config.width as f32,
            state.gpu.config.height as f32,
        );

        // Render from CRT intermediate texture to actual frame
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("CRT Post-Process Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &frame_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        state.gpu.crt_pipeline.render(&mut pass, bind_group);
    }

    timing.render_us = render_start.elapsed().as_micros() as u64;
    shared.queue.submit(std::iter::once(encoder.finish()));

    let present_start = Instant::now();
    frame.present();
    timing.present_us = present_start.elapsed().as_micros() as u64;

    // Record frame timing for profiling
    timing.total_us = frame_start.elapsed().as_micros() as u64;
    profiling::record_frame(timing);

    // Record grid snapshot for debugging (rate limited; checked before the copy)
    if profiling::grid_snapshot_due()
        && let Some(tab_id) = state.gpu.tab_bar.active_tab_id()
        && let Some(shell) = state.shells.get(&tab_id)
    {
        let terminal = shell.terminal();
        let cursor = terminal.cursor();
        let size = terminal.size();

        // Get visible lines content (only lines with index >= 0, i.e. visible area)
        let all_lines = terminal.all_lines_text();
        let visible_content: Vec<String> = all_lines
            .into_iter()
            .filter(|(idx, _)| *idx >= 0)
            .map(|(_, text)| text)
            .collect();

        let cursor_shape_str = match cursor.shape {
            crt_core::CursorShape::Block => "Block",
            crt_core::CursorShape::Beam => "Beam",
            crt_core::CursorShape::Underline => "Underline",
            crt_core::CursorShape::HollowBlock => "HollowBlock",
            crt_core::CursorShape::Hidden => "Hidden",
        };
        let mode_visible = terminal.cursor_mode_visible();
        let shape_str = format!(
            "{} (mode:{})",
            cursor_shape_str,
            if mode_visible { "show" } else { "hide" }
        );
        let snapshot = GridSnapshot {
            columns: size.columns,
            lines: size.lines,
            cursor_col: cursor.point.column.0,
            cursor_line: cursor.point.line.0,
            cursor_visible: mode_visible,
            cursor_shape: shape_str,
            display_offset: terminal.display_offset(),
            history_size: terminal.history_size(),
            visible_content,
        };
        profiling::record_grid_snapshot(snapshot);
    }
}

/// Render tab title text with glow effect
fn render_tab_titles(
    state: &mut WindowState,
    shared: &SharedGpuState,
    encoder: &mut wgpu::CommandEncoder,
    frame_view: &wgpu::TextureView,
) {
    if state.gpu.tab_bar.tab_count() == 0 {
        return;
    }

    // Tab titles change rarely; rebuild the glyph list only when the tab
    // bar reports a new version (titles, active tab, editing, layout, theme).
    let version = state.gpu.tab_bar.titles_version();
    let clobbered = state.gpu.tab_title_renderer.instance_count() != state.gpu.tab_titles_instances;
    debug_assert!(
        !clobbered,
        "tab_title_renderer was modified outside build_tab_title_glyphs; \
         transient UI text must use overlay_text_renderer"
    );
    if state.gpu.tab_titles_version != Some(version) || clobbered {
        state.gpu.tab_titles_version = Some(version);
        build_tab_title_glyphs(state, shared);
        state.gpu.tab_titles_instances = state.gpu.tab_title_renderer.instance_count();
    }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Tab Title Render Pass"),
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
        .render(&shared.device, &shared.queue, &mut pass);
}

/// Rebuild the tab-title glyph instances (glow layers, titles, close and
/// new-tab buttons) into `tab_title_renderer`.
fn build_tab_title_glyphs(state: &mut WindowState, shared: &SharedGpuState) {
    let gpu = &mut state.gpu;
    let tab_labels = gpu.tab_bar.get_tab_labels();
    gpu.tab_title_renderer.clear();

    let active_color = gpu.tab_bar.active_tab_color();
    let inactive_color = gpu.tab_bar.inactive_tab_color();
    let active_shadow = gpu.tab_bar.active_tab_text_shadow();

    // Pre-allocate glyph buffer to avoid per-loop allocations
    // Typical tab title is ~30 chars max, so 64 is plenty
    let mut glyph_buffer: Vec<crt_renderer::PositionedGlyph> = Vec::with_capacity(64);

    // First pass: render glow layers for active tabs
    if let Some((radius, glow_color)) = active_shadow {
        // Tighter glow offsets for a subtle halo effect
        let offsets = [
            (-0.75, -0.75),
            (0.75, -0.75),
            (-0.75, 0.75),
            (0.75, 0.75),
            (-1.0, 0.0),
            (1.0, 0.0),
            (0.0, -1.0),
            (0.0, 1.0),
            (-0.5, 0.0),
            (0.5, 0.0),
            (0.0, -0.5),
            (0.0, 0.5),
        ];

        let glow_alpha = (glow_color[3] * (radius / 25.0).min(1.0)).min(0.4);
        let glow_render_color = [glow_color[0], glow_color[1], glow_color[2], glow_alpha];

        for (x, y, title, is_active, _is_editing) in &tab_labels {
            if *is_active {
                for (ox, oy) in &offsets {
                    glyph_buffer.clear();
                    let mut char_x = *x + ox;
                    for c in title.chars() {
                        if let Some(glyph) = gpu.tab_glyph_cache.position_char(c, char_x, *y + oy) {
                            glyph_buffer.push(glyph);
                        }
                        char_x += gpu.tab_glyph_cache.cell_width();
                    }
                    gpu.tab_title_renderer
                        .push_glyphs(&glyph_buffer, glow_render_color);
                }
            }
        }
    }

    // Second pass: render actual text on top
    for (x, y, title, is_active, is_editing) in tab_labels {
        glyph_buffer.clear();
        let mut char_x = x;
        for c in title.chars() {
            if let Some(glyph) = gpu.tab_glyph_cache.position_char(c, char_x, y) {
                glyph_buffer.push(glyph);
            }
            char_x += gpu.tab_glyph_cache.cell_width();
        }

        let text_color = if is_editing {
            [
                (active_color[0] * 1.2).min(1.0),
                (active_color[1] * 1.2).min(1.0),
                (active_color[2] * 1.2).min(1.0),
                active_color[3],
            ]
        } else if is_active {
            active_color
        } else {
            inactive_color
        };
        gpu.tab_title_renderer
            .push_glyphs(&glyph_buffer, text_color);
    }

    // Render close button 'x' characters
    let close_positions = gpu.tab_bar.get_close_button_labels();
    for (x, y) in close_positions {
        if let Some(glyph) = gpu.tab_glyph_cache.position_char('x', x, y) {
            gpu.tab_title_renderer.push_glyphs(&[glyph], inactive_color);
        }
    }

    // Render the "+" new-tab button glyph
    if let Some((x, y)) = gpu.tab_bar.get_new_tab_button_label()
        && let Some(glyph) = gpu.tab_glyph_cache.position_char('+', x, y)
    {
        gpu.tab_title_renderer.push_glyphs(&[glyph], inactive_color);
    }

    gpu.tab_glyph_cache.flush(&shared.queue);
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use crate::window::OverrideState;
    use crt_core::ShellEvent;
    use crt_theme::{EventOverride, StarfieldPatch, Theme};

    // ── compute_shell_event_overrides tests ──────────────────────

    #[test]
    fn shell_overrides_no_events_produces_empty() {
        let theme = Theme::default();
        let result = compute_shell_event_overrides(&[], &theme);
        assert!(result.activations.is_empty());
        assert!(!result.bell_triggered);
        assert!(!result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_bell_sets_bell_triggered() {
        let theme = Theme::default();
        let events = vec![ShellEvent::Bell];
        let result = compute_shell_event_overrides(&events, &theme);
        assert!(result.bell_triggered);
        assert!(!result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_command_success_sets_clear_fail() {
        let theme = Theme::default();
        let events = vec![ShellEvent::CommandSuccess];
        let result = compute_shell_event_overrides(&events, &theme);
        assert!(!result.bell_triggered);
        assert!(result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_command_fail_no_flags() {
        let theme = Theme::default();
        let events = vec![ShellEvent::CommandFail(1)];
        let result = compute_shell_event_overrides(&events, &theme);
        assert!(!result.bell_triggered);
        assert!(!result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_no_activations_when_theme_has_no_overrides() {
        // Default theme has on_bell = None, on_command_fail = None, etc.
        let theme = Theme::default();
        let events = vec![
            ShellEvent::Bell,
            ShellEvent::CommandSuccess,
            ShellEvent::CommandFail(127),
        ];
        let result = compute_shell_event_overrides(&events, &theme);
        assert!(result.activations.is_empty());
    }

    #[test]
    fn shell_overrides_bell_activation_when_theme_has_on_bell() {
        let mut theme = Theme::default();
        theme.on_bell = Some(EventOverride {
            duration_ms: 500,
            ..Default::default()
        });
        let events = vec![ShellEvent::Bell];
        let result = compute_shell_event_overrides(&events, &theme);
        assert_eq!(result.activations.len(), 1);
        assert_eq!(result.activations[0].0, OverrideEventType::Bell);
        assert_eq!(result.activations[0].1.duration_ms, 500);
    }

    #[test]
    fn shell_overrides_command_fail_activation_when_theme_configured() {
        let mut theme = Theme::default();
        theme.on_command_fail = Some(EventOverride {
            duration_ms: 1000,
            cursor_color: Some(crt_theme::Color::rgb(1.0, 0.0, 0.0)),
            ..Default::default()
        });
        let events = vec![ShellEvent::CommandFail(1)];
        let result = compute_shell_event_overrides(&events, &theme);
        assert_eq!(result.activations.len(), 1);
        assert_eq!(result.activations[0].0, OverrideEventType::CommandFail);
        assert!(result.activations[0].1.cursor_color.is_some());
    }

    #[test]
    fn shell_overrides_command_success_activation() {
        let mut theme = Theme::default();
        theme.on_command_success = Some(EventOverride {
            duration_ms: 300,
            ..Default::default()
        });
        let events = vec![ShellEvent::CommandSuccess];
        let result = compute_shell_event_overrides(&events, &theme);
        assert_eq!(result.activations.len(), 1);
        assert_eq!(result.activations[0].0, OverrideEventType::CommandSuccess);
        assert!(result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_multiple_events_produce_multiple_activations() {
        let mut theme = Theme::default();
        theme.on_bell = Some(EventOverride {
            duration_ms: 200,
            ..Default::default()
        });
        theme.on_command_fail = Some(EventOverride {
            duration_ms: 1000,
            ..Default::default()
        });
        let events = vec![ShellEvent::Bell, ShellEvent::CommandFail(2)];
        let result = compute_shell_event_overrides(&events, &theme);
        assert_eq!(result.activations.len(), 2);
        assert!(result.bell_triggered);
        assert!(!result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_mixed_configured_and_unconfigured() {
        let mut theme = Theme::default();
        // Only bell has an override, success does not
        theme.on_bell = Some(EventOverride {
            duration_ms: 100,
            ..Default::default()
        });
        let events = vec![ShellEvent::Bell, ShellEvent::CommandSuccess];
        let result = compute_shell_event_overrides(&events, &theme);
        assert_eq!(result.activations.len(), 1); // only bell
        assert!(result.bell_triggered);
        assert!(result.clear_command_fail);
    }

    #[test]
    fn shell_overrides_event_type_from_shell_event() {
        assert_eq!(
            OverrideEventType::from(ShellEvent::Bell),
            OverrideEventType::Bell
        );
        assert_eq!(
            OverrideEventType::from(ShellEvent::CommandSuccess),
            OverrideEventType::CommandSuccess
        );
        assert_eq!(
            OverrideEventType::from(ShellEvent::CommandFail(42)),
            OverrideEventType::CommandFail
        );
    }

    // ── OverrideState tests ──────────────────────────────────────

    #[test]
    fn override_state_default_empty() {
        let state = OverrideState::default();
        assert!(state.active.is_empty());
        assert!(!state.is_patched(EffectId::Starfield));
    }

    #[test]
    fn override_state_add_and_has_active() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000, // long duration so it's still active
                ..Default::default()
            },
        );
        assert!(state.has_active());
        assert_eq!(state.active.len(), 1);
    }

    #[test]
    fn override_state_add_replaces_same_type() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 3000,
                ..Default::default()
            },
        );
        // Should still be 1 — replaced the first
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].properties.duration_ms, 3000);
    }

    #[test]
    fn override_state_different_types_coexist() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.add(
            OverrideEventType::CommandFail,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        assert_eq!(state.active.len(), 2);
    }

    #[test]
    fn override_state_patched_tracking() {
        let mut state = OverrideState::default();
        assert!(!state.is_patched(EffectId::Starfield));
        state.set_patched(EffectId::Starfield);
        assert!(state.is_patched(EffectId::Starfield));
        assert!(!state.is_patched(EffectId::Particles));
        state.clear_patched(EffectId::Starfield);
        assert!(!state.is_patched(EffectId::Starfield));
    }

    #[test]
    fn override_state_clear_event() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.add(
            OverrideEventType::CommandFail,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.clear_event(OverrideEventType::Bell);
        assert_eq!(state.active.len(), 1);
        assert_eq!(state.active[0].event_type, OverrideEventType::CommandFail);
    }

    #[test]
    fn override_state_clear_all() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.add(
            OverrideEventType::CommandFail,
            EventOverride {
                duration_ms: 5000,
                ..Default::default()
            },
        );
        state.clear();
        assert!(state.active.is_empty());
    }

    #[test]
    fn override_state_update_removes_expired() {
        let mut state = OverrideState::default();
        // Add override with 0ms duration — immediately expired
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 0,
                ..Default::default()
            },
        );
        // 0ms duration means "persist", but is_active checks elapsed < duration
        // For 0ms, elapsed will always be >= 0ms, so it expires immediately
        let removed = state.update();
        assert!(removed);
        assert!(state.active.is_empty());
    }

    #[test]
    fn override_state_update_keeps_active() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 60000, // 60 seconds — won't expire during test
                ..Default::default()
            },
        );
        let removed = state.update();
        assert!(!removed);
        assert_eq!(state.active.len(), 1);
    }

    // ── compute_effect_patches tests ─────────────────────────────

    #[test]
    fn effect_patches_empty_when_no_overrides() {
        let state = OverrideState::default();
        let theme = Theme::default();
        let patches = compute_effect_patches(&state, &theme);
        assert!(patches.is_empty());
    }

    #[test]
    fn effect_patches_apply_when_starfield_patch_present() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                starfield_patch: Some(StarfieldPatch {
                    density: Some(200),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let theme = Theme::default();
        let patches = compute_effect_patches(&state, &theme);
        // Should have an Apply action for starfield
        assert!(!patches.is_empty());
        let has_starfield_apply = patches.iter().any(|p| {
            matches!(
                p,
                EffectPatchAction::Apply {
                    effect_id: EffectId::Starfield,
                    ..
                }
            )
        });
        assert!(has_starfield_apply, "Expected Apply for starfield");
    }

    #[test]
    fn effect_patches_no_double_apply_when_already_patched() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 5000,
                starfield_patch: Some(StarfieldPatch {
                    density: Some(200),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        // Mark it as already patched
        state.set_patched(EffectId::Starfield);
        let theme = Theme::default();
        let patches = compute_effect_patches(&state, &theme);
        // Should NOT re-apply starfield since it's already patched
        let has_starfield_apply = patches.iter().any(|p| {
            matches!(
                p,
                EffectPatchAction::Apply {
                    effect_id: EffectId::Starfield,
                    ..
                }
            )
        });
        assert!(
            !has_starfield_apply,
            "Should not re-apply already-patched effect"
        );
    }

    #[test]
    fn effect_patches_restore_when_patch_removed_and_theme_has_base() {
        let mut state = OverrideState::default();
        // No active overrides with starfield patch, but starfield IS marked as patched
        state.set_patched(EffectId::Starfield);
        // Theme needs a base starfield config for restore to happen
        let mut theme = Theme::default();
        theme.starfield = Some(crt_theme::StarfieldEffect::default());
        let patches = compute_effect_patches(&state, &theme);
        let has_starfield_restore = patches.iter().any(|p| {
            matches!(
                p,
                EffectPatchAction::Restore {
                    effect_id: EffectId::Starfield,
                    ..
                }
            )
        });
        assert!(has_starfield_restore, "Expected Restore for starfield");
    }

    #[test]
    fn effect_patches_no_restore_when_no_base_theme_config() {
        let mut state = OverrideState::default();
        // Patched but no base config in theme
        state.set_patched(EffectId::Starfield);
        let theme = Theme::default(); // starfield is None in default theme
        let patches = compute_effect_patches(&state, &theme);
        let has_starfield_restore = patches.iter().any(|p| {
            matches!(
                p,
                EffectPatchAction::Restore {
                    effect_id: EffectId::Starfield,
                    ..
                }
            )
        });
        assert!(
            !has_starfield_restore,
            "No restore without base theme config"
        );
    }

    #[test]
    fn effect_patches_multiple_effects() {
        let mut state = OverrideState::default();
        state.add(
            OverrideEventType::CommandFail,
            EventOverride {
                duration_ms: 5000,
                starfield_patch: Some(StarfieldPatch {
                    density: Some(100),
                    ..Default::default()
                }),
                matrix_patch: Some(crt_theme::MatrixPatch {
                    speed: Some(2.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let theme = Theme::default();
        let patches = compute_effect_patches(&state, &theme);
        let effect_ids: Vec<EffectId> = patches
            .iter()
            .filter_map(|p| match p {
                EffectPatchAction::Apply { effect_id, .. } => Some(*effect_id),
                _ => None,
            })
            .collect();
        assert!(effect_ids.contains(&EffectId::Starfield));
        assert!(effect_ids.contains(&EffectId::Matrix));
    }

    // ── ActiveOverride tests ─────────────────────────────────────

    #[test]
    fn active_override_long_duration_is_active() {
        use crate::window::ActiveOverride;
        let ov = ActiveOverride::new(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 60000,
                ..Default::default()
            },
        );
        assert!(ov.is_active());
        assert!(ov.intensity() > 0.9); // should be near 1.0 right after creation
    }

    #[test]
    fn active_override_zero_duration_expired() {
        use crate::window::ActiveOverride;
        let ov = ActiveOverride::new(
            OverrideEventType::Bell,
            EventOverride {
                duration_ms: 0,
                ..Default::default()
            },
        );
        // 0ms means expired immediately
        assert!(!ov.is_active());
        assert_eq!(ov.intensity(), 0.0);
    }

    // ── PtyUpdateResult tests ────────────────────────────────────

    #[test]
    fn pty_update_result_fields() {
        let result = PtyUpdateResult {
            content_changed: true,
            shell_events: vec![ShellEvent::Bell, ShellEvent::CommandFail(1)],
            title_change: Some("test".to_string()),
        };
        assert!(result.content_changed);
        assert_eq!(result.shell_events.len(), 2);
        assert_eq!(result.title_change.as_deref(), Some("test"));
    }

    #[test]
    fn pty_update_result_no_changes() {
        let result = PtyUpdateResult {
            content_changed: false,
            shell_events: vec![],
            title_change: None,
        };
        assert!(!result.content_changed);
        assert!(result.shell_events.is_empty());
        assert!(result.title_change.is_none());
    }

    // ── EffectPatchAction variant tests ──────────────────────────

    #[test]
    fn effect_patch_action_apply_variant() {
        let mut config = EffectConfig::new();
        config.insert("density".to_string(), "100".to_string());
        let action = EffectPatchAction::Apply {
            effect_id: EffectId::Starfield,
            config,
        };
        match action {
            EffectPatchAction::Apply { effect_id, config } => {
                assert_eq!(effect_id, EffectId::Starfield);
                assert_eq!(config.get("density").unwrap(), "100");
            }
            _ => panic!("Expected Apply variant"),
        }
    }

    #[test]
    fn effect_patch_action_restore_variant() {
        let config = EffectConfig::new();
        let action = EffectPatchAction::Restore {
            effect_id: EffectId::Particles,
            config,
        };
        match action {
            EffectPatchAction::Restore {
                effect_id,
                config: _,
            } => {
                assert_eq!(effect_id, EffectId::Particles);
            }
            _ => panic!("Expected Restore variant"),
        }
    }
}
