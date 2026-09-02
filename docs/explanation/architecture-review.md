# Architecture Review (v0.1.3)

> **Status (branch `claude/repo-architecture-review-jmqao6`):** every finding below has been addressed in code except the items listed under *Left open*. The workspace builds on Linux without GTK, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean, and all 745 tests pass (crt-core 21, crt-renderer 229, crt-theme 121, binary 287, integration 87).
>
> **Left open**
> - The theme → backdrop-effect patch path still passes a string `EffectConfig` (D4's typed patch enum); CSS values themselves are now parsed typed, so the remaining string hop is lossless for numbers and colours.
> - `src/bin/benchmark*.rs` are unchanged (still CPU-only loops; `CRT_BENCHMARK` is still read nowhere) and there is no headless full-frame benchmark yet.
> - Linux golden images for the visual tests were not generated (no GPU in the review environment). Visual tests now use the bundled font and fail rather than skip when `CRT_REQUIRE_GPU=1`; CI sets it only on macOS.
> - OSC 133 zones now follow scrolled text, but only until the scrollback buffer is full (absolute line indices stop advancing at the cap).
>
> **Not verified at runtime here.** This environment has no display or GPU, so the frame scheduler, the separable glow, HiDPI layout, the macOS `proc_pidinfo` cwd lookup and the input changes were validated by unit tests and review only. A manual smoke test on macOS and Linux is advised: idle CPU at 0 % with a static theme, 60 fps with the synthwave grid, tab switching, `ls --color` backgrounds, zsh paste, and Ctrl+C on Linux.

A deep read of the whole workspace (~39k lines across `crt-core`, `crt-renderer`, `crt-theme` and the `crt` binary), aimed at two questions: where are the real bugs, and where is the code spending time or memory it does not need to. Every finding below was verified against the source at the cited line, and several theme-parser findings were additionally confirmed by executing the parser on the inputs described.

The short version: the architecture is sound and the crate split is right. The problems are concentrated in a few places: the event loop never sleeps, the render loop redraws everything every frame and has a dead damage-tracking path, one class of GPU buffer is shared between passes in a way wgpu does not allow, and the CSS pipeline round-trips every value through strings three times. On Linux, keyboard handling is broken badly enough that the app is not usable as a shell.

---

## 1. The system as built

```
PTY reader thread ──(mpsc, 4 KiB chunks)──▶ Pty::read_available()
                                                  │  only the ACTIVE tab, once per frame
                                                  ▼
alacritty_terminal::Term  ◀── Terminal::process_input ── scan_osc133 (pre-parse byte scan)
        │ damage() [never reset]            │ selection saved/restored around parse
        ▼
WindowState::update_text_buffer          (runs when render.dirty)
   hash every cell.c ─▶ compare ─▶ collect cells ─▶ URL/path regex per line
   ─▶ active_shell_cwd()  [forks lsof on macOS]  ─▶ path stat() validation
   ─▶ prepare_render_cells ─▶ glyph_cache.position_char ─▶ GridRenderer instances
        ▼
render_frame (every RedrawRequested, ~60 Hz focused / 10 Hz unfocused)
   effects.update(dt = 1/60 constant)
   [vello] effects scene ─▶ Rgba8 target            (when any backdrop effect on)
   pass 1   background gradient   (clear)
   pass 1.25 effects blit
   pass 1.3  sprite (raw wgpu)
   pass 1.5  background image
   pass 3    cell backgrounds      RectRenderer   → rect_instance_buffer        ┐ same buffer,
   pass 3.5  output text           GridRenderer   → output_grid_instance_buffer │ same submit
   pass 4    clear text_texture; cursor-line text → grid_instance_buffer        │
   pass 4.5  composite: 17×17 Gaussian over full screen, every frame            │
   pass 5    cursor/selection/underlines  overlay RectRenderer                  │
   pass 6    tab bar shapes        RectRenderer   → rect_instance_buffer        ┘
   pass 7    tab titles            GridRenderer   → tab_title_instance_buffer
   pass 8+   search/rename/flash/context menu/toasts → overlay_text_instance_buffer (shared)
   final     CRT post-process (when enabled)
   queue.submit(once)  ─▶  present
```

Control flow is `ControlFlow::Poll` (`src/main.rs:43`). `about_to_wait` runs continuously and only *gates* `request_redraw` to 16.6 ms; it never blocks. Theme data flows CSS text → lightningcss typed AST → re-serialised strings → hand-written parsers → `Theme` (5.4 KB, deep-cloned into `WindowState`, `BackgroundPipeline`, `CompositePipeline`, `App`) → `to_uniforms()` per frame, and for backdrop effects → stringly `EffectConfig` → parsed a third time inside each effect.

Three things the existing docs say that the code does not do: `docs/explanation/rendering-pipeline.md` claims the loop runs "without busy-spinning" (it spins), that the glyph atlas is 2048² RGBA8 with fontdue metrics (it is 1024² R8, swash only, fontdue is unused), and that unfocused windows run at 1 fps (they run at 10).

---

## 2. Must-fix bugs (ordered by user impact)

| # | Where | What |
|---|-------|------|
| B1 | `src/input/keyboard.rs:318-321`, `src/input/mod.rs:779-793` | **Linux: no Ctrl+key ever reaches the shell.** On non-macOS the "primary" modifier is Ctrl, and `handle_shell_input` refuses to encode or forward anything while it is held. Ctrl+C/D/Z/L/R/A are dropped; default bindings also map Ctrl+C→Copy, Ctrl+W→CloseTab, Ctrl+T/Q/0-9→app actions. |
| B2 | `src/render/mod.rs:228-237` | **Only the active tab's PTY is drained.** Background tabs' reader threads keep filling an unbounded `mpsc` channel; a chatty background job grows memory without limit, and all of it is parsed at once on tab switch. Title changes and bells from background tabs are never seen. |
| B3 | `crates/crt-renderer/src/rect_renderer.rs:169-187`, `grid_renderer.rs:216`, `src/render/mod.rs:561-603` vs `965-1034` | **Instance buffers are written twice inside one submit.** `Queue::write_buffer` executes at the start of the next `submit`, so pass 3 (cell backgrounds) draws whatever pass 6 (tab bar) wrote into `rect_instance_buffer`. Cell backgrounds and search-match highlights are drawn from tab-bar data; the search bar, rename bar, bell flash, context menu and toasts all share `overlay_text_instance_buffer` the same way. The `overlay_rect_renderer` split at `src/gpu/mod.rs:228` fixed only one instance of this. |
| B4 | `crates/crt-renderer/src/tab_bar/layout.rs:83-87`; consumers `src/window/mod.rs:154`, `src/render/selection.rs:18`, `src/input/mouse.rs:436`, `src/input/mod.rs:980` | **HiDPI origin error.** `content_offset()` returns logical pixels; every consumer except `dialogs.rs` adds it to physical quantities. At 2× the text origin is 40 px too high, the first row sits under the tab bar, and click hit-testing disagrees with the drawn bar. |
| B5 | `crates/crt-theme/src/parser.rs:166-231` | **Theme hot-reload can crash the app.** `parse_hex_color` slices by byte index; `#éa` panics on a char boundary (confirmed by running the parser). A colour typo in any `.css` under `~/.config/crt/themes` takes the terminal down on save. |
| B6 | `crates/crt-theme/src/parser.rs:446-487`, `1623`, `1377` | **Documented properties silently ignored.** lightningcss files unknown property names as `Property::Custom`, which `extract_properties` routes to the `custom` map; `cursor-color`, `padding-x`, `padding-y` are looked up in `standard` and never found. `assets/themes/nyancat-responsive.css` uses `cursor-color` in four event blocks that never apply. Also: `background-color` is extracted but never read; `outline-*` is read but never extracted; `url()` and `deg` values in custom properties are dropped (so `--sprite-path: url(x.png)` yields no sprite). |
| B7 | `crates/crt-theme/src/parser.rs:81-157, 285-346` | **Valid CSS rejects the whole theme.** Any colour lightningcss prints as a keyword (`red`, `indigo`, `salmon`), any gradient with an angle other than 180°, any `oklch()`/`lab()` value → `Err(InvalidColor)`. The typed conversion `css_color_to_color` exists and is `#[allow(dead_code)]`. Parser options have `error_recovery = false`, so one stray rule anywhere rejects the file with a `Debug`-formatted message. |
| B8 | `crates/crt-renderer/src/background_image.rs:405-415`, `shaders/background_image.wgsl:43-49`, `background_image.rs:263-292` | **Background images render wrong.** Cover mode's UV scale is the reciprocal of the intended value (1000×500 screen, 500×500 image → samples v∈[0,2], squashed image plus a smeared last row). Every non-cover mode samples outside [0,1] with a ClampToEdge sampler and no `discard`, so edge pixels smear across the letterbox. `background-repeat` is parsed, a per-texture sampler is built, and neither is ever bound. |
| B9 | `src/render/overlays.rs:26` | **Bell flash is always black.** `Color` is already 0..1; dividing by 255 again makes `--flash-color` irrelevant. |
| B10 | `crates/crt-renderer/src/tab_bar/state.rs:152-165` | **Closing a tab left of the active one switches the active tab.** `close_tab` lacks the `else if active_tab > idx { active_tab -= 1 }` that `remove_tab` has. |
| B11 | `src/input/mod.rs:900-945` vs `src/app/mod.rs:177-192` | **CRT intermediate texture is never resized.** Only `text_texture` is re-checked-out in `handle_resize`; with CRT on, every pass renders into the original-size attachment and the final pass stretches it. |
| B12 | `crates/crt-renderer/src/sprite_renderer.rs:151-152, 618-620`; `crt-theme/src/parser.rs:1257,1263` | `--sprite-columns: 0` or `--sprite-frame-count: 0` → integer `% 0` panic on first frame (also in release). |
| B13 | `src/input/mod.rs:1347-1372` | **Paste is unsanitised.** Clipboard text containing `\x1b[201~` escapes bracketed paste; LF is not converted to CR outside bracketed mode; the first 50 chars of every paste are logged at `info`, which is the default filter level. |
| B14 | `src/input/mod.rs:1465-1473` | **Search panics on non-ASCII.** `start = match_start + 1` is a byte index into a lowercased string; matches are stored as byte offsets and compared to grid columns. |
| B15 | `src/input/mod.rs:1155-1208`, `src/input/mouse.rs:396-430` | **Mouse reporting protocol errors.** Every release is reported as *left* in SGR mode; a release outside the content area is never reported (apps see a stuck button); horizontal-only wheel is sent as wheel-down; no alternate-scroll (wheel→arrows) in the alt screen even though `alacritty_terminal` enables the mode; no modifier bits and no Shift bypass, so local selection is impossible under tmux/vim; trackpad sub-line deltas truncate to zero. |
| B16 | `src/input/mod.rs:729-742`, `src/input/key_encoder.rs:97-103` | Home/End send `^A`/`^E` (in vim, Home increments the number under the cursor). `application_cursor_keys` is hard-coded `false`, so ncurses apps under DECCKM get CSI instead of SS3. On macOS `logical_key` already has Option applied (`Option+f` → `ƒ`) so Meta bindings send `ESC ƒ`. No `WindowEvent::Ime` handling at all. |
| B17 | `src/watcher.rs:95-122` | **Debounce drops the final write.** Leading-edge debounce plus a truncate-then-write editor means the reload reads the half-written file and the completing event is discarded. Combined with `Config::load` returning `Default` on any error, one save can reset the app to default theme/font/shell until the next save. |
| B18 | `crates/crt-renderer/src/tab_bar/mod.rs:227-232` | `hit_test` indexes `state.tabs` with a layout index that is only refreshed in `prepare()` during `render_frame`; a click between a tab removal and the next redraw (or on an occluded window) is an out-of-bounds panic. |
| B19 | `src/window/interaction.rs:278-333` vs `src/render/context_menu.rs:101-178` | Context-menu hit testing and drawing use different geometry (no `padding_y`, separators not accounted for): the top 6–12 px of each item selects the previous one, and the Themes submenu stays open after hovering elsewhere. Keyboard users cannot reach the submenu. |
| B20 | `crates/crt-renderer/src/effects/renderer.rs:50,133`; `src/render/mod.rs:211-214` | Effect time is an `f32` accumulated by `+= 1/60`; it quantises after ~18 h and stops advancing after ~6 days of uptime. `dt` is a constant, so unfocused windows (10 fps) animate at 1/6 speed and frame drops slow time rather than skipping. |
| B21 | `effects/shape.rs:302-331`, `rain.rs:634-638`, `particles.rs:619-625`, `sprite_renderer.rs:674-689`, `matrix.rs:377-378` | Circle/ellipse glow is scaled about the screen origin, not the shape. Rain/particle hash-RNG strides overlap so a drop's brightness *is* the next drop's x. `Wander` motion uses a sawtooth "time" and pins the sprite to the right/bottom edge. Matrix churn is per-frame (fps-dependent) and its seed overflows after 49.7 days. |
| B22 | `src/app/handler.rs:81-86, 159-164` | Title-bar close and the CloseWindow binding remove the window directly, skipping `close_window()` (surface unconfigure, pool shrink). Closing the last window this way never calls `event_loop.exit()`, leaving a spinning process with no windows. |
| B23 | `crates/crt-core/src/lib.rs:175-307` | OSC 133 zones are recorded at the cursor line *before* the chunk is parsed, so a marker after a newline in the same chunk is attributed to the previous line; markers split across the 4 KiB read boundary are missed; zones are keyed by screen line and never shifted on scroll, so stale entries accumulate. |
| B24 | `src/input/drag.rs:111-112, 204` | Merging a tab past the last tab of the target window clamps to `len-1` and inserts second-to-last. Drag state is never cleared on focus loss or source-window close. |
| B25 | `crates/crt-renderer/src/grid_renderer.rs:74,172-179` | `MAX_INSTANCES = 32 768` silently drops glyphs. A 4K display at font scale 0.5 has ~87k cells; text vanishes from the bottom of the screen with no warning. |
| B26 | `crates/crt-renderer/src/glyph_cache.rs:316-348` vs `639-646` | Cell width is the *ink extent* of `M` at startup but the *advance width* after any zoom. Layout changes the first time you zoom, and reset-to-100% does not restore the startup layout. |
| B27 | `crates/crt-theme/src/parser.rs:299-306, 931-1093` | `to top`/`to left` gradient directions are ignored (a test asserts the wrong result). Effects auto-enable on inconsistent property subsets, and a disabled effect is never written back so a later `::backdrop` block cannot turn one off. |

---

## 3. Where the time and memory go

### Event loop and frame cadence

- **P1. Busy loop.** `ControlFlow::Poll` with a never-blocking `about_to_wait` (`src/app/handler.rs:527-647`) spins one core at 100 % while idle. The 60 fps throttle only gates `request_redraw`. This is the single biggest "lean" fix and the docs describe it as already solved.
- **P2. Everything redraws.** `render_frame` encodes all ~12 passes every `RedrawRequested` regardless of `dirty`; `dirty` only gates `update_text_buffer`. With any backdrop effect, sprite, GIF, CRT or cursor blink the window is redrawn at 60 Hz forever. An idle terminal with the default synthwave theme burns GPU continuously.
- **P3. Damage tracking is dead.** `Term::reset_damage()` is never called outside benchmarks. `TermDamageState` starts `full: true`, so `damaged_line_set()` always returns `None` and the whole partial-damage branch in `WindowState::update_text_buffer` (`src/window/mod.rs:373-455`) never executes. The `line_cells` / `line_decorations` per-line caches it maintains (`PreparedCell` cloned into a `HashMap` on every content change) are pure cost. If the branch were enabled it would also double-add `display_offset`: `TermDamageIterator` already returns viewport lines. Meanwhile the content hash (`mod.rs:119-137`) walks every cell each frame when effects are on, and hashes only `cell.c`, not colour or flags.
- **P4. Process spawn on the render path.** `update_text_buffer` calls `active_shell_cwd()` on every content change (`src/window/mod.rs:319`); on macOS that is `Command::new("lsof")` synchronously on the main thread (`crates/crt-core/src/pty.rs:379-382`), tens to hundreds of ms per keystroke echo. On Linux it is a `/proc` readlink per change.
- **P5. Debug formatting at full cost.** `process_pty_output` (`crates/crt-core/src/lib.rs:601-624`) builds an escaped copy of every PTY chunk under 2000 bytes, then `from_utf8_lossy` + two `contains`, before `log::debug!` decides to discard it. This runs on every chunk at the default `warn` level.

### GPU work per frame

- **P6. 289-tap blur, full screen, every frame.** `composite.wgsl:52-53` is a 17×17 loop over the whole render target; the text texture it reads is also cleared and re-rendered every frame although it only changes on dirty frames. (Comments say 25 and 18×18.)
- **P7. Unconditional uploads.** `GridRenderer::render` and `RectRenderer::render` `write_buffer` the full instance vec every frame even when nothing changed (10k glyphs ≈ 480 KB/frame). `ThemeUniforms` (144 B) is written three times per frame (`render/mod.rs:374, 677`) though neither shader reads `time`. Background-image UV/opacity uniforms are recomputed and written per frame.
- **P8. Vello renderer recreated every 5 s.** `VELLO_RESET_INTERVAL` (`render/mod.rs:174,201-208`) drops and rebuilds `vello::Renderer` with `pipeline_cache: None` every 300 frames *per window*, recompiling its compute shaders: a periodic hitch for any theme with effects, N× more often with N windows. It is also rebuilt on every window close.
- **P9. Static effects re-encoded.** `EffectsRenderer::render` resets and re-encodes the vello scene every frame even for a static grid or non-twinkling starfield. Matrix allocates a `BezPath` per trail glyph per frame (up to ~6000), particles one per shape, grid one per curved line; starfield filters all stars once per layer.
- **P10. Dead vello tab renderer.** `TabBar::prepare` rebuilds a vello scene of rounded rects and strokes per frame and owns a `screen_width × bar_height` storage texture per window; `render_vello` has no callers. The tab bar is actually drawn by `render_shapes_to_rects`.
- **P11. Texture pool sizing.** Buckets round up to 64 px and the bucket-sized texture is bound as-is, so the glow layer (and the whole frame under CRT) is rasterised at 1.007–1.03× and resampled: phase-varying blur. No global cap: an interactive resize leaves 1–2 textures in every bucket crossed, released only on window close.

### Memory and allocation

- **P12. Theme copies.** `Theme` is 5.4 KB with heap fields, deep-cloned ~6× per window per reload and stored in four places (`WindowState`, `BackgroundPipeline`, `CompositePipeline`, `App`). Every file event re-parses all 19 shipped themes because the watcher discards the path. `::palette` handling does 240 `format!` + lookups per rule. Colours are parsed three times and quantised twice (`(x*255) as u8` truncates 0.999 → 254) on the way to the effects.
- **P13. Font bytes.** `load_font_variants` reads the font files from disk per window (and per scale change), stores 4 `Vec<u8>` copies (missing variants are `regular.clone()`), then `font_variants.clone()` duplicates all four again for the tab cache: up to 8 copies of a ~2.5 MB file per window.
- **P14. Eager buffers.** Each `GridRenderer`/`RectRenderer` reserves its full capacity (1.5 MB / 512 KB heap) at construction; tab titles and overlay text each hold a 1.5 MB GPU instance buffer for <1k glyphs. The renderer instance vecs are 32-bit-index-capacity but see B25.
- **P15. Per-frame allocations.** `get_tab_labels` clones every title `String` each frame; `ContextMenu::items()`/`theme_items()` rebuild `Vec`s per frame and per hover; `get_line_zone` is boxed per update; `take_events` allocates a `Vec` per frame; `Pty::write` copies every keystroke; during a drag `tab_rects().to_vec()` for every window on every event.
- **P16. Search and profiling.** `update_search_matches` calls `all_lines_text()` and `to_lowercase()` over the whole scrollback per typed character. With `CRT_PROFILE` on, `render_frame` copies the whole scrollback every frame and the 5 s rate limit is applied *after* the work.

### Build and dependencies

- **P17. GTK on Linux for nothing.** `muda` is only used under `#[cfg(target_os = "macos")]` but is an unconditional dependency; it and `clipboard-files` pull `gtk`, `gdk`, `cairo`, `pango`, `gio`, `glib` (the Linux build here fails on `gdk-pixbuf-sys` for lack of system headers). 442 crates in the tree.
- **P18. No release profile.** `Cargo.toml` has no `[profile.release]`: no LTO, default 16 codegen units, unwinding panics. For a binary that ships to end users this is free speed and size.

---

## 4. Proposed designs

These are interface-level sketches, in priority order. Each is independent enough to land on its own.

### D1. A sleeping event loop with explicit wake sources

```rust
enum WakeReason { PtyOutput(WindowId, TabId), ConfigChanged, ThemeChanged(PathBuf) }

// PTY reader thread: after each successful read
proxy.send_event(WakeReason::PtyOutput(window, tab)).ok();

// App::about_to_wait
let next = self.windows.values()
    .filter_map(|w| w.next_deadline(now))   // None when nothing is animating
    .min();
event_loop.set_control_flow(match next {
    Some(t) => ControlFlow::WaitUntil(t),
    None    => ControlFlow::Wait,
});
```

`WindowState::next_deadline` returns `now + 16.6 ms` while an effect/sprite/GIF/CRT flicker is animating, the next blink toggle while the cursor blinks, and `None` otherwise. The macOS drawable-growth concern that motivated `Poll` is preserved: the cap is still 60 fps, it just no longer costs a core to enforce. This subsumes P1 and most of P2.

### D2. One invalidation model built on alacritty damage

Replace the content hash and the dead partial-damage caches with:

```rust
pub enum TextInvalidation { None, Lines(SmallVec<[usize; 8]>), Full }

impl Terminal {
    /// Consumes damage. Call once per frame, after `process_input`.
    pub fn take_damage(&mut self) -> TextInvalidation { let d = ...; self.term.reset_damage(); d }
}
```

Damage from `alacritty_terminal` already covers cursor moves, selection, scroll (as `Full`), and attribute-only changes. `update_text_buffer` then rebuilds glyph instances only when invalidation is not `None`, and uploads (P7) only when it rebuilt. Hover-underline, search-highlight and theme changes call a `mark_full()` on the window. Selection rectangles stay a separate overlay layer as today. Delete `content_hashes`, `line_cells`, `line_decorations`, the paste `INVERSE` normalisation heuristic (it exists to paper over the hash not seeing flags), and the `frame_count < 60` force-redraw.

### D3. Per-pass GPU allocation

The buffer-sharing bug (B3) and the eager 1.5 MB buffers (P14) have the same fix: a per-frame bump allocator over one pooled buffer.

```rust
pub struct FrameArena { buffer: wgpu::Buffer, cursor: u64, staging: Vec<u8> }
impl FrameArena {
    pub fn begin(&mut self);
    pub fn push<T: Pod>(&mut self, items: &[T]) -> BufferSlice;   // returns offset..len
    pub fn flush(&self, queue: &wgpu::Queue);                       // one write_buffer per frame
}
// each renderer: render_pass.set_vertex_buffer(0, arena.buffer.slice(slice.range()))
```

One `write_buffer` per frame, every pass gets its own range, and instance vecs can be `Vec::new()` that grow to what the window actually uses. Grow the arena (and drop B25's silent 32k cap) when a frame exceeds it.

### D4. Typed theme pipeline behind `Arc<Theme>`

- `ThemeRegistry::get_theme(&str) -> Option<Arc<Theme>>`; pipelines and `WindowState` hold the `Arc`; `set_theme(Arc<Theme>)` everywhere. `App.theme` goes away.
- `extract_properties` stops re-serialising: match on `PropertyId` and convert `CssColor` via `to_rgb()`; read `LinearGradient.direction` and stops directly; route `CustomPropertyName::Unknown` into the same typed path as known properties (fixes B6/B7). `ParserOptions { error_recovery: true }` and a `ParseReport { theme, warnings: Vec<Warning> }` return type so the UI toast can show what was ignored.
- `EffectConfig` becomes an enum of typed patches (`GridPatch`, `StarfieldPatch`, …) that the effects consume without string parsing; `to_config_pairs()` and each effect's `parse_color` go away.
- `ThemeChanged(PathBuf)` from the watcher so only the changed theme is re-parsed; trailing-edge debounce.

### D5. PTY ownership and back-pressure

- Drain *every* tab's PTY each wake, not only the active one (B2). It is already cheap; the parse work happens anyway on tab switch.
- Replace the unbounded `mpsc<Vec<u8>>` with a bounded channel (`sync_channel(N)`) so a runaway background job applies back-pressure to the child instead of to the heap, and raise the read chunk from 4 KiB to 64 KiB for bulk output.
- Cache `cwd` in `WindowState`, refreshed at most once per second and on OSC 7; on macOS replace `lsof` with `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
- Wrap the escaped-chunk debug string in `if log::log_enabled!(Debug)`.

### D6. Input model

- Separate the *application* modifier from Ctrl: `enum AppMod { Super, CtrlShift }` chosen per platform. Ctrl+key always goes to the PTY unless a binding explicitly claims that exact chord; on Linux default app bindings to Ctrl+Shift like other terminals.
- `encode_key(key, mods, term_mode: TermMode)`: set `application_cursor_keys` and `newline_mode` from the terminal; drop the Home/End overrides; on macOS use `key_without_modifiers()` when Option is held.
- Route mouse releases through the same button-aware encoder as presses; clamp instead of returning `None` outside the content area; carry modifier bits; Shift bypasses reporting; alternate-scroll in the alt screen; accumulate fractional wheel deltas.
- Sanitise paste (strip `\x1b[201~` and C0 except `\t\r\n`; LF→CR when not bracketed); demote the paste log.

### D7. Effects timing and cost

- `time: f64` accumulated from a real `Instant` delta clamped to `[0, 0.1]`.
- `BackdropEffect::is_animated(&self) -> bool`; skip the vello render and reuse the cached target when nothing is animated and size/config are unchanged.
- Pre-build unit paths once and draw with an `Affine`; group stars by layer at generation.
- Retire the 300-frame vello reset; if atlas growth is real, reset on a wall-clock interval measured in minutes and pass a `wgpu::PipelineCache`.
- Make the glow composite separable (two 17-tap passes) and run it only over the cursor-line rows via scissor, only when the text texture changed.

### D8. Build hygiene

```toml
[profile.release]
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true

[target.'cfg(target_os = "macos")'.dependencies]
muda = { workspace = true }          # move out of [dependencies]
```

Feature-gate or replace `clipboard-files` on Linux (it is the other GTK pull), set `rust-version`, and add `cargo fmt --check` + `clippy` to CI.

---

## 5. Tests and CI

- Visual golden tests exist only as `*.macos.png`; on Linux with a GPU they fail 100 %, and on CI they "pass" because every test returns early without a GPU. Bundle `assets/fonts/MesloLGS-NF-Regular.ttf` via `include_bytes!` so goldens are machine-independent, generate Linux goldens under lavapipe, and fail when `CRT_REQUIRE_GPU=1`.
- The visual tests hand-draw rectangles that *resemble* tabs, cursor and selection; nothing exercises `TabBar`, `render_tab_titles`, `render_selection_rects`, dialogs or the context menu, so none of B3, B4, B9, B10, B18, B19 could be caught today.
- `crt-theme`'s benchmark uses selectors and properties the parser does not recognise (`:root`, `--foreground`, `--cursor-color`); it measures lightningcss parsing of ignored rules and returns `Theme::minimal()` every time.
- `benchmark.rs` / `benchmark_gpu.rs` are CPU-only parser loops with `sleep`; `CRT_BENCHMARK=1`, which `scripts/benchmark.sh` sets, is read nowhere. Nothing benchmarks `update_text_buffer`, glyph layout, or a headless frame.
- Add: `TabBarState` close-left-of-active, zero-tab layout, a context-menu layout↔hit-test round trip, a per-pass buffer test, a non-ASCII search test, a Ctrl+C-on-Linux keyboard test, and a `Terminal::take_damage` test that asserts `reset` happens.

---

## 6. Suggested order of work

1. **Unblock Linux**: B1 (Ctrl keys), B22 (exit on last window), P17 (drop GTK).
2. **Correctness that users will hit daily**: B2 (background tabs), B3 (buffer sharing, via D3), B4 (HiDPI origin), B9, B10, B11, B13, B14, B15.
3. **Lean loop**: D1 (sleep + wake), D2 (damage), P4/P5 (no lsof, no debug formatting), P7/P8 (uploads, vello reset).
4. **Theme pipeline**: B5–B7 and D4 together, since they share the same code path.
5. **Effects and images**: B8, B12, B20, B21, D7.
6. **Memory**: D4 `Arc<Theme>`, P13 `Arc<[u8]>` fonts loaded once, P14 arena, P11 pool cap.
7. **Tests**: section 5, so the above stays fixed.
