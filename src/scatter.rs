//! Per-thread partitioned append buffers.
//!
//! Each partition is a list of fixed-size chunks carved out of large
//! huge-page regions, so appending never copies. Records are staged per
//! partition in a small cache-resident buffer and flushed one full cache line
//! (or a few) at a time with non-temporal stores: the destination lines are
//! never read for ownership and do not evict the working set.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

pub struct Scatter<T: Copy> {
    /// Records per staging flush: a whole number of cache lines.
    group: usize,
    /// Bytes of one partition's staging buffer (`group` records).
    stage_bytes: usize,
    /// Records per chunk (a multiple of `group`), and the chunk's bytes.
    per_chunk: usize,
    chunk_bytes: usize,
    /// Staging buffers, 64-byte aligned, `stage_bytes` per partition.
    stage_buf: Vec<u8>,
    stage_off: usize,
    staged: Vec<u8>,
    heads: Vec<Head<T>>,
    full: Vec<Vec<*const T>>,
    region: *mut u8,
    region_left: usize,
    region_bytes: usize,
}

#[derive(Clone, Copy)]
struct Head<T> {
    ptr: *mut T,
    len: usize,
}

unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// `parts` partitions; `expected` records in total (sizes the regions).
    pub fn new(parts: usize, expected: usize) -> Self {
        let size = std::mem::size_of::<T>();
        assert!(size > 0 && size <= 64 && 64 % size == 0 || size == 24 || size == 48, "record size {size} not supported");
        let group = match size {
            24 => 8,
            48 => 4,
            _ => 64 / size,
        };
        assert!(group * size <= 192 && (group * size) % 64 == 0);
        let stage_bytes = group * size;
        // Chunks near a quarter of what one partition receives, 4..64 KiB, so
        // the partially filled last chunks waste little.
        let per_part = expected * size / parts.max(1);
        let chunk_bytes = (per_part / 4).next_power_of_two().clamp(4096, 65536);
        let per_chunk = chunk_bytes / stage_bytes * group;
        let chunk_bytes = per_chunk * size;
        let region_bytes = ((expected * size) * 5 / 4 + parts * chunk_bytes).clamp(chunk_bytes * 64, 1 << 31);
        let region_bytes = region_bytes / chunk_bytes * chunk_bytes;
        let stage_buf = vec![0u8; parts * stage_bytes + 64];
        let stage_off = stage_buf.as_ptr().align_offset(64);
        Scatter {
            group,
            stage_bytes,
            per_chunk,
            chunk_bytes,
            stage_buf,
            stage_off,
            staged: vec![0; parts],
            heads: vec![Head { ptr: std::ptr::null_mut(), len: 0 }; parts],
            full: (0..parts).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(),
            region_left: 0,
            region_bytes,
        }
    }

    #[inline(always)]
    pub fn push(&mut self, part: usize, value: T) {
        unsafe {
            let n = *self.staged.get_unchecked(part) as usize;
            let stage = self.stage_ptr(part) as *mut T;
            stage.add(n).write(value);
            if n + 1 == self.group {
                *self.staged.get_unchecked_mut(part) = 0;
                self.flush(part);
            } else {
                *self.staged.get_unchecked_mut(part) = (n + 1) as u8;
            }
        }
    }

    /// Streams one full staging group into the partition's head chunk.
    #[inline(always)]
    unsafe fn flush(&mut self, part: usize) {
        let head = *self.heads.get_unchecked(part);
        if head.ptr.is_null() || head.len == self.per_chunk {
            self.next_chunk(part);
        }
        let src = self.stage_ptr(part) as *const __m256i;
        let lanes = self.stage_bytes / 32;
        let head = self.heads.get_unchecked_mut(part);
        let dst = head.ptr.add(head.len) as *mut __m256i;
        for i in 0..lanes {
            _mm256_stream_si256(dst.add(i), _mm256_load_si256(src.add(i)));
        }
        head.len += self.group;
    }

    #[inline(always)]
    fn stage_ptr(&mut self, part: usize) -> *mut u8 {
        unsafe { self.stage_buf.as_mut_ptr().add(self.stage_off + part * self.stage_bytes) }
    }

    #[cold]
    #[inline(never)]
    fn next_chunk(&mut self, part: usize) {
        let head = self.heads[part];
        if !head.ptr.is_null() {
            debug_assert_eq!(head.len, self.per_chunk);
            self.full[part].push(head.ptr);
        }
        if self.region_left < self.chunk_bytes {
            let layout = std::alloc::Layout::from_size_align(self.region_bytes, 4096).expect("region size");
            let p = unsafe { std::alloc::alloc(layout) };
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            // Regions are never freed: they live until the process exits.
            self.region = p;
            self.region_left = self.region_bytes;
        }
        let ptr = self.region as *mut T;
        self.region = unsafe { self.region.add(self.chunk_bytes) };
        self.region_left -= self.chunk_bytes;
        self.heads[part] = Head { ptr, len: 0 };
    }

    /// Moves every partially filled staging buffer into its chunk and makes
    /// all streamed data visible. Call once, when the producer is done.
    pub fn finish(&mut self) {
        for part in 0..self.heads.len() {
            let n = self.staged[part] as usize;
            if n == 0 {
                continue;
            }
            for i in 0..n {
                let head = self.heads[part];
                if head.ptr.is_null() || head.len == self.per_chunk {
                    self.next_chunk(part);
                }
                let v = unsafe { (self.stage_ptr(part) as *const T).add(i).read() };
                let head = &mut self.heads[part];
                unsafe { head.ptr.add(head.len).write(v) };
                head.len += 1;
            }
            self.staged[part] = 0;
        }
        unsafe { _mm_sfence() };
    }

    /// Every record of `part`, as chunk slices. Only valid after `finish`.
    pub fn chunks<'a>(&'a self, part: usize, out: &mut Vec<&'a [T]>) {
        debug_assert!(self.staged[part] == 0);
        for &p in &self.full[part] {
            out.push(unsafe { std::slice::from_raw_parts(p, self.per_chunk) });
        }
        let head = self.heads[part];
        if !head.ptr.is_null() && head.len > 0 {
            out.push(unsafe { std::slice::from_raw_parts(head.ptr, head.len) });
        }
    }

    pub fn len(&self, part: usize) -> usize {
        let head = self.heads[part];
        self.full[part].len() * self.per_chunk + if head.ptr.is_null() { 0 } else { head.len }
    }
}
