//! Threads, memory maps and small helpers shared by every phase.

use std::alloc::GlobalAlloc;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// Worker threads: `NCPU` when the scorer sets it, else what the OS grants us.
pub fn thread_count() -> usize {
    if let Ok(text) = std::env::var("NCPU") {
        if let Ok(n) = text.trim().parse::<usize>() {
            if n > 0 {
                return n.min(256);
            }
        }
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

/// Runs `f(thread_index)` on `n` threads (the caller is thread 0) and returns
/// each thread's result in thread order. A panic in any thread aborts the
/// process (panic = "abort"), so a failure can never produce partial output.
pub fn par_run<T: Send, F: Fn(usize) -> T + Sync>(n: usize, f: F) -> Vec<T> {
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = (1..n)
            .map(|t| {
                std::thread::Builder::new()
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, move || f(t))
                    .expect("spawn worker thread")
            })
            .collect();
        let mut out = Vec::with_capacity(n);
        out.push(f(0));
        for handle in handles {
            out.push(handle.join().expect("worker thread"));
        }
        out
    })
}

mod ffi {
    use std::ffi::c_void;
    extern "C" {
        pub fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut c_void;
        pub fn munmap(addr: *mut c_void, len: usize) -> i32;
        #[cfg(target_os = "linux")]
        pub fn mremap(old: *mut c_void, old_len: usize, new_len: usize, flags: i32, ...) -> *mut c_void;
        pub fn madvise(addr: *mut c_void, len: usize, advice: i32) -> i32;
        pub fn getrusage(who: i32, usage: *mut [i64; 18]) -> i32;
        pub fn _exit(code: i32) -> !;
    }
    pub const PROT_READ: i32 = 1;
    pub const PROT_WRITE: i32 = 2;
    pub const MAP_PRIVATE: i32 = 2;
    #[cfg(target_os = "linux")]
    pub const MAP_ANONYMOUS: i32 = 0x20;
    // macOS is only a local test platform: no huge pages, no mremap.
    #[cfg(target_os = "macos")]
    pub const MAP_ANONYMOUS: i32 = 0x1000;
    #[cfg(target_os = "linux")]
    pub const MREMAP_MAYMOVE: i32 = 1;
    #[cfg(target_os = "linux")]
    const MADV_HUGEPAGE: i32 = 14;

    /// Asks for transparent huge pages (Linux only; a no-op elsewhere).
    #[inline]
    pub unsafe fn advise_huge(_p: *mut c_void, _len: usize) {
        #[cfg(target_os = "linux")]
        madvise(_p, _len, MADV_HUGEPAGE);
    }
}

/// Ends the process with status 0 at once, without running destructors.
/// Call only when all output is written and flushed.
pub fn exit_now() -> ! {
    use std::io::Write;
    let _ = std::io::stderr().flush();
    unsafe { ffi::_exit(0) }
}

/// The global allocator: allocations of 1 MiB or more get their own
/// 2 MiB-aligned anonymous mapping marked for transparent huge pages, so the
/// big buffers fault in 2 MiB at a time instead of 4 KiB; smaller ones use
/// the system allocator.
pub struct HugeAlloc;

const LARGE: usize = 1 << 20;
const HUGE: usize = 2 << 20;

#[inline]
fn huge_len(size: usize) -> usize {
    (size + HUGE - 1) & !(HUGE - 1)
}

unsafe fn map_huge(size: usize) -> *mut u8 {
    let len = huge_len(size);
    let raw = ffi::mmap(
        std::ptr::null_mut(),
        len + HUGE,
        ffi::PROT_READ | ffi::PROT_WRITE,
        ffi::MAP_PRIVATE | ffi::MAP_ANONYMOUS,
        -1,
        0,
    );
    if raw as isize == -1 {
        return std::ptr::null_mut();
    }
    let raw = raw as usize;
    let aligned = (raw + HUGE - 1) & !(HUGE - 1);
    let head = aligned - raw;
    if head > 0 {
        ffi::munmap(raw as *mut _, head);
    }
    let tail = HUGE - head;
    if tail > 0 {
        ffi::munmap((aligned + len) as *mut _, tail);
    }
    ffi::advise_huge(aligned as *mut _, len);
    aligned as *mut u8
}

unsafe impl std::alloc::GlobalAlloc for HugeAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= LARGE && layout.align() <= HUGE {
            map_huge(layout.size())
        } else {
            std::alloc::System.alloc(layout)
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        if layout.size() >= LARGE && layout.align() <= HUGE {
            map_huge(layout.size()) // fresh anonymous pages are zero
        } else {
            std::alloc::System.alloc_zeroed(layout)
        }
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        if layout.size() >= LARGE && layout.align() <= HUGE {
            ffi::munmap(ptr as *mut _, huge_len(layout.size()));
        } else {
            std::alloc::System.dealloc(ptr, layout)
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let old_large = layout.size() >= LARGE && layout.align() <= HUGE;
        let new_large = new_size >= LARGE && layout.align() <= HUGE;
        if old_large && new_large {
            let (old_len, new_len) = (huge_len(layout.size()), huge_len(new_size));
            if old_len == new_len {
                return ptr;
            }
            #[cfg(target_os = "linux")]
            {
                let p = ffi::mremap(ptr as *mut _, old_len, new_len, ffi::MREMAP_MAYMOVE);
                if p as isize == -1 {
                    return std::ptr::null_mut();
                }
                ffi::advise_huge(p, new_len);
                return p as *mut u8;
            }
        }
        if !old_large && !new_large {
            return std::alloc::System.realloc(ptr, layout, new_size);
        }
        let new_layout = std::alloc::Layout::from_size_align_unchecked(new_size, layout.align());
        let p = self.alloc(new_layout);
        if !p.is_null() {
            std::ptr::copy_nonoverlapping(ptr, p, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        p
    }
}

/// A read-only memory map of a whole file. Never unmapped: the process exits
/// right after the work is done.
pub struct Mmap {
    ptr: *const u8,
    len: usize,
}

unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    pub fn open(path: &Path) -> io::Result<Mmap> {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Mmap { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 });
        }
        let ptr = unsafe {
            ffi::mmap(std::ptr::null_mut(), len, ffi::PROT_READ, ffi::MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Mmap { ptr: ptr as *const u8, len })
    }

    #[inline]
    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// A large zero-initialised anonymous allocation backed by huge pages where
/// the kernel allows it. Used for the big scatter arenas.
pub fn alloc_zeroed_huge<T: Copy>(count: usize) -> &'static mut [T] {
    let bytes = (count.max(1) * std::mem::size_of::<T>() + (2 << 20) - 1) & !((2 << 20) - 1);
    let ptr = unsafe {
        ffi::mmap(
            std::ptr::null_mut(),
            bytes,
            ffi::PROT_READ | ffi::PROT_WRITE,
            ffi::MAP_PRIVATE | ffi::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr as isize == -1 {
        panic!("mmap of {bytes} bytes failed: {}", io::Error::last_os_error());
    }
    unsafe {
        ffi::advise_huge(ptr, bytes);
        std::slice::from_raw_parts_mut(ptr as *mut T, count)
    }
}

/// A vector of `n` all-zero values, from zeroed pages the kernel hands out
/// lazily, so the zeroing happens on first touch in whichever thread uses it.
///
/// # Safety
/// The all-zero bit pattern must be a valid `T` (integers, atomics, and plain
/// structs of them).
pub unsafe fn zeroed_vec<T>(n: usize) -> Vec<T> {
    if n == 0 || std::mem::size_of::<T>() == 0 {
        return Vec::new();
    }
    let layout = std::alloc::Layout::array::<T>(n).expect("allocation size");
    let p = std::alloc::alloc_zeroed(layout) as *mut T;
    if p.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    Vec::from_raw_parts(p, n, n)
}

/// Position of the first `needle` in `hay`, or None.
#[inline]
pub fn find_byte(hay: &[u8], needle: u8) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

/// Fibonacci hashing: a single multiply. Use the TOP bits (see `take`):
/// every top bit of the product depends on every bit of `x`.
#[inline(always)]
pub fn fib(x: u64) -> u64 {
    x.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// `bits` bits of the hash `h`, starting `skip` bits below the top.
#[inline(always)]
pub fn take(h: u64, skip: u32, bits: u32) -> usize {
    if bits == 0 {
        0
    } else {
        ((h << skip) >> (64 - bits)) as usize
    }
}

/// Splitmix-style finalizer: a cheap, well-mixed 64-bit hash.
#[inline(always)]
pub fn mix64(mut x: u64) -> u64 {
    x ^= x >> 31;
    x = x.wrapping_mul(0x7FB5_D329_728E_A185);
    x ^= x >> 27;
    x = x.wrapping_mul(0x81DA_DEF4_BC2D_D44D);
    x ^ (x >> 33)
}

/// Shared mutable access to disjoint parts of one buffer from many threads.
/// Callers guarantee that no two threads touch the same element.
pub struct SharedMut<T> {
    ptr: *mut T,
    len: usize,
}

unsafe impl<T: Send> Send for SharedMut<T> {}
unsafe impl<T: Send> Sync for SharedMut<T> {}

impl<T> SharedMut<T> {
    pub fn new(slice: &mut [T]) -> Self {
        SharedMut { ptr: slice.as_mut_ptr(), len: slice.len() }
    }

    /// # Safety
    /// The range must not overlap any range another thread writes or reads concurrently.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self, start: usize, end: usize) -> &mut [T] {
        assert!(start <= end && end <= self.len);
        std::slice::from_raw_parts_mut(self.ptr.add(start), end - start)
    }
}

/// User and system CPU seconds used so far by every thread of the process.
pub fn cpu_times() -> (f64, f64) {
    let mut usage = [0i64; 18];
    if unsafe { ffi::getrusage(0, &mut usage) } != 0 {
        return (0.0, 0.0);
    }
    (usage[0] as f64 + usage[1] as f64 * 1e-6, usage[2] as f64 + usage[3] as f64 * 1e-6)
}

#[inline(always)]
pub fn prefetch(p: *const i8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
    }
}
