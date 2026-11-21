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
use crate::util::{fib, par_run, take, zeroed_vec};


/// Person keys of linked IDs carry this bit; an unlinked ID's person key is its own user key.
pub const LINKED: u64 = 1 << ids::KEY_BITS;
/// Person keys use the low 44 bits; categories ride above them (bit 48+).
pub const PERSON_MASK: u64 = (1 << 44) - 1;

/// User keys are already compact (below 2^43, never 0): see ids.rs.
#[inline(always)]
fn compact(key: u64) -> u64 {
    key
}

const KEY43: u64 = (1 << 43) - 1;
/// Persons that fit next to a compact key in one 8-byte slot.
const NARROW_PERSONS: usize = 1 << 21;

/// Read-only map from user key to person. Narrow slots pack the compact key
/// and the person into 8 bytes; wide slots (key, person) pairs, as two
/// words, are used only when there are too many persons for that.
pub struct LinkTable {
    narrow: Vec<u64>,
    wide: Vec<u64>,
    cap: usize,
    /// Name (the byte-wise smallest user key) of each linked person.
    pub names: Vec<u64>,
}

#[inline(always)]
fn home_of(ck: u64, cap: usize) -> usize {
    // Fast range reduction of the hash's top 32 bits onto 0..cap.
    (((fib(ck) >> 32) * cap as u64) >> 32) as usize
}

impl LinkTable {
    #[inline(always)]
    fn home(&self, ck: u64) -> usize {
        home_of(ck, self.cap)
    }

    #[inline(always)]
    pub fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let h = self.home(compact(key));
            let p = if self.wide.is_empty() {
                self.narrow.as_ptr().add(h) as *const i8
            } else {
                self.wide.as_ptr().add(2 * h) as *const i8
            };
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }

    /// Person key of a user key.
    #[inline(always)]
    pub fn person_of(&self, key: u64) -> u64 {
        let ck = compact(key);
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
                let k = unsafe { *self.wide.get_unchecked(2 * i) };
                if k == ck {
                    return LINKED | unsafe { *self.wide.get_unchecked(2 * i + 1) };
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
}

/// The table while it is being filled by every thread at once. Each key is
/// inserted exactly once, so a slot only ever goes from empty to full.
struct TableBuilder {
    narrow: Vec<AtomicU64>,
    wide: Vec<AtomicU64>,
    cap: usize,
}

impl TableBuilder {
    fn insert(&self, key: u64, person: u64) {
        let ck = compact(key);
        let mut i = home_of(ck, self.cap);
        if self.wide.is_empty() {
            let value = ck | person << 43;
            loop {
                let slot = &self.narrow[i];
                if slot.load(Ordering::Relaxed) == 0
                    && slot.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed).is_ok()
                {
                    return;
                }
                i = if i + 1 == self.cap { 0 } else { i + 1 };
            }
        } else {
            loop {
                let slot = &self.wide[2 * i];
                if slot.load(Ordering::Relaxed) == 0
                    && slot.compare_exchange(0, ck, Ordering::Relaxed, Ordering::Relaxed).is_ok()
                {
                    self.wide[2 * i + 1].store(person, Ordering::Relaxed);
                    return;
                }
                i = if i + 1 == self.cap { 0 } else { i + 1 };
            }
        }
    }
}

/// `Vec<AtomicU64>` as `Vec<u64>`: same size, alignment and bit validity.
fn into_plain(v: Vec<AtomicU64>) -> Vec<u64> {
    let mut v = std::mem::ManuallyDrop::new(v);
    unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut u64, v.len(), v.capacity()) }
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
    let t0 = std::time::Instant::now();
    let lap = |name: &str| if std::env::var_os("SESSIONS_VERBOSE").is_some() { eprintln!("    links.{name}: {:.1} ms", t0.elapsed().as_secs_f64() * 1e3) };
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

    lap("union");
    let per_thread = capacity.div_ceil(threads);
    let range = |t: usize| ((t * per_thread).min(capacity), ((t + 1) * per_thread).min(capacity));

    // Roots and linked IDs in each thread's slot range.
    let counts: Vec<(usize, usize)> = par_run(threads, |t| {
        let (lo, hi) = range(t);
        let (mut roots, mut linked) = (0usize, 0usize);
        for s in lo..hi {
            if builder.keys[s].load(Ordering::Relaxed) != 0 {
                linked += 1;
                roots += (builder.parent[s].load(Ordering::Relaxed) == 0) as usize;
            }
        }
        (roots, linked)
    });
    let mut first_person = vec![0usize; threads + 1];
    for t in 0..threads {
        first_person[t + 1] = first_person[t] + counts[t].0;
    }
    let persons = first_person[threads];
    let linked_ids: usize = counts.iter().map(|c| c.1).sum();
    assert!(persons < u32::MAX as usize, "too many linked persons");

    // Number the roots densely, in slot order.
    let person_of_root: Vec<AtomicU32> = unsafe { zeroed_vec(capacity) };
    par_run(threads, |t| {
        let (lo, hi) = range(t);
        let mut next = first_person[t] as u32;
        for s in lo..hi {
            if builder.is_root(s) {
                person_of_root[s].store(next, Ordering::Relaxed);
                next += 1;
            }
        }
    });
    lap("number");
    if std::env::var_os("SESSIONS_VERBOSE").is_some() {
        eprintln!("    links: {total} pairs, capacity {capacity}, {linked_ids} linked IDs, {persons} persons");
    }

    // One parallel pass fills the read-only table (load ~0.67) and names each
    // person by its byte-wise smallest ID: a CAS-min over `order_key + 1`
    // (0 = none yet) for generator IDs; the rare odd IDs are compared after.
    let cap = (linked_ids + linked_ids / 2).max(64);
    let wide = persons >= NARROW_PERSONS;
    let table = TableBuilder {
        narrow: if wide { Vec::new() } else { unsafe { zeroed_vec(cap) } },
        wide: if wide { unsafe { zeroed_vec(2 * cap) } } else { Vec::new() },
        cap,
    };
    let best: Vec<AtomicU64> = unsafe { zeroed_vec(persons) };
    let odd_members: Vec<Vec<(u32, u64)>> = par_run(threads, |t| {
        let (lo, hi) = range(t);
        let mut odd_members = Vec::new();
        for s in lo..hi {
            let key = builder.keys[s].load(Ordering::Relaxed);
            if key == 0 {
                continue;
            }
            let root = builder.find(s as u32) as usize;
            let person = person_of_root[root].load(Ordering::Relaxed);
            table.insert(key, person as u64);
            if key & ids::ODD != 0 {
                odd_members.push((person, key));
                continue;
            }
            let mine = ids::order_key(key) + 1;
            let cell = &best[person as usize];
            let mut current = cell.load(Ordering::Relaxed);
            while current == 0 || current > mine {
                match cell.compare_exchange_weak(current, mine, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
        odd_members
    });
    let mut names: Vec<u64> =
        best.iter().map(|b| match b.load(Ordering::Relaxed) { 0 => 0, o => ids::key_of_order(o - 1) }).collect();
    for &(person, key) in odd_members.iter().flatten() {
        let current = names[person as usize];
        if current == 0 || ids::cmp_ids(key, current, b'u', odd) == CmpOrdering::Less {
            names[person as usize] = key;
        }
    }
    debug_assert!(names.iter().all(|&n| n != 0));
    lap("fill");
    LinkTable { narrow: into_plain(table.narrow), wide: into_plain(table.wide), cap, names }

}
