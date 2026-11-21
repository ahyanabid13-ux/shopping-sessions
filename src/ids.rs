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
use std::collections::HashMap;
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
pub struct Interner {
    inner: Mutex<InternerInner>,
}

struct InternerInner {
    index: HashMap<Vec<u8>, u64>,
    strings: Vec<Vec<u8>>,
}

impl Interner {
    pub fn new() -> Self {
        Interner { inner: Mutex::new(InternerInner { index: HashMap::new(), strings: Vec::new() }) }
    }

    /// The odd key of `b` (bit 46 set).
    pub fn intern(&self, b: &[u8]) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&id) = inner.index.get(b) {
            return ODD | id;
        }
        let id = inner.strings.len() as u64;
        assert!(id < ODD, "too many distinct odd IDs");
        inner.strings.push(b.to_vec());
        inner.index.insert(b.to_vec(), id);
        ODD | id
    }

    /// A frozen copy of every odd string, indexed by odd key.
    pub fn snapshot(&self) -> Vec<Vec<u8>> {
        self.inner.lock().unwrap().strings.clone()
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
    let mut sa = Vec::with_capacity(16);
    let mut sb = Vec::with_capacity(16);
    write_id(&mut sa, a, prefix, odd);
    write_id(&mut sb, b, prefix, odd);
    sa.cmp(&sb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_order() {
        let interner = Interner::new();
        let ids: Vec<&[u8]> = vec![
            b"u1", b"u10", b"u9", b"u0", b"u00", b"u000a", b"uzzzzzzzz", b"u00000000", b"U1", b"u1 ", b" u1",
            b"u", b"uA", b"u123456789", b"e1", b"\tB", b"u0a", b"u0", b"uzzzzzzz", b"u0000000", b"u10000000",
            b"uz", b"u00", b"u0z",
        ];
        let keys: Vec<u64> = ids.iter().map(|s| key_of(s, b'u', &interner)).collect();
        let odd = interner.snapshot();
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
