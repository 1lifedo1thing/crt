# Rendering Pipeline

CRT's rendering pipeline is designed around a core tension: the background effects animate continuously at 60 fps, but re-rendering the entire terminal grid every frame would be wasteful, because terminal content changes rarely compared to the animation rate. The solution is a multi-pass architecture where different layers of the image are produced at different rates.

## The Big Picture

Each frame follows this sequence:

```
1. Background gradient        ─► render_target (clear + draw)
2. Backdrop effects           ─► intermediate Vello texture ─► blit to render_target
3. Sprite animation           ─► render_target (load + draw)
4. Background image           ─► render_target (load + draw)
5. Cell backgrounds (RectRenderer) ─► render_target
6. Text glyphs (GridRenderer) ─► text_texture (only when content hash changes)
7. Composite (text + glow)    ─► render_target
8. Overlays (cursor, selection, search) ─► render_target
9. Tab bar                    ─► render_target
10. [CRT post-processing]     ─► surface  (only if CRT effect enabled)
```

When the CRT post-processing effect is enabled, `render_target` is an intermediate texture rather than the surface directly. Step 10 reads from that intermediate texture and outputs to the actual window surface. When CRT is disabled, `render_target` is the surface and step 10 is skipped.

Each step uses a separate wgpu render pass with `LoadOp::Load` (except the background pass which uses `LoadOp::Clear`). This means each pass accumulates on top of what previous passes drew, rather than overwriting.

## Two-Pass Text Rendering

The most important performance optimization in CRT is separating background rendering from text rendering.

Text is rendered to a dedicated `text_texture` — a full-resolution RGBA8 offscreen texture. This texture is only updated when the terminal content actually changes. On frames where nothing in the terminal has changed (the user is not typing, no program is producing output), the text rendering passes are skipped entirely and the previous frame's `text_texture` is reused.

The composite pass then samples from `text_texture` to draw the text onto the main render target, applying the glow/shadow effect at composite time. This means glow is computed once in the fragment shader rather than being baked into the texture.

Without this optimization, a 60 fps terminal with an animated background would re-rasterize every glyph 60 times per second regardless of whether anything had changed. With it, glyph rasterization only happens when the terminal content changes, which on a normal interactive session is far less than once per frame.

## Damage Tracking

The text layer is rebuilt only when something changed. `alacritty_terminal` records damage as it parses: which visible lines changed, cursor movement, selection changes, and scrolls or resizes (reported as full damage). Once per frame `Terminal::take_damage()` consumes and resets that record; if it reports no damage and nothing in the UI has forced a rebuild (hover underline, search highlight, theme change), `update_text_buffer` returns early and the previously uploaded glyph instances are drawn again.

Because the damage record already covers attribute-only changes (a colour changing under the same character), there is no separate content hash. The `render.dirty` flag means "terminal content may have changed"; it is set when PTY output was parsed for the active tab and cleared when the text layer is rebuilt.

## Glyph Caching

Font rendering on the CPU is expensive. Rasterizing a glyph involves loading the font file, scaling the outline to the target size, applying anti-aliasing, and producing a pixel bitmap. Doing this for every character on every frame would be untenable.

`GlyphCache` rasterizes each glyph once and stores the bitmap in a GPU texture atlas. The atlas is a 1024×1024 single-channel (`R8Unorm`) texture where each glyph occupies a rectangular region. The `AtlasPacker` assigns regions using a simple shelf-packing algorithm (row-by-row, advancing to a new row when the current one fills). When the atlas fills, it is cleared and repopulated from the glyphs currently on screen.

Glyph lookup uses a `GlyphKey` consisting of character code, size (in tenths of a point for integer representation), and style flags (bold, italic). The cache returns a `CachedGlyph` containing normalized UV coordinates into the atlas and the offset needed to position the glyph within its cell.

CRT uses **swash** for both glyph rasterization and font metrics. The cell width is the advance width of `M`, which is the same metric used after a zoom, so the layout does not shift the first time the font size changes. Font file bytes are loaded once per family and shared between windows and glyph caches as `Arc<[u8]>`.

The glyph cache pre-populates ASCII characters (code points 32–127) during initialization. This means the first frame renders immediately without any cache misses for the common case of plain ASCII text.

When the display scale factor changes — typically from moving the window to a display with a different DPI — the glyph cache is recreated from scratch. Font metrics are resolution-dependent: a glyph rasterized at 1x DPI looks blurry when displayed on a 2x retina screen because it was rasterized at half the required resolution. Rather than caching at multiple resolutions, CRT simply rebuilds the cache for the new DPI.

## GPU Resource Sharing Across Windows

GPU pipelines (render pipelines, bind group layouts, samplers) are expensive to create and are immutable after creation. On macOS/Metal, each pipeline object includes the compiled Metal shader and its cache, costing 5–15 MB each. Naively, three windows with the same shaders would hold three independent copies.

`SharedPipelines` solves this by wrapping each pipeline in an `Arc`. All windows share references to the same compiled pipeline objects. `SharedGpuState` owns the canonical `SharedPipelines` instance and creates it once on the first window. Subsequent windows receive `Arc::clone()` handles — cheap pointer copies.

The split between shared and per-window state is:

**Shared (one copy total):**
- Compiled render pipelines (WGSL compiled to Metal/Vulkan/D3D12)
- Bind group layouts
- Samplers

**Per-window:**
- Uniform buffers (gradient colors, glow parameters, screen dimensions)
- Bind groups (which bind per-window uniform buffers to pipeline slots)
- Instance buffers (per-glyph position, UV, color data)
- Render target textures (text texture, CRT intermediate texture)
- Glyph atlas textures

The `SharedGpuState` also holds the `wgpu::Device`, `wgpu::Queue`, and `wgpu::Adapter`, which are inherently shared resources at the hardware level.

## Texture and Buffer Pooling

GPU texture allocation is slow. On macOS, allocating a full-resolution RGBA8 render target (e.g., 2560×1600 at 2x DPI) involves a kernel call to allocate backing memory and, in some configurations, an IOSurface handoff to the display server.

The `TexturePool` maintains a small set of pre-allocated textures organized by size bucket. When a window needs a render target, it calls `pool.checkout(width, height, format)`, receiving a `PooledTexture` RAII wrapper. When the `PooledTexture` is dropped (e.g., when the window closes), it is returned to the pool rather than destroyed. The next window to request a texture of the same size receives the pooled one immediately.

Pool depth is capped at 2 textures per size bucket. This means at most 2 idle textures of any given resolution are held in memory at once. After a window closes, `pool.shrink()` is called to release excess entries above this cap.

The same pattern applies to `BufferPool` for GPU buffers (instance data, uniforms), though buffer pooling is less critical than texture pooling because buffers are smaller and cheaper to allocate.

## Frame Scheduling

The event loop sleeps. It runs with `ControlFlow::Wait` (or `WaitUntil` when a deadline is pending) and is woken by three things:

- **PTY output.** Each PTY reader thread calls a wake function after queueing a chunk. The wake is coalesced through an atomic flag, so a burst of small reads produces one user event. On wake, `App::drain_ptys` parses output for *every* tab of every window, so background tabs never fall behind and their output queue (bounded, so a runaway job blocks on `write` instead of growing the heap) never fills.
- **File changes.** The config watcher wakes the loop; events are debounced on the trailing edge so a save is read only once it is complete.
- **Deadlines.** Each window reports `next_frame_deadline`: "now" when its content is dirty, the next 16.6 ms tick (100 ms when unfocused) while an effect, sprite, animated image, CRT flicker, bell flash or override is running, the next blink toggle when only the cursor blinks, and nothing at all otherwise. The loop waits until the earliest deadline.

An idle terminal with a static theme therefore uses no CPU and no GPU between keystrokes. The 60 fps cap for animated windows is what keeps Metal's drawable allocations bounded on macOS; it is enforced by the deadline, not by spinning.

**Occluded windows** are skipped entirely; their PTYs are still drained.

## Vello Resource Use

The vello renderer is created lazily the first time a backdrop effect needs it and lives for the process. Backdrop effects re-encode their scene only when something animates or their configuration changed; a static grid or starfield reuses the previously rendered target.

## The Text Rendering Sub-Pipeline

The text rendering sub-pipeline (passes 5–7 in the overall sequence) deserves its own walkthrough because it has the most moving parts.

### Cell Backgrounds (Pass 5)

Before any glyphs are drawn, the `RectRenderer` fills in cell backgrounds. ANSI escape sequences can specify a background color for any cell; many programs (vim, tmux, htop) use this extensively. Each colored background region is one quad in an instanced draw call. A separate `overlay_rect_renderer` handles selection highlights, cursor underlining, and search match highlights on a later pass.

### Text Glyphs (Pass 6)

The `GridRenderer` renders glyphs using instanced quads. Each glyph is a `GlyphInstance` struct containing:
- Screen position (top-left of the glyph bitmap within the cell)
- UV coordinates into the atlas texture (where the glyph bitmap lives)
- Glyph size in pixels
- RGBA color

The GPU renders all glyphs in a single draw call — one instanced quad per glyph, all sampling from the same atlas texture. The vertex shader positions each quad; the fragment shader samples the atlas and multiplies the alpha by the instance color.

CRT uses two separate `GridRenderer` instances for the "glow" pass and the "flat" pass. Prompt and input lines (identified via OSC 133 semantic zones) render with a glow effect applied in the composite shader. Output lines render without glow. This produces the visual effect of the cursor line "glowing" while output text is rendered cleanly.

### Composite Pass (Pass 7)

The composite pass blends `text_texture` onto the main render target. This is where the glow/shadow effect is applied: the fragment shader samples the text texture and applies a Gaussian-like bloom to bright (non-transparent) pixels. The bloom radius and intensity are shader uniforms driven by the `TextShadow` values in the current `Theme`.

This two-step approach (render text flat, apply glow at composite time) means glow can be changed at theme-load time without invalidating the glyph atlas. It also means glow intensity can be animated or event-driven without re-rasterizing any glyphs.

## CRT Post-Processing

The optional CRT post-processing effect is the final transformation in the pipeline. When enabled, all previous passes render to an intermediate texture (`crt_texture`) rather than the window surface. The `CrtPipeline` then reads from this intermediate texture, applies its fragment shader, and outputs to the window surface.

The CRT shader is parameterized by a `CrtUniforms` buffer updated each frame:

- `scanline_intensity` / `scanline_frequency`: Simulates horizontal scan lines
- `curvature`: Barrel distortion that makes the screen look curved like a CRT tube
- `vignette`: Darkens the corners to simulate CRT phosphor falloff
- `chromatic_aberration`: Slightly offsets the R and B channels to simulate imperfect electron gun alignment
- `bloom`: Adds local glow to bright areas
- `flicker`: Applies a subtle per-frame brightness oscillation using `time`

The `time` uniform is the elapsed seconds since the CRT pipeline was created, passed to the shader each frame. This makes flicker genuinely time-varying rather than a static effect.

The `reference_height` uniform normalizes effect intensity across display resolutions. The shader scales scanline frequency relative to a 1080p reference, so the same theme values produce the same visual appearance whether running on a 720p or 4K display.
