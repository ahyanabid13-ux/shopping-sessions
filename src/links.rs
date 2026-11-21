//! Linked IDs: a concurrent union-find over every ID in a valid link line,
//! then a read-only table from user key to person.
//!
//! The table's slot index is the union-find node. Unions always hang the
//! root with the larger index under the smaller one, so parent pointers only
//! ever decrease and concurrent unions cannot form a cycle. Every array
//! starts all-zero (lazily zeroed pages): a parent of 0 means "root", any
//! other value is the parent's index plus one.

use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::ids;
use crate::util::{fib, par_run, take, zeroed_vec, SharedMut};


/// Person keys of linked IDs carry this bit; an unlinked ID's person key is its own user key.
pub const LINKED: u64 = 1 << ids::KEY_BITS;
/// Person keys use the low 48 bits.
pub const PERSON_MASK: u64 = (1 << 48) - 1;

/// Compact user key: fast keys become one dense number (all IDs of 1 digit,
/// then of 2 digits, ...), odd keys keep their interner index under bit 42.
/// Below 2^43 either way.
#[inline(always)]
fn compact(key: u64) -> u64 {
    const OFFSET: [u64; 9] = [
        0,
        0,
        36,
        36 + 1_296,
        36 + 1_296 + 46_656,
        36 + 1_296 + 46_656 + 1_679_616,
        36 + 1_296 + 46_656 + 1_679_616 + 60_466_176,
        36 + 1_296 + 46_656 + 1_679_616 + 60_466_176 + 2_176_782_336,
        36 + 1_296 + 46_656 + 1_679_616 + 60_466_176 + 2_176_782_336 + 78_364_164_096,
    ];
    if key & ids::ODD != 0 {
        (1 << 42) | (key & !ids::ODD)
    } else {
        OFFSET[((key >> 42) & 15) as usize] + (key & ((1 << 42) - 1))
    }
}

const KEY43: u64 = (1 << 43) - 1;
/// Persons that fit next to a compact key in one 8-byte slot.
const NARROW_PERSONS: usize = 1 << 21;

/// Read-only map from user key to person. Narrow slots pack (compact key + 1)
/// and the person into 8 bytes; wide slots are used only when there are too
/// many persons for that.
pub struct LinkTable {
    narrow: Vec<u64>,
    wide: Vec<(u64, u64)>,
    cap: usize,
    /// Name (the byte-wise smallest user key) of each linked person.
    pub names: Vec<u64>,
}

impl LinkTable {
    #[inline(always)]
    fn home(&self, ck: u64) -> usize {
        // Fast range reduction of the hash's top 32 bits onto 0..cap.
        (((fib(ck) >> 32) * self.cap as u64) >> 32) as usize
    }

    #[inline(always)]
    pub fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let h = self.home(compact(key) + 1);
            let p = if self.wide.is_empty() {
                self.narrow.as_ptr().add(h) as *const i8
            } else {
                self.wide.as_ptr().add(h) as *const i8
            };
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }

    /// Person key of a user key.
    #[inline(always)]
    pub fn person_of(&self, key: u64) -> u64 {
        let ck = compact(key) + 1;
        let mut i = self.home(ck);
        if self.wide.is_empty() {
            loop {
                let s = unsafe { *self.narrow.get_unchecked(i) };
                if s & KEY43 == ck {
                    return LINKED | (s >> 43);
                }
                if s == 0 {
                    return key;
                }
                i += 1;
                if i == self.cap {
                    i = 0;
                }
            }
        } else {
            loop {
                let (k, p) = unsafe { *self.wide.get_unchecked(i) };
                if k == ck {
                    return LINKED | p;
                }
                if k == 0 {
                    return key;
                }
                i += 1;
                if i == self.cap {
                    i = 0;
                }
            }
        }
    }

    fn insert(&mut self, key: u64, person: u64) {
        let ck = compact(key) + 1;
        let mut i = self.home(ck);
        if self.wide.is_empty() {
            while self.narrow[i] != 0 {
                i = if i + 1 == self.cap { 0 } else { i + 1 };
            }
            self.narrow[i] = ck | person << 43;
        } else {
            while self.wide[i].0 != 0 {
                i = if i + 1 == self.cap { 0 } else { i + 1 };
            }
            self.wide[i] = (ck, person);
        }
    }
}

struct Builder {
    keys: Vec<AtomicU64>,
    /// 0: root; otherwise parent index + 1.
    parent: Vec<AtomicU32>,
    mask: usize,
    bits: u32,
}

impl Builder {
    fn insert(&self, key: u64) -> u32 {
        let mut i = take(fib(key), 0, self.bits);
        loop {
            let current = self.keys[i].load(Ordering::Acquire);
            if current == key {
                return i as u32;
            }
            if current == 0 {
                match self.keys[i].compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return i as u32,
                    Err(actual) if actual == key => return i as u32,
                    Err(_) => {}
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let p = self.keys.as_ptr().add(take(fib(key), 0, self.bits)) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }

    #[inline]
    fn parent_of(&self, x: u32) -> u32 {
        match self.parent[x as usize].load(Ordering::Acquire) {
            0 => x,
            p => p - 1,
        }
    }

    #[inline]
    fn find(&self, mut x: u32) -> u32 {
        loop {
            let p = self.parent_of(x);
            if p == x {
                return x;
            }
            let gp = self.parent_of(p);
            if gp != p {
                // Path halving; losing this race is harmless.
                let _ = self.parent[x as usize].compare_exchange(p + 1, gp + 1, Ordering::AcqRel, Ordering::Relaxed);
            }
            x = gp;
        }
    }

    fn union(&self, a: u32, b: u32) {
        let (mut a, mut b) = (a, b);
        loop {
            a = self.find(a);
            b = self.find(b);
            if a == b {
                return;
            }
            if a < b {
                std::mem::swap(&mut a, &mut b);
            }
            // a > b: hang root a under b, if a is still a root.
            if self.parent[a as usize]
                .compare_exchange(0, b + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn is_root(&self, s: usize) -> bool {
        self.keys[s].load(Ordering::Relaxed) != 0 && self.parent[s].load(Ordering::Relaxed) == 0
    }
}

/// Builds the person table from every thread's link pairs.
pub fn build(pairs: &[Vec<(u64, u64)>], threads: usize, odd: &[Vec<u8>]) -> LinkTable {
    let total: usize = pairs.iter().map(|p| p.len()).sum();
    // At most 2 IDs per pair; keep the load at or below one half.
    let capacity = (total * 4).max(1024).next_power_of_two();
    let mask = capacity - 1;
    let bits = capacity.trailing_zeros();
    let builder = unsafe { Builder { keys: zeroed_vec(capacity), parent: zeroed_vec(capacity), mask, bits } };

    // Insert and union, every thread taking chunks of every pair list.
    let flat: Vec<&[(u64, u64)]> = pairs.iter().flat_map(|p| p.chunks(16_384)).collect();
    let next = AtomicUsize::new(0);
    par_run(threads, |_| loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(chunk) = flat.get(at) else { break };
        for (i, &(a, b)) in chunk.iter().enumerate() {
            if let Some(&(pa, pb)) = chunk.get(i + 8) {
                builder.prefetch(pa);
                builder.prefetch(pb);
            }
            let na = builder.insert(a);
            let nb = if a == b { na } else { builder.insert(b) };
            builder.union(na, nb);
        }
    });

    // Name each root by its smallest ID: a CAS-min over every member.
    let names_by_slot: Vec<AtomicU64> = unsafe { zeroed_vec(capacity) };
    let per_thread = capacity.div_ceil(threads);
    let range = |t: usize| ((t * per_thread).min(capacity), ((t + 1) * per_thread).min(capacity));
    par_run(threads, |t| {
        let (lo, hi) = range(t);
        for s in lo..hi {
            let key = builder.keys[s].load(Ordering::Relaxed);
            if key == 0 {
                continue;
            }
            let root = builder.find(s as u32) as usize;
            let cell = &names_by_slot[root];
            let mut current = cell.load(Ordering::Acquire);
            loop {
                if current != 0 && ids::cmp_ids(current, key, b'u', odd) != CmpOrdering::Greater {
                    break;
                }
                match cell.compare_exchange(current, key, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
    });

    // Number the roots densely, in slot order.
    let root_counts: Vec<usize> = par_run(threads, |t| {
        let (lo, hi) = range(t);
        (lo..hi).filter(|&s| builder.is_root(s)).count()
    });
    let mut first_person = vec![0usize; threads + 1];
    for t in 0..threads {
        first_person[t + 1] = first_person[t] + root_counts[t];
    }
    let persons = first_person[threads];
    assert!(persons < u32::MAX as usize, "too many linked persons");
    let person_of_root: Vec<AtomicU32> = unsafe { zeroed_vec(capacity) };
    let mut names: Vec<u64> = vec![0; persons];
    {
        let names_out = SharedMut::new(&mut names);
        par_run(threads, |t| {
            let (lo, hi) = range(t);
            let out = unsafe { names_out.slice_mut(first_person[t], first_person[t + 1]) };
            let mut k = 0usize;
            for s in lo..hi {
                if builder.is_root(s) {
                    person_of_root[s].store((first_person[t] + k) as u32, Ordering::Relaxed);
                    out[k] = names_by_slot[s].load(Ordering::Relaxed);
                    k += 1;
                }
            }
        });
    }

    // The read-only table, at load factor ~0.67. Filling it is a cheap
    // sequential pass over the builder's keys (with random writes).
    let linked_ids: usize = par_run(threads, |t| {
        let (lo, hi) = range(t);
        (lo..hi).filter(|&s| builder.keys[s].load(Ordering::Relaxed) != 0).count()
    })
    .iter()
    .sum();
    let cap = (linked_ids + linked_ids / 2).max(64);
    let wide = persons >= NARROW_PERSONS;
    let mut table = LinkTable {
        narrow: if wide { Vec::new() } else { unsafe { zeroed_vec(cap) } },
        wide: if wide { unsafe { zeroed_vec(cap) } } else { Vec::new() },
        cap,
        names,
    };
    for s in 0..capacity {
        let key = builder.keys[s].load(Ordering::Relaxed);
        if key != 0 {
            let root = builder.find(s as u32) as usize;
            table.insert(key, person_of_root[root].load(Ordering::Relaxed) as u64);
        }
    }
    table

}
