//! GPU texture pooling with RAII semantics
//!
//! Pools render target textures for reuse across window lifecycles.
//! Textures are keyed by their exact size and format: a render target that is
//! larger than the surface would be sampled at a fractional scale by the
//! composite and CRT passes (visible as row-dependent blur), so no rounding
//! is applied. The pool holds a small global number of textures so an
//! interactive resize cannot leave one behind for every size crossed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

/// Texture pool key: exact size and format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureBucket {
    /// Exact width in pixels (min 1)
    pub width: u32,
    /// Exact height in pixels (min 1)
    pub height: u32,
    /// Texture format
    pub format: wgpu::TextureFormat,
}

impl TextureBucket {
    /// Create a key for the given dimensions (exact, no rounding).
    pub fn from_size(width: u32, height: u32, format: wgpu::TextureFormat) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
            format,
        }
    }
}

/// Maximum textures kept idle in the pool across all sizes.
const MAX_POOLED_TOTAL: usize = 4;

/// Internal pool state
struct TexturePoolInner {
    device: Arc<wgpu::Device>,
    pools: HashMap<TextureBucket, Vec<TextureEntry>>,
    max_per_bucket: usize,
    stats: TexturePoolStats,
}

/// A pooled texture with its view
struct TextureEntry {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// Pool statistics for monitoring
#[derive(Debug, Default, Clone)]
pub struct TexturePoolStats {
    pub allocations: u64,
    pub reuses: u64,
    pub returns: u64,
}

impl TexturePoolInner {
    fn new(device: Arc<wgpu::Device>, max_per_bucket: usize) -> Self {
        Self {
            device,
            pools: HashMap::new(),
            max_per_bucket,
            stats: TexturePoolStats::default(),
        }
    }

    fn checkout(
        &mut self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> (wgpu::Texture, wgpu::TextureView, TextureBucket) {
        let bucket = TextureBucket::from_size(width, height, format);

        if let Some(entry) = self.pools.get_mut(&bucket).and_then(|v| v.pop()) {
            self.stats.reuses += 1;
            log::debug!(
                "Reusing pooled texture {}x{} {:?} (reuses: {})",
                bucket.width,
                bucket.height,
                bucket.format,
                self.stats.reuses
            );
            (entry.texture, entry.view, bucket)
        } else {
            self.stats.allocations += 1;
            log::debug!(
                "Allocating new texture {}x{} {:?} (allocations: {})",
                bucket.width,
                bucket.height,
                bucket.format,
                self.stats.allocations
            );

            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Pooled Render Target"),
                size: wgpu::Extent3d {
                    width: bucket.width,
                    height: bucket.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: bucket.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });

            let view = texture.create_view(&Default::default());
            (texture, view, bucket)
        }
    }

    fn pooled_total(&self) -> usize {
        self.pools.values().map(Vec::len).sum()
    }

    /// Destroy one idle texture: the largest, on the theory that a window
    /// being resized will not return to it soon.
    fn evict_one(&mut self) {
        let largest = self
            .pools
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(b, _)| *b)
            .max_by_key(|b| b.width as u64 * b.height as u64);
        if let Some(bucket) = largest
            && let Some(entry) = self.pools.get_mut(&bucket).and_then(|v| v.pop())
        {
            log::debug!(
                "Evicting pooled texture {}x{} {:?}",
                bucket.width,
                bucket.height,
                bucket.format
            );
            entry.texture.destroy();
        }
        self.pools.retain(|_, v| !v.is_empty());
    }

    fn return_texture(
        &mut self,
        bucket: TextureBucket,
        texture: wgpu::Texture,
        view: wgpu::TextureView,
    ) {
        while self.pooled_total() >= MAX_POOLED_TOTAL {
            self.evict_one();
        }
        let pool = self.pools.entry(bucket).or_default();
        if pool.len() < self.max_per_bucket {
            self.stats.returns += 1;
            log::debug!(
                "Returning texture {}x{} {:?} to pool (size: {})",
                bucket.width,
                bucket.height,
                bucket.format,
                pool.len() + 1
            );
            pool.push(TextureEntry { texture, view });
        } else {
            log::debug!(
                "Pool full for {}x{} {:?}, destroying texture",
                bucket.width,
                bucket.height,
                bucket.format
            );
            texture.destroy();
        }
    }

    fn shrink(&mut self) {
        for (bucket, pool) in &mut self.pools {
            // Keep at most 1 texture per size when shrinking
            while pool.len() > 1 {
                if let Some(entry) = pool.pop() {
                    log::debug!(
                        "Shrinking pool: destroying texture {}x{} {:?}",
                        bucket.width,
                        bucket.height,
                        bucket.format
                    );
                    entry.texture.destroy();
                }
            }
        }
        self.pools.retain(|_, v| !v.is_empty());
    }
}

/// Texture pool for reusing GPU render targets across window lifecycles
///
/// # Usage
/// ```ignore
/// let pool = TexturePool::new(device.clone(), 2);
/// let texture = pool.checkout(1920, 1080, wgpu::TextureFormat::Rgba8Unorm);
/// // Use texture...
/// // Texture is automatically returned to pool when dropped
/// ```
pub struct TexturePool {
    inner: Arc<Mutex<TexturePoolInner>>,
}

impl TexturePool {
    /// Create a new texture pool
    ///
    /// # Arguments
    /// * `device` - The wgpu device for creating textures
    /// * `max_per_bucket` - Maximum textures to keep pooled per size bucket
    pub fn new(device: Arc<wgpu::Device>, max_per_bucket: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TexturePoolInner::new(device, max_per_bucket))),
        }
    }

    /// Check out a texture from the pool
    ///
    /// Returns a pooled texture if one of sufficient size exists,
    /// otherwise allocates a new one. The texture may be larger than
    /// requested due to power-of-two bucketing.
    ///
    /// The texture is automatically returned to the pool when dropped.
    /// Returns None if the pool lock is poisoned.
    pub fn checkout(
        &self,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> Option<PooledTexture> {
        let (texture, view, bucket) = match self.inner.lock() {
            Ok(mut inner) => inner.checkout(width, height, format),
            Err(e) => {
                log::error!("Texture pool lock poisoned: {}", e);
                return None;
            }
        };
        Some(PooledTexture {
            texture: Some(texture),
            view: Some(view),
            bucket,
            actual_width: width,
            actual_height: height,
            pool: Arc::downgrade(&self.inner),
        })
    }

    /// Get current pool statistics
    #[allow(dead_code)]
    pub fn stats(&self) -> TexturePoolStats {
        match self.inner.lock() {
            Ok(inner) => inner.stats.clone(),
            Err(e) => {
                log::warn!("Texture pool lock poisoned, returning default stats: {}", e);
                TexturePoolStats::default()
            }
        }
    }

    /// Shrink the pool by releasing excess textures
    ///
    /// Call this after closing windows to free up GPU memory.
    pub fn shrink(&self) {
        match self.inner.lock() {
            Ok(mut inner) => inner.shrink(),
            Err(e) => {
                log::warn!("Texture pool lock poisoned, skipping shrink: {}", e);
            }
        }
    }
}

/// A texture checked out from the pool with RAII semantics
///
/// The texture is automatically returned to the pool when dropped.
/// Note: The texture may be larger than the requested size due to
/// power-of-two bucketing. Use `actual_width()` and `actual_height()`
/// to get the originally requested dimensions.
pub struct PooledTexture {
    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    bucket: TextureBucket,
    #[allow(dead_code)]
    actual_width: u32,
    #[allow(dead_code)]
    actual_height: u32,
    pool: Weak<Mutex<TexturePoolInner>>,
}

impl PooledTexture {
    /// Get a reference to the underlying texture
    pub fn texture(&self) -> &wgpu::Texture {
        self.texture
            .as_ref()
            .expect("PooledTexture already returned")
    }

    /// Get a reference to the texture view
    pub fn view(&self) -> &wgpu::TextureView {
        self.view.as_ref().expect("PooledTexture already returned")
    }

    /// Get the actual requested width (may be smaller than texture width)
    #[allow(dead_code)]
    pub fn actual_width(&self) -> u32 {
        self.actual_width
    }

    /// Get the actual requested height (may be smaller than texture height)
    #[allow(dead_code)]
    pub fn actual_height(&self) -> u32 {
        self.actual_height
    }

    /// Get the bucket (power-of-two) dimensions
    #[allow(dead_code)]
    pub fn bucket_size(&self) -> (u32, u32) {
        (self.bucket.width, self.bucket.height)
    }
}

impl Drop for PooledTexture {
    fn drop(&mut self) {
        if let (Some(texture), Some(view)) = (self.texture.take(), self.view.take()) {
            if let Some(pool) = self.pool.upgrade()
                && let Ok(mut inner) = pool.lock()
            {
                inner.return_texture(self.bucket, texture, view);
                return;
            }
            // Pool is gone, destroy the texture
            texture.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_texture_bucket_is_exact() {
        // Exact sizes: a larger render target would be resampled by the
        // composite/CRT passes and blur the frame.
        let bucket = TextureBucket::from_size(1920, 1080, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(bucket.width, 1920);
        assert_eq!(bucket.height, 1080);

        let small = TextureBucket::from_size(100, 50, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!(small.width, 100);
        assert_eq!(small.height, 50);

        // Zero is clamped to a valid texture size
        let zero = TextureBucket::from_size(0, 0, wgpu::TextureFormat::Rgba8Unorm);
        assert_eq!((zero.width, zero.height), (1, 1));
    }
}
