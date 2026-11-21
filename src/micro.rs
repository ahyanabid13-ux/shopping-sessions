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
            match fast::scan_window::<7>(&data, pos, &mut fs, &mut fe) {
                fast::Window::Line { end, ok } => {
                    acc += ok as u64 + fe[3] as u64;
                    pos = end + 1;
                }
                fast::Window::Long => break,
            }
        }
        acc
    });

    time("scan_window + timestamps", lines, || {
        let mut fs = [0usize; 7];
        let mut fe = [0usize; 7];
        let mut pos = start;
        let mut acc = 0u64;
        while pos < fast_end {
            match fast::scan_window::<7>(&data, pos, &mut fs, &mut fe) {
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
                match fast::scan_window::<7>(&data, pos, &mut fs, &mut fe) {
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
                match fast::scan_window::<7>(&data, pos, &mut fs, &mut fe) {
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
            match fast::scan_window::<7>(&data, pos, &mut fs, &mut fe) {
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
