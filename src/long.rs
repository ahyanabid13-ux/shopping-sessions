//! A lock-free exact interner for long user IDs (`u` + 32 hex digits, held
//! as their 128-bit value (hi, lo)).
//!
//! Open addressing over 32-byte slots, two per 64-byte bucket. A slot's `lo`
//! word is claimed with a CAS (0 = empty) and its `hi` word published right
//! after; slots only ever go from empty to full and never move. So "found",
//! "inserted at the first empty slot" and "probe window full without the key"
//! are stable facts: one ID gets the same answer from every thread, always.
//! IDs with a zero half, and IDs whose window is full, get None: the caller
//! then interns their exact bytes with the mutex interner instead.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::util::{fib, take, zeroed_vec};

#[repr(C)]
struct Slot {
    lo: AtomicU64,
    hi: AtomicU64,
    /// Person key of a linked ID (`LINKED | person`), 0 = never linked.
    person: AtomicU64,
    _pad: AtomicU64,
}

#[repr(C, align(64))]
struct Bucket([Slot; 2]);

/// Slots a probe may visit before giving up.
const WINDOW: usize = 16;

pub struct LongTable {
    buckets: Vec<Bucket>,
    /// log2 of the bucket count.
    bits: u32,
}

impl LongTable {
    /// A table of at least `slots_hint` slots (and at least 512 buckets).
    pub fn new(slots_hint: usize) -> LongTable {
        let count = slots_hint.div_ceil(2).next_power_of_two().max(512);
        let buckets: Vec<Bucket> = unsafe { zeroed_vec(count) };
        LongTable { buckets, bits: count.trailing_zeros() }
    }

    #[inline(always)]
    fn home(&self, hi: u64, lo: u64) -> usize {
        take(fib(lo ^ fib(hi)), 0, self.bits)
    }

    #[inline(always)]
    fn at(&self, slot: usize) -> &Slot {
        unsafe { self.buckets.get_unchecked(slot >> 1).0.get_unchecked(slot & 1) }
    }

    #[inline(always)]
    pub fn prefetch(&self, hi: u64, lo: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            let p = self.buckets.as_ptr().add(self.home(hi, lo)) as *const i8;
            std::arch::x86_64::_mm_prefetch(p, std::arch::x86_64::_MM_HINT_T0);
        }
    }

    /// Slot index of (hi, lo), inserting it when absent. None when hi == 0 || lo == 0, or when the
    /// 16-slot probe window starting at the first slot of the home bucket holds neither the key nor
    /// an empty slot.
    #[inline(always)]
    pub fn slot(&self, hi: u64, lo: u64) -> Option<usize> {
        if hi == 0 || lo == 0 {
            return None;
        }
        let mask = (self.buckets.len() << 1) - 1;
        let mut i = self.home(hi, lo) << 1;
        for _ in 0..WINDOW {
            let s = self.at(i);
            let mut cur = s.lo.load(Ordering::Acquire);
            if cur == 0 {
                match s.lo.compare_exchange(0, lo, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        s.hi.store(hi, Ordering::Release);
                        return Some(i);
                    }
                    Err(actual) => cur = actual,
                }
            }
            if cur == lo {
                let h = Self::published_hi(s);
                if h == hi {
                    return Some(i);
                }
            }
            i = (i + 1) & mask;
        }
        None
    }

    /// The `hi` word of a claimed slot, waiting while its writer is between its two stores.
    #[inline(always)]
    fn published_hi(s: &Slot) -> u64 {
        let mut h = s.hi.load(Ordering::Acquire);
        let mut spins = 0u32;
        while h == 0 {
            std::hint::spin_loop();
            spins += 1;
            if spins % 100 == 0 {
                std::thread::yield_now();
            }
            h = s.hi.load(Ordering::Acquire);
        }
        h
    }

    /// Person key of a slot's ID; 0 = never linked.
    #[inline(always)]
    pub fn person(&self, slot: usize) -> u64 {
        self.at(slot).person.load(Ordering::Relaxed)
    }

    pub fn set_person(&self, slot: usize, person: u64) {
        self.at(slot).person.store(person, Ordering::Relaxed);
    }

    /// (hi, lo) of a slot's ID.
    pub fn value(&self, slot: usize) -> (u64, u64) {
        let s = self.at(slot);
        (Self::published_hi(s), s.lo.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn rng(mut x: u64) -> impl FnMut() -> u64 {
        move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        }
    }

    #[test]
    fn concurrent_inserts_match_hashmap() {
        let table = LongTable::new(200_000);
        let mut next = rng(77);
        let keys: Vec<(u64, u64)> = (0..30_000).map(|_| (next() | 1, next() | 1)).collect();
        let threads = 4;
        let results: Vec<Vec<((u64, u64), usize)>> = crate::util::par_run(threads, |t| {
            let mut r = rng(1000 + t as u64);
            let mut out = Vec::new();
            for _ in 0..60_000 {
                let k = keys[(r() % keys.len() as u64) as usize];
                out.push((k, table.slot(k.0, k.1).expect("window full at low load")));
            }
            // Every thread also visits every key once, in its own order.
            for i in 0..keys.len() {
                let k = keys[(i * 7919 + t * 31) % keys.len()];
                out.push((k, table.slot(k.0, k.1).unwrap()));
            }
            out
        });
        let mut slot_of: HashMap<(u64, u64), usize> = HashMap::new();
        let mut key_of: HashMap<usize, (u64, u64)> = HashMap::new();
        for &(k, s) in results.iter().flatten() {
            assert_eq!(*slot_of.entry(k).or_insert(s), s, "one key, two slots");
            assert_eq!(*key_of.entry(s).or_insert(k), k, "one slot, two keys");
            assert_eq!(table.value(s), k);
        }
        assert_eq!(slot_of.len(), keys.len());
    }

    #[test]
    fn shared_halves_are_distinct() {
        let table = LongTable::new(64);
        let mut seen = HashMap::new();
        for a in 1..20u64 {
            for b in 1..4u64 {
                for k in [(a, 0xABCD), (0xABCD, a), (a, b), (b, a)] {
                    let s = table.slot(k.0, k.1).unwrap();
                    assert_eq!(*seen.entry(k).or_insert(s), s);
                    assert_eq!(table.value(s), k);
                }
            }
        }
        let slots: std::collections::HashSet<usize> = seen.values().copied().collect();
        assert_eq!(slots.len(), seen.len());
    }

    #[test]
    fn full_windows_refuse_consistently() {
        let table = LongTable::new(1); // 512 buckets, 1024 slots
        let mut next = rng(5);
        let keys: Vec<(u64, u64)> = (0..5000).map(|_| (next() | 1, next() | 1)).collect();
        let first: Vec<Option<usize>> = keys.iter().map(|k| table.slot(k.0, k.1)).collect();
        assert!(first.iter().any(|s| s.is_none()));
        assert!(first.iter().filter(|s| s.is_some()).count() <= 1024);
        for _ in 0..2 {
            for (k, s) in keys.iter().zip(&first) {
                assert_eq!(table.slot(k.0, k.1), *s);
                if let Some(s) = s {
                    assert_eq!(table.value(*s), *k);
                }
            }
        }
    }

    #[test]
    fn zero_halves_are_refused() {
        let table = LongTable::new(16);
        assert_eq!(table.slot(0, 5), None);
        assert_eq!(table.slot(5, 0), None);
        assert_eq!(table.slot(0, 0), None);
        assert!(table.slot(5, 5).is_some());
    }
}
