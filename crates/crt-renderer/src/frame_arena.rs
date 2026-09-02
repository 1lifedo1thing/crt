//! Per-frame bump allocator for transient vertex data.
//!
//! `wgpu::Queue::write_buffer` is not ordered against draws inside the same
//! `submit`: every pending write lands before the command buffer executes.
//! Writing the same buffer twice in one frame therefore makes *every* pass
//! that references it draw the last write. Transient per-pass data (overlay
//! rects, dialog text, tab bar shapes) is instead bump-allocated from one
//! buffer at distinct offsets, so a frame performs many small writes that
//! never overlap and one buffer serves every pass.
//!
//! The arena never reallocates mid-frame (earlier passes already reference
//! the current buffer). A frame that runs out of room skips the overflowing
//! draws, records the shortfall, and `begin()` grows the buffer before the
//! next frame.

use bytemuck::Pod;

/// A range of the arena buffer holding `count` instances.
#[derive(Clone, Copy, Debug)]
pub struct ArenaSlice {
    pub offset: u64,
    pub len_bytes: u64,
    pub count: u32,
}

pub struct FrameArena {
    buffer: wgpu::Buffer,
    capacity: u64,
    cursor: u64,
    /// Bytes the last frame needed beyond `capacity` (0 = fit).
    shortfall: u64,
    label: &'static str,
}

impl FrameArena {
    /// Offsets are aligned to this many bytes (a multiple of every instance size we use).
    const ALIGN: u64 = 16;
    /// Smallest arena we will create.
    pub const MIN_CAPACITY: u64 = 256 * 1024;

    pub fn new(device: &wgpu::Device, capacity: u64, label: &'static str) -> Self {
        let capacity = capacity.max(Self::MIN_CAPACITY);
        Self {
            buffer: Self::create(device, capacity, label),
            capacity,
            cursor: 0,
            shortfall: 0,
            label,
        }
    }

    fn create(device: &wgpu::Device, capacity: u64, label: &'static str) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Start a new frame. Grows the buffer if the previous frame overflowed.
    pub fn begin(&mut self, device: &wgpu::Device) {
        if self.shortfall > 0 {
            let needed = self.capacity + self.shortfall;
            let new_capacity = needed.next_power_of_two().max(self.capacity * 2);
            log::info!(
                "{}: growing frame arena {} -> {} bytes",
                self.label,
                self.capacity,
                new_capacity
            );
            self.buffer.destroy();
            self.buffer = Self::create(device, new_capacity, self.label);
            self.capacity = new_capacity;
            self.shortfall = 0;
        }
        self.cursor = 0;
    }

    /// Upload `items` at a fresh offset. Returns `None` (and records the
    /// shortfall) when the frame has run out of room.
    pub fn push<T: Pod>(&mut self, queue: &wgpu::Queue, items: &[T]) -> Option<ArenaSlice> {
        if items.is_empty() {
            return None;
        }
        let bytes: &[u8] = bytemuck::cast_slice(items);
        let len_bytes = bytes.len() as u64;
        let offset = self.cursor.div_ceil(Self::ALIGN) * Self::ALIGN;
        let end = offset + len_bytes;
        if end > self.capacity {
            self.shortfall = self.shortfall.max(end - self.capacity);
            return None;
        }
        queue.write_buffer(&self.buffer, offset, bytes);
        self.cursor = end;
        Some(ArenaSlice {
            offset,
            len_bytes,
            count: items.len() as u32,
        })
    }

    pub fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }

    pub fn slice(&self, s: ArenaSlice) -> wgpu::BufferSlice<'_> {
        self.buffer.slice(s.offset..s.offset + s.len_bytes)
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Bytes used so far this frame.
    pub fn used(&self) -> u64 {
        self.cursor
    }
}

impl Drop for FrameArena {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// A vertex buffer owned by one renderer, grown on demand and uploaded only
/// when the instance list changed since the last upload.
pub(crate) struct OwnedInstanceBuffer {
    buffer: Option<wgpu::Buffer>,
    capacity_bytes: u64,
    label: &'static str,
}

impl OwnedInstanceBuffer {
    pub(crate) fn new(label: &'static str) -> Self {
        Self {
            buffer: None,
            capacity_bytes: 0,
            label,
        }
    }

    /// Ensure capacity and upload `items`. Returns the buffer to bind.
    pub(crate) fn upload<T: Pod>(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        items: &[T],
    ) -> &wgpu::Buffer {
        let bytes: &[u8] = bytemuck::cast_slice(items);
        let needed = (bytes.len() as u64).max(4096);
        if self.buffer.is_none() || needed > self.capacity_bytes {
            let capacity = needed.next_power_of_two();
            if let Some(old) = self.buffer.take() {
                old.destroy();
            }
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity_bytes = capacity;
        }
        let buffer = self.buffer.as_ref().expect("buffer just ensured");
        if !bytes.is_empty() {
            queue.write_buffer(buffer, 0, bytes);
        }
        buffer
    }

    pub(crate) fn buffer(&self) -> Option<&wgpu::Buffer> {
        self.buffer.as_ref()
    }
}

impl Drop for OwnedInstanceBuffer {
    fn drop(&mut self) {
        if let Some(b) = self.buffer.take() {
            b.destroy();
        }
    }
}
