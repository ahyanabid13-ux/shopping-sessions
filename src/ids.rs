//! IDs as 64-bit keys.
//!
//! Every ID the generator writes is one prefix letter and 1 to 8 base-36
//! digits (`e…`, `u…`, `k…`). Those become a "fast" key: a dense number over
//! all digit strings (every 1-digit string, then every 2-digit one, ...)
//! plus one, so it is never 0. Any other string (upper-case variants,
//! spaces, carriage returns, anything) is interned and becomes an "odd" key:
//! bit 42 plus its index in the interner. Both forms are injective, below
//! 2^43, and turn back into the exact original bytes.

use std::cmp::Ordering;
use std::sync::Mutex;

pub const ODD: u64 = 1 << 42;
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
/// Interns IDs that are not "fast" keys. Since scorer V5, one in eight
/// distinct IDs is a prefix and 32 hex characters, so this is a hot path.
///
/// 64 shards, each behind its own lock, chosen by a fast hash. Each shard
/// appends every distinct string once to a byte arena (4-byte length, then
/// the bytes) and indexes it with an open-addressing table of
/// `fingerprint << 36 | offset` words (0 = empty; offset 0 is a dummy byte).
/// The key is `ODD | shard << 36 | offset`: exact, unique, and it locates the
/// bytes directly, so nothing is copied for readers (see `OddView`).
pub struct Interner {
    shards: Vec<Mutex<Shard>>,
}

const SHARDS: usize = 64;
const OFF_BITS: u32 = 36;
const OFF_MASK: u64 = (1 << OFF_BITS) - 1;

struct Shard {
    table: Vec<u64>,
    count: usize,
    arena: Vec<u8>,
}

/// Fast non-cryptographic hash of an ID (multiply-rotate over 8-byte words,
/// then an avalanche). Inputs are dataset IDs; no adversarial concern.
#[inline]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h = bytes.len() as u64;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        h = (h.rotate_left(5) ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(K);
    }
    let rest = chunks.remainder();
    let mut tail = [0u8; 8];
    tail[..rest.len()].copy_from_slice(rest);
    h = (h.rotate_left(5) ^ u64::from_le_bytes(tail)).wrapping_mul(K);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

impl Shard {
    #[inline]
    fn bytes_at(&self, off: u64) -> &[u8] {
        let o = off as usize;
        let len = u32::from_le_bytes(self.arena[o..o + 4].try_into().unwrap()) as usize;
        &self.arena[o + 4..o + 4 + len]
    }

    fn grow(&mut self) {
        let cap = self.table.len() * 2;
        let mut table = vec![0u64; cap];
        let mask = cap - 1;
        for &e in &self.table {
            if e != 0 {
                let h = hash_bytes(self.bytes_at(e & OFF_MASK));
                let mut i = h as usize & mask;
                while table[i] != 0 {
                    i = (i + 1) & mask;
                }
                table[i] = e;
            }
        }
        self.table = table;
    }
}

impl Interner {
    /// `expected`: a rough count of distinct odd IDs, to size the tables.
    pub fn new(expected: usize) -> Self {
        let per = ((expected / SHARDS) * 2).max(64).next_power_of_two();
        Interner {
            shards: (0..SHARDS)
                .map(|_| Mutex::new(Shard { table: vec![0; per], count: 0, arena: vec![0u8] }))
                .collect(),
        }
    }

    /// The odd key of `b` (bit 42 set).
    pub fn intern(&self, b: &[u8]) -> u64 {
        let h = hash_bytes(b);
        let shard = (h >> 58) as usize % SHARDS;
        let fp = (h >> 30) & ((1 << 28) - 1);
        let mut sh = self.shards[shard].lock().unwrap();
        let mask = sh.table.len() - 1;
        let mut i = h as usize & mask;
        loop {
            let e = sh.table[i];
            if e == 0 {
                break;
            }
            if e >> OFF_BITS == fp && sh.bytes_at(e & OFF_MASK) == b {
                return ODD | (shard as u64) << OFF_BITS | (e & OFF_MASK);
            }
            i = (i + 1) & mask;
        }
        let off = sh.arena.len() as u64;
        assert!(off + 4 + b.len() as u64 <= OFF_MASK, "odd-ID arena full");
        sh.arena.extend_from_slice(&(b.len() as u32).to_le_bytes());
        sh.arena.extend_from_slice(b);
        sh.table[i] = fp << OFF_BITS | off;
        sh.count += 1;
        if sh.count * 2 > sh.table.len() {
            sh.grow();
        }
        ODD | (shard as u64) << OFF_BITS | off
    }

    /// Read access to every interned string. Holds every shard's lock, so
    /// take it only while no thread interns (between phases), and drop it
    /// before interning again.
    pub fn view(&self) -> OddView<'_> {
        OddView { shards: self.shards.iter().map(|m| m.lock().unwrap()).collect() }
    }
}

/// The bytes behind odd keys (see `Interner::view`).
pub struct OddView<'a> {
    shards: Vec<std::sync::MutexGuard<'a, Shard>>,
}

impl OddView<'_> {
    #[inline]
    pub fn get(&self, key: u64) -> &[u8] {
        let shard = ((key >> OFF_BITS) as usize) & (SHARDS - 1);
        self.shards[shard].bytes_at(key & OFF_MASK)
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
pub fn write_id(out: &mut Vec<u8>, key: u64, prefix: u8, odd: &OddView) {
    if key & ODD != 0 {
        out.extend_from_slice(odd.get(key));
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

/// Byte-wise order of the two IDs `a` and `b` (both written with `prefix`).
pub fn cmp_ids(a: u64, b: u64, prefix: u8, odd: &OddView) -> Ordering {
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
fn id_bytes<'a>(key: u64, prefix: u8, odd: &'a OddView, buf: &'a mut [u8; 9]) -> &'a [u8] {
    if key & ODD != 0 {
        return odd.get(key);
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
    fn interner_exact_under_growth() {
        let interner = Interner::new(1); // tiny: forces many rehashes
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut strings: Vec<Vec<u8>> = Vec::new();
        for i in 0..300_000u64 {
            let s = if i % 3 == 0 {
                // V5-style: prefix + 32 hex characters.
                let mut v = vec![b'u'];
                v.extend(format!("{:016x}{:016x}", rnd(), rnd()).bytes());
                v
            } else {
                let n = (rnd() % 40) as usize;
                (0..n).map(|_| [b'a', b'Z', b' ', 0xC3, 0xA9, b'\r', b'0', b'k'][(rnd() % 8) as usize]).collect()
            };
            strings.push(s);
        }
        // Repeats: intern a random earlier string again.
        let mut keys = Vec::new();
        for (i, s) in strings.iter().enumerate() {
            keys.push(interner.intern(s));
            if i % 7 == 0 {
                let j = (rnd() % (i as u64 + 1)) as usize;
                assert_eq!(interner.intern(&strings[j]), keys[j], "stable key");
            }
        }
        let view = interner.view();
        for (s, &k) in strings.iter().zip(&keys) {
            assert!(k & ODD != 0 && k < (1 << KEY_BITS));
            assert_eq!(view.get(k), &s[..], "round trip");
        }
        let mut by_key = std::collections::HashMap::new();
        for (s, &k) in strings.iter().zip(&keys) {
            if let Some(prev) = by_key.insert(k, s.clone()) {
                assert_eq!(&prev, s, "two strings share a key");
            }
        }
        let distinct: std::collections::HashSet<&Vec<u8>> = strings.iter().collect();
        assert_eq!(by_key.len(), distinct.len(), "one key per distinct string");
    }

    #[test]
    fn roundtrip_and_order() {
        let interner = Interner::new(4);
        let ids: Vec<&[u8]> = vec![
            b"u1", b"u10", b"u9", b"u0", b"u00", b"u000a", b"uzzzzzzzz", b"u00000000", b"U1", b"u1 ", b" u1",
            b"u", b"uA", b"u123456789", b"e1", b"\tB", b"u0a", b"u0", b"uzzzzzzz", b"u0000000", b"u10000000",
            b"uz", b"u00", b"u0z",
        ];
        let keys: Vec<u64> = ids.iter().map(|s| key_of(s, b'u', &interner)).collect();
        let odd = interner.view();
        for (s, &k) in ids.iter().zip(&keys) {
            let mut out = Vec::new();
            write_id(&mut out, k, b'u', &odd);
            assert_eq!(&out[..], *s);
            if k & ODD == 0 {
                assert_eq!(key_of_order(order_key(k)), k);
            }
        }
        for (i, &a) in keys.iter().enumerate() {
            for (j, &b) in keys.iter().enumerate() {
                assert_eq!(cmp_ids(a, b, b'u', &odd), ids[i].cmp(ids[j]), "{:?} {:?}", ids[i], ids[j]);
                assert_eq!(a == b, ids[i] == ids[j]);
                if a & ODD == 0 && b & ODD == 0 {
                    assert_eq!(order_key(a).cmp(&order_key(b)), ids[i].cmp(ids[j]));
                }
            }
        }
    }
}
