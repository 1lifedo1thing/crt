//! Background Image Loading and Rendering
//!
//! Supports loading static images (PNG, JPEG) and animated GIFs with frame timing.

use std::io::BufReader;
use std::path::Path;
use std::time::{Duration, Instant};

use crt_theme::{BackgroundImage, BackgroundPosition, BackgroundRepeat, BackgroundSize};
use image::AnimationDecoder;
use wgpu::util::DeviceExt;

/// Delay used for GIF frames that declare a 0 ms delay (browsers use 100 ms)
pub const DEFAULT_FRAME_DELAY: Duration = Duration::from_millis(100);

/// Maximum number of frames advanced in a single `update_animation` call
/// when catching up after a stall; beyond this the clock is resynchronised.
const MAX_FRAME_SKIP: usize = 8;

/// Effective delay for a frame (0 ms is treated as `DEFAULT_FRAME_DELAY`)
fn effective_delay(delay: Duration) -> Duration {
    if delay.is_zero() {
        DEFAULT_FRAME_DELAY
    } else {
        delay
    }
}

/// A frame of an animated image
#[derive(Debug, Clone)]
pub struct ImageFrame {
    /// RGBA pixel data
    pub data: Vec<u8>,
    /// Width in pixels
    pub width: u32,
    /// Height in pixels
    pub height: u32,
    /// Duration to display this frame
    pub delay: Duration,
}

/// Loaded background image data (static or animated)
#[derive(Debug)]
pub enum LoadedImage {
    /// Single static image
    Static {
        data: Vec<u8>,
        width: u32,
        height: u32,
    },
    /// Animated image with multiple frames
    Animated {
        frames: Vec<ImageFrame>,
        current_frame: usize,
        last_frame_time: Instant,
    },
}

impl LoadedImage {
    /// Load an image from file path
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();

        // Check if it's a GIF (may be animated)
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase());

        if extension.as_deref() == Some("gif") {
            Self::load_gif(path)
        } else {
            Self::load_static(path)
        }
    }

    /// Load a static image (PNG, JPEG, etc.)
    fn load_static(path: &Path) -> Result<Self, String> {
        let img =
            image::open(path).map_err(|e| format!("Failed to load image {:?}: {}", path, e))?;

        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();

        Ok(LoadedImage::Static {
            data: rgba.into_raw(),
            width,
            height,
        })
    }

    /// Load a GIF (may be animated)
    fn load_gif(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Failed to open GIF {:?}: {}", path, e))?;
        let reader = BufReader::new(file);

        let decoder = image::codecs::gif::GifDecoder::new(reader)
            .map_err(|e| format!("Failed to decode GIF {:?}: {}", path, e))?;

        let frames_result: Result<Vec<_>, _> = decoder.into_frames().collect();
        let frames =
            frames_result.map_err(|e| format!("Failed to decode GIF frames {:?}: {}", path, e))?;

        if frames.is_empty() {
            return Err(format!("GIF has no frames: {:?}", path));
        }

        // If only one frame, treat as static
        if frames.len() == 1 {
            let frame = frames.into_iter().next().unwrap();
            let rgba = frame.into_buffer();
            let (width, height) = rgba.dimensions();
            return Ok(LoadedImage::Static {
                data: rgba.into_raw(),
                width,
                height,
            });
        }

        // Multiple frames - animated
        let image_frames: Vec<ImageFrame> = frames
            .into_iter()
            .map(|frame| {
                let delay = effective_delay(Duration::from(frame.delay()));
                let rgba = frame.into_buffer();
                let (width, height) = rgba.dimensions();
                ImageFrame {
                    data: rgba.into_raw(),
                    width,
                    height,
                    delay,
                }
            })
            .collect();

        Ok(LoadedImage::Animated {
            frames: image_frames,
            current_frame: 0,
            last_frame_time: Instant::now(),
        })
    }

    /// Get current frame dimensions
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            LoadedImage::Static { width, height, .. } => (*width, *height),
            LoadedImage::Animated {
                frames,
                current_frame,
                ..
            } => {
                let frame = &frames[*current_frame];
                (frame.width, frame.height)
            }
        }
    }

    /// Get current frame data
    pub fn current_data(&self) -> &[u8] {
        match self {
            LoadedImage::Static { data, .. } => data,
            LoadedImage::Animated {
                frames,
                current_frame,
                ..
            } => &frames[*current_frame].data,
        }
    }

    /// Check if animation frame needs update, returns true if texture should be updated
    ///
    /// Advances by elapsed wall-clock time rather than one frame per call, so
    /// a slow render loop does not slow the GIF down. At most `MAX_FRAME_SKIP`
    /// frames are skipped per call; past that the clock is resynchronised.
    pub fn update_animation(&mut self) -> bool {
        self.update_animation_at(Instant::now())
    }

    /// `update_animation` with an explicit "now" (testable without sleeping)
    pub fn update_animation_at(&mut self, now: Instant) -> bool {
        match self {
            LoadedImage::Static { .. } => false,
            LoadedImage::Animated {
                frames,
                current_frame,
                last_frame_time,
            } => {
                if frames.is_empty() {
                    return false;
                }
                let mut advanced = 0;
                loop {
                    let delay = effective_delay(frames[*current_frame].delay);
                    let elapsed = now.saturating_duration_since(*last_frame_time);
                    if elapsed < delay {
                        break;
                    }
                    if advanced >= MAX_FRAME_SKIP {
                        // Too far behind (stall, sleep, ...): drop the backlog
                        *last_frame_time = now;
                        break;
                    }
                    *current_frame = (*current_frame + 1) % frames.len();
                    *last_frame_time += delay;
                    advanced += 1;
                }
                advanced > 0
            }
        }
    }

    /// Check if this is an animated image
    pub fn is_animated(&self) -> bool {
        matches!(self, LoadedImage::Animated { .. })
    }
}

/// GPU texture for background image
pub struct BackgroundTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub sampler: wgpu::Sampler,
    pub width: u32,
    pub height: u32,
}

impl BackgroundTexture {
    /// Create a new texture from loaded image data with a clamp-to-edge sampler
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, image: &LoadedImage) -> Self {
        Self::new_with_repeat(device, queue, image, BackgroundRepeat::NoRepeat)
    }

    /// Create a new texture whose sampler address modes follow `repeat`
    /// (`background-repeat`). Bind `self.sampler()` instead of a shared
    /// clamp sampler for the repeat mode to take effect.
    pub fn new_with_repeat(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        image: &LoadedImage,
        repeat: BackgroundRepeat,
    ) -> Self {
        let (width, height) = image.dimensions();
        let data = image.current_data();

        let texture = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("Background Image Texture"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            data,
        );

        let view = texture.create_view(&Default::default());

        let sampler = Self::create_sampler_with_repeat(device, repeat);

        Self {
            texture,
            view,
            sampler,
            width,
            height,
        }
    }

    /// The per-texture sampler (address modes derived from `background-repeat`)
    pub fn sampler(&self) -> &wgpu::Sampler {
        &self.sampler
    }

    /// Update texture data (for animated images)
    pub fn update(&self, queue: &wgpu::Queue, image: &LoadedImage) {
        let data = image.current_data();
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Sampler address modes (u, v) for a repeat mode
    pub fn address_modes(repeat: BackgroundRepeat) -> (wgpu::AddressMode, wgpu::AddressMode) {
        match repeat {
            BackgroundRepeat::NoRepeat => (
                wgpu::AddressMode::ClampToEdge,
                wgpu::AddressMode::ClampToEdge,
            ),
            BackgroundRepeat::Repeat => (wgpu::AddressMode::Repeat, wgpu::AddressMode::Repeat),
            BackgroundRepeat::RepeatX => {
                (wgpu::AddressMode::Repeat, wgpu::AddressMode::ClampToEdge)
            }
            BackgroundRepeat::RepeatY => {
                (wgpu::AddressMode::ClampToEdge, wgpu::AddressMode::Repeat)
            }
        }
    }

    /// Create sampler with specific repeat mode
    pub fn create_sampler_with_repeat(
        device: &wgpu::Device,
        repeat: BackgroundRepeat,
    ) -> wgpu::Sampler {
        let (address_u, address_v) = Self::address_modes(repeat);

        device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("Background Image Sampler"),
            address_mode_u: address_u,
            address_mode_v: address_v,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        })
    }
}

/// Complete background image state for rendering
pub struct BackgroundImageState {
    pub image: LoadedImage,
    pub texture: BackgroundTexture,
    pub config: BackgroundImage,
}

impl BackgroundImageState {
    /// Load and create background image state
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: &BackgroundImage,
    ) -> Result<Self, String> {
        // Use resolved_path() to handle relative paths against theme directory
        let path = config
            .resolved_path()
            .ok_or_else(|| "No background image path specified".to_string())?;

        log::debug!("Loading background image from: {:?}", path);
        let image = LoadedImage::from_path(&path)?;
        let texture = BackgroundTexture::new_with_repeat(device, queue, &image, config.repeat);

        Ok(Self {
            image,
            texture,
            config: config.clone(),
        })
    }

    /// Update animation state, returns true if texture was updated
    pub fn update(&mut self, queue: &wgpu::Queue) -> bool {
        if self.image.update_animation() {
            self.texture.update(queue, &self.image);
            true
        } else {
            false
        }
    }

    /// Calculate UV transform for background sizing and positioning
    /// Returns (scale_x, scale_y, offset_x, offset_y)
    pub fn calculate_uv_transform(&self, screen_width: f32, screen_height: f32) -> [f32; 4] {
        calculate_uv_transform(
            self.texture.width as f32,
            self.texture.height as f32,
            screen_width,
            screen_height,
            self.config.size,
            self.config.position,
        )
    }

    /// Repeat flags for the shader: `[repeat_x, repeat_y]` as 1.0 / 0.0.
    ///
    /// The background image shader discards fragments whose texture
    /// coordinates fall outside `[0, 1]` on an axis unless that axis repeats;
    /// pass these through the uniform padding (`_pad[0]`, `_pad[1]`).
    pub fn repeat_flags(&self) -> [f32; 2] {
        repeat_flags(self.config.repeat)
    }

    /// Get opacity
    pub fn opacity(&self) -> f32 {
        self.config.opacity
    }
}

/// Repeat flags for the shader: `[repeat_x, repeat_y]` as 1.0 / 0.0
pub fn repeat_flags(repeat: BackgroundRepeat) -> [f32; 2] {
    match repeat {
        BackgroundRepeat::NoRepeat => [0.0, 0.0],
        BackgroundRepeat::Repeat => [1.0, 1.0],
        BackgroundRepeat::RepeatX => [1.0, 0.0],
        BackgroundRepeat::RepeatY => [0.0, 1.0],
    }
}

/// Pure UV-transform math shared by `BackgroundImageState::calculate_uv_transform`
/// and unit tests. Returns (scale_x, scale_y, offset_x, offset_y).
pub fn calculate_uv_transform(
    img_width: f32,
    img_height: f32,
    screen_width: f32,
    screen_height: f32,
    size: BackgroundSize,
    position: BackgroundPosition,
) -> [f32; 4] {
    {
        let screen_aspect = screen_width / screen_height;
        let img_aspect = img_width / img_height;

        // Calculate the display size of the image in normalized screen coordinates (0-1)
        // norm_w/norm_h represent what fraction of the screen the image should occupy
        let (norm_w, norm_h) = match size {
            BackgroundSize::Cover => {
                // Fill screen completely, may crop
                (1.0, 1.0)
            }
            BackgroundSize::Contain => {
                // Fit within screen, maintaining aspect ratio
                if screen_aspect > img_aspect {
                    // Screen is wider than image - fit height
                    let h = 1.0;
                    let w = img_aspect / screen_aspect;
                    (w, h)
                } else {
                    // Screen is taller than image - fit width
                    let w = 1.0;
                    let h = screen_aspect / img_aspect;
                    (w, h)
                }
            }
            BackgroundSize::Auto => {
                // Original pixel size, doesn't scale with window
                let w = img_width / screen_width;
                let h = img_height / screen_height;
                (w, h)
            }
            BackgroundSize::Fixed(fw, fh) => {
                // Fixed pixel dimensions
                let w = fw as f32 / screen_width;
                let h = fh as f32 / screen_height;
                (w, h)
            }
            BackgroundSize::CanvasPercent(pct) => {
                // Percentage of canvas width, maintain aspect ratio
                let w = pct / 100.0;
                let h = w * screen_aspect / img_aspect;
                (w, h)
            }
            BackgroundSize::ImageScale(scale) => {
                // Scale relative to original image size
                let w = (img_width * scale) / screen_width;
                let h = (img_height * scale) / screen_height;
                (w, h)
            }
        };

        // Calculate position anchor (0-1 range)
        let (anchor_x, anchor_y) = match position {
            BackgroundPosition::Center => (0.5, 0.5),
            BackgroundPosition::TopLeft => (0.0, 0.0),
            BackgroundPosition::Top => (0.5, 0.0),
            BackgroundPosition::TopRight => (1.0, 0.0),
            BackgroundPosition::Left => (0.0, 0.5),
            BackgroundPosition::Right => (1.0, 0.5),
            BackgroundPosition::BottomLeft => (0.0, 1.0),
            BackgroundPosition::Bottom => (0.5, 1.0),
            BackgroundPosition::BottomRight => (1.0, 1.0),
            BackgroundPosition::Percent(x, y) => (x, y),
        };

        // Cover: scale the image up so it fills the screen, cropping the
        // overflowing axis. `scale` is how much larger than the screen the
        // image is on each axis (>= 1); the visible UV range is 1/scale and
        // the crop is distributed according to the anchor.
        //
        // Example: screen 1000x500 (aspect 2), image 500x500 (aspect 1) ->
        // scale = (1, 2), UV scale = (1, 0.5), centre offset y = 0.25.
        if matches!(size, BackgroundSize::Cover) {
            let (scale_x, scale_y) = if screen_aspect > img_aspect {
                // Screen is wider: fit width, crop height
                (1.0, screen_aspect / img_aspect)
            } else {
                // Screen is taller: fit height, crop width
                (img_aspect / screen_aspect, 1.0)
            };
            let uv_offset_x = anchor_x * (1.0 - 1.0 / scale_x).max(0.0);
            let uv_offset_y = anchor_y * (1.0 - 1.0 / scale_y).max(0.0);
            return [1.0 / scale_x, 1.0 / scale_y, uv_offset_x, uv_offset_y];
        }

        // For other modes: UV scale is inverse of normalized size
        // If image takes up 30% of screen (norm_w = 0.3), UV scale = 1/0.3 = 3.33
        // This means UV goes from 0 to 3.33, but we only show 0-1 portion
        let uv_scale_x = 1.0 / norm_w;
        let uv_scale_y = 1.0 / norm_h;

        // Offset to position the image
        // At anchor (0,0) = top-left: offset = 0
        // At anchor (0.5,0.5) = center: offset centers the visible portion
        // At anchor (1,1) = bottom-right: offset moves to end
        let uv_offset_x = -anchor_x * (uv_scale_x - 1.0);
        let uv_offset_y = -anchor_y * (uv_scale_y - 1.0);

        [uv_scale_x, uv_scale_y, uv_offset_x, uv_offset_y]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn test_uv_transform_cover_wide_screen() {
        // 1000x500 (2:1) screen with a 500x500 (1:1) image: the image is
        // scaled to the screen width (1000x1000) and the middle half of its
        // height is visible -> UV scale (1, 0.5), centred offset y = 0.25.
        let uv = calculate_uv_transform(
            500.0,
            500.0,
            1000.0,
            500.0,
            BackgroundSize::Cover,
            BackgroundPosition::Center,
        );
        assert!(approx(uv[0], 1.0), "scale_x {}", uv[0]);
        assert!(approx(uv[1], 0.5), "scale_y {}", uv[1]);
        assert!(approx(uv[2], 0.0), "offset_x {}", uv[2]);
        assert!(approx(uv[3], 0.25), "offset_y {}", uv[3]);

        // Top anchor shows the top half: offset 0; bottom anchor: offset 0.5
        let top = calculate_uv_transform(
            500.0,
            500.0,
            1000.0,
            500.0,
            BackgroundSize::Cover,
            BackgroundPosition::Top,
        );
        assert!(approx(top[3], 0.0));
        let bottom = calculate_uv_transform(
            500.0,
            500.0,
            1000.0,
            500.0,
            BackgroundSize::Cover,
            BackgroundPosition::Bottom,
        );
        assert!(approx(bottom[3], 0.5));
    }

    #[test]
    fn test_uv_transform_cover_tall_screen() {
        // 500x1000 screen with a 500x500 image: fit height, crop width
        let uv = calculate_uv_transform(
            500.0,
            500.0,
            500.0,
            1000.0,
            BackgroundSize::Cover,
            BackgroundPosition::Center,
        );
        assert!(approx(uv[0], 0.5), "scale_x {}", uv[0]);
        assert!(approx(uv[1], 1.0), "scale_y {}", uv[1]);
        assert!(approx(uv[2], 0.25), "offset_x {}", uv[2]);
        assert!(approx(uv[3], 0.0), "offset_y {}", uv[3]);
    }

    #[test]
    fn test_uv_transform_cover_always_within_unit_square() {
        // Cover must never sample outside [0,1] on either axis
        for (iw, ih, sw, sh) in [
            (500.0, 500.0, 1000.0, 500.0),
            (1920.0, 1080.0, 800.0, 600.0),
            (300.0, 900.0, 1600.0, 400.0),
        ] {
            let uv = calculate_uv_transform(
                iw,
                ih,
                sw,
                sh,
                BackgroundSize::Cover,
                BackgroundPosition::Center,
            );
            let (sx, sy, ox, oy) = (uv[0], uv[1], uv[2], uv[3]);
            assert!(sx <= 1.0 + 1e-5 && sy <= 1.0 + 1e-5, "{:?}", uv);
            assert!(ox >= -1e-5 && oy >= -1e-5, "{:?}", uv);
            assert!(ox + sx <= 1.0 + 1e-5 && oy + sy <= 1.0 + 1e-5, "{:?}", uv);
        }
    }

    #[test]
    fn test_uv_transform_contain_matches_expectation() {
        // Contain on a wide screen with a square image: image occupies the
        // middle 50% of the width; UV scale 2 on x, offset -0.5 (centred)
        let uv = calculate_uv_transform(
            500.0,
            500.0,
            1000.0,
            500.0,
            BackgroundSize::Contain,
            BackgroundPosition::Center,
        );
        assert!(approx(uv[0], 2.0));
        assert!(approx(uv[1], 1.0));
        assert!(approx(uv[2], -0.5));
        assert!(approx(uv[3], 0.0));
    }

    #[test]
    fn test_repeat_flags() {
        assert_eq!(repeat_flags(BackgroundRepeat::NoRepeat), [0.0, 0.0]);
        assert_eq!(repeat_flags(BackgroundRepeat::Repeat), [1.0, 1.0]);
        assert_eq!(repeat_flags(BackgroundRepeat::RepeatX), [1.0, 0.0]);
        assert_eq!(repeat_flags(BackgroundRepeat::RepeatY), [0.0, 1.0]);
    }

    #[test]
    fn test_address_modes_follow_repeat() {
        use wgpu::AddressMode::{ClampToEdge, Repeat};
        assert_eq!(
            BackgroundTexture::address_modes(BackgroundRepeat::NoRepeat),
            (ClampToEdge, ClampToEdge)
        );
        assert_eq!(
            BackgroundTexture::address_modes(BackgroundRepeat::Repeat),
            (Repeat, Repeat)
        );
        assert_eq!(
            BackgroundTexture::address_modes(BackgroundRepeat::RepeatX),
            (Repeat, ClampToEdge)
        );
        assert_eq!(
            BackgroundTexture::address_modes(BackgroundRepeat::RepeatY),
            (ClampToEdge, Repeat)
        );
    }

    fn animated(delays_ms: &[u64]) -> LoadedImage {
        LoadedImage::Animated {
            frames: delays_ms
                .iter()
                .map(|&ms| ImageFrame {
                    data: vec![0; 4],
                    width: 1,
                    height: 1,
                    delay: Duration::from_millis(ms),
                })
                .collect(),
            current_frame: 0,
            last_frame_time: Instant::now(),
        }
    }

    fn frame_of(img: &LoadedImage) -> usize {
        match img {
            LoadedImage::Animated { current_frame, .. } => *current_frame,
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_gif_catches_up_by_elapsed_time() {
        let mut img = animated(&[50, 50, 50, 50]);
        let start = Instant::now();
        if let LoadedImage::Animated {
            last_frame_time, ..
        } = &mut img
        {
            *last_frame_time = start;
        }
        // 30 ms: nothing yet
        assert!(!img.update_animation_at(start + Duration::from_millis(30)));
        assert_eq!(frame_of(&img), 0);
        // 160 ms: three frames elapsed in one call, not one
        assert!(img.update_animation_at(start + Duration::from_millis(160)));
        assert_eq!(frame_of(&img), 3);
    }

    #[test]
    fn test_gif_zero_delay_uses_default() {
        let mut img = animated(&[0, 0]);
        let start = Instant::now();
        if let LoadedImage::Animated {
            last_frame_time, ..
        } = &mut img
        {
            *last_frame_time = start;
        }
        assert!(!img.update_animation_at(start + Duration::from_millis(50)));
        assert!(img.update_animation_at(start + Duration::from_millis(100)));
        assert_eq!(frame_of(&img), 1);
    }

    #[test]
    fn test_gif_max_skip_resyncs_clock() {
        let mut img = animated(&[10, 10, 10]);
        let start = Instant::now();
        if let LoadedImage::Animated {
            last_frame_time, ..
        } = &mut img
        {
            *last_frame_time = start;
        }
        // 10 s behind: should advance at most MAX_FRAME_SKIP frames and resync
        let now = start + Duration::from_secs(10);
        assert!(img.update_animation_at(now));
        assert_eq!(frame_of(&img), MAX_FRAME_SKIP % 3);
        if let LoadedImage::Animated {
            last_frame_time, ..
        } = &img
        {
            assert_eq!(*last_frame_time, now);
        }
        // Immediately after, nothing more is due
        assert!(!img.update_animation_at(now));
    }
}
