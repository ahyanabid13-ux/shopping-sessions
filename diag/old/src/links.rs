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

/// Words per bucket: one 64-byte cache line.
const LINE_WORDS: usize = 8;

/// Read-only map from user key to person: linear probing over 64-byte
/// buckets at load <= 1/2. A key's probe starts at the first slot of its home
/// bucket, so a lookup almost always touches the one cache line that was
/// prefetched for it. Narrow slots (one word) pack the compact key and the
/// person; wide slots (two words: key, person) are used only when there are
/// too many persons for that.
pub struct LinkTable {
    words: Vec<u64>,
    /// Words skipped so that bucket 0 starts on a cache line.
    off: usize,
    wide: bool,
    buckets: usize,
    /// Name (the byte-wise smallest user key) of each linked person.
    pub names: Vec<u64>,
}

/// First word of the home bucket of `ck`.
#[inline(always)]
fn home_of(ck: u64, buckets: usize) -> usize {
    // Fast range reduction of the hash's top 32 bits onto 0..buckets.
    ((((fib(ck) >> 32) * buckets as u64) >> 32) as usize) * LINE_WORDS
}

/// Buckets for `ids` keys at load <= 1/2.
fn buckets_for(ids: usize, wide: bool) -> usize {
    let per_bucket = if wide { LINE_WORDS / 2 } else { LINE_WORDS };
    (2 * ids).div_ceil(per_bucket).max(1)
}

/// Offset (in words) of the first 64-byte boundary in `v`.
fn line_offset(v: &[u64]) -> usize {
    v.as_ptr().align_offset(64)
}

impl LinkTable {
    #[inline(always)]
    pub fn prefetch(&self, key: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let h = home_of(compact(key), self.buckets);
            let p = self.words.as_ptr().add(self.off + h) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }

    /// Person key of a user key.
    #[inline(always)]
    pub fn person_of(&self, key: u64) -> u64 {
        let ck = compact(key);
        let total = self.buckets * LINE_WORDS;
        let mut i = home_of(ck, self.buckets);
        let base = unsafe { self.words.as_ptr().add(self.off) };
        if !self.wide {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                use std::arch::x86_64::*;
                // A whole bucket at once: the key's slot, or an empty slot
                // that ends the probe, is almost always in the home bucket.
                let km = _mm256_set1_epi64x(KEY43 as i64);
                let want = _mm256_set1_epi64x(ck as i64);
                let zero = _mm256_setzero_si256();
                loop {
                    let line = base.add(i) as *const __m256i;
                    let a = _mm256_load_si256(line);
                    let b = _mm256_load_si256(line.add(1));
                    let hit = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(_mm256_and_si256(a, km), want)))
                        | _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(_mm256_and_si256(b, km), want))) << 4;
                    if hit != 0 {
                        return LINKED | *base.add(i + hit.trailing_zeros() as usize) >> 43;
                    }
                    let empty = _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(a, zero)))
                        | _mm256_movemask_pd(_mm256_castsi256_pd(_mm256_cmpeq_epi64(b, zero)));
                    if empty != 0 {
                        return key;
                    }
                    i += LINE_WORDS;
                    if i == total {
                        i = 0;
                    }
                }
            }
        }
        loop {
            let k = unsafe { *base.add(i) };
            if k == ck {
                return LINKED | unsafe { *base.add(i + 1) };
            }
            if k == 0 {
                return key;
            }
            i += 2;
            if i == total {
                i = 0;
            }
        }
    }
}

/// The table while it is being filled by every thread at once. Each key is
/// inserted exactly once, so a slot only ever goes from empty to full, and
/// every slot a probe passes stays full: a lookup that meets an empty slot
/// can stop.
struct TableBuilder {
    words: Vec<AtomicU64>,
    off: usize,
    wide: bool,
    buckets: usize,
}

impl TableBuilder {
    fn new(ids: usize, wide: bool) -> TableBuilder {
        let buckets = buckets_for(ids, wide);
        let words: Vec<AtomicU64> = unsafe { zeroed_vec(buckets * LINE_WORDS + LINE_WORDS) };
        let off = words.as_ptr().align_offset(64);
        assert!(off < LINE_WORDS);
        TableBuilder { words, off, wide, buckets }
    }

    fn insert(&self, key: u64, person: u64) {
        let ck = compact(key);
        let total = self.buckets * LINE_WORDS;
        let mut i = home_of(ck, self.buckets);
        let (step, value) = if self.wide { (2, ck) } else { (1, ck | person << 43) };
        loop {
            let slot = &self.words[self.off + i];
            if slot.load(Ordering::Relaxed) == 0
                && slot.compare_exchange(0, value, Ordering::Relaxed, Ordering::Relaxed).is_ok()
            {
                if self.wide {
                    self.words[self.off + i + 1].store(person, Ordering::Relaxed);
                }
                return;
            }
            i += step;
            if i == total {
                i = 0;
            }
        }
    }

    fn finish(self, names: Vec<u64>) -> LinkTable {
        let words = into_plain(self.words);
        debug_assert_eq!(line_offset(&words), self.off);
        LinkTable { words, off: self.off, wide: self.wide, buckets: self.buckets, names }
    }
}

/// `Vec<AtomicU64>` as `Vec<u64>`: same size, alignment and bit validity.
fn into_plain(v: Vec<AtomicU64>) -> Vec<u64> {
    let mut v = std::mem::ManuallyDrop::new(v);
    unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut u64, v.len(), v.capacity()) }
}

/// Marks a numbered root's parent word (see `build`).
const ROOT: u32 = 1 << 31;

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
            let home = take(fib(key), 0, self.bits);
            std::arch::x86_64::_mm_prefetch(self.keys.as_ptr().add(home) as *const i8, std::arch::x86_64::_MM_HINT_T0);
            std::arch::x86_64::_mm_prefetch(self.parent.as_ptr().add(home) as *const i8, std::arch::x86_64::_MM_HINT_T0);
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

    /// Person number of slot `x`, once roots carry ROOT | person.
    #[inline]
    fn person_of(&self, mut x: u32) -> u32 {
        loop {
            let p = self.parent[x as usize].load(Ordering::Relaxed);
            if p & ROOT != 0 {
                return p & !ROOT;
            }
            x = p - 1;
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
    // At most 2 IDs per pair: keep the worst-case load at or below 0.7.
    let capacity = (total * 20 / 7 + 1).max(1024).next_power_of_two();
    assert!(capacity < ROOT as usize, "too many linked IDs");
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

    // Number the roots densely, in slot order. A root's parent word becomes
    // ROOT | its person number (the union-find is finished).
    par_run(threads, |t| {
        let (lo, hi) = range(t);
        let mut next = first_person[t] as u32;
        for s in lo..hi {
            if builder.is_root(s) {
                builder.parent[s].store(ROOT | next, Ordering::Relaxed);
                next += 1;
            }
        }
    });
    lap("number");
    if std::env::var_os("SESSIONS_VERBOSE").is_some() {
        eprintln!("    links: {total} pairs, capacity {capacity}, {linked_ids} linked IDs, {persons} persons");
    }

    // One parallel pass fills the read-only table and names each
    // person by its byte-wise smallest ID: a CAS-min over `order_key + 1`
    // (0 = none yet) for generator IDs; the rare odd IDs are compared after.
    let wide = persons >= NARROW_PERSONS;
    let table = TableBuilder::new(linked_ids, wide);
    let best: Vec<AtomicU64> = unsafe { zeroed_vec(persons) };
    let odd_members: Vec<Vec<(u32, u64)>> = par_run(threads, |t| {
        let (lo, hi) = range(t);
        let mut odd_members = Vec::new();
        for s in lo..hi {
            let key = builder.keys[s].load(Ordering::Relaxed);
            if key == 0 {
                continue;
            }
            let person = builder.person_of(s as u32);
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
    table.finish(names)

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn check(wide: bool, n: usize, seed: u64) {
        let mut x = seed;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut truth: HashMap<u64, u64> = HashMap::new();
        while truth.len() < n {
            let key = next() % ((1 << 43) - 1) + 1;
            let person = next() % if wide { 1 << 40 } else { NARROW_PERSONS as u64 };
            truth.entry(key).or_insert(person);
        }
        let builder = TableBuilder::new(n, wide);
        for (&k, &p) in &truth {
            builder.insert(k, p);
        }
        let table = builder.finish(Vec::new());
        for (&k, &p) in &truth {
            assert_eq!(table.person_of(k), LINKED | p);
        }
        for _ in 0..n * 4 {
            let key = next() % ((1 << 43) - 1) + 1;
            if !truth.contains_key(&key) {
                assert_eq!(table.person_of(key), key);
            }
        }
    }

    #[test]
    fn table_matches_hashmap() {
        for (i, n) in [1usize, 2, 7, 8, 9, 100, 4096, 100_000].into_iter().enumerate() {
            check(false, n, 0x9E37_79B9 + i as u64);
            check(true, n, 0x51ED_2701 + i as u64);
        }
    }
}
