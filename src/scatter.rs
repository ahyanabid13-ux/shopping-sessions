//! Per-thread partitioned append buffers.
//!
//! Each partition is a list of fixed-size chunks carved out of large
//! huge-page regions, so appending never copies and the write heads of all
//! partitions stay few enough to live in cache.

pub const CHUNK: usize = 512;

pub struct Scatter<T: Copy> {
    heads: Vec<Head<T>>,
    full: Vec<Vec<*const T>>,
    region: *mut T,
    region_left: usize,
    region_records: usize,
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
        let region_records = (expected + parts * CHUNK).clamp(CHUNK * 64, 1 << 26) / CHUNK * CHUNK;
        Scatter {
            heads: vec![Head { ptr: std::ptr::null_mut(), len: CHUNK }; parts],
            full: (0..parts).map(|_| Vec::new()).collect(),
            region: std::ptr::null_mut(),
            region_left: 0,
            region_records,
        }
    }

    #[inline(always)]
    pub fn push(&mut self, part: usize, value: T) {
        let head = unsafe { self.heads.get_unchecked_mut(part) };
        if head.len == CHUNK {
            self.next_chunk(part);
        }
        let head = unsafe { self.heads.get_unchecked_mut(part) };
        unsafe { head.ptr.add(head.len).write(value) };
        head.len += 1;
    }

    #[cold]
    #[inline(never)]
    fn next_chunk(&mut self, part: usize) {
        let head = self.heads[part];
        if !head.ptr.is_null() {
            self.full[part].push(head.ptr);
        }
        if self.region_left < CHUNK {
            let records = self.region_records;
            let layout = std::alloc::Layout::array::<T>(records).expect("region size");
            let p = unsafe { std::alloc::alloc(layout) } as *mut T;
            if p.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            // Regions are never freed: they live until the process exits.
            self.region = p;
            self.region_left = records;
        }
        let ptr = self.region;
        self.region = unsafe { self.region.add(CHUNK) };
        self.region_left -= CHUNK;
        self.heads[part] = Head { ptr, len: 0 };
    }

    /// Every record of `part`, as chunk slices.
    pub fn chunks(&self, part: usize, out: &mut Vec<&[T]>) {
        for &p in &self.full[part] {
            out.push(unsafe { std::slice::from_raw_parts(p, CHUNK) });
        }
        let head = self.heads[part];
        if !head.ptr.is_null() && head.len > 0 {
            out.push(unsafe { std::slice::from_raw_parts(head.ptr, head.len) });
        }
    }

    pub fn len(&self, part: usize) -> usize {
        let head = self.heads[part];
        self.full[part].len() * CHUNK + if head.ptr.is_null() { 0 } else { head.len }
    }
}
