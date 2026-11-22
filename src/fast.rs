//! The hot path: SIMD line splitting and field decoding for the big files.
//! Every function here has an exact scalar meaning in csv.rs, ids.rs or
//! ts.rs and falls back to it for anything outside the common shapes.

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

use crate::ts;

/// Splits the line starting at `start`. Fills the raw byte range of the
/// first `N` fields into `fs`/`fe` and returns (line end, field count,
/// quote count). The line ends at its `\n` or at the end of `data`.
#[inline(always)]
pub fn scan<const N: usize>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N]) -> (usize, usize, u32) {
    let len = data.len();
    let mut fields = 0usize;
    let mut cur = start;
    let mut quotes = 0u32;
    let mut pos = start;
    macro_rules! field_end {
        ($at:expr) => {{
            let at = $at;
            if fields < N {
                fs[fields] = cur;
                fe[fields] = at;
            }
            fields += 1;
            cur = at + 1;
        }};
    }
    unsafe {
        let p = data.as_ptr();
        let comma = _mm256_set1_epi8(b',' as i8);
        let newline = _mm256_set1_epi8(b'\n' as i8);
        let quote = _mm256_set1_epi8(b'"' as i8);
        while pos + 32 <= len {
            let v = _mm256_loadu_si256(p.add(pos) as *const __m256i);
            let nl = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, newline)) as u32;
            let mut cm = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, comma)) as u32;
            let mut qm = _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, quote)) as u32;
            if nl != 0 {
                let lim = nl.trailing_zeros();
                let keep = (1u32 << lim).wrapping_sub(1);
                cm &= keep;
                qm &= keep;
                quotes += qm.count_ones();
                while cm != 0 {
                    field_end!(pos + cm.trailing_zeros() as usize);
                    cm &= cm - 1;
                }
                let end = pos + lim as usize;
                field_end!(end);
                return (end, fields, quotes);
            }
            quotes += qm.count_ones();
            while cm != 0 {
                field_end!(pos + cm.trailing_zeros() as usize);
                cm &= cm - 1;
            }
            pos += 32;
        }
    }
    while pos < len {
        match data[pos] {
            b'\n' => break,
            b',' => field_end!(pos),
            b'"' => quotes += 1,
            _ => {}
        }
        pos += 1;
    }
    field_end!(pos);
    (pos, fields, quotes)
}

#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let comma = _mm256_set1_epi8(b',' as i8);
    let quote = _mm256_set1_epi8(b'"' as i8);
    let m = |v: __m256i, c: __m256i| _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, c)) as u32 as u64;
    (m(a, newline) | m(b, newline) << 32, m(a, comma) | m(b, comma) << 32, m(a, quote) | m(b, quote) << 32)
}

/// Outcome of `scan_window`.
pub enum Window {
    /// The line ends at `end` (its `\n`); `ok` when it has exactly N fields
    /// whose quotes are all wrapping pairs (fields are then unquoted).
    Line { end: usize, ok: bool },
    /// The line is 128 bytes or longer: use `scan`.
    Long,
}

/// Splits the line at `start` when it fits in the 128-byte window
/// `data[start..start + 128]`, which the caller guarantees is readable.
///
/// Newline, comma and quote masks cover the window as one 128-bit mask
/// each. The commas are visited in order with tzcnt / clear-lowest-bit
/// (fast on every x86-64 CPU, unlike pdep/pext on AMD Zen 1 and 2). A field
/// is wrapped when its first and last bytes are quotes and it is at least
/// two bytes long; the line is well formed when every quote is one end of a
/// wrapping pair, i.e. quotes == 2 x wrapped fields.
///
/// A line whose first field is empty reports `ok == false`: such a line is
/// dropped by every caller (empty event_id, click_id or link side).
#[inline(always)]
fn scan_window_tz<const N: usize>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N]) -> Window {
    debug_assert!(start + 128 <= data.len());
    unsafe {
        let p = data.as_ptr().add(start);
        let (nl0, cm0, q0) = masks64(p);
        let (nl1, cm1, q1) = masks64(p.add(64));
        if nl0 | nl1 == 0 {
            return Window::Long;
        }
        let e = if nl0 != 0 { nl0.trailing_zeros() } else { 64 + nl1.trailing_zeros() };
        let end = start + e as usize;
        // Keep bits below the newline (bzhi leaves everything for index >= 64).
        let k0 = _bzhi_u64(u64::MAX, e);
        let k1 = _bzhi_u64(u64::MAX, e.saturating_sub(64));
        let (c0, c1, q0, q1) = (cm0 & k0, cm1 & k1, q0 & k0, q1 & k1);
        if c0.count_ones() + c1.count_ones() != (N - 1) as u32 || c0 & 1 != 0 || e == 0 {
            return Window::Line { end, ok: false };
        }
        let mut rest = c0 as u128 | (c1 as u128) << 64;
        let q = q0 as u128 | (q1 as u128) << 64;
        let mut cur = 0u32;
        let mut wrapped = 0u32;
        for i in 0..N {
            let at = if i + 1 < N {
                let b = rest.trailing_zeros();
                rest &= rest - 1;
                b
            } else {
                e
            };
            // Field bytes are [cur, at); at >= cur + 1 > 0 except for an
            // empty first field, rejected above.
            let w = ((q >> cur) & (q >> (at - 1)) & 1) as u32 & (at >= cur + 2) as u32;
            fs[i] = start + (cur + w) as usize;
            fe[i] = start + (at - w) as usize;
            wrapped += w;
            cur = at + 1;
        }
        let ok = q0.count_ones() + q1.count_ones() == 2 * wrapped;
        Window::Line { end, ok }
    }
}

/// Bits of `x` selected by the 128-bit mask (lo, hi), packed in order.
#[inline(always)]
fn pext2(x0: u64, x1: u64, m0: u64, m1: u64) -> u64 {
    unsafe { _pext_u64(x0, m0) | _pext_u64(x1, m1).unbounded_shl(m0.count_ones()) }
}

/// Splits the line at `start` when it fits in the 128-byte window
/// `data[start..start + 128]`, which the caller guarantees is readable.
///
/// Newline, comma and quote masks cover the window as two 64-bit halves.
/// Each field boundary is found independently (the i-th comma is
/// `tzcnt(pdep(1 << i, commas))`), and quotes are checked with bit
/// operations: `pext` gathers, per field, whether its first and its last
/// byte is a quote. A line is well formed when every quote is one end of a
/// wrapping pair, i.e. quotes == 2 x wrapped fields.
///
/// A line whose first field is empty reports `ok == false`: such a line is
/// dropped by every caller (empty event_id, click_id or link side).
#[inline(always)]
fn scan_window_pdep<const N: usize>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N]) -> Window {
    debug_assert!(start + 128 <= data.len());
    unsafe {
        let p = data.as_ptr().add(start);
        let (nl0, cm0, q0) = masks64(p);
        let (nl1, cm1, q1) = masks64(p.add(64));
        if nl0 | nl1 == 0 {
            return Window::Long;
        }
        let e = if nl0 != 0 { nl0.trailing_zeros() } else { 64 + nl1.trailing_zeros() };
        let end = start + e as usize;
        // Keep bits below the newline (bzhi leaves everything for index >= 64).
        let k0 = _bzhi_u64(u64::MAX, e);
        let k1 = _bzhi_u64(u64::MAX, e.saturating_sub(64));
        let (c0, c1, q0, q1) = (cm0 & k0, cm1 & k1, q0 & k0, q1 & k1);
        let n0 = c0.count_ones();
        if n0 + c1.count_ones() != (N - 1) as u32 || c0 & 1 != 0 || e == 0 {
            return Window::Line { end, ok: false };
        }
        // Field starts: 0 and after each comma. Field last bytes: before each
        // comma and before the newline. A field starting and ending on the
        // same byte is a one-byte field (a lone quote is not a wrapper).
        let s0 = 1 | (c0 << 1);
        let s1 = (c1 << 1) | (c0 >> 63);
        let last = e - 1;
        let lbit = 1u64 << (last & 63);
        let l0 = (c0 >> 1) | (c1 << 63) | if last < 64 { lbit } else { 0 };
        let l1 = (c1 >> 1) | if last >= 64 { lbit } else { 0 };
        let wrapped = pext2(q0, q1, s0, s1) & pext2(q0, q1, l0, l1) & !pext2(s0 & l0, s1 & l1, s0, s1);
        let ok = q0.count_ones() + q1.count_ones() == 2 * wrapped.count_ones();
        let mut cur = start;
        for i in 0..N - 1 {
            let lo = _pdep_u64(1u64 << i, c0).trailing_zeros();
            let hi = 64 + _pdep_u64(1u64 << ((i as u32).wrapping_sub(n0) & 63), c1).trailing_zeros();
            let at = start + if (i as u32) < n0 { lo } else { hi } as usize;
            let w = ((wrapped >> i) & 1) as usize;
            fs[i] = cur + w;
            fe[i] = at - w;
            cur = at + 1;
        }
        let w = ((wrapped >> (N - 1)) & 1) as usize;
        fs[N - 1] = cur + w;
        fe[N - 1] = end - w;
        Window::Line { end, ok }
    }
}

/// `scan_window_pdep` for the common shape, from two 64-byte masks instead
/// of one 128-byte mask: the first is anchored at the line start and must
/// hold the first N/2 commas; the second is anchored at the field after
/// them and must hold the newline. Every position then comes from a single
/// 64-bit word (no per-field choice between two halves). Any other line
/// (a longer one, an empty first field, an empty field at the second
/// anchor) is handed to `scan_window_pdep`, and for the lines handled here
/// the result is the same as that function's.
#[inline(always)]
fn scan_window_anchored<const N: usize>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N], h: usize) -> Option<Window> {
    debug_assert!(start + 128 <= data.len());
    debug_assert!(h >= 1 && h < N);
    unsafe {
        let p = data.as_ptr().add(start);
        let (nl_a, cm_a, q_a) = masks64(p);
        // Commas before the first newline of these 64 bytes (all, if none).
        let ca = _bzhi_u64(cm_a, nl_a.trailing_zeros());
        if ca.count_ones() < h as u32 || ca & 1 != 0 {
            return None;
        }
        let mut ends = [0u32; N];
        let mut rest = ca;
        for end in ends.iter_mut().take(h) {
            *end = rest.trailing_zeros();
            rest &= rest - 1;
        }
        // Field h starts at b (1..=64), right after the h-th comma.
        let b = ends[h - 1] + 1;
        let fm = _bzhi_u64(ca, b);
        let (nl_b, cm_b, q_b) = masks64(p.add(b as usize));
        if nl_b == 0 {
            return None;
        }
        let e = nl_b.trailing_zeros();
        let cb = _bzhi_u64(cm_b, e);
        if cb & 1 != 0 {
            return None;
        }
        let end = start + (b + e) as usize;
        if cb.count_ones() != (N - 1 - h) as u32 {
            return Some(Window::Line { end, ok: false });
        }
        // Wrapped fields, as in `scan_window_pdep`: a quote on the first and
        // on the last byte of a field of two bytes or more.
        let qa = _bzhi_u64(q_a, b);
        let qb = _bzhi_u64(q_b, e);
        let sa = 1 | (fm ^ (1u64 << (b - 1))) << 1;
        let la = fm >> 1;
        let wa = _pext_u64(qa, sa) & _pext_u64(qa, la) & !_pext_u64(sa & la, sa);
        let sb = 1 | cb << 1;
        let last = if e == 0 { 0 } else { 1u64 << (e - 1) };
        let lb = cb >> 1 | last;
        let wb = _pext_u64(qb, sb) & _pext_u64(qb, lb) & !_pext_u64(sb & lb, sb);
        let ok = qa.count_ones() + qb.count_ones() == 2 * (wa.count_ones() + wb.count_ones());
        let mut cur = 0u32;
        for i in 0..h {
            let w = ((wa >> i) & 1) as usize;
            fs[i] = start + cur as usize + w;
            fe[i] = start + ends[i] as usize - w;
            cur = ends[i] + 1;
        }
        let base = start + b as usize;
        let mut rest = cb;
        let mut cur = 0u32;
        for i in h..N - 1 {
            let at = rest.trailing_zeros();
            rest &= rest - 1;
            let w = ((wb >> (i - h)) & 1) as usize;
            fs[i] = base + cur as usize + w;
            fe[i] = base + at as usize - w;
            cur = at + 1;
        }
        let w = ((wb >> (N - 1 - h)) & 1) as usize;
        fs[N - 1] = base + cur as usize + w;
        fe[N - 1] = end - w;
        Some(Window::Line { end, ok })
    }
}

/// `scan_window_anchored` with the second anchor after half of the fields;
/// an event line whose first three fields do not fit in 64 bytes (a long
/// ID) is tried again with the anchor after two. What neither handles goes
/// to `scan_window_pdep`.
#[inline(always)]
fn scan_window_two<const N: usize>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N]) -> Window {
    if let Some(w) = scan_window_anchored::<N>(data, start, fs, fe, N / 2) {
        return w;
    }
    if N == 7 {
        if let Some(w) = scan_window_anchored::<N>(data, start, fs, fe, 2) {
            return w;
        }
    }
    scan_window_pdep::<N>(data, start, fs, fe)
}

/// True when pdep/pext are fast here. They are microcoded (hundreds of
/// cycles) on AMD family 17h (Zen, Zen+, Zen 2) and on Hygon (family 18h).
pub fn pdep_is_fast() -> bool {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let leaf0 = __cpuid(0);
        let mut vendor = [0u8; 12];
        vendor[0..4].copy_from_slice(&leaf0.ebx.to_le_bytes());
        vendor[4..8].copy_from_slice(&leaf0.edx.to_le_bytes());
        vendor[8..12].copy_from_slice(&leaf0.ecx.to_le_bytes());
        if &vendor == b"HygonGenuine" {
            return false;
        }
        if &vendor == b"AuthenticAMD" {
            let eax = __cpuid(1).eax;
            let base = (eax >> 8) & 0xF;
            let family = if base == 0xF { base + ((eax >> 20) & 0xFF) } else { base };
            return family != 0x17;
        }
    }
    true
}

/// Splits the line at `start` when it fits in the 128-byte window
/// `data[start..start + 128]` (readable, guaranteed by the caller), with
/// the pdep/pext form when `PDEP` and the tzcnt form otherwise. Both give
/// the same result for every input.
#[inline(always)]
pub fn scan_window<const N: usize, const PDEP: bool>(data: &[u8], start: usize, fs: &mut [usize; N], fe: &mut [usize; N]) -> Window {
    if PDEP {
        scan_window_two::<N>(data, start, fs, fe)
    } else {
        scan_window_tz::<N>(data, start, fs, fe)
    }
}

/// Strips the wrapping quotes of every field. False when a quote stands
/// anywhere but as a wrapping pair (a malformed line).
#[inline(always)]
pub fn unquote<const N: usize>(data: &[u8], fs: &mut [usize; N], fe: &mut [usize; N], quotes: u32) -> bool {
    if quotes == 0 {
        return true;
    }
    let mut wrapped = 0u32;
    for i in 0..N {
        let (s, e) = (fs[i], fe[i]);
        if e - s >= 2 && data[s] == b'"' && data[e - 1] == b'"' {
            fs[i] = s + 1;
            fe[i] = e - 1;
            wrapped += 1;
        }
    }
    quotes == 2 * wrapped
}

/// The fast ID key of `data[s..e]` (see ids.rs), or 0 when it is not
/// `prefix` + 1..=8 base-36 digits.
#[inline(always)]
pub fn id_key(data: &[u8], s: usize, e: usize, prefix: u8) -> u64 {
    let n = e - s;
    if !(2..=9).contains(&n) || data[s] != prefix {
        return 0;
    }
    if e < 8 {
        return crate::ids::fast_key(&data[s..e], prefix);
    }
    let digits = n - 1;
    unsafe {
        let w = (data.as_ptr().add(e - 8) as *const u64).read_unaligned();
        let keep: u64 = u64::MAX << (64 - 8 * digits);
        let w = (w & keep) | (0x3030_3030_3030_3030 & !keep);
        let v = _mm_cvtsi64_si128(w as i64);
        let dig = _mm_and_si128(
            _mm_cmpgt_epi8(v, _mm_set1_epi8(b'0' as i8 - 1)),
            _mm_cmplt_epi8(v, _mm_set1_epi8(b'9' as i8 + 1)),
        );
        let alpha = _mm_and_si128(
            _mm_cmpgt_epi8(v, _mm_set1_epi8(b'a' as i8 - 1)),
            _mm_cmplt_epi8(v, _mm_set1_epi8(b'z' as i8 + 1)),
        );
        if _mm_movemask_epi8(_mm_or_si128(dig, alpha)) & 0xFF != 0xFF {
            return 0;
        }
        let val = _mm_sub_epi8(_mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8)), _mm_and_si128(alpha, _mm_set1_epi8(39)));
        let pairs = _mm_maddubs_epi16(val, _mm_setr_epi8(36, 1, 36, 1, 36, 1, 36, 1, 0, 0, 0, 0, 0, 0, 0, 0));
        let quads = _mm_madd_epi16(pairs, _mm_setr_epi16(1296, 1, 1296, 1, 0, 0, 0, 0));
        let hi = _mm_cvtsi128_si32(quads) as u32 as u64;
        let lo = _mm_extract_epi32::<1>(quads) as u32 as u64;
        crate::ids::fast_from(digits, hi * 1_679_616 + lo)
    }
}

/// (hi, lo) of `data[s..e]` when it is `prefix` + exactly 32 lowercase hex digits [0-9a-f], else None.
/// hi holds the first 16 digits, first digit most significant, so (hi, lo) numeric order == byte-wise order.
#[inline(always)]
pub fn hex_key(data: &[u8], s: usize, e: usize, prefix: u8) -> Option<(u64, u64)> {
    if e - s != 33 || data[s] != prefix {
        return None;
    }
    unsafe {
        // data[s + 1..s + 33] is in bounds: e == s + 33 <= data.len().
        let p = data.as_ptr().add(s + 1);
        let a = _mm_loadu_si128(p as *const __m128i);
        let b = _mm_loadu_si128(p.add(16) as *const __m128i);
        // Signed compares: bytes >= 0x80 are negative and fail both ranges.
        let dig = |v: __m128i| {
            _mm_and_si128(_mm_cmpgt_epi8(v, _mm_set1_epi8(b'0' as i8 - 1)), _mm_cmplt_epi8(v, _mm_set1_epi8(b'9' as i8 + 1)))
        };
        let alpha = |v: __m128i| {
            _mm_and_si128(_mm_cmpgt_epi8(v, _mm_set1_epi8(b'a' as i8 - 1)), _mm_cmplt_epi8(v, _mm_set1_epi8(b'f' as i8 + 1)))
        };
        let (da, aa, db, ab) = (dig(a), alpha(a), dig(b), alpha(b));
        let ok = _mm_and_si128(_mm_or_si128(da, aa), _mm_or_si128(db, ab));
        if _mm_movemask_epi8(ok) != 0xFFFF {
            return None;
        }
        // Nibbles: c - '0', minus 39 more for 'a'..'f'.
        let zero = _mm_set1_epi8(b'0' as i8);
        let gap = _mm_set1_epi8(39);
        let na = _mm_sub_epi8(_mm_sub_epi8(a, zero), _mm_and_si128(aa, gap));
        let nb = _mm_sub_epi8(_mm_sub_epi8(b, zero), _mm_and_si128(ab, gap));
        let w = _mm_set1_epi16(0x0110); // bytes (16, 1): the first digit of each pair is the high nibble
        let bytes = _mm_packus_epi16(_mm_maddubs_epi16(na, w), _mm_maddubs_epi16(nb, w));
        let hi = (_mm_cvtsi128_si64(bytes) as u64).swap_bytes();
        let lo = (_mm_extract_epi64::<1>(bytes) as u64).swap_bytes();
        Some((hi, lo))
    }
}

const FIRST_YEAR: i64 = 1900;
const YEARS: usize = 300;

/// Days since 1970-01-01 of the first of each month, 1900-01 .. 2199-12.
static MONTH_START: [i32; YEARS * 12] = {
    let mut table = [0i32; YEARS * 12];
    let mut i = 0;
    while i < YEARS * 12 {
        let year = FIRST_YEAR + (i / 12) as i64;
        let month = (i % 12) as i64 + 1;
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y / 400 } else { (y - 399) / 400 };
        let yoe = y - era * 400;
        let doy = (153 * ((month + 9) % 12) + 2) / 5;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        table[i] = (era * 146_097 + doe - 719_468) as i32;
        i += 1;
    }
    table
};

/// Days in each month, same indexing.
static MONTH_DAYS: [u8; YEARS * 12] = {
    let mut table = [0u8; YEARS * 12];
    let mut i = 0;
    while i < YEARS * 12 {
        let year = FIRST_YEAR + (i / 12) as i64;
        let month = i % 12 + 1;
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        table[i] = match month {
            2 => {
                if leap {
                    29
                } else {
                    28
                }
            }
            4 | 6 | 9 | 11 => 30,
            _ => 31,
        };
        i += 1;
    }
    table
};

/// The common part of F1 and F2, `YYYY-MM-DDTHH:MM:SS` at `data[s..s+19]`,
/// as epoch ms of that wall-clock time read as UTC. Requires `s + 20 <= len`.
#[inline(always)]
fn iso19(data: &[u8], s: usize) -> Option<i64> {
    debug_assert!(s + 20 <= data.len());
    unsafe {
        let p = data.as_ptr().add(s);
        let v = _mm_loadu_si128(p as *const __m128i);
        let template = _mm_setr_epi8(
            b'0' as i8, b'0' as i8, b'0' as i8, b'0' as i8, b'-' as i8, b'0' as i8, b'0' as i8, b'-' as i8, b'0' as i8,
            b'0' as i8, b'T' as i8, b'0' as i8, b'0' as i8, b':' as i8, b'0' as i8, b'0' as i8,
        );
        let limit = _mm_setr_epi8(9, 9, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9);
        let t = _mm_xor_si128(v, template);
        if _mm_movemask_epi8(_mm_cmpeq_epi8(_mm_max_epu8(t, limit), limit)) != 0xFFFF {
            return None;
        }
        let tail = (p.add(16) as *const u32).read_unaligned();
        // ':' S1 S2 (byte 19 is the caller's)
        let s1 = ((tail >> 8) & 0xFF).wrapping_sub(b'0' as u32);
        let s2 = ((tail >> 16) & 0xFF).wrapping_sub(b'0' as u32);
        if tail & 0xFF != b':' as u32 || s1 > 9 || s2 > 9 {
            return None;
        }
        let g = _mm_shuffle_epi8(t, _mm_setr_epi8(0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, -1, -1, -1, -1));
        let pairs = _mm_maddubs_epi16(g, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 0, 0, 0, 0));
        let mut lanes = [0u16; 8];
        _mm_storeu_si128(lanes.as_mut_ptr() as *mut __m128i, pairs);
        let year = lanes[0] as i64 * 100 + lanes[1] as i64;
        let (month, day, hour, minute) = (lanes[2] as i64, lanes[3] as i64, lanes[4] as i64, lanes[5] as i64);
        let second = (s1 * 10 + s2) as i64;
        if !(1..=12).contains(&month) || day == 0 || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let index = (year - FIRST_YEAR) * 12 + month - 1;
        let days = if (0..(YEARS * 12) as i64).contains(&index) {
            if day > MONTH_DAYS[index as usize] as i64 {
                return None;
            }
            MONTH_START[index as usize] as i64 + day - 1
        } else {
            if day > ts::days_in_month(year, month) {
                return None;
            }
            ts::days_from_civil(year, month, day)
        };
        Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000)
    }
}

/// Exact calendar-prefix cache. Slots shared by different months are safe:
/// every hit compares all eleven bytes of "YYYY-MM-DDT".
pub struct WindowDates([(u64, u32, i64); 32]);

impl WindowDates {
    pub fn new(from: i64, until: i64) -> Self {
        let mut table = Self([(0, u32::MAX, 0); 32]);
        for day in from.div_euclid(ts::DAY)..=until.div_euclid(ts::DAY) {
            let (y, m, d) = ts::civil_from_days(day);
            if !(0..=9999).contains(&y) { continue; }
            let date = format!("{y:04}-{m:02}-{d:02}T");
            let bytes = date.as_bytes();
            let prefix = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            let suffix = u32::from_le_bytes([bytes[8], bytes[9], b'T', 0]);
            table.0[d as usize] = (prefix, suffix, day * ts::DAY);
        }
        table
    }

    #[inline(always)]
    pub fn parse(&self, data: &[u8], start: usize, end: usize) -> Option<i64> {
        debug_assert!(start <= end && end <= data.len());
        if end - start != 20 {
            return timestamp(data, start, end);
        }
        // Bytes: 0..8 "YYYY-MM-", 8..10 "DD", 10 'T', 11..19 "HH:MM:SS", 19 'Z'.
        let (prefix, mid, clock, z) = unsafe {
            let p = data.as_ptr().add(start);
            (
                (p as *const u64).read_unaligned(),
                (p.add(8) as *const u32).read_unaligned(),
                (p.add(11) as *const u64).read_unaligned(),
                *p.add(19),
            )
        };
        if z != b'Z' {
            return timestamp(data, start, end);
        }
        let suffix = mid & 0x00FF_FFFF;
        let slot = (((suffix & 15) * 10 + ((suffix >> 8) & 15)) & 31) as usize;
        let (head, tail, midnight) = self.0[slot];
        if head != prefix || tail != suffix {
            return iso19(data, start);
        }
        // The date is one of the table's (valid) days. The clock: every
        // digit within its own limit (H1 <= 2, M1 and S1 <= 5, the rest
        // <= 9) and both colons in place, all bytes at once. A byte over
        // its limit, or not a digit at all, sets a high bit in `x` or in
        // the sum (no carry leaves a byte while `x` has no high bit).
        let x = clock ^ u64::from_le_bytes(*b"00:00:00");
        const ROOM: u64 = u64::from_le_bytes([0x7F - 2, 0x7F - 9, 0x7F, 0x7F - 5, 0x7F - 9, 0x7F, 0x7F - 5, 0x7F - 9]);
        if (x | x.wrapping_add(ROOM)) & 0x8080_8080_8080_8080 != 0 {
            return None;
        }
        // Byte k of x * 2561 is 10 * byte(k-1) + byte(k): bytes 1, 4 and 7
        // are the hour, the minute and the second.
        let pairs = x.wrapping_mul(2561);
        let hour = (pairs >> 8) & 0xFF;
        let minute = (pairs >> 32) & 0xFF;
        let second = pairs >> 56;
        if hour > 23 {
            return None;
        }
        Some(midnight + ((hour * 3600 + minute * 60 + second) * 1000) as i64)
    }
}


/// F1 `YYYY-MM-DDTHH:MM:SSZ` at `data[s..s+20]`; None if it is not exactly that.
#[inline(always)]
pub fn f1(data: &[u8], s: usize) -> Option<i64> {
    if data[s + 19] != b'Z' {
        return None;
    }
    iso19(data, s)
}

/// F2 `YYYY-MM-DDTHH:MM:SS+HH:MM` at `data[s..s+25]`; None if it is not exactly that.
#[inline(always)]
fn f2(data: &[u8], s: usize) -> Option<i64> {
    let sign = match data[s + 19] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let d = |i: usize| (data[s + i] as i64).wrapping_sub(b'0' as i64);
    let (h1, h2, m1, m2) = (d(20), d(21), d(23), d(24));
    if data[s + 22] != b':' || !(0..10).contains(&h1) || !(0..10).contains(&h2) || !(0..10).contains(&m1) || !(0..10).contains(&m2) {
        return None;
    }
    let (oh, om) = (h1 * 10 + h2, m1 * 10 + m2);
    if oh > 23 || om > 59 {
        return None;
    }
    Some(iso19(data, s)? - sign * (oh * 60 + om) * 60_000)
}

/// Months near the window, keyed by their 8 bytes "YYYY-MM-": the day number
/// of the month's first day and its length. Lets F1 skip calendar arithmetic.
pub struct Months {
    keys: [u64; 128],
    first_day: [i32; 128],
    len: [u8; 128],
}

impl Months {
    /// Every month that overlaps [from_day, to_day] (day numbers since 1970-01-01).
    pub fn new(from_day: i64, to_day: i64) -> Months {
        let mut m = Months { keys: [0; 128], first_day: [0; 128], len: [0; 128] };
        let (y0, m0, _) = ts::civil_from_days(from_day);
        let (mut y, mut mo) = (y0, m0);
        let mut count = 0;
        loop {
            let first = ts::days_from_civil(y, mo, 1);
            if first > to_day || !(0..=9999).contains(&y) {
                break;
            }
            let next = if mo == 12 { ts::days_from_civil(y + 1, 1, 1) } else { ts::days_from_civil(y, mo + 1, 1) };
            let text = format!("{y:04}-{mo:02}-");
            let key = u64::from_le_bytes(text.as_bytes().try_into().unwrap());
            let mut h = Self::home(key);
            while m.keys[h] != 0 {
                h = (h + 1) & 127;
            }
            m.keys[h] = key;
            m.first_day[h] = first as i32;
            m.len[h] = (next - first) as u8;
            count += 1;
            assert!(count <= 96, "month table too small");
            if mo == 12 {
                y += 1;
                mo = 1;
            } else {
                mo += 1;
            }
        }
        m
    }

    #[inline(always)]
    fn home(key: u64) -> usize {
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 57) as usize
    }

    /// (first day, length) of the month whose "YYYY-MM-" bytes are `key`.
    #[inline(always)]
    fn get(&self, key: u64) -> Option<(i64, i64)> {
        let mut h = Self::home(key);
        loop {
            let k = self.keys[h];
            if k == key {
                return Some((self.first_day[h] as i64, self.len[h] as i64));
            }
            if k == 0 {
                return None;
            }
            h = (h + 1) & 127;
        }
    }
}

const D8: u64 = 0x3030_3030_3030_3030;
const HI: u64 = 0xF0F0_F0F0_F0F0_F0F0;
const SIX: u64 = 0x0606_0606_0606_0606;

/// All of `w`'s lanes selected by `lanes` (0xFF per lane) are ASCII digits.
#[inline(always)]
fn digits_in(w: u64, lanes: u64) -> bool {
    let x = w & lanes;
    (x & HI) == (D8 & lanes) && ((x.wrapping_add(SIX & lanes)) & HI & lanes) == (D8 & lanes)
}

#[inline(always)]
fn byte(w: u64, i: u32) -> i64 {
    ((w >> (8 * i)) & 0xFF) as i64 - b'0' as i64
}

/// F1 at `data[s..s+20]` with SWAR; `s + 24 <= data.len()` is required.
/// Months outside the table fall back to `ts::parse`.
#[inline(always)]
pub fn f1_swar(data: &[u8], s: usize, months: &Months) -> Option<i64> {
    debug_assert!(s + 24 <= data.len());
    let (w0, w1, w2) = unsafe {
        let p = data.as_ptr().add(s);
        (
            (p as *const u64).read_unaligned(),
            (p.add(8) as *const u64).read_unaligned(),
            (p.add(16) as *const u32).read_unaligned() as u64,
        )
    };
    // w0 "YYYY-MM-", w1 "DDTHH:MM", w2 ":SSZ"
    const W0_SEP: u64 = 0xFF00_00FF_0000_0000;
    const W0_VAL: u64 = (b'-' as u64) << 56 | (b'-' as u64) << 32;
    const W1_SEP: u64 = 0x0000_FF00_00FF_0000;
    const W1_VAL: u64 = (b':' as u64) << 40 | (b'T' as u64) << 16;
    const W2_SEP: u64 = 0xFF00_00FF;
    const W2_VAL: u64 = (b'Z' as u64) << 24 | b':' as u64;
    let ok = (w0 & W0_SEP) == W0_VAL
        && (w1 & W1_SEP) == W1_VAL
        && (w2 & W2_SEP) == W2_VAL
        && digits_in(w0, 0x00FF_FF00_FFFF_FFFF)
        && digits_in(w1, 0xFFFF_00FF_FF00_FFFF)
        && digits_in(w2, 0x00FF_FF00);
    if !ok {
        return None;
    }
    let day = byte(w1, 0) * 10 + byte(w1, 1);
    let hour = byte(w1, 3) * 10 + byte(w1, 4);
    let minute = byte(w1, 6) * 10 + byte(w1, 7);
    let second = byte(w2, 1) * 10 + byte(w2, 2);
    let Some((first, len)) = months.get(w0) else { return ts::parse(&data[s..s + 20]) };
    if day == 0 || day > len || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some((first + day - 1) * ts::DAY + hour * 3_600_000 + minute * 60_000 + second * 1000)
}

/// Same meaning as `timestamp`, using SWAR F1 and the month table when the
/// field leaves 24 readable bytes.
#[inline(always)]
pub fn timestamp_m(data: &[u8], s: usize, e: usize, months: &Months) -> Option<i64> {
    let n = e - s;
    if s + 24 <= data.len() {
        // A 20-byte field ending in 'Z' can only be F1 (a 20-byte F3 is all digits).
        if n == 20 && data[s + 19] == b'Z' {
            return f1_swar(data, s, months);
        }
        if n == 13 {
            unsafe {
                let p = data.as_ptr().add(s);
                let hi = eight_digits((p as *const u64).read_unaligned());
                let lo = eight_digits((p.add(5) as *const u64).read_unaligned());
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    return Some((hi * 100_000 + lo % 100_000) as i64);
                }
            }
        }
    }
    ts::parse(&data[s..e])
}

/// Eight ASCII digits (first byte most significant) to their value, or None.
#[inline(always)]
fn eight_digits(w: u64) -> Option<u64> {
    if (w & 0xF0F0_F0F0_F0F0_F0F0) != 0x3030_3030_3030_3030
        || ((w.wrapping_add(0x0606_0606_0606_0606)) & 0xF0F0_F0F0_F0F0_F0F0) != 0x3030_3030_3030_3030
    {
        return None;
    }
    let mut v = w & 0x0F0F_0F0F_0F0F_0F0F;
    v = (v.wrapping_mul(2561)) >> 8 & 0x00FF_00FF_00FF_00FF;
    v = (v.wrapping_mul(6_553_601)) >> 16 & 0x0000_FFFF_0000_FFFF;
    v = (v.wrapping_mul(42_949_672_960_001)) >> 32;
    Some(v)
}

/// A timestamp field `data[s..e]` to epoch ms; None when bad. Same meaning as `ts::parse`.
#[inline(always)]
pub fn timestamp(data: &[u8], s: usize, e: usize) -> Option<i64> {
    let n = e - s;
    if n == 20 && data[s + 19] == b'Z' {
        return f1(data, s);
    }
    if n == 13 {
        unsafe {
            let p = data.as_ptr().add(s);
            let hi = eight_digits((p as *const u64).read_unaligned());
            let lo = eight_digits((p.add(5) as *const u64).read_unaligned());
            if let (Some(hi), Some(lo)) = (hi, lo) {
                return Some((hi * 100_000 + lo % 100_000) as i64);
            }
        }
    }
    if n == 25 && data[s + 4] == b'-' {
        return f2(data, s);
    }
    ts::parse(&data[s..e])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_ref<const N: usize>(line: &[u8]) -> Option<Vec<Vec<u8>>> {
        crate::csv::split::<N>(line).map(|f| f.iter().map(|x| x.to_vec()).collect())
    }

    fn scan_fast<const N: usize>(data: &[u8], start: usize) -> (usize, Option<Vec<Vec<u8>>>) {
        let mut fs = [0usize; N];
        let mut fe = [0usize; N];
        let (end, n, q) = scan::<N>(data, start, &mut fs, &mut fe);
        if n != N || !unquote::<N>(data, &mut fs, &mut fe, q) {
            return (end, None);
        }
        (end, Some((0..N).map(|i| data[fs[i]..fe[i]].to_vec()).collect()))
    }

    #[test]
    fn scan_matches_reference() {
        let mut seed = 12345u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"ab,\"\r 9,,\"x";
        for _ in 0..200_000 {
            let len = (rnd() % 90) as usize;
            let mut line: Vec<u8> = (0..len).map(|_| alphabet[(rnd() % alphabet.len() as u64) as usize]).collect();
            if rnd() % 3 == 0 {
                line = b"e1,\"u1\",2026-10-03T10:00:00Z,\"2026-10-03T10:01:00Z\",view,p1,".to_vec();
                let at = (rnd() % line.len() as u64) as usize;
                line[at] = alphabet[(rnd() % alphabet.len() as u64) as usize];
            }
            let mut data = b"header\n".to_vec();
            data.extend_from_slice(&line);
            data.push(b'\n');
            data.extend_from_slice(b"next,line\n");
            let (end, got) = scan_fast::<7>(&data, 7);
            assert_eq!(end, 7 + line.len());
            assert_eq!(got, scan_ref::<7>(&line), "{:?}", String::from_utf8_lossy(&line));
            let (_, got2) = scan_fast::<2>(&data, 7);
            assert_eq!(got2, scan_ref::<2>(&line));
            // Without a trailing newline.
            let mut tail = b"header\n".to_vec();
            tail.extend_from_slice(&line);
            let (end, got) = scan_fast::<4>(&tail, 7);
            assert_eq!(end, tail.len());
            assert_eq!(got, scan_ref::<4>(&line));
        }
    }

    #[test]
    fn window_matches_reference() {
        let mut seed = 987654321u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"ab,\"\r 9,,\"xyz0123\"\",";
        for _ in 0..1_000_000 {
            let len = (rnd() % 140) as usize;
            let mut line: Vec<u8> = (0..len).map(|_| alphabet[(rnd() % alphabet.len() as u64) as usize]).collect();
            if rnd() % 3 == 0 {
                line = b"e1,\"u1\",2026-10-03T10:00:00Z,\"2026-10-03T10:01:00Z\",view,p1,".to_vec();
                let at = (rnd() % line.len() as u64) as usize;
                line[at] = alphabet[(rnd() % alphabet.len() as u64) as usize];
            }
            let mut data = b"header\n".to_vec();
            data.extend_from_slice(&line);
            data.push(b'\n');
            data.extend_from_slice(&[b'z'; 200]);
            check_window::<7, true>(&data, &line);
            check_window::<7, false>(&data, &line);
            check_window::<4, true>(&data, &line);
            check_window::<4, false>(&data, &line);
            check_window::<2, true>(&data, &line);
            check_window::<2, false>(&data, &line);
        }
    }

    /// `scan_window` of the line at offset 7 of `data` against the reference.
    fn check_window<const N: usize, const PDEP: bool>(data: &[u8], line: &[u8]) {
        let mut fs = [0usize; N];
        let mut fe = [0usize; N];
        match scan_window::<N, PDEP>(data, 7, &mut fs, &mut fe) {
            Window::Long => assert!(line.len() >= 128),
            Window::Line { end, ok } => {
                assert!(line.len() < 128);
                assert_eq!(end, 7 + line.len());
                let got = ok
                    .then(|| (0..N).map(|i| data[fs[i]..fe[i]].to_vec()).collect::<Vec<_>>())
                    .filter(|f| !f[0].is_empty());
                // Lines whose first field is empty are dropped by every caller.
                let want = scan_ref::<N>(line).filter(|f| !f[0].is_empty());
                assert_eq!(got, want, "N={N} pdep={PDEP} {:?}", String::from_utf8_lossy(line));
            }
        }
    }

    /// Lines shaped like real ones (N fields of many lengths, some wrapped,
    /// some with a stray quote, sometimes a field too many or too few), so
    /// that commas, quotes and the newline fall on every side of bytes 63/64
    /// and of the second anchor.
    #[test]
    fn window_shapes_match_reference() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let lens = [0usize, 0, 1, 1, 2, 2, 3, 5, 8, 9, 13, 20, 20, 25, 30, 31, 32, 33, 34, 40, 61, 62, 63, 64, 65, 70];
        let chars = b"abz019 \r";
        for round in 0..600_000u32 {
            let n = [7usize, 4, 2][(round % 3) as usize];
            let fields = match rnd() % 16 {
                0 => n + 1,
                1 => n - 1,
                _ => n,
            };
            let mut line: Vec<u8> = Vec::new();
            for f in 0..fields {
                if f > 0 {
                    line.push(b',');
                }
                let len = lens[(rnd() % lens.len() as u64) as usize];
                let wrap = rnd() % 3 == 0;
                if wrap {
                    line.push(b'"');
                }
                for _ in 0..len {
                    line.push(chars[(rnd() % chars.len() as u64) as usize]);
                }
                if wrap && rnd() % 12 != 0 {
                    line.push(b'"');
                }
                if rnd() % 40 == 0 {
                    line.push(b'"');
                }
            }
            if rnd() % 50 == 0 && !line.is_empty() {
                let at = (rnd() % line.len() as u64) as usize;
                line[at] = b'"';
            }
            let mut data = b"header\n".to_vec();
            data.extend_from_slice(&line);
            data.push(b'\n');
            data.extend_from_slice(&[b'z'; 200]);
            check_window::<7, true>(&data, &line);
            check_window::<7, false>(&data, &line);
            check_window::<4, true>(&data, &line);
            check_window::<4, false>(&data, &line);
            check_window::<2, true>(&data, &line);
            check_window::<2, false>(&data, &line);
        }
    }

    #[test]
    fn ids_match_reference() {
        let samples: Vec<&[u8]> = vec![
            b"e1", b"ezzzzzzzz", b"e00000000", b"e0", b"e123456789", b"eA1", b"e1 ", b"E1", b"e", b"e\x80", b"e9az",
            b"e{", b"e/", b"e:", b"e`",
        ];
        for s in samples {
            let mut data = b"xxxxxxxxxxxxxxxx,".to_vec();
            let start = data.len();
            data.extend_from_slice(s);
            let end = data.len();
            data.extend_from_slice(b",yyyyyyyy");
            assert_eq!(id_key(&data, start, end, b'e'), crate::ids::fast_key(s, b'e'), "{:?}", s);
        }
    }

    fn hex_ref(b: &[u8], prefix: u8) -> Option<(u64, u64)> {
        if b.len() != 33 || b[0] != prefix {
            return None;
        }
        let mut v: u128 = 0;
        for &c in &b[1..] {
            let d = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => return None,
            };
            v = v << 4 | d as u128;
        }
        Some(((v >> 64) as u64, v as u64))
    }

    #[test]
    fn hex_keys_match_reference() {
        let mut seed = 0xDEAD_BEEF_u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        const HEX: &[u8] = b"0123456789abcdef";
        let bad: &[u8] = b"AFgG/:`@{\x80\xff \"";
        let mut samples: Vec<Vec<u8>> = vec![
            [b"u".as_slice(), &[b'0'; 32]].concat(),
            [b"u".as_slice(), &[b'f'; 32]].concat(),
            b"u02423c3d84ab053af78764ce49f23b5c".to_vec(),
        ];
        for i in 0..200_000 {
            let mut s = vec![b'u'];
            s.extend((0..32).map(|_| HEX[(rnd() % 16) as usize]));
            match i % 6 {
                0 => {}
                1 => {
                    let at = 1 + (rnd() % 32) as usize;
                    s[at] = bad[(rnd() % bad.len() as u64) as usize];
                }
                2 => {
                    if rnd() % 2 == 0 {
                        s.pop();
                    } else {
                        s.push(HEX[(rnd() % 16) as usize]);
                    }
                }
                3 => s[0] = b'e',
                4 => {
                    let at = 1 + (rnd() % 32) as usize;
                    s[at] = 0x80 | (rnd() as u8);
                }
                _ => {
                    let at = 1 + (rnd() % 32) as usize;
                    s[at] = rnd() as u8;
                }
            }
            samples.push(s);
        }
        for s in &samples {
            let mut data = b"xxxxxxxxxxxxxxxx,".to_vec();
            let start = data.len();
            data.extend_from_slice(s);
            let end = data.len();
            data.extend_from_slice(b",yyyyyyyy");
            assert_eq!(hex_key(&data, start, end, b'u'), hex_ref(s, b'u'), "{:?}", String::from_utf8_lossy(s));
            // At the very end of the buffer as well: no over-read.
            assert_eq!(hex_key(s, 0, s.len(), b'u'), hex_ref(s, b'u'));
        }
        assert_eq!(hex_key(&samples[0], 0, 33, b'u'), Some((0, 0)));
        assert_eq!(hex_key(&samples[1], 0, 33, b'u'), Some((u64::MAX, u64::MAX)));
    }

    #[test]
    fn window_date_cache_matches_scalar() {
        let d = ts::days_from_civil(2028, 2, 29) * ts::DAY;
        let dates = WindowDates::new(d - ts::DAY * 32, d + ts::DAY * 3);
        for text in [*b"2028-02-29T23:59:59Z", *b"2028-01-29T10:00:00Z", *b"1899-01-31T00:00:00Z"] {
            for pos in 0..20 {
                for byte in 0..=255 {
                    let mut field = text;
                    field[pos] = byte;
                    assert_eq!(dates.parse(&field, 0, 20), ts::parse(&field));
                }
            }
        }
    }

    /// Random F1-shaped fields on days inside and outside the table, half of
    /// them damaged in one or two bytes or given an out-of-range clock.
    #[test]
    fn window_date_cache_random() {
        let d = ts::days_from_civil(2028, 2, 20);
        let dates = WindowDates::new((d - 7) * ts::DAY, (d + 15) * ts::DAY);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let junk = b"0123456789:-TZtz +/.";
        let mut hits = 0u32;
        for _ in 0..3_000_000 {
            let day = d - 40 + (rnd() % 80) as i64;
            let (y, m, dd) = ts::civil_from_days(day);
            let (mut hh, mut mi, mut ss) = (rnd() % 24, rnd() % 60, rnd() % 60);
            match rnd() % 12 {
                0 => hh = 24 + rnd() % 76,
                1 => mi = 60 + rnd() % 40,
                2 => ss = 60 + rnd() % 40,
                _ => {}
            }
            let mut field = format!("{y:04}-{m:02}-{dd:02}T{hh:02}:{mi:02}:{ss:02}Z").into_bytes();
            assert_eq!(field.len(), 20);
            if rnd() % 2 == 0 {
                for _ in 0..1 + rnd() % 2 {
                    let at = (rnd() % 20) as usize;
                    field[at] = if rnd() % 8 == 0 { rnd() as u8 } else { junk[(rnd() % junk.len() as u64) as usize] };
                }
            }
            let got = dates.parse(&field, 0, 20);
            assert_eq!(got, ts::parse(&field), "{:?}", String::from_utf8_lossy(&field));
            hits += got.is_some() as u32;
        }
        assert!(hits > 500_000);
        // Other lengths go to the general parser.
        for s in [&b"1791071890000"[..], b"2028-02-20T10:00:00+02:00", b"", b"2028-02-20T10:00:00", b"2028-02-20T10:00:00ZZ"] {
            assert_eq!(dates.parse(s, 0, s.len()), ts::parse(s));
        }
    }

    #[test]
    fn timestamps_match_reference() {
        let samples: Vec<&[u8]> = vec![
            b"2026-10-03T23:58:10Z",
            b"2028-02-29T00:00:00Z",
            b"2027-02-29T00:00:00Z",
            b"2026-13-03T23:58:10Z",
            b"2026-10-03T24:58:10Z",
            b"2026-10-03T23:60:10Z",
            b"2026-10-03T23:58:60Z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-00T23:58:10Z",
            b"1899-12-31T23:59:59Z",
            b"2200-01-01T00:00:00Z",
            b"0000-01-01T00:00:00Z",
            b"9999-12-31T23:59:59Z",
            b"1791071890000",
            b"0791071890000",
            b"1791071890a00",
            b"179107189000",
            b"2026-10-03T19:58:10-04:00",
            b"2026-10-04T01:30:00+02:00",
            b"2026-10-03T10:00:00-00:00",
            b"2026-10-03T10:00:00+23:59",
            b"2026-10-03T10:00:00+24:00",
            b"2026-10-03T10:00:00+05:60",
            b"2026-10-03T10:00:00*05:00",
            b"2026-10-03T10:00:00+05-00",
            b"2026-10-03T10:00:00+0a:00",
            b"2028-02-29T23:00:00-10:00",
            b"2027-02-29T23:00:00-10:00",
            b"1850-03-01T00:00:00+01:00",
            b"0000000000000000000000000",
            b"00000000000000000000",
        ];
        for s in samples {
            let mut data = b"yyyyyyyyyyyyyyyy,".to_vec();
            let start = data.len();
            data.extend_from_slice(s);
            let end = data.len();
            data.extend_from_slice(b",zzzzzzzzzzzzzzzzzzzzzzzz");
            assert_eq!(timestamp(&data, start, end), ts::parse(s), "{:?}", String::from_utf8_lossy(s));
        }
        let months = Months::new(ts::days_from_civil(2026, 9, 1), ts::days_from_civil(2028, 4, 1));
        for s in [
            &b"2026-10-03T23:58:10Z"[..],
            b"2028-02-29T00:00:00Z",
            b"2027-02-29T00:00:00Z",
            b"2027-02-28T23:59:59Z",
            b"2026-13-03T23:58:10Z",
            b"2026-10-03T24:58:10Z",
            b"2026-10-03T23:60:10Z",
            b"2026-10-03T23:58:60Z",
            b"2026-10-03 23:58:10Z",
            b"2026-10-03t23:58:10Z",
            b"2026-10-03T23:58:10z",
            b"2026-10-00T23:58:10Z",
            b"2026-10-32T23:58:10Z",
            b"2026-11-31T23:58:10Z",
            b"2030-01-01T00:00:00Z",
            b"1999-01-01T00:00:00Z",
            b"2026/10/03T23:58:10Z",
            b"2026-10-03T23:58:1\xffZ",
            b"20261003T235810Z1234",
            b"00000000000000000000",
            b"1791071890000",
        ] {
            let mut data = s.to_vec();
            data.extend_from_slice(b"padpadpadpadpad");
            assert_eq!(timestamp_m(&data, 0, s.len(), &months), ts::parse(s), "{:?}", String::from_utf8_lossy(s));
        }
        // Every second of a few days around a leap day agrees with the reference.
        let start = ts::parse(b"2028-02-27T00:00:00Z").unwrap();
        for k in (0..4 * 86_400).step_by(7) {
            let ms = start + k * 1000;
            let (y, m, d) = ts::civil_from_days(ms.div_euclid(ts::DAY));
            let sod = ms.rem_euclid(ts::DAY) / 1000;
            let text = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", sod / 3600, sod / 60 % 60, sod % 60);
            let mut data = text.clone().into_bytes();
            data.extend_from_slice(b"padpadpadpad");
            assert_eq!(f1(&data, 0), Some(ms), "{text}");
            assert_eq!(f1_swar(&data, 0, &months), Some(ms), "{text}");
        }
    }
}
