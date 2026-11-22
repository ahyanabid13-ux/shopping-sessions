//! IDs as 64-bit keys.
//!
//! Every ID the generator writes is one prefix letter and 1 to 8 base-36
//! digits (`e…`, `u…`, `k…`). Those become a "fast" key: a dense number over
//! all digit strings (every 1-digit string, then every 2-digit one, ...)
//! plus one, so it is never 0. Any other string (upper-case variants,
//! spaces, carriage returns, anything) is interned and becomes an "odd" key:
//! bit 42 plus its index in the interner. A user ID that is `u` + exactly 32
//! lowercase hex digits (a "long" ID) gets a slot in the lock-free
//! `LongTable` instead (long.rs) and becomes bits 42 and 41 plus that slot
//! (event and click IDs of that shape are carried as their 128-bit value).
//! All forms are injective, below 2^43, and turn back into the exact bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;

use crate::long::LongTable;

pub const ODD: u64 = 1 << 42;
/// With ODD: a long user ID, whose low 41 bits are its `LongTable` slot.
pub const LONG: u64 = 1 << 41;
/// Keys use the low 43 bits.
pub const KEY_BITS: u32 = 43;

/// OFFSET[d]: how many digit strings are shorter than d digits.
pub const OFFSET: [u64; 9] = [0, 0, 36, 1_332, 47_988, 1_727_604, 62_193_780, 2_238_976_116, 80_603_140_212];

static DIGIT_VALUE: [u8; 256] = {
    let mut table = [255u8; 256];
    let mut i = 0;
    while i < 10 {
        table[b'0' as usize + i] = i as u8;
        i += 1;
    }
    let mut j = 0;
    while j < 26 {
        table[b'a' as usize + j] = 10 + j as u8;
        j += 1;
    }
    table
};

/// The fast key of `digits` base-36 digits with value `value`.
#[inline(always)]
pub fn fast_from(digits: usize, value: u64) -> u64 {
    debug_assert!((1..=8).contains(&digits));
    unsafe { *OFFSET.get_unchecked(digits) + value + 1 }
}

/// (digit count, value) of a fast key.
#[inline]
fn fast_parts(key: u64) -> (usize, u64) {
    let k = key - 1;
    let mut d = 8;
    while OFFSET[d] > k {
        d -= 1;
    }
    (d, k - OFFSET[d])
}

/// The fast key of `b` when it is `prefix` and 1..=8 base-36 digits, else 0.
#[inline(always)]
pub fn fast_key(b: &[u8], prefix: u8) -> u64 {
    let n = b.len();
    if !(2..=9).contains(&n) || b[0] != prefix {
        return 0;
    }
    let mut value: u64 = 0;
    let mut bad: u8 = 0;
    for &c in &b[1..] {
        let d = DIGIT_VALUE[c as usize];
        bad |= d;
        value = value * 36 + d as u64;
    }
    // Any 255 entry sets the high bit of `bad`.
    if bad & 0x80 != 0 {
        return 0;
    }
    fast_from(n - 1, value)
}

/// Thread-safe store of the "odd" ID strings. Rare by construction.
/// Interns IDs that are not "fast" keys (since scorer V5, one in eight
/// distinct IDs carries a 32-character hex suffix, so this is a hot path).
///
/// 64 shards, each its own lock, chosen by a fast hash: parse threads rarely
/// contend. Keys come from one atomic counter, so they are unique and dense
/// and `snapshot()` keeps its flat layout (index = key without ODD). One heap
/// allocation per distinct string.
pub struct Interner {
    shards: Vec<Mutex<HashMap<Box<[u8]>, u64, FastBuild>>>,
    next: AtomicU64,
}

const SHARDS: usize = 64;

/// A fast, non-cryptographic hash (multiply-rotate over 8-byte words). The
/// inputs are IDs from the dataset; there is no adversarial-hashing concern.
#[derive(Default, Clone, Copy)]
pub struct FastBuild;

pub struct FastHasher(u64);

impl std::hash::BuildHasher for FastBuild {
    type Hasher = FastHasher;
    #[inline]
    fn build_hasher(&self) -> FastHasher {
        FastHasher(0)
    }
}

impl std::hash::Hasher for FastHasher {
    #[inline]
    fn finish(&self) -> u64 {
        // Final avalanche so the low bits (hashbrown's buckets) are mixed.
        let mut x = self.0;
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        const K: u64 = 0x517c_c1b7_2722_0a95;
        let mut h = self.0;
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            h = (h.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(K);
        }
        let rest = chunks.remainder();
        let mut tail = [0u8; 8];
        tail[..rest.len()].copy_from_slice(rest);
        h = (h.rotate_left(5) ^ u64::from_le_bytes(tail) ^ (bytes.len() as u64) << 56).wrapping_mul(K);
        self.0 = h;
    }
}

#[inline]
fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{BuildHasher, Hash, Hasher};
    let mut h = FastBuild.build_hasher();
    b.hash(&mut h);
    h.finish()
}

impl Interner {
    pub fn new() -> Self {
        Interner {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::with_hasher(FastBuild))).collect(),
            next: AtomicU64::new(0),
        }
    }

    /// The odd key of `b` (bit 42 set, bit 41 clear).
    pub fn intern(&self, b: &[u8]) -> u64 {
        let shard = (hash_bytes(b) >> 58) as usize % SHARDS;
        let mut map = self.shards[shard].lock().unwrap();
        if let Some(&id) = map.get(b) {
            return ODD | id;
        }
        let id = self.next.fetch_add(1, AtomicOrdering::Relaxed);
        assert!(id < LONG, "too many distinct odd IDs");
        map.insert(b.into(), id);
        ODD | id
    }

    /// A frozen copy of every odd string, indexed by odd key.
    pub fn snapshot(&self) -> Vec<Vec<u8>> {
        // Lock every shard first so the counter and the maps agree.
        let guards: Vec<_> = self.shards.iter().map(|m| m.lock().unwrap()).collect();
        let n = self.next.load(AtomicOrdering::Relaxed) as usize;
        let mut out = vec![Vec::new(); n];
        for g in &guards {
            for (bytes, &id) in g.iter() {
                out[id as usize] = bytes.to_vec();
            }
        }
        out
    }
}

/// Key of a non-empty ID.
#[inline(always)]
pub fn key_of(b: &[u8], prefix: u8, interner: &Interner) -> u64 {
    let k = fast_key(b, prefix);
    if k != 0 {
        k
    } else {
        interner.intern(b)
    }
}

/// Appends the exact bytes of `key` to `out`.
pub fn write_id(out: &mut Vec<u8>, key: u64, prefix: u8, odd: &[Vec<u8>]) {
    if key & ODD != 0 {
        out.extend_from_slice(&odd[(key & !ODD) as usize]);
        return;
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let (len, mut value) = fast_parts(key);
    let mut buf = [b'0'; 8];
    for slot in buf[..len].iter_mut().rev() {
        *slot = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    out.push(prefix);
    out.extend_from_slice(&buf[..len]);
}

const POW36: [u64; 9] = [
    1,
    36,
    1_296,
    46_656,
    1_679_616,
    60_466_176,
    2_176_782_336,
    78_364_164_096,
    2_821_109_907_456,
];

/// A number whose order is the byte-wise order of fast keys' IDs (same
/// prefix): the digits padded with '0' to 8, then the length. Padding with the
/// smallest digit keeps the order; equal padded values mean one string is a
/// prefix of the other, and the shorter sorts first. Below 2^46.
#[inline]
pub fn order_key(key: u64) -> u64 {
    debug_assert!(key & ODD == 0 && key != 0);
    let (len, value) = fast_parts(key);
    (value * POW36[8 - len]) << 4 | len as u64
}

/// The fast key whose `order_key` is `order`.
#[inline]
pub fn key_of_order(order: u64) -> u64 {
    let len = (order & 15) as usize;
    fast_from(len, (order >> 4) / POW36[8 - len])
}

/// Whether `key` is a long user key (`ODD | LONG | slot`).
#[inline(always)]
pub fn is_long(key: u64) -> bool {
    key & (ODD | LONG) == ODD | LONG
}

/// The coarse order key of a long ID whose first 16 hex digits are `hi`: its
/// first 8 digits read as base-36 digits, shifted like `order_key`, with 15
/// as the length. Against `order_key` of a fast ID it orders exactly like the
/// bytes (a tie in the padded value makes the short ID a prefix, and its
/// length <= 8 < 15); two long IDs with equal first 8 digits tie.
#[inline]
pub fn long_order(hi: u64) -> u64 {
    let mut p8 = 0u64;
    for i in 0..8 {
        p8 = p8 * 36 + (hi >> (60 - 4 * i) & 15);
    }
    p8 << 4 | 15
}

/// Appends `prefix` and the 32 hex digits of (hi, lo).
pub fn write_long(out: &mut Vec<u8>, prefix: u8, hi: u64, lo: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(prefix);
    for w in [hi, lo] {
        for i in 0..16 {
            out.push(HEX[(w >> (60 - 4 * i) & 15) as usize]);
        }
    }
}

/// Appends the exact bytes of the user key `key` (fast, odd or long).
pub fn write_user_id(out: &mut Vec<u8>, key: u64, odd: &[Vec<u8>], longs: &LongTable) {
    if is_long(key) {
        let (hi, lo) = longs.value((key & (LONG - 1)) as usize);
        write_long(out, b'u', hi, lo);
    } else {
        write_id(out, key, b'u', odd);
    }
}

/// Byte-wise order of the user IDs of keys `a` and `b`.
pub fn cmp_user_ids(a: u64, b: u64, odd: &[Vec<u8>], longs: &LongTable) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    if a & ODD == 0 && b & ODD == 0 {
        return cmp_ids(a, b, b'u', odd);
    }
    if is_long(a) && is_long(b) {
        return longs.value((a & (LONG - 1)) as usize).cmp(&longs.value((b & (LONG - 1)) as usize));
    }
    let mut sa = Vec::with_capacity(40);
    let mut sb = Vec::with_capacity(40);
    write_user_id(&mut sa, a, odd, longs);
    write_user_id(&mut sb, b, odd, longs);
    sa.cmp(&sb)
}

/// Byte-wise order of the two IDs `a` and `b` (both written with `prefix`).
pub fn cmp_ids(a: u64, b: u64, prefix: u8, odd: &[Vec<u8>]) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    if a & ODD == 0 && b & ODD == 0 {
        // Same prefix: compare the digit strings. Padding each with '0' (the
        // smallest digit) to 8 digits keeps lexicographic order; on a tie the
        // shorter string is a prefix of the longer one and sorts first.
        let ((la, va), (lb, vb)) = (fast_parts(a), fast_parts(b));
        let pa = va * POW36[8 - la];
        let pb = vb * POW36[8 - lb];
        return pa.cmp(&pb).then(la.cmp(&lb));
    }
    // At least one odd key: compare the bytes, without allocating (this is
    // hot in link naming since V5's long IDs). A fast key is at most 9 bytes.
    let mut ba = [0u8; 9];
    let mut bb = [0u8; 9];
    let sa = id_bytes(a, prefix, odd, &mut ba);
    let sb = id_bytes(b, prefix, odd, &mut bb);
    sa.cmp(sb)
}

/// The bytes of a fast or odd key; fast keys are spelled into `buf`.
#[inline]
fn id_bytes<'a>(key: u64, prefix: u8, odd: &'a [Vec<u8>], buf: &'a mut [u8; 9]) -> &'a [u8] {
    if key & ODD != 0 {
        return &odd[(key & !ODD) as usize];
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let (len, mut value) = fast_parts(key);
    buf[0] = prefix;
    for slot in buf[1..=len].iter_mut().rev() {
        *slot = DIGITS[(value % 36) as usize];
        value /= 36;
    }
    &buf[..=len]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_order() {
        let interner = Interner::new();
        let longs = LongTable::new(64);
        // `u` + head, padded with '0' to 33 bytes but ending in '1' (a zero
        // half would send the ID to the interner).
        let z = |head: &str| -> Vec<u8> {
            let mut v = format!("u{head}").into_bytes();
            if v.len() < 33 {
                v.resize(33, b'0');
                v[32] = b'1';
            }
            v
        };
        let mut ids: Vec<Vec<u8>> = [
            &b"u1"[..], b"u10", b"u9", b"u0", b"u00", b"u000a", b"uzzzzzzzz", b"u00000000", b"U1", b"u1 ", b" u1",
            b"u", b"uA", b"u123456789", b"e1", b"\tB", b"u0a", b"u0", b"uzzzzzzz", b"u0000000", b"u10000000",
            b"uz", b"u00", b"u0z", b"u0a1b",
        ]
        .iter()
        .map(|s| s.to_vec())
        .collect();
        ids.extend([
            z("0a1b"),
            z("0a1b0000000000000000000000000001"),
            z("0a1b2c3d"),
            z("0a1b2c3d1"),
            z("0a1b2c3d0000000000000000000000ff"),
            z("0a1b2c3e"),
            z("ffffffffffffffffffffffffffffffff"),
            z("f"),
            z("9"),
            z("1"),
            z("00000001"),
            z("0a1b2c3"),
            z("0000000000000000000000000000001"), // hi == 0: falls back to the interner
            z(""),                                // hi == 0: interner
            [&b"u"[..], &[b'0'; 32]].concat(),     // all zero: interner
            [&b"u12345678"[..], &[b'0'; 24]].concat(), // lo == 0: interner
            b"U0A1B0000000000000000000000000000".to_vec(),
            b"u0A1B0000000000000000000000000000".to_vec(),
            b"u0a1b00000000000000000000000000000".to_vec(),
            b"u0a1b2c3d".to_vec(),
            b"u0a1b2c3e".to_vec(),
            b"uf".to_vec(),
            b"ug".to_vec(),
        ]);
        let key_of_user = |s: &[u8]| -> u64 {
            if let Some((hi, lo)) = crate::fast::hex_key(s, 0, s.len(), b'u') {
                if let Some(slot) = longs.slot(hi, lo) {
                    return ODD | LONG | slot as u64;
                }
            }
            key_of(s, b'u', &interner)
        };
        let keys: Vec<u64> = ids.iter().map(|s| key_of_user(s)).collect();
        assert!(keys.iter().filter(|&&k| is_long(k)).count() >= 10);
        let odd = interner.snapshot();
        for (s, &k) in ids.iter().zip(&keys) {
            let mut out = Vec::new();
            write_user_id(&mut out, k, &odd, &longs);
            assert_eq!(&out[..], &s[..]);
            if k & ODD == 0 {
                out.clear();
                write_id(&mut out, k, b'u', &odd);
                assert_eq!(&out[..], &s[..]);
                assert_eq!(key_of_order(order_key(k)), k);
            }
        }
        // The order number of a fast key or the coarse one of a long key.
        let coarse = |k: u64| -> Option<u64> {
            if k & ODD == 0 {
                Some(order_key(k))
            } else if is_long(k) {
                Some(long_order(longs.value((k & (LONG - 1)) as usize).0))
            } else {
                None
            }
        };
        for (i, &a) in keys.iter().enumerate() {
            for (j, &b) in keys.iter().enumerate() {
                let want = ids[i].cmp(&ids[j]);
                assert_eq!(cmp_user_ids(a, b, &odd, &longs), want, "{:?} {:?}", ids[i], ids[j]);
                assert_eq!(a == b, ids[i] == ids[j]);
                if a & ODD == 0 && b & ODD == 0 {
                    assert_eq!(cmp_ids(a, b, b'u', &odd), want);
                    assert_eq!(order_key(a).cmp(&order_key(b)), want);
                }
                if let (Some(oa), Some(ob)) = (coarse(a), coarse(b)) {
                    if is_long(a) && is_long(b) && oa == ob {
                        continue; // decided by the full value
                    }
                    assert_eq!(oa.cmp(&ob), want, "{:?} {:?}", ids[i], ids[j]);
                }
            }
        }
    }
}
