//! The CSV dialect of rules.md.
//!
//! Only `\n` ends a line (a `\r` is an ordinary byte). Every comma separates
//! fields. A field may be wrapped in double quotes, which are syntax; a quote
//! anywhere else makes the line malformed, as does a wrong field count or an
//! empty line. An empty field (including `""`) is null.

/// Splits a line (without its `\n`) into exactly `N` unquoted fields.
/// Returns None for a malformed line. Null fields are empty slices.
#[inline(always)]
pub fn split<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut start = 0usize;
    let mut field = 0usize;
    let mut quotes = false;
    for (i, &b) in line.iter().enumerate() {
        if b == b',' {
            if field + 1 >= N {
                return None;
            }
            out[field] = &line[start..i];
            field += 1;
            start = i + 1;
        } else if b == b'"' {
            quotes = true;
        }
    }
    if field + 1 != N {
        return None;
    }
    out[field] = &line[start..];
    if quotes {
        for f in out.iter_mut() {
            *f = unquote(f)?;
        }
    }
    Some(out)
}

/// The value of one raw field, or None when its quotes are not exactly a
/// wrapping pair.
#[inline(always)]
pub fn unquote(f: &[u8]) -> Option<&[u8]> {
    match f.iter().position(|&b| b == b'"') {
        None => Some(f),
        Some(0) => {
            let n = f.len();
            if n >= 2 && f[n - 1] == b'"' && !f[1..n - 1].contains(&b'"') {
                Some(&f[1..n - 1])
            } else {
                None
            }
        }
        Some(_) => None,
    }
}

/// Calls `on_line` for every line of `data[start..end)` that this segment
/// owns: the lines that begin inside it. The first line of the file (offset
/// 0) is the header and is skipped. A final line without `\n` still counts.
#[inline]
pub fn for_each_line(data: &[u8], start: usize, end: usize, mut on_line: impl FnMut(&[u8])) {
    let len = data.len();
    let mut pos = if start == 0 {
        match memchr_nl(data, 0) {
            Some(nl) => nl + 1,
            None => len,
        }
    } else if data[start - 1] == b'\n' {
        start
    } else {
        match memchr_nl(data, start) {
            Some(nl) => nl + 1,
            None => len,
        }
    };
    while pos < end && pos < len {
        let nl = memchr_nl(data, pos).unwrap_or(len);
        on_line(&data[pos..nl]);
        pos = nl + 1;
    }
}

/// Index of the first `\n` at or after `from`.
#[inline(always)]
pub fn memchr_nl(data: &[u8], from: usize) -> Option<usize> {
    let hay = &data[from..];
    // Word-at-a-time scan: 8 bytes per step.
    let mut i = 0usize;
    let n = hay.len();
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    const NL: u64 = 0x0A0A_0A0A_0A0A_0A0A;
    while i + 8 <= n {
        let w = u64::from_le_bytes(hay[i..i + 8].try_into().unwrap()) ^ NL;
        let found = w.wrapping_sub(ONES) & !w & HIGHS;
        if found != 0 {
            return Some(from + i + (found.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < n {
        if hay[i] == b'\n' {
            return Some(from + i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect() {
        assert_eq!(split::<2>(b"a,b"), Some([&b"a"[..], b"b"]));
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], b""]));
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a"), None);
        assert_eq!(split::<2>(b""), None);
        assert_eq!(split::<2>(b"u1,u5\""), None);
        assert_eq!(split::<2>(b"\"u1,u5"), None);
        assert_eq!(split::<2>(b"\",u5"), None);
        assert_eq!(split::<2>(b"\"\"\",u5"), None);
        assert_eq!(split::<2>(b"a b, c\r"), Some([&b"a b"[..], b" c\r"]));
        assert_eq!(split::<2>(b"\" u1\",x"), Some([&b" u1"[..], b"x"]));
        assert_eq!(split::<2>(b","), Some([&b""[..], b""]));
    }

    #[test]
    fn lines() {
        let data = b"header\nl1\n\nl3\nlast";
        let mut seen = Vec::new();
        for_each_line(data, 0, data.len(), |l| seen.push(l.to_vec()));
        assert_eq!(seen, vec![b"l1".to_vec(), b"".to_vec(), b"l3".to_vec(), b"last".to_vec()]);
        // Every split point gives the same lines, each exactly once.
        for cut in 0..=data.len() {
            let mut parts = Vec::new();
            for_each_line(data, 0, cut, |l| parts.push(l.to_vec()));
            for_each_line(data, cut.max(0), data.len(), |l| parts.push(l.to_vec()));
            if cut == 0 {
                // A segment starting at 0 always skips the header.
                parts.clear();
                for_each_line(data, 0, data.len(), |l| parts.push(l.to_vec()));
            }
            assert_eq!(parts, seen, "cut {cut}");
        }
    }
}
