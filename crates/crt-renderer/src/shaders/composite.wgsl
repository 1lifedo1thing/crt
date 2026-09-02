// Composite shader - glow blur for the cursor-line text layer.
//
// The Gaussian is separable: `fs_hblur` writes the horizontally blurred text
// alpha into an R8 texture (only when the text changed), and `fs_main` blurs
// that vertically and composites the text on top. 17 + 17 taps per pixel
// instead of 17 x 17, and the app scissors both passes to the rows that hold
// glyphs.

struct Params {
    screen_size: vec2<f32>,
    time: f32,
    grid_intensity: f32,
    gradient_top: vec4<f32>,
    gradient_bottom: vec4<f32>,
    grid_color: vec4<f32>,
    grid_spacing: f32,
    grid_line_width: f32,
    grid_perspective: f32,
    grid_horizon: f32,
    glow_color: vec4<f32>,
    glow_radius: f32,
    glow_intensity: f32,
    text_color: vec4<f32>,
    _pad: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var text_texture: texture_2d<f32>;
@group(0) @binding(2) var text_sampler: sampler;

// Horizontally blurred alpha (written by fs_hblur, read by fs_main)
@group(1) @binding(0) var blur_texture: texture_2d<f32>;
@group(1) @binding(1) var blur_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32(i32(vertex_index & 1u)) * 4.0 - 1.0;
    let y = f32(i32(vertex_index >> 1u)) * 4.0 - 1.0;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

// Number of taps on each side of the centre (17 taps total)
const SAMPLES: i32 = 8;

fn effective_radius() -> f32 {
    return min(params.glow_radius, 50.0);
}

fn gauss_weight(i: i32, sigma: f32) -> f32 {
    let d = f32(i);
    return exp(-(d * d) / (2.0 * sigma * sigma));
}

// One-dimensional 17-tap Gaussian of `channel` along `dir` (in texels).
fn blur_1d(tex: texture_2d<f32>, samp: sampler, uv: vec2<f32>, dir: vec2<f32>, use_alpha: bool) -> f32 {
    let radius = effective_radius();
    let sigma = radius / 3.0;
    let step = dir * (1.0 / params.screen_size) * (radius / f32(SAMPLES));

    var total = 0.0;
    var weight_sum = 0.0;
    for (var i = -SAMPLES; i <= SAMPLES; i++) {
        let w = gauss_weight(i, sigma);
        let s = textureSample(tex, samp, uv + step * f32(i));
        let v = select(s.r, s.a, use_alpha);
        total += v * w;
        weight_sum += w;
    }
    return total / weight_sum;
}

// Pass 1: horizontal blur of the text alpha into the R8 blur texture.
@fragment
fn fs_hblur(in: VertexOutput) -> @location(0) vec4<f32> {
    if params.glow_intensity <= 0.0 {
        return vec4<f32>(0.0);
    }
    let a = blur_1d(text_texture, text_sampler, in.uv, vec2<f32>(1.0, 0.0), true);
    return vec4<f32>(a, 0.0, 0.0, 1.0);
}

// Pass 2: vertical blur of the horizontal result, then text on top.
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sample the text texture (contains colored glyphs)
    let text = textureSample(text_texture, text_sampler, in.uv);
    let text_alpha = text.a;

    // Start with transparent
    var color = vec3<f32>(0.0, 0.0, 0.0);
    var alpha = 0.0;

    // Glow effect (if enabled) - render glow behind text
    if params.glow_intensity > 0.0 {
        let blur = blur_1d(blur_texture, blur_sampler, in.uv, vec2<f32>(0.0, 1.0), false);
        let glow_alpha = blur * params.glow_intensity * 2.0;
        color = params.glow_color.rgb;
        alpha = min(glow_alpha, 0.8);
    }

    // Blend text on top (preserving original colors from texture)
    if text_alpha > 0.01 {
        color = mix(color, text.rgb, text_alpha);
        alpha = max(alpha, text_alpha);
    }

    return vec4<f32>(color, alpha);
}
