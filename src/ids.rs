//! IDs as 64-bit keys.
//!
//! Every ID the generator writes is one prefix letter and 1 to 8 base-36
//! digits (`e…`, `u…`, `k…`). Those become a "fast" key: digit count in bits
//! 42..46 and the digits' base-36 value below. Any other string (upper-case
//! variants, spaces, carriage returns, anything) is interned and becomes an
//! "odd" key: bit 46 plus its index in the interner. Both forms are
//! injective, so two IDs are equal exactly when their keys are equal, and
//! every key turns back into its exact original bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub const ODD: u64 = 1 << 46;
/// Keys use the low 47 bits.
pub const KEY_BITS: u32 = 47;
pub const KEY_MASK: u64 = (1 << KEY_BITS) - 1;
const VALUE_MASK: u64 = (1 << 42) - 1;

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
    (((n - 1) as u64) << 42) | value
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
    let len = ((key >> 42) & 15) as usize;
    let mut value = key & VALUE_MASK;
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

/// Byte-wise order of the two IDs `a` and `b` (both written with `prefix`).
pub fn cmp_ids(a: u64, b: u64, prefix: u8, odd: &[Vec<u8>]) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    if a & ODD == 0 && b & ODD == 0 {
        // Same prefix: compare the digit strings. Padding each with '0' (the
        // smallest digit) to 8 digits keeps lexicographic order; on a tie the
        // shorter string is a prefix of the longer one and sorts first.
        let (la, lb) = ((a >> 42) & 15, (b >> 42) & 15);
        let pa = (a & VALUE_MASK) * POW36[(8 - la) as usize];
        let pb = (b & VALUE_MASK) * POW36[(8 - lb) as usize];
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
            b"u", b"uA", b"u123456789", b"e1", b"\tB", b"u0a", b"u0",
        ];
        let keys: Vec<u64> = ids.iter().map(|s| key_of(s, b'u', &interner)).collect();
        let odd = interner.snapshot();
        for (s, &k) in ids.iter().zip(&keys) {
            let mut out = Vec::new();
            write_id(&mut out, k, b'u', &odd);
            assert_eq!(&out[..], *s);
        }
        for (i, &a) in keys.iter().enumerate() {
            for (j, &b) in keys.iter().enumerate() {
                assert_eq!(cmp_ids(a, b, b'u', &odd), ids[i].cmp(ids[j]), "{:?} {:?}", ids[i], ids[j]);
                assert_eq!(a == b, ids[i] == ids[j]);
            }
        }
    }
}
