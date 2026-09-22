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
//! draws and reports `overflowed()`; the caller must schedule another frame,
//! because `begin()` grows the buffer to the frame's full demand only when
//! that next frame starts.

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
    /// Where `cursor` would be had every push this frame fit. Exceeds
    /// `capacity` once the frame has overflowed.
    demand: u64,
    label: &'static str,
}

impl FrameArena {
    /// Offsets are aligned to this many bytes (`write_buffer` needs 4; vertex
    /// buffer offsets have no stride requirement).
    const ALIGN: u64 = 16;
    /// Smallest arena we will create.
    pub const MIN_CAPACITY: u64 = 256 * 1024;

    pub fn new(device: &wgpu::Device, capacity: u64, label: &'static str) -> Self {
        let capacity = capacity.max(Self::MIN_CAPACITY);
        Self {
            buffer: Self::create(device, capacity, label),
            capacity,
            cursor: 0,
            demand: 0,
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
        if self.overflowed() {
            let new_capacity = self.demand.next_power_of_two().max(self.capacity * 2);
            log::info!(
                "{}: growing frame arena {} -> {} bytes",
                self.label,
                self.capacity,
                new_capacity
            );
            self.buffer.destroy();
            self.buffer = Self::create(device, new_capacity, self.label);
            self.capacity = new_capacity;
        }
        self.cursor = 0;
        self.demand = 0;
    }

    /// Upload `items` at a fresh offset. Returns `None` (and records the
    /// unmet demand) when the frame has run out of room.
    pub fn push<T: Pod>(&mut self, queue: &wgpu::Queue, items: &[T]) -> Option<ArenaSlice> {
        if items.is_empty() {
            return None;
        }
        let bytes: &[u8] = bytemuck::cast_slice(items);
        let len_bytes = bytes.len() as u64;
        self.demand = self.demand.div_ceil(Self::ALIGN) * Self::ALIGN + len_bytes;
        let offset = self.cursor.div_ceil(Self::ALIGN) * Self::ALIGN;
        let end = offset + len_bytes;
        if end > self.capacity {
            return None;
        }
        queue.write_buffer(&self.buffer, offset, bytes);
        self.cursor = end;
        // A smaller push can still fit after a larger one was skipped
        self.demand = self.demand.max(end);
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

    /// Whether a push this frame was skipped for lack of room. The skipped
    /// draws are missing from the frame, so the caller must render another
    /// one; `begin()` will have grown the arena to fit by then.
    pub fn overflowed(&self) -> bool {
        self.demand > self.capacity
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
