//! Rain backdrop effect - falling raindrops with wind angle
//!
//! Renders raindrops falling from top to bottom with configurable
//! density, speed, angle (wind), and drop appearance.
//!
//! ## CSS Properties
//!
//! - `--rain-enabled: true|false`
//! - `--rain-color: <color>` (raindrop color)
//! - `--rain-density: <number>` (number of drops, default 100)
//! - `--rain-speed: <number>` (fall speed multiplier)
//! - `--rain-angle: <number>` (wind angle in degrees, 0 = vertical)
//! - `--rain-length: <number>` (drop length in pixels)
//! - `--rain-thickness: <number>` (drop thickness in pixels)
//! - `--rain-glow-radius: <number>` (glow spread, 0 = no glow)
//! - `--rain-glow-intensity: <number>` (0-1 glow brightness)

use vello::Scene;
use vello::kurbo::{Affine, Line, Rect, Stroke};
use vello::peniko::{Brush, Color};

use super::{BackdropEffect, EffectConfig};

/// Individual raindrop data
#[derive(Debug, Clone, Copy)]
struct Raindrop {
    /// X position (0-1 normalized)
    x: f64,
    /// Y position (0-1 normalized, 0 = top)
    y: f64,
    /// Speed multiplier for this drop
    speed: f64,
    /// Length multiplier for this drop
    length: f64,
    /// Brightness multiplier (0-1)
    brightness: f64,
}

/// RGBA color stored as u8 components
#[derive(Debug, Clone, Copy)]
struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl Rgba {
    fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    fn with_alpha(&self, alpha: u8) -> Color {
        Color::from_rgba8(self.r, self.g, self.b, alpha)
    }
}

/// Rain effect with configurable angle and density
pub struct RainEffect {
    /// Whether the effect is enabled
    enabled: bool,

    /// Raindrop color
    color: Rgba,

    /// Number of raindrops
    density: usize,

    /// Base fall speed
    speed: f64,

    /// Wind angle in degrees (0 = vertical, positive = right, negative = left)
    angle: f64,

    /// Base drop length in pixels
    length: f64,

    /// Drop thickness in pixels
    thickness: f64,

    /// Glow radius in pixels (0 = no glow)
    glow_radius: f64,

    /// Glow intensity (0-1)
    glow_intensity: f64,

    /// All raindrops
    drops: Vec<Raindrop>,

    /// Current time for animation
    time: f64,

    /// Seed for reproducible positions
    seed: u64,

    /// Whether drops need regeneration
    needs_regeneration: bool,
}

impl Default for RainEffect {
    fn default() -> Self {
        Self {
            enabled: false,
            color: Rgba::new(150, 180, 220, 180), // Light blue-gray
            density: 150,
            speed: 1.0,
            angle: 0.0,
            length: 20.0,
            thickness: 1.5,
            glow_radius: 0.0,
            glow_intensity: 0.0,
            drops: Vec::new(),
            time: 0.0,
            seed: 54321,
            needs_regeneration: true,
        }
    }
}

impl RainEffect {
    /// Create a new rain effect with default settings
    pub fn new() -> Self {
        Self::default()
    }

    /// Hash-based pseudo-random number generator
    fn random(&self, index: usize) -> f64 {
        let mut x = (index as u64).wrapping_add(self.seed);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^= x >> 33;
        (x as f64) / (u64::MAX as f64)
    }

    /// Number of random values consumed per drop.
    ///
    /// The hash RNG is indexed, so each drop must own a non-overlapping
    /// range of indices; otherwise one drop's last value is the next drop's
    /// first value and neighbours become correlated.
    const RNG_STRIDE: usize = 5;

    /// Generate raindrops
    fn generate_drops(&mut self) {
        self.drops.clear();
        self.drops.reserve(self.density);

        for i in 0..self.density {
            let base = i * Self::RNG_STRIDE;
            let x = self.random(base);
            let y = self.random(base + 1);
            let speed = 0.7 + self.random(base + 2) * 0.6; // 0.7-1.3
            let length = 0.6 + self.random(base + 3) * 0.8; // 0.6-1.4
            let brightness = 0.4 + self.random(base + 4) * 0.6; // 0.4-1.0

            self.drops.push(Raindrop {
                x,
                y,
                speed,
                length,
                brightness,
            });
        }

        self.needs_regeneration = false;
    }

    /// Calculate raindrop position with wrapping
    fn drop_position(&self, drop: &Raindrop, width: f64, height: f64) -> (f64, f64) {
        // Convert angle to radians
        let angle_rad = self.angle.to_radians();

        // Calculate movement per second
        let base_speed = 300.0 * self.speed * drop.speed;
        let dx = angle_rad.sin() * base_speed;
        let dy = angle_rad.cos() * base_speed;

        // Calculate current position with time offset
        let x_offset = dx * self.time;
        let y_offset = dy * self.time;

        // Base position scaled to screen
        let base_x = drop.x * width;
        let base_y = drop.y * height;

        // Add movement and wrap
        let mut x = base_x + x_offset;
        let mut y = (base_y + y_offset) % height;

        // Handle negative wrap for y
        if y < 0.0 {
            y += height;
        }

        // Wrap x with extra padding for angled rain
        let x_range = width + (self.angle.abs() / 45.0 * width).min(width);
        x = ((x % x_range) + x_range) % x_range - (x_range - width) / 2.0;

        (x, y)
    }

    /// Draw a raindrop with optional glow
    fn draw_drop(&self, scene: &mut Scene, x: f64, y: f64, length: f64, color: Color) {
        // Calculate end point based on angle
        let angle_rad = self.angle.to_radians();
        let end_x = x + angle_rad.sin() * length;
        let end_y = y + angle_rad.cos() * length;

        let line = Line::new((x, y), (end_x, end_y));

        // Draw glow layers if enabled
        if self.glow_radius > 0.0 && self.glow_intensity > 0.0 {
            let [r, g, b, a] = color.components;
            let glow_layers = 3;

            for i in (0..glow_layers).rev() {
                let t = (i + 1) as f64 / glow_layers as f64;
                let glow_width = self.thickness + self.glow_radius * 2.0 * t;
                let glow_alpha = (a as f64 * self.glow_intensity * (1.0 - t) * 0.4) as f32;

                if glow_alpha > 0.001 {
                    let glow_color = Color::new([r, g, b, glow_alpha]);
                    let stroke = Stroke::new(glow_width);
                    scene.stroke(
                        &stroke,
                        Affine::IDENTITY,
                        &Brush::Solid(glow_color),
                        None,
                        &line,
                    );
                }
            }
        }

        // Draw core drop
        let stroke = Stroke::new(self.thickness);
        scene.stroke(&stroke, Affine::IDENTITY, &Brush::Solid(color), None, &line);
    }
}

impl BackdropEffect for RainEffect {
    fn effect_type(&self) -> &'static str {
        "rain"
    }

    fn update(&mut self, _dt: f64, time: f64) {
        self.time = time;

        if self.needs_regeneration {
            self.generate_drops();
        }
    }

    fn render(&self, scene: &mut Scene, bounds: Rect) {
        if !self.enabled || self.drops.is_empty() {
            return;
        }

        let width = bounds.width();
        let height = bounds.height();

        for drop in &self.drops {
            let (x, y) = self.drop_position(drop, width, height);

            // Calculate drop length
            let drop_length = self.length * drop.length;

            // Skip if completely off screen
            if x < -drop_length || x > width + drop_length {
                continue;
            }

            // Apply brightness to alpha
            let alpha = (self.color.a as f64 * drop.brightness) as u8;
            let color = self.color.with_alpha(alpha);

            self.draw_drop(scene, x, y, drop_length, color);
        }
    }

    fn configure(&mut self, config: &EffectConfig) {
        if let Some(enabled) = config.get_bool("enabled") {
            self.enabled = enabled;
        }

        if let Some(density) = config.get_usize("density") {
            // Clamp before comparing so an out-of-range value does not
            // regenerate on every configure/patch/restore.
            let density = density.clamp(10, 1000);
            if density != self.density {
                self.density = density;
                self.needs_regeneration = true;
            }
        }

        if let Some(speed) = config.get_f64("speed") {
            self.speed = speed.max(0.0);
        }

        if let Some(angle) = config.get_f64("angle") {
            self.angle = angle.clamp(-60.0, 60.0);
        }

        if let Some(length) = config.get_f64("length") {
            self.length = length.max(1.0);
        }

        if let Some(thickness) = config.get_f64("thickness") {
            self.thickness = thickness.clamp(0.5, 10.0);
        }

        if let Some(glow_radius) = config.get_f64("glow-radius") {
            self.glow_radius = glow_radius.max(0.0);
        }

        if let Some(glow_intensity) = config.get_f64("glow-intensity") {
            self.glow_intensity = glow_intensity.clamp(0.0, 1.0);
        }

        // Parse color
        if let Some(color_str) = config.get("color") {
            if let Some(color) = parse_simple_color(color_str) {
                self.color = color;
            }
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn is_animated(&self) -> bool {
        self.enabled && self.speed > 0.0
    }
}

/// Simple color parser for common formats
fn parse_simple_color(s: &str) -> Option<Rgba> {
    let s = s.trim();

    // Handle rgba(r, g, b, a)
    if s.starts_with("rgba(") && s.ends_with(')') {
        let inner = &s[5..s.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 4 {
            let r: u8 = parts[0].parse().ok()?;
            let g: u8 = parts[1].parse().ok()?;
            let b: u8 = parts[2].parse().ok()?;
            let a: f64 = parts[3].parse().ok()?;
            return Some(Rgba::new(r, g, b, (a * 255.0) as u8));
        }
    }

    // Handle rgb(r, g, b)
    if s.starts_with("rgb(") && s.ends_with(')') {
        let inner = &s[4..s.len() - 1];
        let parts: Vec<&str> = inner.split(',').map(|p| p.trim()).collect();
        if parts.len() == 3 {
            let r: u8 = parts[0].parse().ok()?;
            let g: u8 = parts[1].parse().ok()?;
            let b: u8 = parts[2].parse().ok()?;
            return Some(Rgba::new(r, g, b, 255));
        }
    }

    // Handle #RRGGBB or #RRGGBBAA
    if s.starts_with('#') {
        let hex = &s[1..];
        match hex.len() {
            6 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                return Some(Rgba::new(r, g, b, 255));
            }
            8 => {
                let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
                let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
                let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
                let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
                return Some(Rgba::new(r, g, b, a));
            }
            _ => {}
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_rain_is_disabled() {
        let rain = RainEffect::default();
        assert!(!rain.is_enabled());
    }

    #[test]
    fn test_configure_enables_rain() {
        let mut rain = RainEffect::default();
        let mut config = EffectConfig::new();
        config.insert("enabled", "true");

        rain.configure(&config);

        assert!(rain.is_enabled());
    }

    #[test]
    fn test_angle_clamped() {
        let mut rain = RainEffect::default();
        let mut config = EffectConfig::new();
        config.insert("angle", "90");

        rain.configure(&config);

        assert_eq!(rain.angle, 60.0); // Clamped to max
    }

    #[test]
    fn test_rng_stride_does_not_correlate_neighbours() {
        // With the old stride of 4 (but 5 values per drop) the brightness of
        // drop i was derived from the same hash as the x of drop i+1.
        let mut rain = RainEffect {
            density: 50,
            ..Default::default()
        };
        rain.generate_drops();
        for pair in rain.drops.windows(2) {
            let brightness_raw = (pair[0].brightness - 0.4) / 0.6;
            assert!(
                (brightness_raw - pair[1].x).abs() > 1e-9,
                "neighbouring drops share RNG output"
            );
        }
    }

    #[test]
    fn test_out_of_range_density_does_not_regenerate_repeatedly() {
        let mut rain = RainEffect::default();
        let mut config = EffectConfig::new();
        config.insert("density", "5000");
        rain.configure(&config);
        assert_eq!(rain.density, 1000);
        rain.update(0.0, 0.0); // regenerates and clears the flag
        assert!(!rain.needs_regeneration);
        rain.configure(&config);
        assert!(
            !rain.needs_regeneration,
            "same clamped value must not regenerate"
        );
    }

    #[test]
    fn test_is_animated_follows_speed() {
        let mut rain = RainEffect::default();
        assert!(!rain.is_animated(), "disabled effect is not animated");
        rain.enabled = true;
        assert!(rain.is_animated());
        rain.speed = 0.0;
        assert!(!rain.is_animated());
    }
}
