//! CSS Theme Parser using lightningcss
//!
//! Parses CSS theme files into Theme structs using lightningcss for proper
//! CSS parsing including support for calc(), color functions and gradients.
//!
//! Typed values (colours, gradients, text shadows) are converted directly from
//! lightningcss's AST at extraction time.  The string based `parse_*` helpers
//! remain as fallbacks for values that arrive as raw tokens (custom properties
//! and unparsed declarations).
//!
//! Parsing is lenient: lightningcss runs with error recovery enabled and every
//! recoverable problem (unknown selectors, unparsable numbers, unsupported
//! gradient directions, ...) is reported through [`ParseReport::warnings`]
//! instead of rejecting the whole theme.

use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use thiserror::Error;

use lightningcss::printer::PrinterOptions;
use lightningcss::properties::Property;
use lightningcss::properties::border::BorderSideWidth;
use lightningcss::properties::custom::{CustomPropertyName, Token, TokenList, TokenOrValue};
use lightningcss::properties::font::FontFamily;
use lightningcss::properties::ui::ColorOrAuto;
use lightningcss::rules::CssRule;
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use lightningcss::traits::ToCss;
use lightningcss::values::color::CssColor;
use lightningcss::values::gradient::{Gradient, GradientItem, LineDirection};
use lightningcss::values::image::Image;
use lightningcss::values::position::{HorizontalPositionKeyword, VerticalPositionKeyword};

use crate::{
    BackgroundImage, BackgroundPosition, BackgroundRepeat, BackgroundSize, Color, CrtEffect,
    CursorShape, EventOverride, GridEffect, GridPatch, LinearGradient, MatrixEffect, MatrixPatch,
    ParticleBehavior, ParticleEffect, ParticlePatch, ParticleShape, RainEffect, RainPatch,
    ShapeEffect, ShapeMotion, ShapePatch, ShapeRotation, ShapeType, SpriteEffect, SpriteMotion,
    SpriteOverlay, SpriteOverlayPosition, SpritePatch, SpritePosition, StarDirection,
    StarfieldEffect, StarfieldPatch, TextShadow, Theme, Typography,
};

#[derive(Error, Debug)]
pub enum ThemeParseError {
    #[error("CSS parse error: {0}")]
    CssError(String),

    #[error("Invalid color: {0}")]
    InvalidColor(String),

    #[error("Invalid gradient: {0}")]
    InvalidGradient(String),

    #[error("Missing required property: {0}")]
    MissingProperty(String),
}

/// Alias for [`ThemeParseError`].
pub type ThemeError = ThemeParseError;

/// Result of parsing a theme: the theme plus every non-fatal problem found on the way.
#[derive(Debug, Clone)]
pub struct ParseReport {
    /// The parsed theme.
    pub theme: Theme,
    /// Human readable warnings (unknown selectors, unparsable values, recovered CSS errors, ...).
    pub warnings: Vec<String>,
}

/// Helper to get PrinterOptions (since it doesn't implement Copy)
fn opts() -> PrinterOptions<'static> {
    PrinterOptions::default()
}

// ============================================================================
// Colour conversion
// ============================================================================

/// Convert a lightningcss colour to our `Color` type.
///
/// Every colour space lightningcss understands (`oklch()`, `lab()`, `color()`,
/// `hsl()`, named colours, hex, ...) is converted to sRGB.  `currentcolor` and
/// system colours have no fixed value and yield `None`.
fn css_color_to_color(css_color: &CssColor) -> Option<Color> {
    match css_color {
        CssColor::CurrentColor | CssColor::System(_) => None,
        other => match other.to_rgb() {
            Ok(CssColor::RGBA(rgba)) => Some(Color::rgba(
                rgba.red as f32 / 255.0,
                rgba.green as f32 / 255.0,
                rgba.blue as f32 / 255.0,
                rgba.alpha as f32 / 255.0,
            )),
            _ => None,
        },
    }
}

/// Convert a lightningcss colour, falling back to the string parser for
/// anything that cannot be converted directly.
fn convert_css_color(css_color: &CssColor) -> Result<Color, ThemeParseError> {
    if let Some(c) = css_color_to_color(css_color) {
        return Ok(c);
    }
    let text = css_color.to_css_string(opts()).unwrap_or_default();
    parse_color_string(&text)
}

/// Parse a color from a CSS string value
fn parse_color_string(value: &str) -> Result<Color, ThemeParseError> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();

    if value.starts_with('#') {
        parse_hex_color(value)
    } else if lower.starts_with("rgb") {
        parse_rgb_color(value)
    } else {
        // Try named colors
        parse_named_color(value).ok_or_else(|| ThemeParseError::InvalidColor(value.to_string()))
    }
}

/// Parse CSS named colors (the full CSS Color Level 4 table plus `transparent`).
pub fn parse_named_color(name: &str) -> Option<Color> {
    let (r, g, b) = match name.trim().to_ascii_lowercase().as_str() {
        "aliceblue" => (240, 248, 255),
        "antiquewhite" => (250, 235, 215),
        "aqua" | "cyan" => (0, 255, 255),
        "aquamarine" => (127, 255, 212),
        "azure" => (240, 255, 255),
        "beige" => (245, 245, 220),
        "bisque" => (255, 228, 196),
        "black" => (0, 0, 0),
        "blanchedalmond" => (255, 235, 205),
        "blue" => (0, 0, 255),
        "blueviolet" => (138, 43, 226),
        "brown" => (165, 42, 42),
        "burlywood" => (222, 184, 135),
        "cadetblue" => (95, 158, 160),
        "chartreuse" => (127, 255, 0),
        "chocolate" => (210, 105, 30),
        "coral" => (255, 127, 80),
        "cornflowerblue" => (100, 149, 237),
        "cornsilk" => (255, 248, 220),
        "crimson" => (220, 20, 60),
        "darkblue" => (0, 0, 139),
        "darkcyan" => (0, 139, 139),
        "darkgoldenrod" => (184, 134, 11),
        "darkgray" | "darkgrey" => (169, 169, 169),
        "darkgreen" => (0, 100, 0),
        "darkkhaki" => (189, 183, 107),
        "darkmagenta" => (139, 0, 139),
        "darkolivegreen" => (85, 107, 47),
        "darkorange" => (255, 140, 0),
        "darkorchid" => (153, 50, 204),
        "darkred" => (139, 0, 0),
        "darksalmon" => (233, 150, 122),
        "darkseagreen" => (143, 188, 143),
        "darkslateblue" => (72, 61, 139),
        "darkslategray" | "darkslategrey" => (47, 79, 79),
        "darkturquoise" => (0, 206, 209),
        "darkviolet" => (148, 0, 211),
        "deeppink" => (255, 20, 147),
        "deepskyblue" => (0, 191, 255),
        "dimgray" | "dimgrey" => (105, 105, 105),
        "dodgerblue" => (30, 144, 255),
        "firebrick" => (178, 34, 34),
        "floralwhite" => (255, 250, 240),
        "forestgreen" => (34, 139, 34),
        "fuchsia" | "magenta" => (255, 0, 255),
        "gainsboro" => (220, 220, 220),
        "ghostwhite" => (248, 248, 255),
        "gold" => (255, 215, 0),
        "goldenrod" => (218, 165, 32),
        "gray" | "grey" => (128, 128, 128),
        "green" => (0, 128, 0),
        "greenyellow" => (173, 255, 47),
        "honeydew" => (240, 255, 240),
        "hotpink" => (255, 105, 180),
        "indianred" => (205, 92, 92),
        "indigo" => (75, 0, 130),
        "ivory" => (255, 255, 240),
        "khaki" => (240, 230, 140),
        "lavender" => (230, 230, 250),
        "lavenderblush" => (255, 240, 245),
        "lawngreen" => (124, 252, 0),
        "lemonchiffon" => (255, 250, 205),
        "lightblue" => (173, 216, 230),
        "lightcoral" => (240, 128, 128),
        "lightcyan" => (224, 255, 255),
        "lightgoldenrodyellow" => (250, 250, 210),
        "lightgray" | "lightgrey" => (211, 211, 211),
        "lightgreen" => (144, 238, 144),
        "lightpink" => (255, 182, 193),
        "lightsalmon" => (255, 160, 122),
        "lightseagreen" => (32, 178, 170),
        "lightskyblue" => (135, 206, 250),
        "lightslategray" | "lightslategrey" => (119, 136, 153),
        "lightsteelblue" => (176, 196, 222),
        "lightyellow" => (255, 255, 224),
        "lime" => (0, 255, 0),
        "limegreen" => (50, 205, 50),
        "linen" => (250, 240, 230),
        "maroon" => (128, 0, 0),
        "mediumaquamarine" => (102, 205, 170),
        "mediumblue" => (0, 0, 205),
        "mediumorchid" => (186, 85, 211),
        "mediumpurple" => (147, 112, 219),
        "mediumseagreen" => (60, 179, 113),
        "mediumslateblue" => (123, 104, 238),
        "mediumspringgreen" => (0, 250, 154),
        "mediumturquoise" => (72, 209, 204),
        "mediumvioletred" => (199, 21, 133),
        "midnightblue" => (25, 25, 112),
        "mintcream" => (245, 255, 250),
        "mistyrose" => (255, 228, 225),
        "moccasin" => (255, 228, 181),
        "navajowhite" => (255, 222, 173),
        "navy" => (0, 0, 128),
        "oldlace" => (253, 245, 230),
        "olive" => (128, 128, 0),
        "olivedrab" => (107, 142, 35),
        "orange" => (255, 165, 0),
        "orangered" => (255, 69, 0),
        "orchid" => (218, 112, 214),
        "palegoldenrod" => (238, 232, 170),
        "palegreen" => (152, 251, 152),
        "paleturquoise" => (175, 238, 238),
        "palevioletred" => (219, 112, 147),
        "papayawhip" => (255, 239, 213),
        "peachpuff" => (255, 218, 185),
        "peru" => (205, 133, 63),
        "pink" => (255, 192, 203),
        "plum" => (221, 160, 221),
        "powderblue" => (176, 224, 230),
        "purple" => (128, 0, 128),
        "rebeccapurple" => (102, 51, 153),
        "red" => (255, 0, 0),
        "rosybrown" => (188, 143, 143),
        "royalblue" => (65, 105, 225),
        "saddlebrown" => (139, 69, 19),
        "salmon" => (250, 128, 114),
        "sandybrown" => (244, 164, 96),
        "seagreen" => (46, 139, 87),
        "seashell" => (255, 245, 238),
        "sienna" => (160, 82, 45),
        "silver" => (192, 192, 192),
        "skyblue" => (135, 206, 235),
        "slateblue" => (106, 90, 205),
        "slategray" | "slategrey" => (112, 128, 144),
        "snow" => (255, 250, 250),
        "springgreen" => (0, 255, 127),
        "steelblue" => (70, 130, 180),
        "tan" => (210, 180, 140),
        "teal" => (0, 128, 128),
        "thistle" => (216, 191, 216),
        "tomato" => (255, 99, 71),
        "turquoise" => (64, 224, 208),
        "violet" => (238, 130, 238),
        "wheat" => (245, 222, 179),
        "white" => (255, 255, 255),
        "whitesmoke" => (245, 245, 245),
        "yellow" => (255, 255, 0),
        "yellowgreen" => (154, 205, 50),
        "transparent" => return Some(Color::rgba(0.0, 0.0, 0.0, 0.0)),
        _ => return None,
    };
    Some(Color::rgb(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
    ))
}

/// Parse a hex color (#rgb, #rgba, #rrggbb, #rrggbbaa)
///
/// Never indexes into the string by byte position: the input is validated to
/// be pure ASCII hex digits first, so non-ASCII input is rejected rather than
/// panicking.
pub fn parse_hex_color(hex: &str) -> Result<Color, ThemeParseError> {
    let trimmed = hex.trim();
    let digits_str = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let invalid = || ThemeParseError::InvalidColor(hex.to_string());

    if digits_str.is_empty() || !digits_str.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }

    // Validated above: every byte is an ASCII hex digit.
    let digits: Vec<u8> = digits_str
        .bytes()
        .map(|b| (b as char).to_digit(16).unwrap_or(0) as u8)
        .collect();

    let unit = |v: u8| v as f32 / 255.0;
    let (r, g, b, a) = match digits.as_slice() {
        [r, g, b] => (r * 17, g * 17, b * 17, 255),
        [r, g, b, a] => (r * 17, g * 17, b * 17, a * 17),
        [r1, r2, g1, g2, b1, b2] => (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, 255),
        [r1, r2, g1, g2, b1, b2, a1, a2] => {
            (r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2, a1 * 16 + a2)
        }
        _ => return Err(invalid()),
    };

    Ok(Color::rgba(unit(r), unit(g), unit(b), unit(a)))
}

/// Split `s` on characters matching `is_sep`, ignoring separators nested in parentheses.
/// Empty parts are dropped and every part is trimmed.
fn split_top_level(s: &str, is_sep: impl Fn(char) -> bool) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 && is_sep(c) => {
                parts.push(s[start..i].trim());
                start = i + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// Parse a single rgb() channel using CSS semantics: `0-255` numbers or percentages.
fn parse_rgb_channel(s: &str, whole: &str) -> Result<f32, ThemeParseError> {
    let s = s.trim();
    let invalid = || ThemeParseError::InvalidColor(whole.to_string());
    let value = if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f32>().map_err(|_| invalid())? / 100.0
    } else {
        s.parse::<f32>().map_err(|_| invalid())? / 255.0
    };
    Ok(value.clamp(0.0, 1.0))
}

/// Parse an alpha value using CSS semantics: `0-1` numbers or percentages, clamped.
fn parse_alpha(s: &str, whole: &str) -> Result<f32, ThemeParseError> {
    let s = s.trim();
    let invalid = || ThemeParseError::InvalidColor(whole.to_string());
    let value = if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f32>().map_err(|_| invalid())? / 100.0
    } else {
        s.parse::<f32>().map_err(|_| invalid())?
    };
    Ok(value.clamp(0.0, 1.0))
}

/// Parse rgb(r, g, b) or rgba(r, g, b, a)
///
/// Uses CSS semantics: colour channels are `0-255` integers or percentages,
/// alpha is `0-1` (or a percentage) and is clamped.  Both the legacy comma
/// syntax and the modern `rgb(r g b / a)` syntax are accepted.
pub fn parse_rgb_color(input: &str) -> Result<Color, ThemeParseError> {
    let input = input.trim();
    let invalid = || ThemeParseError::InvalidColor(input.to_string());
    let lower = input.to_ascii_lowercase();

    let (is_rgba, rest) = if let Some(rest) = lower.strip_prefix("rgba(") {
        (true, rest)
    } else if let Some(rest) = lower.strip_prefix("rgb(") {
        (false, rest)
    } else {
        return Err(invalid());
    };
    let inner = rest.strip_suffix(')').ok_or_else(invalid)?;

    // Modern syntax: "r g b / a"
    let (channels, slash_alpha) = match inner.split_once('/') {
        Some((c, a)) => (c, Some(a)),
        None => (inner, None),
    };

    let mut parts: Vec<&str> = if channels.contains(',') {
        split_top_level(channels, |c| c == ',')
    } else {
        channels.split_whitespace().collect()
    };

    let alpha_str = if let Some(a) = slash_alpha {
        Some(a)
    } else {
        let expected = if is_rgba { 4 } else { 3 };
        if parts.len() != expected {
            return Err(invalid());
        }
        if is_rgba { parts.pop() } else { None }
    };

    if parts.len() != 3 {
        return Err(invalid());
    }

    let r = parse_rgb_channel(parts[0], input)?;
    let g = parse_rgb_channel(parts[1], input)?;
    let b = parse_rgb_channel(parts[2], input)?;
    let a = match alpha_str {
        Some(a) => parse_alpha(a, input)?,
        None => 1.0,
    };

    Ok(Color::rgba(r, g, b, a))
}

/// Parse any CSS color value (string fallback)
pub fn parse_color(value: &str) -> Result<Color, ThemeParseError> {
    parse_color_string(value)
}

// ============================================================================
// Gradients
// ============================================================================

fn solid_gradient(color: Color) -> LinearGradient {
    LinearGradient {
        top: color,
        bottom: color,
    }
}

/// Orient two gradient stops.  `downward` means the first stop sits at the top.
fn orient_gradient(first: Color, last: Color, downward: bool) -> LinearGradient {
    if downward {
        LinearGradient {
            top: first,
            bottom: last,
        }
    } else {
        LinearGradient {
            top: last,
            bottom: first,
        }
    }
}

/// Decide whether a gradient angle (CSS: 0deg = to top, 180deg = to bottom) runs downward.
fn angle_is_downward(degrees: f32, warnings: &mut Vec<String>) -> bool {
    let deg = degrees.rem_euclid(360.0);
    const EPS: f32 = 0.01;
    if (deg - 180.0).abs() < EPS {
        return true;
    }
    if deg.abs() < EPS {
        return false;
    }
    if (deg - 90.0).abs() < EPS || (deg - 270.0).abs() < EPS {
        warnings.push(format!(
            "linear-gradient: horizontal angle {degrees}deg is not supported; falling back to `to bottom`"
        ));
        return true;
    }
    warnings.push(format!(
        "linear-gradient: angle {degrees}deg is not supported; using only its vertical component"
    ));
    deg > 90.0 && deg < 270.0
}

/// Decide whether a `to <side-or-corner>` keyword list runs downward.
fn keywords_are_downward(keywords: &str, warnings: &mut Vec<String>) -> bool {
    let mut vertical: Option<bool> = None;
    let mut horizontal: Option<&str> = None;
    for word in keywords.split_whitespace() {
        match word {
            "top" => vertical = Some(false),
            "bottom" => vertical = Some(true),
            "left" | "right" => horizontal = Some(word),
            _ => {}
        }
    }
    match (vertical, horizontal) {
        (Some(down), None) => down,
        (Some(down), Some(h)) => {
            warnings.push(format!(
                "linear-gradient: corner direction `to {keywords}` is not supported; ignoring the `{h}` component"
            ));
            down
        }
        (None, Some(h)) => {
            warnings.push(format!(
                "linear-gradient: horizontal direction `to {h}` is not supported; falling back to `to bottom`"
            ));
            true
        }
        (None, None) => true,
    }
}

/// Decide whether a typed lightningcss gradient direction runs downward.
fn direction_is_downward(direction: &LineDirection, warnings: &mut Vec<String>) -> bool {
    match direction {
        LineDirection::Vertical(VerticalPositionKeyword::Bottom) => true,
        LineDirection::Vertical(VerticalPositionKeyword::Top) => false,
        LineDirection::Horizontal(h) => {
            let name = match h {
                HorizontalPositionKeyword::Left => "left",
                HorizontalPositionKeyword::Right => "right",
            };
            warnings.push(format!(
                "linear-gradient: horizontal direction `to {name}` is not supported; falling back to `to bottom`"
            ));
            true
        }
        LineDirection::Corner {
            horizontal,
            vertical,
        } => {
            let name = match horizontal {
                HorizontalPositionKeyword::Left => "left",
                HorizontalPositionKeyword::Right => "right",
            };
            warnings.push(format!(
                "linear-gradient: corner directions are not supported; ignoring the `{name}` component"
            ));
            matches!(vertical, VerticalPositionKeyword::Bottom)
        }
        LineDirection::Angle(angle) => angle_is_downward(angle.to_degrees(), warnings),
    }
}

/// Convert a typed lightningcss linear gradient into our two-stop gradient.
fn convert_linear_gradient(
    gradient: &lightningcss::values::gradient::LinearGradient,
    warnings: &mut Vec<String>,
) -> Result<LinearGradient, ThemeParseError> {
    let mut stops = Vec::new();
    for item in &gradient.items {
        if let GradientItem::ColorStop(stop) = item {
            stops.push(convert_css_color(&stop.color)?);
        }
    }
    if stops.len() < 2 {
        return Err(ThemeParseError::InvalidGradient(
            "linear-gradient() needs at least two colour stops".to_string(),
        ));
    }
    if stops.len() > 2 {
        warnings.push(
            "linear-gradient: only two colour stops are supported; using the first and last"
                .to_string(),
        );
    }
    let downward = direction_is_downward(&gradient.direction, warnings);
    Ok(orient_gradient(stops[0], stops[stops.len() - 1], downward))
}

/// Parse a CSS angle string (`45deg`, `0.5turn`, `1rad`, `100grad`) into degrees.
fn parse_angle_degrees(s: &str) -> Option<f32> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(v) = s.strip_suffix("deg") {
        v.trim().parse().ok()
    } else if let Some(v) = s.strip_suffix("grad") {
        v.trim().parse::<f32>().ok().map(|g| g * 0.9)
    } else if let Some(v) = s.strip_suffix("rad") {
        v.trim().parse::<f32>().ok().map(|r| r.to_degrees())
    } else if let Some(v) = s.strip_suffix("turn") {
        v.trim().parse::<f32>().ok().map(|t| t * 360.0)
    } else {
        None
    }
}

/// Parse linear-gradient from string
pub fn parse_linear_gradient(value: &str) -> Result<LinearGradient, ThemeParseError> {
    let mut warnings = Vec::new();
    parse_linear_gradient_with_warnings(value, &mut warnings)
}

/// String fallback gradient parser.  `to top` swaps the stops, horizontal
/// directions fall back to vertical with a warning.
fn parse_linear_gradient_with_warnings(
    value: &str,
    warnings: &mut Vec<String>,
) -> Result<LinearGradient, ThemeParseError> {
    let value = value.trim();
    let invalid = || ThemeParseError::InvalidGradient(value.to_string());

    let inner = value
        .strip_prefix("linear-gradient(")
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(invalid)?;

    let parts = split_top_level(inner, |c| c == ',');
    let mut downward = true;
    let mut first_stop = 0;

    if let Some(first) = parts.first() {
        let lower = first.to_ascii_lowercase();
        if let Some(keywords) = lower.strip_prefix("to ") {
            downward = keywords_are_downward(keywords, warnings);
            first_stop = 1;
        } else if let Some(deg) = parse_angle_degrees(&lower) {
            downward = angle_is_downward(deg, warnings);
            first_stop = 1;
        }
    }

    let stops = &parts[first_stop.min(parts.len())..];
    if stops.len() < 2 {
        return Err(invalid());
    }
    if stops.len() > 2 {
        warnings.push(
            "linear-gradient: only two colour stops are supported; using the first and last"
                .to_string(),
        );
    }

    // Strip stop positions (e.g. "#ff0000 0%")
    fn stop_color(stop: &str) -> &str {
        split_top_level(stop, char::is_whitespace)
            .first()
            .copied()
            .unwrap_or(stop)
    }

    let first = parse_color(stop_color(stops[0]))?;
    let last = parse_color(stop_color(stops[stops.len() - 1]))?;

    Ok(orient_gradient(first, last, downward))
}

// ============================================================================
// Other value parsers
// ============================================================================

/// Parse text-shadow: offset-x offset-y blur-radius color
pub fn parse_text_shadow(value: &str) -> Result<TextShadow, ThemeParseError> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();

    // Handle rgba() colors which contain spaces
    let color_start = lower.find("rgb").or_else(|| value.find('#'));

    let Some(idx) = color_start else {
        return Err(ThemeParseError::InvalidColor(format!(
            "text-shadow: {}",
            value
        )));
    };

    let prefix = value[..idx].trim();
    let parts: Vec<&str> = prefix.split_whitespace().collect();
    let radius = parts
        .last()
        .and_then(|s| parse_number::<f32>(s))
        .unwrap_or(TextShadow::default().radius);

    let color = parse_color(&value[idx..])?;

    Ok(TextShadow {
        color,
        radius,
        intensity: color.a,
    })
}

/// Convert a typed lightningcss text shadow.
fn convert_text_shadow(
    shadow: &lightningcss::properties::text::TextShadow,
    warnings: &mut Vec<String>,
) -> Result<TextShadow, ThemeParseError> {
    let color = convert_css_color(&shadow.color)?;
    let radius = match shadow.blur.to_px() {
        Some(px) => px,
        None => {
            let default = TextShadow::default().radius;
            warnings.push(format!(
                "text-shadow: blur radius `{}` is not an absolute length; using {default}px",
                shadow.blur.to_css_string(opts()).unwrap_or_default()
            ));
            default
        }
    };
    Ok(TextShadow {
        color,
        radius,
        intensity: color.a,
    })
}

/// Parse background-size from CSS string
pub fn parse_background_size(value: &str) -> BackgroundSize {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "cover" => BackgroundSize::Cover,
        "contain" => BackgroundSize::Contain,
        "auto" | "auto auto" => BackgroundSize::Auto,
        _ => {
            // Try to parse as percentage (e.g., "30%" for canvas-relative)
            if value.ends_with('%')
                && let Ok(pct) = value.trim_end_matches('%').parse::<f32>()
            {
                return BackgroundSize::CanvasPercent(pct);
            }
            // Try to parse as scale factor (e.g., "2x" or "0.5x" for image-relative)
            if value.ends_with('x')
                && let Ok(scale) = value.trim_end_matches('x').parse::<f32>()
            {
                return BackgroundSize::ImageScale(scale);
            }
            // Try to parse as fixed dimensions (e.g., "100px 200px")
            let parts: Vec<&str> = value.split_whitespace().collect();
            if parts.len() >= 2 {
                let w = parts[0].trim_end_matches("px").parse().unwrap_or(0);
                let h = parts[1].trim_end_matches("px").parse().unwrap_or(0);
                if w > 0 && h > 0 {
                    return BackgroundSize::Fixed(w, h);
                }
            }
            // Single value in pixels
            if value.ends_with("px")
                && let Ok(px) = value.trim_end_matches("px").parse::<u32>()
            {
                return BackgroundSize::Fixed(px, px);
            }
            BackgroundSize::Cover
        }
    }
}

/// Parse background-position from CSS string
pub fn parse_background_position(value: &str) -> BackgroundPosition {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "center" | "center center" | "50% 50%" => BackgroundPosition::Center,
        "top" | "center top" | "top center" | "50% 0%" => BackgroundPosition::Top,
        "bottom" | "center bottom" | "bottom center" | "50% 100%" => BackgroundPosition::Bottom,
        "left" | "left center" | "center left" | "0% 50%" => BackgroundPosition::Left,
        "right" | "right center" | "center right" | "100% 50%" => BackgroundPosition::Right,
        "top left" | "left top" | "0% 0%" | "0 0" => BackgroundPosition::TopLeft,
        "top right" | "right top" | "100% 0%" => BackgroundPosition::TopRight,
        "bottom left" | "left bottom" | "0% 100%" => BackgroundPosition::BottomLeft,
        "bottom right" | "right bottom" | "100% 100%" => BackgroundPosition::BottomRight,
        _ => {
            // Try to parse as percentage values
            let parts: Vec<&str> = value.split_whitespace().collect();
            if parts.len() >= 2 {
                let x = parts[0].trim_end_matches('%').parse::<f32>().ok();
                let y = parts[1].trim_end_matches('%').parse::<f32>().ok();
                if let (Some(x), Some(y)) = (x, y) {
                    return BackgroundPosition::Percent(x / 100.0, y / 100.0);
                }
            }
            BackgroundPosition::Center
        }
    }
}

/// Parse background-repeat from CSS string
pub fn parse_background_repeat(value: &str) -> BackgroundRepeat {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "no-repeat" => BackgroundRepeat::NoRepeat,
        "repeat" | "repeat repeat" => BackgroundRepeat::Repeat,
        "repeat-x" | "repeat no-repeat" => BackgroundRepeat::RepeatX,
        "repeat-y" | "no-repeat repeat" => BackgroundRepeat::RepeatY,
        _ => BackgroundRepeat::NoRepeat,
    }
}

/// Parse cursor shape from string
pub fn parse_cursor_shape(value: &str) -> Option<CursorShape> {
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "block" => Some(CursorShape::Block),
        "bar" | "beam" => Some(CursorShape::Bar),
        "underline" => Some(CursorShape::Underline),
        _ => None,
    }
}

/// Parse duration string (e.g., "500ms", "1.5s", "1000") to milliseconds
fn parse_duration(s: &str, warnings: &mut Vec<String>) -> u32 {
    let trimmed = s.trim();
    let parsed = if let Some(ms) = trimmed.strip_suffix("ms") {
        ms.trim().parse::<f32>().ok().map(|v| v.max(0.0) as u32)
    } else if let Some(secs) = trimmed.strip_suffix('s') {
        secs.trim()
            .parse::<f32>()
            .ok()
            .map(|v| (v.max(0.0) * 1000.0) as u32)
    } else {
        // Assume milliseconds if no unit
        trimmed.parse::<f32>().ok().map(|v| v.max(0.0) as u32)
    };
    parsed.unwrap_or_else(|| {
        warnings.push(format!("--duration: could not parse `{s}`; using 0"));
        0
    })
}

/// Strip surrounding quotes (and a `url(...)` wrapper) from a string
fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("url(")
        .and_then(|inner| inner.strip_suffix(')'))
        .map(str::trim)
        .unwrap_or(s);
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Parse a number, tolerating trailing `px` / `deg` units.
fn parse_number<T: FromStr>(v: &str) -> Option<T> {
    let s = v.trim();
    let s = s
        .strip_suffix("px")
        .or_else(|| s.strip_suffix("deg"))
        .unwrap_or(s)
        .trim();
    s.parse().ok()
}

/// Parse a number, falling back to `fallback` (with a warning) when it cannot be parsed.
fn number_or<T: FromStr + Display>(
    v: &str,
    prop: &str,
    fallback: T,
    warnings: &mut Vec<String>,
) -> T {
    parse_number(v).unwrap_or_else(|| {
        warnings.push(format!(
            "{prop}: could not parse `{v}`; using default {fallback}"
        ));
        fallback
    })
}

/// Parse an optional number (for patches), warning when the value is unparsable.
fn number_opt<T: FromStr>(v: &str, prop: &str, warnings: &mut Vec<String>) -> Option<T> {
    let parsed = parse_number(v);
    if parsed.is_none() {
        warnings.push(format!("{prop}: could not parse `{v}`; ignoring"));
    }
    parsed
}

/// Parse a boolean custom property value.
fn parse_bool(v: &str, prop: &str, warnings: &mut Vec<String>) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => {
            warnings.push(format!(
                "{prop}: expected `true` or `false`, got `{v}`; ignoring"
            ));
            None
        }
    }
}

/// Parse an enum-like keyword, warning when it is not recognised.
fn keyword_opt<T>(
    v: &str,
    prop: &str,
    parse: impl Fn(&str) -> Option<T>,
    warnings: &mut Vec<String>,
) -> Option<T> {
    let parsed = parse(v.trim());
    if parsed.is_none() {
        warnings.push(format!("{prop}: unknown value `{v}`; ignoring"));
    }
    parsed
}

// ============================================================================
// Property extraction
// ============================================================================

/// Collected properties from a CSS rule.
///
/// Typed values (converted straight from lightningcss's AST) take precedence;
/// the string maps are kept for values that only exist as raw tokens.
#[derive(Default)]
struct RuleProperties {
    /// Standard (non `--`) properties as CSS strings.
    standard: HashMap<String, String>,
    /// Custom (`--*`) properties as CSS strings.
    custom: HashMap<String, String>,
    /// Typed colours for standard properties.
    colors: HashMap<String, Color>,
    /// Typed colours for custom properties.
    custom_colors: HashMap<String, Color>,
    /// Unescaped string / url() values for custom properties given as a single token.
    custom_strings: HashMap<String, String>,
    /// Typed background gradient (from `background: linear-gradient(...)`).
    background: Option<LinearGradient>,
    /// Typed text shadow.
    text_shadow: Option<TextShadow>,
}

impl RuleProperties {
    fn get(&self, key: &str) -> Option<&String> {
        self.standard.get(key)
    }

    fn custom(&self, key: &str) -> Option<&String> {
        self.custom.get(key)
    }

    fn has_custom_prefix(&self, prefix: &str) -> bool {
        self.custom.keys().any(|k| k.starts_with(prefix))
    }

    /// String value of a custom property with quotes / `url()` removed.
    /// Single string or url tokens are taken verbatim (unescaped) from the AST.
    fn custom_string(&self, key: &str) -> Option<String> {
        if let Some(s) = self.custom_strings.get(key) {
            return Some(s.clone());
        }
        self.custom.get(key).map(|s| strip_quotes(s))
    }

    /// Typed colour for a standard property, falling back to the string parser.
    fn color(&self, key: &str) -> Result<Option<Color>, ThemeParseError> {
        if let Some(c) = self.colors.get(key) {
            return Ok(Some(*c));
        }
        match self.standard.get(key) {
            Some(s) => parse_color(s).map(Some),
            None => Ok(None),
        }
    }

    /// Typed colour for a custom property, falling back to the string parser.
    fn custom_color(&self, key: &str) -> Result<Option<Color>, ThemeParseError> {
        if let Some(c) = self.custom_colors.get(key) {
            return Ok(Some(*c));
        }
        match self.custom.get(key) {
            Some(s) => parse_color(s).map(Some),
            None => Ok(None),
        }
    }

    /// Background as a gradient: typed gradient, typed solid colour, or string fallback.
    fn background_gradient(
        &self,
        warnings: &mut Vec<String>,
    ) -> Result<Option<LinearGradient>, ThemeParseError> {
        if let Some(g) = self.background {
            return Ok(Some(g));
        }
        if let Some(c) = self.colors.get("background") {
            return Ok(Some(solid_gradient(*c)));
        }
        match self.standard.get("background") {
            Some(s) if s.contains("linear-gradient") => {
                parse_linear_gradient_with_warnings(s, warnings).map(Some)
            }
            Some(s) => parse_color(s).map(|c| Some(solid_gradient(c))),
            None => Ok(None),
        }
    }

    /// Text shadow: typed value or string fallback.
    fn text_shadow(&self) -> Result<Option<TextShadow>, ThemeParseError> {
        if let Some(ts) = self.text_shadow {
            return Ok(Some(ts));
        }
        match self.standard.get("text-shadow") {
            Some(s) => parse_text_shadow(s).map(Some),
            None => Ok(None),
        }
    }
}

/// If a token list consists of exactly one colour value, convert it.
fn single_color(tokens: &TokenList) -> Option<Color> {
    let mut iter = tokens
        .0
        .iter()
        .filter(|t| !matches!(t, TokenOrValue::Token(Token::WhiteSpace(_))));
    match (iter.next(), iter.next()) {
        (Some(TokenOrValue::Color(color)), None) => css_color_to_color(color),
        _ => None,
    }
}

/// If a token list consists of exactly one string or url() token, return its raw value.
fn single_string(tokens: &TokenList) -> Option<String> {
    let mut iter = tokens
        .0
        .iter()
        .filter(|t| !matches!(t, TokenOrValue::Token(Token::WhiteSpace(_))));
    match (iter.next(), iter.next()) {
        (Some(TokenOrValue::Token(Token::String(s))), None) => Some(s.as_ref().to_string()),
        (Some(TokenOrValue::Url(url)), None) => Some(url.url.as_ref().to_string()),
        _ => None,
    }
}

/// Extract properties from a style rule's declarations
fn extract_properties(
    rule: &lightningcss::rules::style::StyleRule,
    warnings: &mut Vec<String>,
) -> Result<RuleProperties, ThemeParseError> {
    let mut props = RuleProperties::default();

    let declarations = rule
        .declarations
        .declarations
        .iter()
        .chain(rule.declarations.important_declarations.iter());

    for decl in declarations {
        match decl {
            Property::Custom(prop) => {
                let name = prop.name.as_ref().to_string();
                let value = decl
                    .value_to_css_string(opts())
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let typed = single_color(&prop.value);
                match prop.name {
                    CustomPropertyName::Custom(_) => {
                        if let Some(c) = typed {
                            props.custom_colors.insert(name.clone(), c);
                        }
                        if let Some(text) = single_string(&prop.value) {
                            props.custom_strings.insert(name.clone(), text);
                        }
                        props.custom.insert(name, value);
                    }
                    // Property names lightningcss does not know (cursor-color,
                    // padding-x, ...) are theme properties, not custom properties.
                    CustomPropertyName::Unknown(_) => {
                        if let Some(c) = typed {
                            props.colors.insert(name.clone(), c);
                        }
                        props.standard.insert(name, value);
                    }
                }
            }
            Property::Color(color) => {
                insert_color(&mut props, "color", color)?;
            }
            Property::BackgroundColor(color) => {
                insert_color(&mut props, "background-color", color)?;
            }
            Property::BorderColor(color) => {
                insert_color(&mut props, "border-color", &color.top)?;
            }
            Property::AccentColor(ColorOrAuto::Color(color)) => {
                insert_color(&mut props, "accent-color", color)?;
            }
            Property::AccentColor(ColorOrAuto::Auto) => {}
            Property::OutlineColor(color) => {
                insert_color(&mut props, "outline-color", color)?;
            }
            Property::OutlineWidth(width) => {
                props
                    .standard
                    .insert("outline-width".to_string(), border_side_width_string(width));
            }
            Property::Outline(outline) => {
                insert_color(&mut props, "outline-color", &outline.color)?;
                props.standard.insert(
                    "outline-width".to_string(),
                    border_side_width_string(&outline.width),
                );
            }
            Property::Background(backgrounds) => {
                for bg in backgrounds.iter() {
                    match &bg.image {
                        Image::Gradient(gradient) => {
                            if let Ok(css_str) = gradient.to_css_string(opts()) {
                                props.standard.insert("background".to_string(), css_str);
                            }
                            match gradient.as_ref() {
                                Gradient::Linear(lg) | Gradient::RepeatingLinear(lg) => {
                                    props.background = Some(convert_linear_gradient(lg, warnings)?);
                                }
                                _ => warnings.push(
                                    "background: only linear-gradient() is supported; ignoring gradient"
                                        .to_string(),
                                ),
                            }
                        }
                        Image::Url(url) => {
                            props
                                .standard
                                .insert("background-image".to_string(), url.url.to_string());
                            // A colour given alongside the image is still the base colour
                            if bg.color != CssColor::default() {
                                insert_color(&mut props, "background", &bg.color)?;
                            }
                        }
                        Image::None => {
                            insert_color(&mut props, "background", &bg.color)?;
                        }
                        Image::ImageSet(_) => {
                            warnings.push("background: image-set() is not supported".to_string());
                        }
                    }
                    // Also extract background-size, position, repeat from shorthand.
                    // Compare against the typed defaults rather than serialised text.
                    if bg.size != Default::default()
                        && let Ok(css_str) = bg.size.to_css_string(opts())
                    {
                        props
                            .standard
                            .insert("background-size".to_string(), css_str);
                    }
                    if bg.position != Default::default()
                        && let Ok(css_str) = bg.position.to_css_string(opts())
                    {
                        props
                            .standard
                            .insert("background-position".to_string(), css_str);
                    }
                    if bg.repeat != Default::default()
                        && let Ok(css_str) = bg.repeat.to_css_string(opts())
                    {
                        props
                            .standard
                            .insert("background-repeat".to_string(), css_str);
                    }
                }
            }
            Property::BackgroundImage(images) => {
                for img in images.iter() {
                    match img {
                        Image::Url(url) => {
                            props
                                .standard
                                .insert("background-image".to_string(), url.url.to_string());
                        }
                        Image::Gradient(gradient) => match gradient.as_ref() {
                            Gradient::Linear(lg) | Gradient::RepeatingLinear(lg) => {
                                props.background = Some(convert_linear_gradient(lg, warnings)?);
                            }
                            _ => warnings.push(
                                "background-image: only linear-gradient() is supported".to_string(),
                            ),
                        },
                        _ => {}
                    }
                }
            }
            Property::BackgroundSize(sizes) => {
                if let Some(size) = sizes.first()
                    && let Ok(css_str) = size.to_css_string(opts())
                {
                    props
                        .standard
                        .insert("background-size".to_string(), css_str);
                }
            }
            Property::BackgroundPosition(positions) => {
                if let Some(pos) = positions.first()
                    && let Ok(css_str) = pos.to_css_string(opts())
                {
                    props
                        .standard
                        .insert("background-position".to_string(), css_str);
                }
            }
            Property::BackgroundRepeat(repeats) => {
                if let Some(repeat) = repeats.first()
                    && let Ok(css_str) = repeat.to_css_string(opts())
                {
                    props
                        .standard
                        .insert("background-repeat".to_string(), css_str);
                }
            }
            Property::FontFamily(families) => {
                let names: Vec<String> = families
                    .iter()
                    .filter_map(|f| match f {
                        FontFamily::FamilyName(name) => {
                            name.to_css_string(opts()).ok().map(|s| strip_quotes(&s))
                        }
                        FontFamily::Generic(g) => g.to_css_string(opts()).ok(),
                    })
                    .collect();
                props
                    .standard
                    .insert("font-family".to_string(), names.join(", "));
            }
            Property::FontSize(size) => {
                insert_string(&mut props, "font-size", size)?;
            }
            Property::LineHeight(height) => {
                insert_string(&mut props, "line-height", height)?;
            }
            Property::BorderRadius(radius, _) => {
                insert_string(&mut props, "border-radius", radius)?;
            }
            Property::TextShadow(shadows) => {
                if let Some(shadow) = shadows.first() {
                    if let Ok(css_str) = shadow.to_css_string(opts()) {
                        props.standard.insert("text-shadow".to_string(), css_str);
                    }
                    props.text_shadow = Some(convert_text_shadow(shadow, warnings)?);
                }
            }
            Property::Width(width) => {
                insert_string(&mut props, "width", width)?;
            }
            Property::Height(height) => {
                insert_string(&mut props, "height", height)?;
            }
            Property::MinWidth(width) => {
                insert_string(&mut props, "min-width", width)?;
            }
            Property::MaxWidth(width) => {
                insert_string(&mut props, "max-width", width)?;
            }
            Property::Padding(padding) => {
                insert_string(&mut props, "padding", padding)?;
            }
            Property::Unparsed(unparsed) => {
                // Known property whose value lightningcss could not type: keep the raw tokens.
                if let Ok(name) = unparsed.property_id.to_css_string(opts()) {
                    let value = decl
                        .value_to_css_string(opts())
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if let Some(c) = single_color(&unparsed.value) {
                        props.colors.insert(name.clone(), c);
                    }
                    if !value.is_empty() {
                        props.standard.insert(name, value);
                    }
                }
            }
            other => {
                let name = other
                    .property_id()
                    .to_css_string(opts())
                    .unwrap_or_default();
                warnings.push(format!("unsupported property `{name}` ignored"));
            }
        }
    }

    Ok(props)
}

fn insert_color(
    props: &mut RuleProperties,
    key: &str,
    color: &CssColor,
) -> Result<(), ThemeParseError> {
    if let Ok(css_str) = color.to_css_string(opts()) {
        props.standard.insert(key.to_string(), css_str);
    }
    props
        .colors
        .insert(key.to_string(), convert_css_color(color)?);
    Ok(())
}

fn insert_string<T: ToCss>(
    props: &mut RuleProperties,
    key: &str,
    value: &T,
) -> Result<(), ThemeParseError> {
    if let Ok(css_str) = value.to_css_string(opts()) {
        props.standard.insert(key.to_string(), css_str);
    }
    Ok(())
}

fn border_side_width_string(width: &BorderSideWidth) -> String {
    match width {
        BorderSideWidth::Thin => "1px".to_string(),
        BorderSideWidth::Medium => "3px".to_string(),
        BorderSideWidth::Thick => "5px".to_string(),
        BorderSideWidth::Length(len) => len
            .to_px()
            .map(|px| format!("{px}px"))
            .or_else(|| len.to_css_string(opts()).ok())
            .unwrap_or_default(),
    }
}

/// Get selector string from a style rule
fn get_selector_string(rule: &lightningcss::rules::style::StyleRule) -> String {
    rule.selectors.to_css_string(opts()).unwrap_or_default()
}

// ============================================================================
// Selectors
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    Bell,
    CommandFail,
    CommandSuccess,
    Focus,
    Blur,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorTarget {
    Terminal,
    Selection,
    Highlight,
    Cursor,
    Backdrop,
    TabBar,
    Tab,
    TabActive,
    TabClose,
    Palette,
    Focus,
    Hover,
    ContextMenu,
    SearchBar,
    RenameBar,
    Event(EventKind),
}

/// Map a selector string to the part of the theme it configures.
fn resolve_selector(selector: &str) -> Option<SelectorTarget> {
    let selector = selector.trim();
    let selector = selector.strip_prefix(':').unwrap_or(selector);
    let target = match selector {
        "terminal" => SelectorTarget::Terminal,
        "terminal::selection" => SelectorTarget::Selection,
        "terminal::highlight" => SelectorTarget::Highlight,
        "terminal::cursor" => SelectorTarget::Cursor,
        "terminal::backdrop" => SelectorTarget::Backdrop,
        "terminal::tab-bar" => SelectorTarget::TabBar,
        "terminal::tab" | "tab" => SelectorTarget::Tab,
        "terminal::tab-active" | "tab.active" => SelectorTarget::TabActive,
        "terminal::tab-close" => SelectorTarget::TabClose,
        "terminal::palette" => SelectorTarget::Palette,
        "terminal::ui-focus" | "focus" => SelectorTarget::Focus,
        "terminal::ui-hover" | "hover" => SelectorTarget::Hover,
        "terminal::context-menu" => SelectorTarget::ContextMenu,
        "terminal::search-bar" => SelectorTarget::SearchBar,
        "terminal::rename-bar" => SelectorTarget::RenameBar,
        "terminal::on-bell" => SelectorTarget::Event(EventKind::Bell),
        "terminal::on-command-fail" => SelectorTarget::Event(EventKind::CommandFail),
        "terminal::on-command-success" => SelectorTarget::Event(EventKind::CommandSuccess),
        "terminal::on-focus" => SelectorTarget::Event(EventKind::Focus),
        "terminal::on-blur" => SelectorTarget::Event(EventKind::Blur),
        _ => return None,
    };
    Some(target)
}

// ============================================================================
// Parsing entry points
// ============================================================================

/// Mutable state threaded through the rule application functions.
struct ParseState {
    theme: Theme,
    warnings: Vec<String>,
    /// Background image accumulated across every `:terminal` rule.  Only
    /// installed on the theme once a path has been given.
    bg_image: BackgroundImage,
}

/// Parse CSS theme using lightningcss, returning the theme plus warnings.
pub fn parse_theme_report(css: &str) -> Result<ParseReport, ThemeParseError> {
    let css_warnings = Arc::new(RwLock::new(Vec::new()));
    let options = ParserOptions {
        error_recovery: true,
        warnings: Some(css_warnings.clone()),
        ..ParserOptions::default()
    };

    let stylesheet =
        StyleSheet::parse(css, options).map_err(|e| ThemeParseError::CssError(e.to_string()))?;

    let theme = Theme::minimal();
    let bg_image = theme.background_image.clone().unwrap_or(BackgroundImage {
        path: None,
        base_dir: None,
        size: BackgroundSize::default(),
        position: BackgroundPosition::default(),
        repeat: BackgroundRepeat::default(),
        opacity: 1.0,
    });
    let mut state = ParseState {
        theme,
        warnings: Vec::new(),
        bg_image,
    };

    if let Ok(recovered) = css_warnings.read() {
        for w in recovered.iter() {
            let text = w.to_string();
            // `:terminal::cursor` & co. are our own pseudo-selectors; lightningcss
            // flags each of them.  Genuinely unknown selectors are reported below.
            if text.contains("is not recognized as a valid pseudo-") {
                continue;
            }
            state.warnings.push(format!("CSS: {text}"));
        }
    }

    for rule in &stylesheet.rules.0 {
        match rule {
            CssRule::Style(style_rule) => {
                // Resolve the selector first so unknown rules cost nothing.
                let selector = get_selector_string(style_rule);
                let mut targets = Vec::new();
                for part in selector.split(',') {
                    match resolve_selector(part) {
                        Some(target) => targets.push(target),
                        None => state
                            .warnings
                            .push(format!("unknown selector `{}` ignored", part.trim())),
                    }
                }
                if targets.is_empty() {
                    continue;
                }

                let props = extract_properties(style_rule, &mut state.warnings)?;
                for target in targets {
                    apply_properties(&mut state, target, &props)?;
                }
            }
            CssRule::Ignored => {}
            other => {
                let text = other.to_css_string(opts()).unwrap_or_default();
                let head: String = text.lines().next().unwrap_or("").chars().take(40).collect();
                state
                    .warnings
                    .push(format!("unsupported rule ignored: {head}"));
            }
        }
    }

    let ParseState {
        mut theme,
        warnings,
        bg_image,
    } = state;
    if bg_image.path.is_some() {
        theme.background_image = Some(bg_image);
    }

    Ok(ParseReport { theme, warnings })
}

/// Log every warning of a report through the `log` crate.
pub fn log_warnings(warnings: &[String]) {
    for w in warnings {
        log::warn!("theme: {w}");
    }
}

/// Parse CSS theme using lightningcss.  Warnings are logged with `log::warn!`.
pub fn parse_theme(css: &str) -> Result<Theme, ThemeParseError> {
    let report = parse_theme_report(css)?;
    log_warnings(&report.warnings);
    Ok(report.theme)
}

/// Apply parsed properties to theme based on selector target
fn apply_properties(
    st: &mut ParseState,
    target: SelectorTarget,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    match target {
        SelectorTarget::Terminal => apply_terminal_properties(st, props),
        SelectorTarget::Selection => apply_selection_properties(st, props),
        SelectorTarget::Highlight => apply_highlight_properties(st, props),
        SelectorTarget::Cursor => apply_cursor_properties(st, props),
        SelectorTarget::Backdrop => apply_backdrop_properties(st, props),
        SelectorTarget::TabBar => apply_tab_bar_properties(st, props),
        SelectorTarget::Tab => apply_tab_properties(st, props),
        SelectorTarget::TabActive => apply_tab_active_properties(st, props),
        SelectorTarget::TabClose => apply_tab_close_properties(st, props),
        SelectorTarget::Palette => apply_palette_properties(st, props),
        SelectorTarget::Focus => apply_focus_properties(st, props),
        SelectorTarget::Hover => apply_hover_properties(st, props),
        SelectorTarget::ContextMenu => apply_context_menu_properties(st, props),
        SelectorTarget::SearchBar => apply_search_bar_properties(st, props),
        SelectorTarget::RenameBar => apply_rename_bar_properties(st, props),
        SelectorTarget::Event(kind) => {
            let ParseState {
                theme, warnings, ..
            } = st;
            let slot = match kind {
                EventKind::Bell => &mut theme.on_bell,
                EventKind::CommandFail => &mut theme.on_command_fail,
                EventKind::CommandSuccess => &mut theme.on_command_success,
                EventKind::Focus => &mut theme.on_focus,
                EventKind::Blur => &mut theme.on_blur,
            };
            apply_event_properties(slot, props, warnings)
        }
    }
}

fn apply_terminal_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme,
        warnings,
        bg_image,
    } = st;
    let typo_defaults = Typography::default();

    // Typography
    if let Some(font) = props.get("font-family") {
        theme.typography.font_family = font.split(',').map(strip_quotes).collect();
    }
    if let Some(size) = props.get("font-size") {
        theme.typography.font_size =
            number_or(size, "font-size", typo_defaults.font_size, warnings);
    }
    if let Some(height) = props.get("line-height") {
        theme.typography.line_height =
            number_or(height, "line-height", typo_defaults.line_height, warnings);
    }

    // Colors
    if let Some(color) = props.color("color")? {
        theme.foreground = color;
    }
    if let Some(bg) = props.background_gradient(warnings)? {
        theme.background = bg;
    } else if let Some(color) = props.color("background-color")? {
        theme.background = solid_gradient(color);
    }

    // Text shadow / glow
    if let Some(shadow) = props.text_shadow()? {
        theme.text_shadow = Some(shadow);
    }

    // Background image: accumulated across rules so `background-size` or
    // `--background-opacity` in a different `:terminal` block still applies.
    if let Some(url) = props.get("background-image") {
        bg_image.path = Some(url.clone());
    }
    if let Some(s) = props.get("background-size") {
        bg_image.size = parse_background_size(s);
    }
    if let Some(s) = props.get("background-position") {
        bg_image.position = parse_background_position(s);
    }
    if let Some(s) = props.get("background-repeat") {
        bg_image.repeat = parse_background_repeat(s);
    }
    if let Some(v) = props.custom("--background-opacity") {
        bg_image.opacity =
            number_or::<f32>(v, "--background-opacity", 1.0, warnings).clamp(0.0, 1.0);
    }

    // ANSI palette colors - supports both --ansi-* and --color-* naming
    apply_ansi_palette(theme, props)?;

    // Font variants
    if let Some(f) = props.custom_string("--font-bold") {
        theme.typography.font_bold = Some(f);
    }
    if let Some(f) = props.custom_string("--font-italic") {
        theme.typography.font_italic = Some(f);
    }
    if let Some(f) = props.custom_string("--font-bold-italic") {
        theme.typography.font_bold_italic = Some(f);
    }
    if let Some(v) = props.get("ligatures")
        && let Some(b) = parse_bool(v, "ligatures", warnings)
    {
        theme.typography.ligatures = b;
    }

    Ok(())
}

fn apply_selection_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.selection.background = bg;
    }
    if let Some(fg) = props.color("color")? {
        st.theme.selection.foreground = fg;
    }
    Ok(())
}

fn apply_highlight_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.highlight.background = bg;
    }
    if let Some(fg) = props.color("color")? {
        st.theme.highlight.foreground = fg;
    }
    if let Some(bg) = props.custom_color("--current-background")? {
        st.theme.highlight.current_background = bg;
    }
    Ok(())
}

fn apply_cursor_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.cursor_color = bg;
    }
    // Cursor glow uses same syntax as text-shadow
    if let Some(shadow) = props.text_shadow()? {
        st.theme.cursor_glow = Some(shadow);
    }
    Ok(())
}

/// Decide whether an effect is enabled after this rule.
///
/// `--<effect>-enabled` always wins; otherwise the presence of any
/// `--<effect>-*` key enables the effect; otherwise the previous state is kept.
fn resolve_enabled(
    props: &RuleProperties,
    prefix: &str,
    current: bool,
    warnings: &mut Vec<String>,
) -> bool {
    let key = format!("{prefix}enabled");
    if let Some(v) = props.custom(&key) {
        return parse_bool(v, &key, warnings).unwrap_or(true);
    }
    if props.has_custom_prefix(prefix) {
        return true;
    }
    current
}

fn apply_backdrop_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;

    // Grid
    {
        let d = GridEffect::default();
        let mut grid = theme.grid.unwrap_or(GridEffect {
            enabled: false,
            ..d
        });
        if let Some(c) = props.custom_color("--grid-color")? {
            grid.color = c;
        }
        if let Some(v) = props.custom("--grid-spacing") {
            grid.spacing = number_or(v, "--grid-spacing", d.spacing, warnings);
        }
        if let Some(v) = props.custom("--grid-line-width") {
            grid.line_width = number_or(v, "--grid-line-width", d.line_width, warnings);
        }
        if let Some(v) = props.custom("--grid-perspective") {
            grid.perspective = number_or(v, "--grid-perspective", d.perspective, warnings);
        }
        if let Some(v) = props.custom("--grid-horizon") {
            grid.horizon = number_or(v, "--grid-horizon", d.horizon, warnings);
        }
        if let Some(v) = props.custom("--grid-animation-speed") {
            grid.animation_speed =
                number_or(v, "--grid-animation-speed", d.animation_speed, warnings);
        }
        if let Some(v) = props.custom("--grid-glow-radius") {
            grid.glow_radius = number_or(v, "--grid-glow-radius", d.glow_radius, warnings);
        }
        if let Some(v) = props.custom("--grid-glow-intensity") {
            grid.glow_intensity = number_or(v, "--grid-glow-intensity", d.glow_intensity, warnings);
        }
        if let Some(v) = props.custom("--grid-vanishing-spread") {
            grid.vanishing_spread =
                number_or(v, "--grid-vanishing-spread", d.vanishing_spread, warnings);
        }
        if let Some(v) = props.custom("--grid-curved")
            && let Some(b) = parse_bool(v, "--grid-curved", warnings)
        {
            grid.curved = b;
        }
        grid.enabled = resolve_enabled(props, "--grid-", grid.enabled, warnings);
        theme.grid = if grid.enabled { Some(grid) } else { None };
    }

    // Starfield
    {
        let d = StarfieldEffect::default();
        let mut starfield = theme.starfield.unwrap_or(StarfieldEffect {
            enabled: false,
            ..d
        });
        if let Some(c) = props.custom_color("--starfield-color")? {
            starfield.color = c;
        }
        if let Some(v) = props.custom("--starfield-density") {
            starfield.density = number_or(v, "--starfield-density", d.density, warnings);
        }
        if let Some(v) = props.custom("--starfield-layers") {
            starfield.layers = number_or(v, "--starfield-layers", d.layers, warnings);
        }
        if let Some(v) = props.custom("--starfield-speed") {
            starfield.speed = number_or(v, "--starfield-speed", d.speed, warnings);
        }
        if let Some(v) = props.custom("--starfield-direction")
            && let Some(dir) = keyword_opt(
                v,
                "--starfield-direction",
                StarDirection::from_str,
                warnings,
            )
        {
            starfield.direction = dir;
        }
        if let Some(v) = props.custom("--starfield-glow-radius") {
            starfield.glow_radius =
                number_or(v, "--starfield-glow-radius", d.glow_radius, warnings);
        }
        if let Some(v) = props.custom("--starfield-glow-intensity") {
            starfield.glow_intensity =
                number_or(v, "--starfield-glow-intensity", d.glow_intensity, warnings);
        }
        if let Some(v) = props.custom("--starfield-twinkle")
            && let Some(b) = parse_bool(v, "--starfield-twinkle", warnings)
        {
            starfield.twinkle = b;
        }
        if let Some(v) = props.custom("--starfield-twinkle-speed") {
            starfield.twinkle_speed =
                number_or(v, "--starfield-twinkle-speed", d.twinkle_speed, warnings);
        }
        if let Some(v) = props.custom("--starfield-min-size") {
            starfield.min_size = number_or(v, "--starfield-min-size", d.min_size, warnings);
        }
        if let Some(v) = props.custom("--starfield-max-size") {
            starfield.max_size = number_or(v, "--starfield-max-size", d.max_size, warnings);
        }
        starfield.enabled = resolve_enabled(props, "--starfield-", starfield.enabled, warnings);
        theme.starfield = if starfield.enabled {
            Some(starfield)
        } else {
            None
        };
    }

    // Rain
    {
        let d = RainEffect::default();
        let mut rain = theme.rain.unwrap_or(RainEffect {
            enabled: false,
            ..d
        });
        if let Some(c) = props.custom_color("--rain-color")? {
            rain.color = c;
        }
        if let Some(v) = props.custom("--rain-density") {
            rain.density = number_or(v, "--rain-density", d.density, warnings);
        }
        if let Some(v) = props.custom("--rain-speed") {
            rain.speed = number_or(v, "--rain-speed", d.speed, warnings);
        }
        if let Some(v) = props.custom("--rain-angle") {
            rain.angle = parse_angle_degrees(v)
                .unwrap_or_else(|| number_or(v, "--rain-angle", d.angle, warnings));
        }
        if let Some(v) = props.custom("--rain-length") {
            rain.length = number_or(v, "--rain-length", d.length, warnings);
        }
        if let Some(v) = props.custom("--rain-thickness") {
            rain.thickness = number_or(v, "--rain-thickness", d.thickness, warnings);
        }
        if let Some(v) = props.custom("--rain-glow-radius") {
            rain.glow_radius = number_or(v, "--rain-glow-radius", d.glow_radius, warnings);
        }
        if let Some(v) = props.custom("--rain-glow-intensity") {
            rain.glow_intensity = number_or(v, "--rain-glow-intensity", d.glow_intensity, warnings);
        }
        rain.enabled = resolve_enabled(props, "--rain-", rain.enabled, warnings);
        theme.rain = if rain.enabled { Some(rain) } else { None };
    }

    // Particles
    {
        let d = ParticleEffect::default();
        let mut particles = theme.particles.unwrap_or(ParticleEffect {
            enabled: false,
            ..d
        });
        if let Some(c) = props.custom_color("--particles-color")? {
            particles.color = c;
        }
        if let Some(v) = props.custom("--particles-count") {
            particles.count = number_or(v, "--particles-count", d.count, warnings);
        }
        if let Some(v) = props.custom("--particles-shape")
            && let Some(shape) =
                keyword_opt(v, "--particles-shape", ParticleShape::from_str, warnings)
        {
            particles.shape = shape;
        }
        if let Some(v) = props.custom("--particles-behavior")
            && let Some(behavior) = keyword_opt(
                v,
                "--particles-behavior",
                ParticleBehavior::from_str,
                warnings,
            )
        {
            particles.behavior = behavior;
        }
        if let Some(v) = props.custom("--particles-size") {
            particles.size = number_or(v, "--particles-size", d.size, warnings);
        }
        if let Some(v) = props.custom("--particles-speed") {
            particles.speed = number_or(v, "--particles-speed", d.speed, warnings);
        }
        if let Some(v) = props.custom("--particles-glow-radius") {
            particles.glow_radius =
                number_or(v, "--particles-glow-radius", d.glow_radius, warnings);
        }
        if let Some(v) = props.custom("--particles-glow-intensity") {
            particles.glow_intensity =
                number_or(v, "--particles-glow-intensity", d.glow_intensity, warnings);
        }
        particles.enabled = resolve_enabled(props, "--particles-", particles.enabled, warnings);
        theme.particles = if particles.enabled {
            Some(particles)
        } else {
            None
        };
    }

    // Matrix
    {
        let d = MatrixEffect::default();
        let mut matrix = theme.matrix.clone().unwrap_or_else(|| MatrixEffect {
            enabled: false,
            ..d.clone()
        });
        if let Some(c) = props.custom_color("--matrix-color")? {
            matrix.color = c;
        }
        if let Some(v) = props.custom("--matrix-density") {
            matrix.density = number_or(v, "--matrix-density", d.density, warnings);
        }
        if let Some(v) = props.custom("--matrix-speed") {
            matrix.speed = number_or(v, "--matrix-speed", d.speed, warnings);
        }
        if let Some(v) = props.custom("--matrix-font-size") {
            matrix.font_size = number_or(v, "--matrix-font-size", d.font_size, warnings);
        }
        if let Some(v) = props.custom_string("--matrix-charset") {
            matrix.charset = v;
        }
        matrix.enabled = resolve_enabled(props, "--matrix-", matrix.enabled, warnings);
        theme.matrix = if matrix.enabled { Some(matrix) } else { None };
    }

    // Shape
    {
        let d = ShapeEffect::default();
        let mut shape = theme.shape.clone().unwrap_or_else(|| ShapeEffect {
            enabled: false,
            ..d.clone()
        });
        if let Some(v) = props.custom("--shape-type")
            && let Some(t) = keyword_opt(v, "--shape-type", ShapeType::from_str, warnings)
        {
            shape.shape_type = t;
        }
        if let Some(v) = props.custom("--shape-size") {
            shape.size = number_or(v, "--shape-size", d.size, warnings);
        }
        if let Some(v) = props.custom("--shape-fill") {
            shape.fill = if v.trim().eq_ignore_ascii_case("none") {
                None
            } else {
                props.custom_color("--shape-fill")?
            };
        }
        if let Some(v) = props.custom("--shape-stroke") {
            shape.stroke = if v.trim().eq_ignore_ascii_case("none") {
                None
            } else {
                props.custom_color("--shape-stroke")?
            };
        }
        if let Some(v) = props.custom("--shape-stroke-width") {
            shape.stroke_width = number_or(v, "--shape-stroke-width", d.stroke_width, warnings);
        }
        if let Some(v) = props.custom("--shape-glow-radius") {
            shape.glow_radius = number_or(v, "--shape-glow-radius", d.glow_radius, warnings);
        }
        if let Some(c) = props.custom_color("--shape-glow-color")? {
            shape.glow_color = Some(c);
        }
        if let Some(v) = props.custom("--shape-rotation")
            && let Some(r) = keyword_opt(v, "--shape-rotation", ShapeRotation::from_str, warnings)
        {
            shape.rotation = r;
        }
        if let Some(v) = props.custom("--shape-rotation-speed") {
            shape.rotation_speed =
                number_or(v, "--shape-rotation-speed", d.rotation_speed, warnings);
        }
        if let Some(v) = props.custom("--shape-motion")
            && let Some(m) = keyword_opt(v, "--shape-motion", ShapeMotion::from_str, warnings)
        {
            shape.motion = m;
        }
        if let Some(v) = props.custom("--shape-motion-speed") {
            shape.motion_speed = number_or(v, "--shape-motion-speed", d.motion_speed, warnings);
        }
        if let Some(v) = props.custom("--shape-polygon-sides") {
            shape.polygon_sides = number_or(v, "--shape-polygon-sides", d.polygon_sides, warnings);
        }
        shape.enabled = resolve_enabled(props, "--shape-", shape.enabled, warnings);
        theme.shape = if shape.enabled { Some(shape) } else { None };
    }

    // Sprite
    {
        let d = SpriteEffect::default();
        let mut sprite = theme.sprite.take().unwrap_or_else(|| SpriteEffect {
            enabled: false,
            ..d.clone()
        });
        if let Some(v) = props.custom_string("--sprite-path") {
            sprite.path = Some(v);
        }
        if let Some(v) = props.custom("--sprite-frame-width") {
            sprite.frame_width = number_or(v, "--sprite-frame-width", d.frame_width, warnings);
        }
        if let Some(v) = props.custom("--sprite-frame-height") {
            sprite.frame_height = number_or(v, "--sprite-frame-height", d.frame_height, warnings);
        }
        if let Some(v) = props.custom("--sprite-columns") {
            sprite.columns = number_or(v, "--sprite-columns", d.columns, warnings);
        }
        if let Some(v) = props.custom("--sprite-rows") {
            sprite.rows = number_or(v, "--sprite-rows", d.rows, warnings);
        }
        if let Some(v) = props.custom("--sprite-frame-count") {
            sprite.frame_count = number_opt(v, "--sprite-frame-count", warnings);
        }
        if let Some(v) = props.custom("--sprite-fps") {
            sprite.fps = number_or(v, "--sprite-fps", d.fps, warnings);
        }
        if let Some(v) = props.custom("--sprite-scale") {
            sprite.scale = number_or(v, "--sprite-scale", d.scale, warnings);
        }
        if let Some(v) = props.custom("--sprite-opacity") {
            sprite.opacity = number_or(v, "--sprite-opacity", d.opacity, warnings);
        }
        if let Some(v) = props.custom("--sprite-motion")
            && let Some(m) = keyword_opt(v, "--sprite-motion", SpriteMotion::from_str, warnings)
        {
            sprite.motion = m;
        }
        if let Some(v) = props.custom("--sprite-motion-speed") {
            sprite.motion_speed = number_or(v, "--sprite-motion-speed", d.motion_speed, warnings);
        }
        if let Some(v) = props.custom("--sprite-position")
            && let Some(p) = keyword_opt(v, "--sprite-position", SpritePosition::from_str, warnings)
        {
            sprite.position = p;
        }
        sprite.enabled = resolve_enabled(props, "--sprite-", sprite.enabled, warnings);
        if sprite.enabled && sprite.path.is_none() {
            warnings.push("sprite effect enabled without a --sprite-path".to_string());
        }
        theme.sprite = if sprite.enabled { Some(sprite) } else { None };
    }

    // CRT post-processing
    {
        let d = CrtEffect::default();
        let mut crt = theme.crt.unwrap_or(CrtEffect {
            enabled: false,
            ..d
        });
        if let Some(v) = props.custom("--crt-scanline-intensity") {
            crt.scanline_intensity = number_or(
                v,
                "--crt-scanline-intensity",
                d.scanline_intensity,
                warnings,
            );
        }
        if let Some(v) = props.custom("--crt-scanline-frequency") {
            crt.scanline_frequency = number_or(
                v,
                "--crt-scanline-frequency",
                d.scanline_frequency,
                warnings,
            );
        }
        if let Some(v) = props.custom("--crt-curvature") {
            crt.curvature = number_or(v, "--crt-curvature", d.curvature, warnings);
        }
        if let Some(v) = props.custom("--crt-vignette") {
            crt.vignette = number_or(v, "--crt-vignette", d.vignette, warnings);
        }
        if let Some(v) = props.custom("--crt-chromatic-aberration") {
            crt.chromatic_aberration = number_or(
                v,
                "--crt-chromatic-aberration",
                d.chromatic_aberration,
                warnings,
            );
        }
        if let Some(v) = props.custom("--crt-bloom") {
            crt.bloom = number_or(v, "--crt-bloom", d.bloom, warnings);
        }
        if let Some(v) = props.custom("--crt-flicker") {
            crt.flicker = number_or(v, "--crt-flicker", d.flicker, warnings);
        }
        crt.enabled = resolve_enabled(props, "--crt-", crt.enabled, warnings);
        theme.crt = if crt.enabled { Some(crt) } else { None };
    }

    Ok(())
}

fn apply_tab_bar_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;
    let d = crate::TabBarStyle::default();
    if let Some(bg) = props.color("background")? {
        theme.tabs.bar.background = bg;
    }
    if let Some(c) = props.color("border-color")? {
        theme.tabs.bar.border_color = c;
    }
    if let Some(v) = props.get("height") {
        theme.tabs.bar.height = number_or(v, "height", d.height, warnings);
    }
    if let Some(v) = props.get("padding") {
        theme.tabs.bar.padding = number_or(v, "padding", d.padding, warnings);
    }
    if let Some(v) = props.custom("--content-padding") {
        theme.tabs.bar.content_padding =
            number_or(v, "--content-padding", d.content_padding, warnings);
    }
    Ok(())
}

fn apply_tab_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;
    let d = crate::TabStyle::default();
    if let Some(bg) = props.color("background")? {
        theme.tabs.tab.background = bg;
    }
    if let Some(fg) = props.color("color")? {
        theme.tabs.tab.foreground = fg;
    }
    if let Some(v) = props.get("border-radius") {
        theme.tabs.tab.border_radius = number_or(v, "border-radius", d.border_radius, warnings);
    }
    if let Some(v) = props.get("padding-x") {
        theme.tabs.tab.padding_x = number_or(v, "padding-x", d.padding_x, warnings);
    }
    if let Some(v) = props.get("padding-y") {
        theme.tabs.tab.padding_y = number_or(v, "padding-y", d.padding_y, warnings);
    }
    if let Some(v) = props.get("min-width") {
        theme.tabs.tab.min_width = number_or(v, "min-width", d.min_width, warnings);
    }
    if let Some(v) = props.get("max-width") {
        theme.tabs.tab.max_width = number_or(v, "max-width", d.max_width, warnings);
    }
    if let Some(shadow) = props.text_shadow()? {
        theme.tabs.tab.text_shadow = Some(shadow);
    }
    Ok(())
}

fn apply_tab_active_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.tabs.active.background = bg;
    }
    if let Some(fg) = props.color("color")? {
        st.theme.tabs.active.foreground = fg;
    }
    if let Some(c) = props.color("accent-color")? {
        st.theme.tabs.active.accent = c;
    }
    if let Some(shadow) = props.text_shadow()? {
        st.theme.tabs.active.text_shadow = Some(shadow);
    }
    Ok(())
}

fn apply_tab_close_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;
    if let Some(bg) = props.color("background")? {
        theme.tabs.close.background = bg;
    }
    if let Some(fg) = props.color("color")? {
        theme.tabs.close.foreground = fg;
    }
    if let Some(bg) = props.custom_color("--hover-background")? {
        theme.tabs.close.hover_background = bg;
    }
    if let Some(fg) = props.custom_color("--hover-color")? {
        theme.tabs.close.hover_foreground = fg;
    }
    if let Some(v) = props.get("width") {
        theme.tabs.close.size =
            number_or(v, "width", crate::TabCloseStyle::default().size, warnings);
    }
    Ok(())
}

fn apply_focus_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;
    let d = crate::FocusStyle::default();

    // Standard properties
    if let Some(c) = props.color("outline-color")? {
        theme.ui.focus.ring_color = c;
    }
    if let Some(v) = props.get("outline-width") {
        theme.ui.focus.ring_thickness = number_or(v, "outline-width", d.ring_thickness, warnings);
    }

    // Custom properties
    if let Some(c) = props.custom_color("--ring-color")? {
        theme.ui.focus.ring_color = c;
    }
    if let Some(c) = props.custom_color("--glow-color")? {
        theme.ui.focus.glow_color = c;
    }
    if let Some(v) = props.custom("--ring-thickness") {
        theme.ui.focus.ring_thickness =
            number_or(v, "--ring-thickness", d.ring_thickness, warnings);
    }
    if let Some(v) = props.custom("--glow-size") {
        theme.ui.focus.glow_size = number_or(v, "--glow-size", d.glow_size, warnings);
    }
    Ok(())
}

fn apply_hover_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.ui.hover.background = bg;
    }
    Ok(())
}

fn apply_context_menu_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.ui.context_menu.background = bg;
    }
    if let Some(c) = props.color("border-color")? {
        st.theme.ui.context_menu.border_color = c;
    }
    if let Some(c) = props.color("color")? {
        st.theme.ui.context_menu.text_color = c;
    }
    Ok(())
}

fn apply_search_bar_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.ui.search_bar.background = bg;
    }
    if let Some(c) = props.color("color")? {
        st.theme.ui.search_bar.text_color = c;
    }
    if let Some(c) = props.custom_color("--placeholder-color")? {
        st.theme.ui.search_bar.placeholder_color = c;
    }
    if let Some(c) = props.custom_color("--no-match-color")? {
        st.theme.ui.search_bar.no_match_color = c;
    }
    Ok(())
}

fn apply_rename_bar_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    if let Some(bg) = props.color("background")? {
        st.theme.ui.rename_bar.background = bg;
    }
    if let Some(c) = props.color("color")? {
        st.theme.ui.rename_bar.text_color = c;
    }
    if let Some(c) = props.custom_color("--label-color")? {
        st.theme.ui.rename_bar.label_color = c;
    }
    Ok(())
}

/// Apply event override properties (::on-bell, ::on-command-fail, etc.)
/// Multiple blocks for the same event are merged (CSS cascade)
fn apply_event_properties(
    event: &mut Option<EventOverride>,
    props: &RuleProperties,
    warnings: &mut Vec<String>,
) -> Result<(), ThemeParseError> {
    // Create new override or merge into existing
    let mut override_block = EventOverride::default();

    // Parse --duration (e.g., "500ms", "1s", "1000").  Presence is tracked so
    // that a later `--duration: 0` can override an earlier non-zero value.
    if let Some(v) = props.custom("--duration") {
        override_block.duration_ms = parse_duration(v, warnings);
        override_block.duration_set = true;
    }

    // Parse sprite patch properties (--sprite-*)
    let has_sprite_patch = props
        .custom
        .keys()
        .any(|k| k.starts_with("--sprite-") && !k.starts_with("--sprite-overlay"));
    if has_sprite_patch {
        let mut patch = SpritePatch::default();

        if let Some(v) = props.custom_string("--sprite-path") {
            patch.path = Some(v);
        }
        if let Some(v) = props.custom("--sprite-columns") {
            patch.columns = number_opt(v, "--sprite-columns", warnings);
        }
        if let Some(v) = props.custom("--sprite-rows") {
            patch.rows = number_opt(v, "--sprite-rows", warnings);
        }
        if let Some(v) = props.custom("--sprite-fps") {
            patch.fps = number_opt(v, "--sprite-fps", warnings);
        }
        if let Some(v) = props.custom("--sprite-opacity") {
            patch.opacity = number_opt(v, "--sprite-opacity", warnings);
        }
        if let Some(v) = props.custom("--sprite-scale") {
            patch.scale = number_opt(v, "--sprite-scale", warnings);
        }
        if let Some(v) = props.custom("--sprite-motion-speed") {
            patch.motion_speed = number_opt(v, "--sprite-motion-speed", warnings);
        }

        override_block.sprite_patch = Some(patch);
    }

    // Parse sprite overlay properties (--sprite-overlay-*)
    if let Some(path) = props.custom_string("--sprite-overlay") {
        let d = SpriteOverlay::default();
        let mut overlay = SpriteOverlay { path, ..d.clone() };

        if let Some(v) = props.custom("--sprite-overlay-position")
            && let Some(pos) = keyword_opt(
                v,
                "--sprite-overlay-position",
                SpriteOverlayPosition::from_str,
                warnings,
            )
        {
            overlay.position = pos;
        }
        if let Some(v) = props.custom("--sprite-overlay-columns") {
            overlay.columns = number_or(v, "--sprite-overlay-columns", d.columns, warnings);
        }
        if let Some(v) = props.custom("--sprite-overlay-rows") {
            overlay.rows = number_or(v, "--sprite-overlay-rows", d.rows, warnings);
        }
        if let Some(v) = props.custom("--sprite-overlay-fps") {
            overlay.fps = number_or(v, "--sprite-overlay-fps", d.fps, warnings);
        }
        if let Some(v) = props.custom("--sprite-overlay-scale") {
            overlay.scale = number_or(v, "--sprite-overlay-scale", d.scale, warnings);
        }
        if let Some(v) = props.custom("--sprite-overlay-opacity") {
            overlay.opacity = number_or(v, "--sprite-overlay-opacity", d.opacity, warnings);
        }

        override_block.sprite_overlay = Some(overlay);
    }

    // Parse theme property overrides
    if let Some(c) = props.color("color")? {
        override_block.foreground = Some(c);
    }
    if let Some(bg) = props.background_gradient(warnings)? {
        override_block.background = Some(bg);
    } else if let Some(c) = props.color("background-color")? {
        override_block.background = Some(solid_gradient(c));
    }
    if let Some(c) = props.color("cursor-color")? {
        override_block.cursor_color = Some(c);
    }
    if let Some(c) = props.custom_color("--cursor-color")? {
        override_block.cursor_color = Some(c);
    }
    if let Some(s) = props.custom("--cursor-shape") {
        override_block.cursor_shape =
            keyword_opt(s, "--cursor-shape", parse_cursor_shape, warnings);
    }
    if let Some(shadow) = props.text_shadow()? {
        override_block.text_shadow = Some(shadow);
    }

    // Parse flash overlay properties
    if let Some(c) = props.custom_color("--flash-color")? {
        override_block.flash_color = Some(c);
    }
    if let Some(v) = props.custom("--flash-intensity")
        && let Some(intensity) = number_opt::<f32>(v, "--flash-intensity", warnings)
    {
        override_block.flash_intensity = Some(intensity.clamp(0.0, 1.0));
    }

    // Parse starfield patch properties
    if props.has_custom_prefix("--starfield-") {
        let mut patch = StarfieldPatch::default();
        if let Some(c) = props.custom_color("--starfield-color")? {
            patch.color = Some(c);
        }
        if let Some(v) = props.custom("--starfield-density") {
            patch.density = number_opt(v, "--starfield-density", warnings);
        }
        if let Some(v) = props.custom("--starfield-layers") {
            patch.layers = number_opt(v, "--starfield-layers", warnings);
        }
        if let Some(v) = props.custom("--starfield-speed") {
            patch.speed = number_opt(v, "--starfield-speed", warnings);
        }
        if let Some(v) = props.custom("--starfield-direction") {
            patch.direction = keyword_opt(
                v,
                "--starfield-direction",
                StarDirection::from_str,
                warnings,
            );
        }
        if let Some(v) = props.custom("--starfield-glow-radius") {
            patch.glow_radius = number_opt(v, "--starfield-glow-radius", warnings);
        }
        if let Some(v) = props.custom("--starfield-glow-intensity") {
            patch.glow_intensity = number_opt(v, "--starfield-glow-intensity", warnings);
        }
        if let Some(v) = props.custom("--starfield-twinkle") {
            patch.twinkle = parse_bool(v, "--starfield-twinkle", warnings);
        }
        if let Some(v) = props.custom("--starfield-twinkle-speed") {
            patch.twinkle_speed = number_opt(v, "--starfield-twinkle-speed", warnings);
        }
        if let Some(v) = props.custom("--starfield-min-size") {
            patch.min_size = number_opt(v, "--starfield-min-size", warnings);
        }
        if let Some(v) = props.custom("--starfield-max-size") {
            patch.max_size = number_opt(v, "--starfield-max-size", warnings);
        }
        override_block.starfield_patch = Some(patch);
    }

    // Parse particle patch properties
    if props.has_custom_prefix("--particles-") {
        let mut patch = ParticlePatch::default();
        if let Some(c) = props.custom_color("--particles-color")? {
            patch.color = Some(c);
        }
        if let Some(v) = props.custom("--particles-count") {
            patch.count = number_opt(v, "--particles-count", warnings);
        }
        if let Some(v) = props.custom("--particles-shape") {
            patch.shape = keyword_opt(v, "--particles-shape", ParticleShape::from_str, warnings);
        }
        if let Some(v) = props.custom("--particles-behavior") {
            patch.behavior = keyword_opt(
                v,
                "--particles-behavior",
                ParticleBehavior::from_str,
                warnings,
            );
        }
        if let Some(v) = props.custom("--particles-size") {
            patch.size = number_opt(v, "--particles-size", warnings);
        }
        if let Some(v) = props.custom("--particles-speed") {
            patch.speed = number_opt(v, "--particles-speed", warnings);
        }
        if let Some(v) = props.custom("--particles-glow-radius") {
            patch.glow_radius = number_opt(v, "--particles-glow-radius", warnings);
        }
        if let Some(v) = props.custom("--particles-glow-intensity") {
            patch.glow_intensity = number_opt(v, "--particles-glow-intensity", warnings);
        }
        override_block.particle_patch = Some(patch);
    }

    // Parse grid patch properties
    if props.has_custom_prefix("--grid-") {
        let mut patch = GridPatch::default();
        if let Some(c) = props.custom_color("--grid-color")? {
            patch.color = Some(c);
        }
        if let Some(v) = props.custom("--grid-spacing") {
            patch.spacing = number_opt(v, "--grid-spacing", warnings);
        }
        if let Some(v) = props.custom("--grid-line-width") {
            patch.line_width = number_opt(v, "--grid-line-width", warnings);
        }
        if let Some(v) = props.custom("--grid-perspective") {
            patch.perspective = number_opt(v, "--grid-perspective", warnings);
        }
        if let Some(v) = props.custom("--grid-horizon") {
            patch.horizon = number_opt(v, "--grid-horizon", warnings);
        }
        if let Some(v) = props.custom("--grid-animation-speed") {
            patch.animation_speed = number_opt(v, "--grid-animation-speed", warnings);
        }
        if let Some(v) = props.custom("--grid-glow-radius") {
            patch.glow_radius = number_opt(v, "--grid-glow-radius", warnings);
        }
        if let Some(v) = props.custom("--grid-glow-intensity") {
            patch.glow_intensity = number_opt(v, "--grid-glow-intensity", warnings);
        }
        if let Some(v) = props.custom("--grid-vanishing-spread") {
            patch.vanishing_spread = number_opt(v, "--grid-vanishing-spread", warnings);
        }
        if let Some(v) = props.custom("--grid-curved") {
            patch.curved = parse_bool(v, "--grid-curved", warnings);
        }
        override_block.grid_patch = Some(patch);
    }

    // Parse rain patch properties
    if props.has_custom_prefix("--rain-") {
        let mut patch = RainPatch::default();
        if let Some(c) = props.custom_color("--rain-color")? {
            patch.color = Some(c);
        }
        if let Some(v) = props.custom("--rain-density") {
            patch.density = number_opt(v, "--rain-density", warnings);
        }
        if let Some(v) = props.custom("--rain-speed") {
            patch.speed = number_opt(v, "--rain-speed", warnings);
        }
        if let Some(v) = props.custom("--rain-angle") {
            patch.angle =
                parse_angle_degrees(v).or_else(|| number_opt(v, "--rain-angle", warnings));
        }
        if let Some(v) = props.custom("--rain-length") {
            patch.length = number_opt(v, "--rain-length", warnings);
        }
        if let Some(v) = props.custom("--rain-thickness") {
            patch.thickness = number_opt(v, "--rain-thickness", warnings);
        }
        if let Some(v) = props.custom("--rain-glow-radius") {
            patch.glow_radius = number_opt(v, "--rain-glow-radius", warnings);
        }
        if let Some(v) = props.custom("--rain-glow-intensity") {
            patch.glow_intensity = number_opt(v, "--rain-glow-intensity", warnings);
        }
        override_block.rain_patch = Some(patch);
    }

    // Parse matrix patch properties
    if props.has_custom_prefix("--matrix-") {
        let mut patch = MatrixPatch::default();
        if let Some(c) = props.custom_color("--matrix-color")? {
            patch.color = Some(c);
        }
        if let Some(v) = props.custom("--matrix-density") {
            patch.density = number_opt(v, "--matrix-density", warnings);
        }
        if let Some(v) = props.custom("--matrix-speed") {
            patch.speed = number_opt(v, "--matrix-speed", warnings);
        }
        if let Some(v) = props.custom("--matrix-font-size") {
            patch.font_size = number_opt(v, "--matrix-font-size", warnings);
        }
        if let Some(v) = props.custom_string("--matrix-charset") {
            patch.charset = Some(v);
        }
        override_block.matrix_patch = Some(patch);
    }

    // Parse shape patch properties
    if props.has_custom_prefix("--shape-") {
        let mut patch = ShapePatch::default();
        if let Some(v) = props.custom("--shape-type") {
            patch.shape_type = keyword_opt(v, "--shape-type", ShapeType::from_str, warnings);
        }
        if let Some(v) = props.custom("--shape-size") {
            patch.size = number_opt(v, "--shape-size", warnings);
        }
        if let Some(v) = props.custom("--shape-fill")
            && !v.trim().eq_ignore_ascii_case("none")
        {
            patch.fill = props.custom_color("--shape-fill")?;
        }
        if let Some(v) = props.custom("--shape-stroke")
            && !v.trim().eq_ignore_ascii_case("none")
        {
            patch.stroke = props.custom_color("--shape-stroke")?;
        }
        if let Some(v) = props.custom("--shape-stroke-width") {
            patch.stroke_width = number_opt(v, "--shape-stroke-width", warnings);
        }
        if let Some(v) = props.custom("--shape-glow-radius") {
            patch.glow_radius = number_opt(v, "--shape-glow-radius", warnings);
        }
        if let Some(c) = props.custom_color("--shape-glow-color")? {
            patch.glow_color = Some(c);
        }
        if let Some(v) = props.custom("--shape-rotation") {
            patch.rotation = keyword_opt(v, "--shape-rotation", ShapeRotation::from_str, warnings);
        }
        if let Some(v) = props.custom("--shape-rotation-speed") {
            patch.rotation_speed = number_opt(v, "--shape-rotation-speed", warnings);
        }
        if let Some(v) = props.custom("--shape-motion") {
            patch.motion = keyword_opt(v, "--shape-motion", ShapeMotion::from_str, warnings);
        }
        if let Some(v) = props.custom("--shape-motion-speed") {
            patch.motion_speed = number_opt(v, "--shape-motion-speed", warnings);
        }
        if let Some(v) = props.custom("--shape-polygon-sides") {
            patch.polygon_sides = number_opt(v, "--shape-polygon-sides", warnings);
        }
        override_block.shape_patch = Some(patch);
    }

    // Merge with existing or set as new
    if let Some(existing) = event.as_mut() {
        existing.merge(override_block);
    } else {
        *event = Some(override_block);
    }

    Ok(())
}

/// Apply ANSI palette colors from custom properties
/// Supports multiple naming conventions:
/// - --ansi-black, --ansi-red, etc. (preferred)
/// - --ansi-bright-black, --ansi-bright-red, etc. (preferred)
/// - --color-black, --color-red, etc. (legacy)
/// - --color-bright-black, etc. (legacy)
fn apply_ansi_palette(theme: &mut Theme, props: &RuleProperties) -> Result<(), ThemeParseError> {
    // Legacy `--color-*` first, then the preferred `--ansi-*` so it wins when both are given.
    for prefix in ["--color-", "--ansi-"] {
        for key in props.custom.keys() {
            let Some(name) = key.strip_prefix(prefix) else {
                continue;
            };
            let Some(slot) = palette_slot_by_name(theme, name) else {
                continue;
            };
            if let Some(color) = props.custom_color(key)? {
                *slot = color;
            }
        }
    }
    Ok(())
}

fn palette_slot_by_name<'a>(theme: &'a mut Theme, name: &str) -> Option<&'a mut Color> {
    let p = &mut theme.palette;
    Some(match name {
        "black" => &mut p.black,
        "red" => &mut p.red,
        "green" => &mut p.green,
        "yellow" => &mut p.yellow,
        "blue" => &mut p.blue,
        "magenta" => &mut p.magenta,
        "cyan" => &mut p.cyan,
        "white" => &mut p.white,
        "bright-black" => &mut p.bright_black,
        "bright-red" => &mut p.bright_red,
        "bright-green" => &mut p.bright_green,
        "bright-yellow" => &mut p.bright_yellow,
        "bright-blue" => &mut p.bright_blue,
        "bright-magenta" => &mut p.bright_magenta,
        "bright-cyan" => &mut p.bright_cyan,
        "bright-white" => &mut p.bright_white,
        _ => return None,
    })
}

fn palette_slot_by_index(theme: &mut Theme, idx: u8) -> Option<&mut Color> {
    let p = &mut theme.palette;
    Some(match idx {
        0 => &mut p.black,
        1 => &mut p.red,
        2 => &mut p.green,
        3 => &mut p.yellow,
        4 => &mut p.blue,
        5 => &mut p.magenta,
        6 => &mut p.cyan,
        7 => &mut p.white,
        8 => &mut p.bright_black,
        9 => &mut p.bright_red,
        10 => &mut p.bright_green,
        11 => &mut p.bright_yellow,
        12 => &mut p.bright_blue,
        13 => &mut p.bright_magenta,
        14 => &mut p.bright_cyan,
        15 => &mut p.bright_white,
        _ => return None,
    })
}

/// Apply `::palette` colours: `--color-0` .. `--color-255`.
/// Iterates the rule's custom properties once instead of probing 256 keys.
fn apply_palette_properties(
    st: &mut ParseState,
    props: &RuleProperties,
) -> Result<(), ThemeParseError> {
    let ParseState {
        theme, warnings, ..
    } = st;
    for key in props.custom.keys() {
        let Some(suffix) = key.strip_prefix("--color-") else {
            continue;
        };
        let Ok(idx) = suffix.parse::<u8>() else {
            if suffix.parse::<u32>().is_ok() {
                warnings.push(format!("{key}: palette index out of range (0-255)"));
            }
            continue;
        };
        let Some(color) = props.custom_color(key)? else {
            continue;
        };
        if let Some(slot) = palette_slot_by_index(theme, idx) {
            *slot = color;
        } else {
            theme.palette.set_extended(idx, color);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_color() {
        let c = parse_hex_color("#ff5555").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);
        assert!((c.g - 0.333).abs() < 0.01);

        let c = parse_hex_color("#fff").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_rgb_color() {
        let c = parse_rgb_color("rgb(255, 85, 85)").unwrap();
        assert!((c.r - 1.0).abs() < 0.01);

        let c = parse_rgb_color("rgba(0, 255, 255, 0.6)").unwrap();
        assert!((c.g - 1.0).abs() < 0.01);
        assert!((c.a - 0.6).abs() < 0.01);
    }

    #[test]
    fn test_parse_simple_theme() {
        let css = r#"
            :terminal {
                color: #c8c8c8;
                background: #1a1a1a;
            }
        "#;

        let theme = parse_theme(css).unwrap();
        assert!((theme.foreground.r - 0.784).abs() < 0.01);
    }

    #[test]
    fn test_parse_theme_with_comments() {
        let css = r#"
            :terminal {
                /* Typography */
                font-family: "JetBrains Mono", monospace;
                font-size: 14px;

                /* Base colors - teal text */
                color: #61e2fe;
                background: #1a1a1a;
            }
        "#;

        let theme = parse_theme(css).unwrap();
        assert!((theme.foreground.r - 97.0 / 255.0).abs() < 0.01);
        assert!((theme.foreground.g - 226.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_gradient() {
        let g = parse_linear_gradient("linear-gradient(to bottom, #1a0a2e, #16213e)").unwrap();
        assert!(g.top.r < 0.2);
    }

    #[test]
    fn test_parse_ansi_palette() {
        let css = r#"
            :terminal {
                --ansi-black: #1a1a2e;
                --ansi-red: #ff5555;
                --ansi-green: #50fa7b;
                --ansi-yellow: #f1fa8c;
                --ansi-blue: #6272a4;
                --ansi-magenta: #ff79c6;
                --ansi-cyan: #8be9fd;
                --ansi-white: #f8f8f2;
                --ansi-bright-black: #44475a;
                --ansi-bright-red: #ff6e6e;
                --ansi-bright-green: #69ff94;
                --ansi-bright-yellow: #ffffa5;
                --ansi-bright-blue: #d6acff;
                --ansi-bright-magenta: #ff92df;
                --ansi-bright-cyan: #a4ffff;
                --ansi-bright-white: #ffffff;
            }
        "#;

        let theme = parse_theme(css).unwrap();

        // Check normal colors
        assert!((theme.palette.red.r - 1.0).abs() < 0.01); // #ff5555
        assert!((theme.palette.green.g - 0.98).abs() < 0.02); // #50fa7b
        assert!((theme.palette.cyan.b - 0.99).abs() < 0.02); // #8be9fd

        // Check bright colors
        assert!((theme.palette.bright_white.r - 1.0).abs() < 0.01); // #ffffff
        assert!((theme.palette.bright_black.r - 0.267).abs() < 0.02); // #44475a

        // Test palette.get() method
        let red = theme.palette.get(1);
        assert!((red.r - 1.0).abs() < 0.01);

        let bright_cyan = theme.palette.get(14);
        assert!((bright_cyan.r - 0.643).abs() < 0.02); // #a4ffff
    }

    #[test]
    fn test_parse_background_image() {
        let css = r#"
            :terminal {
                background-image: url("/path/to/image.png");
                background-size: cover;
                background-position: center;
                background-repeat: no-repeat;
                --background-opacity: 0.8;
            }
        "#;

        let theme = parse_theme(css).unwrap();
        let bg = theme
            .background_image
            .expect("background_image should be set");
        assert_eq!(bg.path, Some("/path/to/image.png".to_string()));
        assert_eq!(bg.size, BackgroundSize::Cover);
        assert_eq!(bg.position, BackgroundPosition::Center);
        assert_eq!(bg.repeat, BackgroundRepeat::NoRepeat);
        assert!((bg.opacity - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_parse_background_size() {
        assert_eq!(parse_background_size("cover"), BackgroundSize::Cover);
        assert_eq!(parse_background_size("contain"), BackgroundSize::Contain);
        assert_eq!(parse_background_size("auto"), BackgroundSize::Auto);
        assert_eq!(
            parse_background_size("100px 200px"),
            BackgroundSize::Fixed(100, 200)
        );
    }

    #[test]
    fn test_parse_background_position() {
        assert_eq!(
            parse_background_position("center"),
            BackgroundPosition::Center
        );
        assert_eq!(
            parse_background_position("top left"),
            BackgroundPosition::TopLeft
        );
        assert_eq!(
            parse_background_position("bottom right"),
            BackgroundPosition::BottomRight
        );
    }

    #[test]
    fn test_parse_background_repeat() {
        assert_eq!(
            parse_background_repeat("no-repeat"),
            BackgroundRepeat::NoRepeat
        );
        assert_eq!(parse_background_repeat("repeat"), BackgroundRepeat::Repeat);
        assert_eq!(
            parse_background_repeat("repeat-x"),
            BackgroundRepeat::RepeatX
        );
        assert_eq!(
            parse_background_repeat("repeat-y"),
            BackgroundRepeat::RepeatY
        );
    }

    #[test]
    fn test_parse_extended_palette() {
        let css = r#"
            :terminal::palette {
                --color-0: #000000;
                --color-15: #ffffff;
                --color-226: #61fe71;
                --color-178: #71fe81;
                --color-255: #7d8d80;
            }
        "#;

        let theme = parse_theme(css).unwrap();

        // Base colors
        assert!((theme.palette.black.r - 0.0).abs() < 0.01);
        assert!((theme.palette.bright_white.r - 1.0).abs() < 0.01);

        // Extended colors should be set
        let color_226 = theme.palette.get_extended(226);
        assert!(color_226.is_some());
        let c = color_226.unwrap();
        assert!((c.r - 0.38).abs() < 0.02); // #61 = 97/255 = 0.38

        let color_178 = theme.palette.get_extended(178);
        assert!(color_178.is_some());

        let color_255 = theme.palette.get_extended(255);
        assert!(color_255.is_some());

        // Non-overridden extended color should return None
        let color_100 = theme.palette.get_extended(100);
        assert!(color_100.is_none());
    }

    #[test]
    fn test_parse_cursor_with_glow() {
        let css = r#"
            :terminal::cursor {
                background: #ff00ff;
                text-shadow: 0 0 15px rgba(255, 0, 255, 0.8);
            }
        "#;

        let theme = parse_theme(css).unwrap();

        // Check cursor color
        assert!((theme.cursor_color.r - 1.0).abs() < 0.01); // #ff00ff
        assert!((theme.cursor_color.g - 0.0).abs() < 0.01);
        assert!((theme.cursor_color.b - 1.0).abs() < 0.01);

        // Check cursor glow
        assert!(theme.cursor_glow.is_some());
        let glow = theme.cursor_glow.unwrap();
        assert!((glow.radius - 15.0).abs() < 0.01);
        assert!((glow.color.r - 1.0).abs() < 0.01);
        assert!((glow.color.g - 0.0).abs() < 0.01);
        assert!((glow.color.b - 1.0).abs() < 0.01);
        assert!((glow.color.a - 0.8).abs() < 0.01);
    }

    // ========== Color Parsing Edge Cases ==========

    #[test]
    fn test_hex_color_3_digit() {
        // #rgb shorthand expands to #rrggbb
        let c = parse_hex_color("#f00").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);
        assert!((c.a - 1.0).abs() < 0.001);

        let c = parse_hex_color("#abc").unwrap();
        assert!((c.r - 0xaa as f32 / 255.0).abs() < 0.001);
        assert!((c.g - 0xbb as f32 / 255.0).abs() < 0.001);
        assert!((c.b - 0xcc as f32 / 255.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_4_digit() {
        // #rgba shorthand
        let c = parse_hex_color("#f008").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);
        assert!((c.a - 0x88 as f32 / 255.0).abs() < 0.001);

        let c = parse_hex_color("#0000").unwrap();
        assert!((c.r - 0.0).abs() < 0.001);
        assert!((c.a - 0.0).abs() < 0.001);

        let c = parse_hex_color("#ffff").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.a - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_6_digit() {
        let c = parse_hex_color("#000000").unwrap();
        assert!((c.r - 0.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);

        let c = parse_hex_color("#ffffff").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 1.0).abs() < 0.001);
        assert!((c.b - 1.0).abs() < 0.001);

        let c = parse_hex_color("#1a2b3c").unwrap();
        assert!((c.r - 0x1a as f32 / 255.0).abs() < 0.001);
        assert!((c.g - 0x2b as f32 / 255.0).abs() < 0.001);
        assert!((c.b - 0x3c as f32 / 255.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_8_digit() {
        let c = parse_hex_color("#ff000080").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);
        assert!((c.a - 0x80 as f32 / 255.0).abs() < 0.001);

        let c = parse_hex_color("#00000000").unwrap();
        assert!((c.a - 0.0).abs() < 0.001);

        let c = parse_hex_color("#ffffffff").unwrap();
        assert!((c.a - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_uppercase() {
        let c = parse_hex_color("#FF5500").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0x55 as f32 / 255.0).abs() < 0.001);

        let c = parse_hex_color("#ABC").unwrap();
        assert!((c.r - 0xaa as f32 / 255.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_without_hash() {
        // parse_hex_color strips leading #
        let c = parse_hex_color("ff0000").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hex_color_invalid() {
        // Invalid length
        assert!(parse_hex_color("#f").is_err());
        assert!(parse_hex_color("#ff").is_err());
        assert!(parse_hex_color("#fffff").is_err());
        assert!(parse_hex_color("#fffffff").is_err());

        // Invalid characters
        assert!(parse_hex_color("#gggggg").is_err());
        assert!(parse_hex_color("#xyz").is_err());
    }

    #[test]
    fn test_rgb_color_0_255_values() {
        let c = parse_rgb_color("rgb(255, 128, 0)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 128.0 / 255.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);

        let c = parse_rgb_color("rgb(0, 0, 0)").unwrap();
        assert!((c.r - 0.0).abs() < 0.001);

        let c = parse_rgb_color("rgb(255, 255, 255)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_rgb_color_small_components_use_css_semantics() {
        // rgb() channels are always 0-255: rgb(1, 1, 1) is near-black, not white
        let c = parse_rgb_color("rgb(1, 1, 1)").unwrap();
        assert!((c.r - 1.0 / 255.0).abs() < 0.001);
        assert!((c.g - 1.0 / 255.0).abs() < 0.001);
        assert!((c.b - 1.0 / 255.0).abs() < 0.001);

        let c = parse_rgb_color("rgb(1.0, 0.5, 0.0)").unwrap();
        assert!((c.r - 1.0 / 255.0).abs() < 0.001);
        assert!((c.g - 0.5 / 255.0).abs() < 0.001);
    }

    #[test]
    fn test_rgb_color_percentages_and_alpha_clamp() {
        let c = parse_rgb_color("rgb(100%, 50%, 0%)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.5).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);

        // Alpha is clamped to 0-1 and channels to 0-255
        let c = parse_rgb_color("rgba(300, -5, 0, 7)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.a - 1.0).abs() < 0.001);
        let c = parse_rgb_color("rgba(0, 0, 0, -1)").unwrap();
        assert!((c.a - 0.0).abs() < 0.001);

        // Modern slash syntax
        let c = parse_rgb_color("rgb(255 0 0 / 50%)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.a - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_rgba_color() {
        let c = parse_rgb_color("rgba(255, 0, 0, 0.5)").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.a - 0.5).abs() < 0.001);

        let c = parse_rgb_color("rgba(0, 0, 0, 0)").unwrap();
        assert!((c.a - 0.0).abs() < 0.001);

        let c = parse_rgb_color("rgba(255, 255, 255, 1.0)").unwrap();
        assert!((c.a - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_rgb_color_whitespace() {
        let c = parse_rgb_color("rgb( 255 , 128 , 64 )").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 128.0 / 255.0).abs() < 0.001);

        let c = parse_rgb_color("  rgb(100, 100, 100)  ").unwrap();
        assert!((c.r - 100.0 / 255.0).abs() < 0.001);
    }

    #[test]
    fn test_rgb_color_invalid() {
        // Missing parenthesis
        assert!(parse_rgb_color("rgb(255, 128, 64").is_err());
        assert!(parse_rgb_color("rgb 255, 128, 64)").is_err());

        // Wrong number of components
        assert!(parse_rgb_color("rgb(255, 128)").is_err());
        assert!(parse_rgb_color("rgba(255, 128, 64)").is_err());
        assert!(parse_rgb_color("rgb(255, 128, 64, 0.5)").is_err());

        // Invalid values
        assert!(parse_rgb_color("rgb(abc, 128, 64)").is_err());
    }

    #[test]
    fn test_named_color_basic() {
        let c = parse_named_color("black").unwrap();
        assert!((c.r - 0.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);

        let c = parse_named_color("white").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);

        let c = parse_named_color("red").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);

        let c = parse_named_color("blue").unwrap();
        assert!((c.b - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_named_color_case_insensitive() {
        let c1 = parse_named_color("red").unwrap();
        let c2 = parse_named_color("RED").unwrap();
        let c3 = parse_named_color("Red").unwrap();
        let c4 = parse_named_color("rEd").unwrap();

        assert!((c1.r - c2.r).abs() < 0.001);
        assert!((c2.r - c3.r).abs() < 0.001);
        assert!((c3.r - c4.r).abs() < 0.001);
    }

    #[test]
    fn test_named_color_gray_grey_variants() {
        let gray = parse_named_color("gray").unwrap();
        let grey = parse_named_color("grey").unwrap();
        assert!((gray.r - grey.r).abs() < 0.001);

        let lightgray = parse_named_color("lightgray").unwrap();
        let lightgrey = parse_named_color("lightgrey").unwrap();
        assert!((lightgray.r - lightgrey.r).abs() < 0.001);

        let darkgray = parse_named_color("darkgray").unwrap();
        let darkgrey = parse_named_color("darkgrey").unwrap();
        assert!((darkgray.r - darkgrey.r).abs() < 0.001);

        let slategray = parse_named_color("slategray").unwrap();
        let slategrey = parse_named_color("slategrey").unwrap();
        assert!((slategray.r - slategrey.r).abs() < 0.001);
    }

    #[test]
    fn test_named_color_cyan_aqua_alias() {
        let cyan = parse_named_color("cyan").unwrap();
        let aqua = parse_named_color("aqua").unwrap();
        assert!((cyan.r - aqua.r).abs() < 0.001);
        assert!((cyan.g - aqua.g).abs() < 0.001);
        assert!((cyan.b - aqua.b).abs() < 0.001);
    }

    #[test]
    fn test_named_color_magenta_fuchsia_alias() {
        let magenta = parse_named_color("magenta").unwrap();
        let fuchsia = parse_named_color("fuchsia").unwrap();
        assert!((magenta.r - fuchsia.r).abs() < 0.001);
        assert!((magenta.g - fuchsia.g).abs() < 0.001);
        assert!((magenta.b - fuchsia.b).abs() < 0.001);
    }

    #[test]
    fn test_named_color_transparent() {
        let c = parse_named_color("transparent").unwrap();
        assert!((c.r - 0.0).abs() < 0.001);
        assert!((c.g - 0.0).abs() < 0.001);
        assert!((c.b - 0.0).abs() < 0.001);
        assert!((c.a - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_named_color_extended() {
        // Test various extended CSS colors
        let gold = parse_named_color("gold").unwrap();
        assert!((gold.r - 1.0).abs() < 0.001);
        assert!((gold.g - 215.0 / 255.0).abs() < 0.001);

        let coral = parse_named_color("coral").unwrap();
        assert!((coral.r - 1.0).abs() < 0.001);

        let hotpink = parse_named_color("hotpink").unwrap();
        assert!((hotpink.r - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_named_color_unknown() {
        assert!(parse_named_color("notacolor").is_none());
        assert!(parse_named_color("").is_none());
        assert!(parse_named_color("redd").is_none());
    }

    #[test]
    fn test_parse_color_dispatch() {
        // parse_color should dispatch to correct parser
        let hex = parse_color("#ff0000").unwrap();
        assert!((hex.r - 1.0).abs() < 0.001);

        let rgb = parse_color("rgb(0, 255, 0)").unwrap();
        assert!((rgb.g - 1.0).abs() < 0.001);

        let rgba = parse_color("rgba(0, 0, 255, 0.5)").unwrap();
        assert!((rgba.b - 1.0).abs() < 0.001);
        assert!((rgba.a - 0.5).abs() < 0.001);

        let named = parse_color("blue").unwrap();
        assert!((named.b - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_color_whitespace_handling() {
        let c = parse_color("  #ff0000  ").unwrap();
        assert!((c.r - 1.0).abs() < 0.001);

        let c = parse_color("  blue  ").unwrap();
        assert!((c.b - 1.0).abs() < 0.001);
    }

    // ========== Gradient Parsing Edge Cases ==========

    #[test]
    fn test_gradient_with_direction() {
        let g = parse_linear_gradient("linear-gradient(to bottom, #ff0000, #0000ff)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.top.b - 0.0).abs() < 0.001);
        assert!((g.bottom.r - 0.0).abs() < 0.001);
        assert!((g.bottom.b - 1.0).abs() < 0.001);

        // `to top` runs upward: the first stop is at the bottom of the screen
        let g = parse_linear_gradient("linear-gradient(to top, #000000, #ffffff)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.r - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_gradient_without_direction() {
        let g = parse_linear_gradient("linear-gradient(#ff0000, #00ff00)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.top.g - 0.0).abs() < 0.001);
        assert!((g.bottom.r - 0.0).abs() < 0.001);
        assert!((g.bottom.g - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_gradient_with_color_stops() {
        // Color stops with percentages should work (percentage gets stripped)
        let g =
            parse_linear_gradient("linear-gradient(to bottom, #ff0000 0%, #0000ff 100%)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.b - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_gradient_with_named_colors() {
        let g = parse_linear_gradient("linear-gradient(red, blue)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.b - 1.0).abs() < 0.001);

        let g = parse_linear_gradient("linear-gradient(to bottom, white, black)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.r - 0.0).abs() < 0.001);
    }

    // Note: rgb()/rgba() colors inside gradients are not supported by the simple
    // string-based parser due to comma splitting. Gradients with rgb() colors
    // should be pre-parsed by lightningcss which handles them correctly.

    #[test]
    fn test_gradient_whitespace_handling() {
        let g = parse_linear_gradient("  linear-gradient(#ff0000, #00ff00)  ").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);

        let g = parse_linear_gradient("linear-gradient( to bottom , #ff0000 , #00ff00 )").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_gradient_invalid_format() {
        // Not a gradient
        assert!(parse_linear_gradient("red").is_err());
        assert!(parse_linear_gradient("#ff0000").is_err());
        assert!(parse_linear_gradient("rgb(255, 0, 0)").is_err());

        // Missing parenthesis
        assert!(parse_linear_gradient("linear-gradient(#ff0000, #00ff00").is_err());
        assert!(parse_linear_gradient("linear-gradient #ff0000, #00ff00)").is_err());

        // Only one color
        assert!(parse_linear_gradient("linear-gradient(#ff0000)").is_err());
    }

    #[test]
    fn test_gradient_mixed_color_formats() {
        // Hex top, named bottom
        let g = parse_linear_gradient("linear-gradient(#ff0000, blue)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.b - 1.0).abs() < 0.001);

        // Named top, hex bottom
        let g = parse_linear_gradient("linear-gradient(red, #0000ff)").unwrap();
        assert!((g.top.r - 1.0).abs() < 0.001);
        assert!((g.bottom.b - 1.0).abs() < 0.001);
    }

    // ========== Text Shadow Parsing ==========

    #[test]
    fn test_text_shadow_basic() {
        let s = parse_text_shadow("0 0 10px #ff00ff").unwrap();
        assert!((s.radius - 10.0).abs() < 0.01);
        assert!((s.color.r - 1.0).abs() < 0.001);
        assert!((s.color.g - 0.0).abs() < 0.001);
        assert!((s.color.b - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_text_shadow_with_rgba() {
        let s = parse_text_shadow("0 0 15px rgba(255, 0, 255, 0.8)").unwrap();
        assert!((s.radius - 15.0).abs() < 0.01);
        assert!((s.color.r - 1.0).abs() < 0.001);
        assert!((s.color.a - 0.8).abs() < 0.001);
        assert!((s.intensity - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_text_shadow_with_rgb() {
        let s = parse_text_shadow("0 0 8px rgb(0, 255, 0)").unwrap();
        assert!((s.radius - 8.0).abs() < 0.01);
        assert!((s.color.g - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_text_shadow_different_radii() {
        let s = parse_text_shadow("0 0 5px #ffffff").unwrap();
        assert!((s.radius - 5.0).abs() < 0.01);

        let s = parse_text_shadow("0 0 20px #ffffff").unwrap();
        assert!((s.radius - 20.0).abs() < 0.01);

        let s = parse_text_shadow("0 0 0px #ffffff").unwrap();
        assert!((s.radius - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_text_shadow_invalid() {
        // No color found
        assert!(parse_text_shadow("0 0 10px").is_err());
        assert!(parse_text_shadow("notacolor").is_err());
    }

    // ========== Background Size Parsing ==========

    #[test]
    fn test_background_size_keywords() {
        assert_eq!(parse_background_size("cover"), BackgroundSize::Cover);
        assert_eq!(parse_background_size("COVER"), BackgroundSize::Cover);
        assert_eq!(parse_background_size("contain"), BackgroundSize::Contain);
        assert_eq!(parse_background_size("auto"), BackgroundSize::Auto);
        assert_eq!(parse_background_size("auto auto"), BackgroundSize::Auto);
    }

    #[test]
    fn test_background_size_fixed_dimensions() {
        assert_eq!(
            parse_background_size("100px 200px"),
            BackgroundSize::Fixed(100, 200)
        );
        assert_eq!(
            parse_background_size("50px 50px"),
            BackgroundSize::Fixed(50, 50)
        );
    }

    #[test]
    fn test_background_size_single_px() {
        assert_eq!(
            parse_background_size("100px"),
            BackgroundSize::Fixed(100, 100)
        );
    }

    #[test]
    fn test_background_size_canvas_percent() {
        assert_eq!(
            parse_background_size("50%"),
            BackgroundSize::CanvasPercent(50.0)
        );
        assert_eq!(
            parse_background_size("100%"),
            BackgroundSize::CanvasPercent(100.0)
        );
    }

    #[test]
    fn test_background_size_image_scale() {
        assert_eq!(parse_background_size("2x"), BackgroundSize::ImageScale(2.0));
        assert_eq!(
            parse_background_size("0.5x"),
            BackgroundSize::ImageScale(0.5)
        );
    }

    #[test]
    fn test_background_size_whitespace() {
        assert_eq!(parse_background_size("  cover  "), BackgroundSize::Cover);
        assert_eq!(
            parse_background_size("  100px 200px  "),
            BackgroundSize::Fixed(100, 200)
        );
    }

    // ========== Background Position Parsing ==========

    #[test]
    fn test_background_position_keywords() {
        assert_eq!(
            parse_background_position("center"),
            BackgroundPosition::Center
        );
        assert_eq!(
            parse_background_position("center center"),
            BackgroundPosition::Center
        );
        assert_eq!(parse_background_position("top"), BackgroundPosition::Top);
        assert_eq!(
            parse_background_position("bottom"),
            BackgroundPosition::Bottom
        );
        assert_eq!(parse_background_position("left"), BackgroundPosition::Left);
        assert_eq!(
            parse_background_position("right"),
            BackgroundPosition::Right
        );
    }

    #[test]
    fn test_background_position_corners() {
        assert_eq!(
            parse_background_position("top left"),
            BackgroundPosition::TopLeft
        );
        assert_eq!(
            parse_background_position("left top"),
            BackgroundPosition::TopLeft
        );
        assert_eq!(
            parse_background_position("top right"),
            BackgroundPosition::TopRight
        );
        assert_eq!(
            parse_background_position("bottom left"),
            BackgroundPosition::BottomLeft
        );
        assert_eq!(
            parse_background_position("bottom right"),
            BackgroundPosition::BottomRight
        );
    }

    #[test]
    fn test_background_position_percentages() {
        assert_eq!(
            parse_background_position("50% 50%"),
            BackgroundPosition::Center
        );
        assert_eq!(
            parse_background_position("0% 0%"),
            BackgroundPosition::TopLeft
        );
        assert_eq!(
            parse_background_position("100% 100%"),
            BackgroundPosition::BottomRight
        );

        // Non-standard percentages
        if let BackgroundPosition::Percent(x, y) = parse_background_position("25% 75%") {
            assert!((x - 0.25).abs() < 0.001);
            assert!((y - 0.75).abs() < 0.001);
        } else {
            panic!("Expected Percent variant");
        }
    }

    #[test]
    fn test_background_position_case_insensitive() {
        assert_eq!(
            parse_background_position("CENTER"),
            BackgroundPosition::Center
        );
        assert_eq!(
            parse_background_position("Top Left"),
            BackgroundPosition::TopLeft
        );
    }

    // ========== Background Repeat Parsing ==========

    #[test]
    fn test_background_repeat_keywords() {
        assert_eq!(
            parse_background_repeat("no-repeat"),
            BackgroundRepeat::NoRepeat
        );
        assert_eq!(parse_background_repeat("repeat"), BackgroundRepeat::Repeat);
        assert_eq!(
            parse_background_repeat("repeat-x"),
            BackgroundRepeat::RepeatX
        );
        assert_eq!(
            parse_background_repeat("repeat-y"),
            BackgroundRepeat::RepeatY
        );
    }

    #[test]
    fn test_background_repeat_two_value_syntax() {
        assert_eq!(
            parse_background_repeat("repeat repeat"),
            BackgroundRepeat::Repeat
        );
        assert_eq!(
            parse_background_repeat("repeat no-repeat"),
            BackgroundRepeat::RepeatX
        );
        assert_eq!(
            parse_background_repeat("no-repeat repeat"),
            BackgroundRepeat::RepeatY
        );
    }

    #[test]
    fn test_background_repeat_case_insensitive() {
        assert_eq!(
            parse_background_repeat("NO-REPEAT"),
            BackgroundRepeat::NoRepeat
        );
        assert_eq!(parse_background_repeat("REPEAT"), BackgroundRepeat::Repeat);
    }

    #[test]
    fn test_background_repeat_unknown_defaults_to_no_repeat() {
        assert_eq!(
            parse_background_repeat("invalid"),
            BackgroundRepeat::NoRepeat
        );
        assert_eq!(parse_background_repeat(""), BackgroundRepeat::NoRepeat);
    }

    // ============================================
    // Event Override / Responsive Theming Tests
    // ============================================

    #[test]
    fn test_parse_on_bell_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell {
                --duration: 300ms;
                --cursor-color: #ff0000;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_bell.is_some());
        let on_bell = theme.on_bell.unwrap();
        assert_eq!(on_bell.duration_ms, 300);
        assert!(on_bell.cursor_color.is_some());
        let cursor = on_bell.cursor_color.unwrap();
        assert_eq!((cursor.r * 255.0) as u8, 255);
        assert_eq!((cursor.g * 255.0) as u8, 0);
        assert_eq!((cursor.b * 255.0) as u8, 0);
    }

    #[test]
    fn test_parse_on_command_success_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-success {
                --duration: 500ms;
                --cursor-color: #00ff00;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_success.is_some());
        let event = theme.on_command_success.unwrap();
        assert_eq!(event.duration_ms, 500);
        assert!(event.cursor_color.is_some());
    }

    #[test]
    fn test_parse_on_command_fail_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 1000ms;
                --cursor-color: #ff0000;
                text-shadow: 0 0 10px rgba(255, 0, 0, 0.8);
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_fail.is_some());
        let event = theme.on_command_fail.unwrap();
        assert_eq!(event.duration_ms, 1000);
        assert!(event.cursor_color.is_some());
        assert!(event.text_shadow.is_some());
    }

    #[test]
    fn test_parse_on_focus_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-focus {
                --duration: 200ms;
                --cursor-color: #00ffff;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_focus.is_some());
        let event = theme.on_focus.unwrap();
        assert_eq!(event.duration_ms, 200);
    }

    #[test]
    fn test_parse_on_blur_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-blur {
                --duration: 0;
                --cursor-color: #808080;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_blur.is_some());
        let event = theme.on_blur.unwrap();
        // Duration 0 means persist until cleared
        assert_eq!(event.duration_ms, 0);
    }

    #[test]
    fn test_parse_starfield_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 5000ms;
                --starfield-color: rgba(255, 100, 50, 0.9);
                --starfield-speed: 0.3;
                --starfield-glow-radius: 6;
                --starfield-glow-intensity: 0.8;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_fail.is_some());
        let event = theme.on_command_fail.unwrap();
        assert!(event.starfield_patch.is_some());
        let patch = event.starfield_patch.unwrap();
        assert!(patch.color.is_some());
        assert_eq!(patch.speed, Some(0.3));
        assert_eq!(patch.glow_radius, Some(6.0));
        assert_eq!(patch.glow_intensity, Some(0.8));
    }

    #[test]
    fn test_parse_particle_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 5000ms;
                --particles-color: #ff4500;
                --particles-count: 50;
                --particles-speed: 0.6;
                --particles-shape: sparkle;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_fail.is_some());
        let event = theme.on_command_fail.unwrap();
        assert!(event.particle_patch.is_some());
        let patch = event.particle_patch.unwrap();
        assert!(patch.color.is_some());
        assert_eq!(patch.count, Some(50));
        assert_eq!(patch.speed, Some(0.6));
        assert_eq!(patch.shape, Some(crate::ParticleShape::Sparkle));
    }

    #[test]
    fn test_parse_grid_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell {
                --duration: 300ms;
                --grid-color: rgba(255, 0, 0, 0.5);
                --grid-animation-speed: 2.0;
                --grid-glow-intensity: 0.9;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_bell.is_some());
        let event = theme.on_bell.unwrap();
        assert!(event.grid_patch.is_some());
        let patch = event.grid_patch.unwrap();
        assert!(patch.color.is_some());
        assert_eq!(patch.animation_speed, Some(2.0));
        assert_eq!(patch.glow_intensity, Some(0.9));
    }

    #[test]
    fn test_parse_rain_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 10000ms;
                --rain-color: rgba(180, 0, 0, 0.8);
                --rain-speed: 2.0;
                --rain-density: 200;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_fail.is_some());
        let event = theme.on_command_fail.unwrap();
        assert!(event.rain_patch.is_some());
        let patch = event.rain_patch.unwrap();
        assert!(patch.color.is_some());
        assert_eq!(patch.speed, Some(2.0));
        assert_eq!(patch.density, Some(200));
    }

    #[test]
    fn test_parse_matrix_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell {
                --duration: 500ms;
                --matrix-color: #ff0000;
                --matrix-speed: 15.0;
                --matrix-density: 2.0;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_bell.is_some());
        let event = theme.on_bell.unwrap();
        assert!(event.matrix_patch.is_some());
        let patch = event.matrix_patch.unwrap();
        assert!(patch.color.is_some());
        assert_eq!(patch.speed, Some(15.0));
        assert_eq!(patch.density, Some(2.0));
    }

    #[test]
    fn test_parse_shape_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-success {
                --duration: 1000ms;
                --shape-type: star;
                --shape-size: 150;
                --shape-fill: rgba(0, 255, 0, 0.8);
                --shape-rotation-speed: 2.0;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_success.is_some());
        let event = theme.on_command_success.unwrap();
        assert!(event.shape_patch.is_some());
        let patch = event.shape_patch.unwrap();
        assert_eq!(patch.shape_type, Some(crate::ShapeType::Star));
        assert_eq!(patch.size, Some(150.0));
        assert!(patch.fill.is_some());
        assert_eq!(patch.rotation_speed, Some(2.0));
    }

    #[test]
    fn test_parse_sprite_patch_in_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 86400000ms;
                --sprite-path: "flames.png";
                --sprite-fps: 16;
                --sprite-opacity: 0.6;
                --sprite-motion-speed: 0.5;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_command_fail.is_some());
        let event = theme.on_command_fail.unwrap();
        assert!(event.sprite_patch.is_some());
        let patch = event.sprite_patch.unwrap();
        assert_eq!(patch.path, Some("flames.png".to_string()));
        assert_eq!(patch.fps, Some(16.0));
        assert_eq!(patch.opacity, Some(0.6));
        assert_eq!(patch.motion_speed, Some(0.5));
    }

    #[test]
    fn test_parse_duration_formats() {
        // Test milliseconds
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell { --duration: 500ms; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert_eq!(theme.on_bell.unwrap().duration_ms, 500);

        // Test seconds
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell { --duration: 1.5s; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert_eq!(theme.on_bell.unwrap().duration_ms, 1500);

        // Test bare number (treated as ms)
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell { --duration: 750; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert_eq!(theme.on_bell.unwrap().duration_ms, 750);
    }

    #[test]
    fn test_multiple_events_in_theme() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell {
                --duration: 300ms;
                --cursor-color: #ffff00;
            }
            :terminal::on-command-success {
                --duration: 500ms;
                --cursor-color: #00ff00;
            }
            :terminal::on-command-fail {
                --duration: 1000ms;
                --cursor-color: #ff0000;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.on_bell.is_some());
        assert!(theme.on_command_success.is_some());
        assert!(theme.on_command_fail.is_some());

        assert_eq!(theme.on_bell.unwrap().duration_ms, 300);
        assert_eq!(theme.on_command_success.unwrap().duration_ms, 500);
        assert_eq!(theme.on_command_fail.unwrap().duration_ms, 1000);
    }

    #[test]
    fn test_combined_patches_in_single_event() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-command-fail {
                --duration: 5000ms;
                --cursor-color: #ff0000;
                --starfield-color: rgba(255, 100, 50, 0.9);
                --starfield-speed: 0.3;
                --particles-color: #ff4500;
                --particles-count: 50;
                --sprite-fps: 16;
                --sprite-opacity: 0.6;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        let event = theme.on_command_fail.unwrap();

        // Check all patches are present
        assert!(event.cursor_color.is_some());
        assert!(event.starfield_patch.is_some());
        assert!(event.particle_patch.is_some());
        assert!(event.sprite_patch.is_some());

        // Verify values
        let starfield = event.starfield_patch.unwrap();
        assert_eq!(starfield.speed, Some(0.3));

        let particles = event.particle_patch.unwrap();
        assert_eq!(particles.count, Some(50));

        let sprite = event.sprite_patch.unwrap();
        assert_eq!(sprite.fps, Some(16.0));
    }

    // ============================================
    // Regression tests for parser fixes
    // ============================================

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    #[test]
    fn test_hex_color_non_ascii_does_not_panic() {
        assert!(parse_hex_color("#éa").is_err());
        assert!(parse_hex_color("#ééé").is_err());
        assert!(parse_hex_color("#ffé").is_err());
        assert!(parse_hex_color("#日本語").is_err());
        assert!(parse_hex_color("é").is_err());
        assert!(parse_hex_color("#").is_err());
        assert!(parse_hex_color("").is_err());
    }

    #[test]
    fn test_parse_color_non_ascii_does_not_panic() {
        assert!(parse_color("#éa").is_err());
        assert!(parse_color("rgb(é, 0, 0)").is_err());
        assert!(parse_color("rgb(1,").is_err());
        assert!(parse_color("linear-gradient(é").is_err());
        assert!(parse_linear_gradient("linear-gradient(é, ü)").is_err());
        // Whole theme: an invalid colour is an error, never a panic
        assert!(parse_theme(":terminal { color: #éa; }").is_err());
    }

    #[test]
    fn test_unknown_property_names_reach_standard_map() {
        let css = r#"
            :terminal { color: #ffffff; background: #000000; }
            :terminal::on-bell {
                --duration: 300ms;
                cursor-color: #ff0000;
            }
            :terminal::tab {
                padding-x: 20px;
                padding-y: 9px;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        let bell = theme.on_bell.expect("on-bell");
        let cursor = bell
            .cursor_color
            .expect("cursor-color applies from ::on-bell");
        assert!(close(cursor.r, 1.0) && close(cursor.g, 0.0) && close(cursor.b, 0.0));
        assert!(close(theme.tabs.tab.padding_x, 20.0));
        assert!(close(theme.tabs.tab.padding_y, 9.0));
    }

    #[test]
    fn test_background_color_applies_as_solid_background() {
        let theme = parse_theme(":terminal { background-color: #112233; }").unwrap();
        assert!(close(theme.background.top.r, 0x11 as f32 / 255.0));
        assert!(close(theme.background.top.g, 0x22 as f32 / 255.0));
        assert!(close(theme.background.bottom.b, 0x33 as f32 / 255.0));

        // `background` still wins over `background-color`
        let theme = parse_theme(
            ":terminal { background-color: #112233; background: linear-gradient(#ff0000, #0000ff); }",
        )
        .unwrap();
        assert!(close(theme.background.top.r, 1.0));
        assert!(close(theme.background.bottom.b, 1.0));
    }

    #[test]
    fn test_outline_properties_extracted() {
        let css = r#"
            :terminal::ui-focus {
                outline-color: #00ff00;
                outline-width: 3px;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(close(theme.ui.focus.ring_color.g, 1.0));
        assert!(close(theme.ui.focus.ring_color.r, 0.0));
        assert!(close(theme.ui.focus.ring_thickness, 3.0));

        let theme = parse_theme(":terminal::ui-focus { outline: thick solid red; }").unwrap();
        assert!(close(theme.ui.focus.ring_color.r, 1.0));
        assert!(close(theme.ui.focus.ring_thickness, 5.0));
    }

    #[test]
    fn test_custom_property_url_and_angle_survive() {
        let css = r#"
            :terminal::backdrop {
                --sprite-path: url(sprites/cat.png);
                --sprite-fps: 12;
                --rain-angle: 15deg;
                --rain-density: 100;
            }
        "#;
        let report = parse_theme_report(css).unwrap();
        let sprite = report.theme.sprite.expect("sprite enabled");
        assert_eq!(sprite.path.as_deref(), Some("sprites/cat.png"));
        let rain = report.theme.rain.expect("rain enabled");
        assert!(close(rain.angle, 15.0));
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("--rain-angle") || w.contains("--sprite-path")),
            "no warnings expected, got {:?}",
            report.warnings
        );

        // Quoted url() is kept too
        let css = r#"
            :terminal::backdrop {
                --sprite-path: url("sprites/dog.png");
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert_eq!(
            theme.sprite.unwrap().path.as_deref(),
            Some("sprites/dog.png")
        );

        // Raw token serialisation keeps url(), var() and functions intact
        let props = extract_first_rule(
            r#"
            :terminal::backdrop {
                --sprite-path: url(sprites/cat.png);
                --accent: var(--base, #ff00ff);
                --rain-angle: 15deg;
                --fn: calc(1px + 2px);
            }
        "#,
        );
        let sprite_path = &props.custom["--sprite-path"];
        assert!(
            sprite_path == "url(sprites/cat.png)" || sprite_path == "url(\"sprites/cat.png\")",
            "got {sprite_path}"
        );
        assert!(props.custom["--accent"].starts_with("var(--base"));
        assert_eq!(props.custom["--rain-angle"], "15deg");
        assert!(props.custom["--fn"].starts_with("calc("));
    }

    /// Extract the properties of the first style rule in `css`.
    fn extract_first_rule(css: &str) -> RuleProperties {
        let stylesheet = StyleSheet::parse(css, ParserOptions::default()).unwrap();
        let mut warnings = Vec::new();
        for rule in &stylesheet.rules.0 {
            if let CssRule::Style(style_rule) = rule {
                return extract_properties(style_rule, &mut warnings).unwrap();
            }
        }
        panic!("no style rule in {css}");
    }

    #[test]
    fn test_named_colors_from_lightningcss_keywords() {
        // lightningcss prints #ff0000 as `red`, #4b0082 as `indigo`, #fa8072 as `salmon`
        let css = r#"
            :terminal {
                color: #ff0000;
                background: #4b0082;
                --ansi-red: salmon;
                --ansi-blue: #4B0082;
            }
            :terminal::cursor { background: red; }
            :terminal::selection { background: indigo; color: salmon; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(close(theme.foreground.r, 1.0) && close(theme.foreground.g, 0.0));
        assert!(close(theme.background.top.r, 75.0 / 255.0));
        assert!(close(theme.background.top.b, 130.0 / 255.0));
        assert!(close(theme.cursor_color.r, 1.0));
        assert!(close(theme.selection.background.r, 75.0 / 255.0));
        assert!(close(theme.selection.foreground.r, 250.0 / 255.0));
        assert!(close(theme.palette.red.g, 128.0 / 255.0));
        assert!(close(theme.palette.blue.b, 130.0 / 255.0));

        // The string fallback knows the whole named colour table
        assert!(close(
            parse_named_color("rebeccapurple").unwrap().r,
            102.0 / 255.0
        ));
        assert!(close(parse_named_color("indigo").unwrap().b, 130.0 / 255.0));
        assert!(close(parse_named_color("salmon").unwrap().g, 128.0 / 255.0));
        assert!(close(parse_named_color("chartreuse").unwrap().g, 1.0));
    }

    #[test]
    fn test_modern_color_spaces_parse() {
        let css = r#"
            :terminal {
                color: oklch(62.8% 0.2577 29.23);
                background: lab(50% 40 30);
                --ansi-green: color(display-p3 0 1 0);
            }
            :terminal::cursor { background: hsl(120, 100%, 50%); }
            :terminal::selection { background: hwb(240 0% 0%); }
        "#;
        let theme = parse_theme(css).unwrap();
        // oklch(62.8% 0.2577 29.23) is approximately sRGB red
        assert!(theme.foreground.r > 0.9, "got {:?}", theme.foreground);
        assert!(theme.foreground.g < 0.15);
        assert!(theme.foreground.b < 0.15);
        // lab(50% 40 30) is a reddish brown
        assert!(theme.background.top.r > theme.background.top.g);
        assert!(close(theme.cursor_color.g, 1.0) && close(theme.cursor_color.r, 0.0));
        assert!(close(theme.selection.background.b, 1.0));
        assert!(theme.palette.green.g > 0.9);
    }

    #[test]
    fn test_gradient_angles_parse_typed() {
        // 180deg == to bottom
        let theme =
            parse_theme(":terminal { background: linear-gradient(180deg, #ff0000, #0000ff); }")
                .unwrap();
        assert!(close(theme.background.top.r, 1.0));
        assert!(close(theme.background.bottom.b, 1.0));

        // 0deg == to top: stops are swapped
        let theme =
            parse_theme(":terminal { background: linear-gradient(0deg, #ff0000, #0000ff); }")
                .unwrap();
        assert!(close(theme.background.top.b, 1.0));
        assert!(close(theme.background.bottom.r, 1.0));

        // Other angles no longer reject the theme; the vertical component is used
        let report = parse_theme_report(
            ":terminal { background: linear-gradient(135deg, #ff0000, #0000ff); }",
        )
        .unwrap();
        assert!(close(report.theme.background.top.r, 1.0));
        assert!(report.warnings.iter().any(|w| w.contains("135")));

        let report = parse_theme_report(
            ":terminal { background: linear-gradient(45deg, #ff0000 0%, #00ff00 50%, #0000ff 100%); }",
        )
        .unwrap();
        assert!(close(report.theme.background.top.b, 1.0));
        assert!(close(report.theme.background.bottom.r, 1.0));

        // 0.5turn == 180deg
        let theme =
            parse_theme(":terminal { background: linear-gradient(0.5turn, #ff0000, #0000ff); }")
                .unwrap();
        assert!(close(theme.background.top.r, 1.0));
    }

    #[test]
    fn test_gradient_to_top_swaps_typed_and_string() {
        let theme =
            parse_theme(":terminal { background: linear-gradient(to top, #000000, #ffffff); }")
                .unwrap();
        assert!(close(theme.background.top.r, 1.0));
        assert!(close(theme.background.bottom.r, 0.0));

        let g = parse_linear_gradient("linear-gradient(to top, #000000, #ffffff)").unwrap();
        assert!(close(g.top.r, 1.0));
        assert!(close(g.bottom.r, 0.0));

        // Events use the same conversion
        let theme = parse_theme(
            ":terminal::on-bell { background: linear-gradient(to top, #000000, #ffffff); }",
        )
        .unwrap();
        let bg = theme.on_bell.unwrap().background.unwrap();
        assert!(close(bg.top.r, 1.0));
    }

    #[test]
    fn test_gradient_horizontal_direction_warns_and_falls_back() {
        let report = parse_theme_report(
            ":terminal { background: linear-gradient(to right, #ff0000, #0000ff); }",
        )
        .unwrap();
        assert!(close(report.theme.background.top.r, 1.0));
        assert!(close(report.theme.background.bottom.b, 1.0));
        assert!(
            report.warnings.iter().any(|w| w.contains("to right")),
            "expected a warning, got {:?}",
            report.warnings
        );

        let mut warnings = Vec::new();
        let g = parse_linear_gradient_with_warnings(
            "linear-gradient(to left, #ff0000, #0000ff)",
            &mut warnings,
        )
        .unwrap();
        assert!(close(g.top.r, 1.0));
        assert_eq!(warnings.len(), 1);

        // Corner: vertical component is honoured
        let mut warnings = Vec::new();
        let g = parse_linear_gradient_with_warnings(
            "linear-gradient(to top right, #ff0000, #0000ff)",
            &mut warnings,
        )
        .unwrap();
        assert!(close(g.top.b, 1.0));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn test_gradient_string_parser_handles_rgba_stops() {
        let g = parse_linear_gradient(
            "linear-gradient(to bottom, rgba(255, 0, 0, 0.5) 0%, rgb(0, 0, 255) 100%)",
        )
        .unwrap();
        assert!(close(g.top.r, 1.0) && close(g.top.a, 0.5));
        assert!(close(g.bottom.b, 1.0));
    }

    #[test]
    fn test_later_block_can_disable_effect() {
        let css = r#"
            :terminal::backdrop {
                --grid-color: #ff00ff;
                --grid-spacing: 6;
            }
            :terminal::backdrop {
                --grid-enabled: false;
            }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(
            theme.grid.is_none(),
            "grid should be disabled by the later block"
        );

        // ... and a later block can re-enable it (disabling drops the effect,
        // so it comes back with defaults)
        let css = r#"
            :terminal::backdrop { --grid-spacing: 6; }
            :terminal::backdrop { --grid-enabled: false; }
            :terminal::backdrop { --grid-enabled: true; }
        "#;
        let theme = parse_theme(css).unwrap();
        let grid = theme.grid.expect("grid re-enabled");
        assert!(close(grid.spacing, GridEffect::default().spacing));

        // Explicit enable + values in one block, disabled in a later one
        let css = r#"
            :terminal::backdrop { --grid-enabled: true; --grid-spacing: 6; }
            :terminal::backdrop { --grid-enabled: false; }
        "#;
        assert!(parse_theme(css).unwrap().grid.is_none());

        // Same rule for the other effects
        for (on, off) in [
            ("--starfield-speed: 0.3;", "--starfield-enabled: false;"),
            ("--rain-speed: 0.3;", "--rain-enabled: false;"),
            ("--particles-count: 10;", "--particles-enabled: false;"),
            ("--matrix-speed: 3;", "--matrix-enabled: false;"),
            ("--shape-size: 10;", "--shape-enabled: false;"),
            ("--crt-vignette: 0.3;", "--crt-enabled: false;"),
            ("--sprite-fps: 3;", "--sprite-enabled: false;"),
        ] {
            let css = format!(":terminal::backdrop {{ {on} }} :terminal::backdrop {{ {off} }}");
            let theme = parse_theme(&css).unwrap();
            assert!(theme.starfield.is_none());
            assert!(theme.rain.is_none());
            assert!(theme.particles.is_none());
            assert!(theme.matrix.is_none());
            assert!(theme.shape.is_none());
            assert!(theme.crt.is_none());
            assert!(theme.sprite.is_none());
        }
    }

    #[test]
    fn test_any_effect_key_auto_enables() {
        // Keys that previously did not count as "has props" now enable the effect
        let theme = parse_theme(":terminal::backdrop { --grid-line-width: 2; }").unwrap();
        assert!(theme.grid.is_some());
        let theme = parse_theme(":terminal::backdrop { --starfield-speed: 0.1; }").unwrap();
        assert!(theme.starfield.is_some());
        let theme = parse_theme(":terminal::backdrop { --rain-speed: 0.1; }").unwrap();
        assert!(theme.rain.is_some());
        let theme = parse_theme(":terminal::backdrop { --particles-size: 3; }").unwrap();
        assert!(theme.particles.is_some());

        // A backdrop rule without any keys for an effect leaves it alone
        let css = r#"
            :terminal::backdrop { --grid-spacing: 6; }
            :terminal::backdrop { --rain-speed: 1; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert!(theme.grid.is_some());
        assert!(theme.rain.is_some());
    }

    #[test]
    fn test_unparsable_numbers_fall_back_to_defaults_with_warning() {
        let css = r#"
            :terminal::backdrop {
                --grid-line-width: wide;
                --grid-spacing: 6;
                --crt-vignette: lots;
            }
        "#;
        let report = parse_theme_report(css).unwrap();
        let grid = report.theme.grid.expect("grid");
        assert!(close(grid.line_width, GridEffect::default().line_width));
        assert!(close(grid.spacing, 6.0));
        let crt = report.theme.crt.expect("crt");
        assert!(close(crt.vignette, CrtEffect::default().vignette));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("--grid-line-width") && w.contains("wide"))
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("--crt-vignette") && w.contains("lots"))
        );
    }

    #[test]
    fn test_error_recovery_keeps_valid_rules() {
        let css = r#"
            :terminal { color: #ff0000; }
            :terminal::cursor { background: #00ff00; ;; color: ; }
            :terminal::nonsense::more { color: #0000ff; }
            @media screen { :terminal { color: #ffffff; } }
            :terminal::selection { background: #0000ff; }
            :terminal::selection { background: #0000ff
        "#;
        let report = parse_theme_report(css).expect("recoverable errors must not fail");
        assert!(close(report.theme.foreground.r, 1.0));
        assert!(close(report.theme.cursor_color.g, 1.0));
        assert!(close(report.theme.selection.background.b, 1.0));
        assert!(!report.warnings.is_empty(), "expected warnings");
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("nonsense") || w.contains("unknown selector")),
            "unknown selector should be reported: {:?}",
            report.warnings
        );
        // Warnings are formatted for humans, not with Debug
        assert!(!report.warnings.iter().any(|w| w.contains("ParserError")));
    }

    #[test]
    fn test_selector_list_applies_to_each_target() {
        let theme =
            parse_theme(":terminal::selection, :terminal::highlight { background: #ff0000; }")
                .unwrap();
        assert!(close(theme.selection.background.r, 1.0));
        assert!(close(theme.highlight.background.r, 1.0));
    }

    #[test]
    fn test_event_matrix_charset_strips_quotes() {
        let css = r#"
            :terminal::on-bell { --matrix-charset: "01"; }
            :terminal::backdrop { --matrix-charset: 'アイウ'; }
        "#;
        let theme = parse_theme(css).unwrap();
        let patch = theme.on_bell.unwrap().matrix_patch.unwrap();
        assert_eq!(patch.charset.as_deref(), Some("01"));
        assert_eq!(theme.matrix.unwrap().charset, "アイウ");
    }

    #[test]
    fn test_generic_font_families_keep_css_names() {
        let theme = parse_theme(
            r#":terminal { font-family: "Fira Code", sans-serif, ui-monospace, monospace; }"#,
        )
        .unwrap();
        assert_eq!(
            theme.typography.font_family,
            vec!["Fira Code", "sans-serif", "ui-monospace", "monospace"]
        );
    }

    #[test]
    fn test_event_duration_zero_cascades() {
        let css = r#"
            :terminal::on-blur { --duration: 500ms; }
            :terminal::on-blur { --duration: 0; }
        "#;
        let theme = parse_theme(css).unwrap();
        let blur = theme.on_blur.unwrap();
        assert_eq!(blur.duration_ms, 0);
        assert!(blur.duration_set);

        // A block without --duration keeps the earlier value
        let css = r#"
            :terminal::on-bell { --duration: 500ms; }
            :terminal::on-bell { --cursor-color: #ff0000; }
        "#;
        let theme = parse_theme(css).unwrap();
        assert_eq!(theme.on_bell.unwrap().duration_ms, 500);
    }

    #[test]
    fn test_strip_quotes_short_strings() {
        assert_eq!(strip_quotes("\""), "\"");
        assert_eq!(strip_quotes("'"), "'");
        assert_eq!(strip_quotes(""), "");
        assert_eq!(strip_quotes("\"\""), "");
        assert_eq!(strip_quotes("  'a.png' "), "a.png");
        assert_eq!(strip_quotes("url(\"a.png\")"), "a.png");
        assert_eq!(strip_quotes("url(a.png)"), "a.png");

        // Sprite path made of a single quote character does not panic
        let theme = parse_theme(":terminal::backdrop { --sprite-path: '\"'; }").unwrap();
        assert_eq!(theme.sprite.unwrap().path.as_deref(), Some("\""));
    }

    #[test]
    fn test_background_shorthand_default_position_is_centred() {
        // Shorthand without a position must behave exactly like the longhand form
        let shorthand = parse_theme(r#":terminal { background: url("bg.png"); }"#).unwrap();
        let longhand = parse_theme(r#":terminal { background-image: url("bg.png"); }"#).unwrap();
        let s = shorthand.background_image.unwrap();
        let l = longhand.background_image.unwrap();
        assert_eq!(s.position, l.position);
        assert_eq!(s.size, l.size);
        assert_eq!(s.repeat, l.repeat);

        let theme =
            parse_theme(r#":terminal { background: url("bg.png") top left / cover no-repeat; }"#)
                .unwrap();
        let bg = theme.background_image.unwrap();
        assert_eq!(bg.position, BackgroundPosition::TopLeft);
        assert_eq!(bg.size, BackgroundSize::Cover);
        assert_eq!(bg.repeat, BackgroundRepeat::NoRepeat);
    }

    #[test]
    fn test_background_image_properties_apply_across_rules() {
        let css = r#"
            :terminal { background-image: url("bg.png"); }
            :terminal { background-size: contain; --background-opacity: 0.4; }
            :terminal { background-position: bottom right; }
        "#;
        let theme = parse_theme(css).unwrap();
        let bg = theme.background_image.expect("image");
        assert_eq!(bg.path.as_deref(), Some("bg.png"));
        assert_eq!(bg.size, BackgroundSize::Contain);
        assert_eq!(bg.position, BackgroundPosition::BottomRight);
        assert!(close(bg.opacity, 0.4));

        // Order does not matter either
        let css = r#"
            :terminal { background-size: contain; --background-opacity: 0.4; }
            :terminal { background-image: url("bg.png"); }
        "#;
        let bg = parse_theme(css).unwrap().background_image.expect("image");
        assert_eq!(bg.size, BackgroundSize::Contain);
        assert!(close(bg.opacity, 0.4));

        // No image, no background_image
        let theme = parse_theme(":terminal { background-size: contain; }").unwrap();
        assert!(theme.background_image.is_none());
    }

    #[test]
    fn test_palette_iteration_and_precedence() {
        let css = r#"
            :terminal {
                --color-red: #110000;
                --ansi-red: #ff0000;
                --color-blue: #0000ff;
            }
            :terminal::palette {
                --color-42: #424242;
                --color-999: #ffffff;
                --color-abc: #ffffff;
            }
        "#;
        let report = parse_theme_report(css).unwrap();
        assert!(close(report.theme.palette.red.r, 1.0));
        assert!(close(report.theme.palette.blue.b, 1.0));
        assert!(close(
            report.theme.palette.get_extended(42).unwrap().r,
            0x42 as f32 / 255.0
        ));
        assert!(report.warnings.iter().any(|w| w.contains("--color-999")));
    }

    #[test]
    fn test_text_shadow_typed_conversion() {
        let theme = parse_theme(":terminal { text-shadow: 0 0 12px salmon; }").unwrap();
        let ts = theme.text_shadow.unwrap();
        assert!(close(ts.radius, 12.0));
        assert!(close(ts.color.r, 250.0 / 255.0));
        assert!(close(ts.intensity, 1.0));

        let theme = parse_theme(":terminal { text-shadow: 0 0 6px oklch(70% 0.2 150); }").unwrap();
        let ts = theme.text_shadow.unwrap();
        assert!(close(ts.radius, 6.0));
        assert!(ts.color.g > ts.color.r);
    }

    #[test]
    fn test_parse_report_no_warnings_for_bundled_synthwave_style_theme() {
        let css = r#"
            :terminal {
                font-family: "MesloLGS NF", "Fira Code", monospace;
                font-size: 14;
                line-height: 1.4;
                color: #61e2fe;
                background: linear-gradient(to bottom, #0a0c24, #080118);
                text-shadow: 0 0 12px rgba(97, 226, 254, 0.5);
            }
            :terminal::backdrop {
                --grid-enabled: true;
                --grid-color: rgba(255, 0, 255, 0.2);
                --grid-spacing: 6;
                --grid-curved: false;
            }
        "#;
        let report = parse_theme_report(css).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(close(report.theme.typography.font_size, 14.0));
        let grid = report.theme.grid.unwrap();
        assert!(!grid.curved);
        assert!(close(grid.color.a, 0.2));
    }

    #[test]
    fn test_bundled_themes_parse_without_errors() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/themes");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipping: {} not found", dir.display());
            return;
        };
        let mut checked = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("css") {
                continue;
            }
            let css = std::fs::read_to_string(&path).unwrap();
            let report = parse_theme_report(&css)
                .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
            for w in &report.warnings {
                eprintln!("{}: {w}", path.display());
            }
            checked += 1;
        }
        assert!(checked > 0, "no themes found in {}", dir.display());
    }
}
