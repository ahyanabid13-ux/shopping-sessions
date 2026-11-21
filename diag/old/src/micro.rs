//! Development aid, never used in a scored run: `SESSIONS_MICRO=<events csv>`
//! times the hot loops single-threaded on one file held in memory.

use std::time::Instant;

use crate::fast;

fn time<F: FnMut() -> u64>(name: &str, lines: usize, mut f: F) {
    let mut best = f64::MAX;
    let mut check = 0;
    for _ in 0..7 {
        let t = Instant::now();
        check = f();
        best = best.min(t.elapsed().as_secs_f64());
    }
    eprintln!("{name:<28} {:6.2} ns/line   ({check})", best * 1e9 / lines as f64);
}

pub fn run(path: &str) {
    let data = std::fs::read(path).expect("read micro file");
    let lines = data.iter().filter(|&&b| b == b'\n').count();
    eprintln!("{path}: {} bytes, {lines} lines", data.len());
    let start = data.iter().position(|&b| b == b'\n').unwrap() + 1;
    let fast_end = data.len().saturating_sub(128);

    time("raw newline scan", lines, || {
        let mut n = 0u64;
        for chunk in data.chunks_exact(32) {
            for &b in chunk {
                n += (b == b'\n') as u64;
            }
        }
        n
    });

    time("scan_window", lines, || {
        let mut fs = [0usize; 7];
        let mut fe = [0usize; 7];
        let mut pos = start;
        let mut acc = 0u64;
        while pos < fast_end {
            match fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) {
                fast::Window::Line { end, ok } => {
                    acc += ok as u64 + fe[3] as u64;
                    pos = end + 1;
                }
                fast::Window::Long => break,
            }
        }
        acc
    });

    for pdep in [true, false] {
        time(if pdep { "scan_window+pf (pdep)" } else { "scan_window+pf (tzcnt)" }, lines, || {
            let mut fs = [0usize; 7];
            let mut fe = [0usize; 7];
            let mut pos = start;
            let mut acc = 0u64;
            while pos < fast_end {
                crate::util::prefetch(data.as_ptr().wrapping_add(pos + 2048) as *const i8);
                crate::util::prefetch(data.as_ptr().wrapping_add(pos + 2048 + 64) as *const i8);
                let r = if pdep { fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) } else { fast::scan_window::<7, false>(&data, pos, &mut fs, &mut fe) };
                match r {
                    fast::Window::Line { end, ok } => {
                        acc += ok as u64 + fe[3] as u64;
                        pos = end + 1;
                    }
                    fast::Window::Long => break,
                }
            }
            acc
        });
    }
    time("prefetch-only line walk", lines, || {
        let mut pos = start;
        let mut acc = 0u64;
        while pos < fast_end {
            crate::util::prefetch(data.as_ptr().wrapping_add(pos + 2048) as *const i8);
            crate::util::prefetch(data.as_ptr().wrapping_add(pos + 2048 + 64) as *const i8);
            let nl = crate::csv::memchr_nl(&data, pos).unwrap();
            acc += data[pos] as u64;
            pos = nl + 1;
        }
        acc
    });

    time("scan_window + timestamps", lines, || {
        let mut fs = [0usize; 7];
        let mut fe = [0usize; 7];
        let mut pos = start;
        let mut acc = 0u64;
        while pos < fast_end {
            match fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) {
                fast::Window::Line { end, ok } => {
                    if ok {
                        acc = acc.wrapping_add(fast::timestamp(&data, fs[2], fe[2]).unwrap_or(0) as u64);
                        acc = acc.wrapping_add(fast::timestamp(&data, fs[3], fe[3]).unwrap_or(0) as u64);
                    }
                    pos = end + 1;
                }
                fast::Window::Long => break,
            }
        }
        acc
    });

    for (label, field) in [("scan_window + ts only", 2usize), ("scan_window + ingest only", 3usize)] {
        time(label, lines, || {
            let mut fs = [0usize; 7];
            let mut fe = [0usize; 7];
            let mut pos = start;
            let mut acc = 0u64;
            while pos < fast_end {
                match fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) {
                    fast::Window::Line { end, ok } => {
                        if ok {
                            acc = acc.wrapping_add(fast::timestamp(&data, fs[field], fe[field]).unwrap_or(0) as u64);
                        }
                        pos = end + 1;
                    }
                    fast::Window::Long => break,
                }
            }
            acc
        });
    }

    let months = fast::Months::new(crate::ts::days_from_civil(2025, 1, 1), crate::ts::days_from_civil(2028, 12, 31));
    for (label, field) in [("scan_window + ts (swar)", 2usize), ("scan_window + ingest (swar)", 3usize)] {
        time(label, lines, || {
            let mut fs = [0usize; 7];
            let mut fe = [0usize; 7];
            let mut pos = start;
            let mut acc = 0u64;
            while pos < fast_end {
                match fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) {
                    fast::Window::Line { end, ok } => {
                        if ok {
                            acc = acc.wrapping_add(fast::timestamp_m(&data, fs[field], fe[field], &months).unwrap_or(0) as u64);
                        }
                        pos = end + 1;
                    }
                    fast::Window::Long => break,
                }
            }
            acc
        });
    }

    time("scan_window + ids", lines, || {
        let mut fs = [0usize; 7];
        let mut fe = [0usize; 7];
        let mut pos = start;
        let mut acc = 0u64;
        while pos < fast_end {
            match fast::scan_window::<7, true>(&data, pos, &mut fs, &mut fe) {
                fast::Window::Line { end, ok } => {
                    if ok {
                        acc = acc.wrapping_add(fast::id_key(&data, fs[0], fe[0], b'e'));
                        acc = acc.wrapping_add(fast::id_key(&data, fs[1], fe[1], b'u'));
                    }
                    pos = end + 1;
                }
                fast::Window::Long => break,
            }
        }
        acc
    });
}

/// Scatter and table-lookup costs in isolation.
pub fn run_parts() {
    use crate::scatter::Scatter;
    use crate::util::{fib, take};
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct R {
        a: u64,
        b: u64,
        c: u64,
        d: u64,
    }
    let n = 4_000_000usize;
    let keys: Vec<u64> = (0..n as u64).map(|i| fib(i ^ 0x5555)).collect();
    for bits in [10u32, 12, 13] {
        let t = Instant::now();
        let mut sc: Scatter<R> = Scatter::new(1 << bits, 3 * n);
        for &k in &keys {
            sc.push(take(k, 0, bits), R { a: k, b: k, c: k, d: k });
        }
        sc.finish();
        let first = t.elapsed().as_secs_f64();
        let t = Instant::now();
        for &k in &keys {
            sc.push(take(k, 0, bits), R { a: k, b: k, c: k, d: k });
        }
        sc.finish();
        eprintln!("scatter 32B, {:5} parts: first {:5.2} ns/rec, warm {:5.2} ns/rec", 1 << bits, first * 1e9 / n as f64, t.elapsed().as_secs_f64() * 1e9 / n as f64);
    }
    for mb in [16usize, 64, 256] {
        let slots = mb << 17; // 8-byte slots
        let table: Vec<u64> = (0..slots as u64).map(|i| i.wrapping_mul(0x9E37)).collect();
        let mask = slots - 1;
        let t = Instant::now();
        let mut acc = 0u64;
        for &k in &keys {
            acc = acc.wrapping_add(table[(k as usize) & mask]);
        }
        let plain = t.elapsed().as_secs_f64();
        let t = Instant::now();
        let mut acc2 = 0u64;
        for i in 0..n {
            if i + 16 < n {
                unsafe {
                    std::arch::x86_64::_mm_prefetch(table.as_ptr().add((keys[i + 16] as usize) & mask) as *const i8, std::arch::x86_64::_MM_HINT_T0);
                }
            }
            acc2 = acc2.wrapping_add(table[(keys[i] as usize) & mask]);
        }
        eprintln!("lookup {mb:4} MB table: plain {:5.2} ns, prefetched {:5.2} ns ({})", plain * 1e9 / n as f64, t.elapsed().as_secs_f64() * 1e9 / n as f64, acc ^ acc2);
    }
}
