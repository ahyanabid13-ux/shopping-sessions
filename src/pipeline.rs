//! The whole run, phase by phase. Every phase runs on all threads and hands
//! its per-thread results to the next one.
//!
//! 1. links:   parse link lines, union-find, person table
//! 2. parse:   events (scattered by event ID for dedup) and clicks (scattered by person)
//! 3. dedup:   keep each event's earliest-ingested copy, drop invalid ones, scatter by person
//! 4. people:  sessions, attribution, assists, both tables' accumulators
//! 5. write:   Parquet

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::fast;
use crate::ids::{self, Interner};
use crate::links::{self, LinkTable, PERSON_MASK};
use crate::lookup::{self, Campaigns, Products, NOT_A_CAMPAIGN};
use crate::output;
use crate::person::{self, Ck2, Ev2, REACH_MS};
use crate::scatter::Scatter;
use crate::ts::DAY;
use crate::util::{fib, par_run, take, thread_count, Mmap};

const LATE_MS: i64 = 86_400_000;
const MAX_PRICE: u64 = 10_000_000;
const SEGMENT: usize = 2 << 20;
/// User keys use the low 47 bits of a pending record's `pk`.
const USER_MASK: u64 = (1 << ids::KEY_BITS) - 1;

/// An event copy as parsed, before dedup.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Ev1 {
    key: u64,
    ingest: i64,
    /// Person key (48 bits) and category (16 bits); 0 when invalid.
    /// While waiting in the prefetch ring it holds the user key instead.
    pk: u64,
    /// Time since `base` (32 bits), price (bits 32..56), VALID (bit 63).
    tp: u64,
}

const VALID: u64 = 1 << 63;

struct Segment {
    file: usize,
    start: usize,
    end: usize,
}

fn csv_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return files };
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("list {}: {e}", dir.display()));
        let path = entry.path();
        let is_csv = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".csv"));
        if is_csv && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    files
}

fn segments(maps: &[Mmap], first_file: usize, count: usize) -> Vec<Segment> {
    let mut out = Vec::new();
    for file in first_file..first_file + count {
        let len = maps[file].bytes().len();
        let mut start = 0;
        while start < len {
            let end = (start + SEGMENT).min(len);
            out.push(Segment { file, start, end });
            start = end;
        }
    }
    out
}

/// Start of the first line owned by a segment beginning at `start`: the
/// header is skipped at offset 0, and a line already begun belongs to the
/// previous segment.
#[inline]
fn first_line(data: &[u8], start: usize) -> usize {
    if start > 0 && data[start - 1] == b'\n' {
        return start;
    }
    match crate::csv::memchr_nl(data, start) {
        Some(nl) => nl + 1,
        None => data.len(),
    }
}

struct Parser<'a> {
    window_start: i64,
    window_end: i64,
    click_start: i64,
    base: i64,
    products: &'a Products,
    campaigns: &'a Campaigns,
    interner: &'a Interner,
}

impl Parser<'_> {
    #[inline(always)]
    fn key(&self, d: &[u8], s: usize, e: usize, prefix: u8) -> u64 {
        let k = fast::id_key(d, s, e, prefix);
        if k != 0 {
            k
        } else {
            self.interner.intern(&d[s..e])
        }
    }

    #[inline(always)]
    fn price(b: &[u8]) -> Option<u64> {
        if b.is_empty() {
            return None;
        }
        let mut v: u64 = 0;
        for &c in b {
            let d = c.wrapping_sub(b'0');
            if d >= 10 {
                return None;
            }
            v = (v * 10 + d as u64).min(MAX_PRICE + 1);
        }
        (1..=MAX_PRICE).contains(&v).then_some(v)
    }

    /// One well-formed event line. None when its event_id is empty. A valid
    /// copy comes back with VALID set and its user key (and category) in `pk`.
    #[inline(always)]
    fn event(&self, d: &[u8], fs: &[usize; 7], fe: &[usize; 7]) -> Option<Ev1> {
        if fs[0] == fe[0] {
            return None;
        }
        // rules.md: ingest_ts is always valid F1. A line without one cannot be ordered.
        let ingest = fast::timestamp(d, fs[3], fe[3])?;
        let key = self.key(d, fs[0], fe[0], b'e');
        let mut rec = Ev1 { key, ingest, pk: 0, tp: 0 };
        if fs[1] == fe[1] || fs[5] == fe[5] {
            return Some(rec);
        }
        let Some(t) = fast::timestamp(d, fs[2], fe[2]) else { return Some(rec) };
        if t < self.window_start || t >= self.window_end {
            return Some(rec);
        }
        // event_type is exactly view, cart or purchase: fixed-width compares.
        let purchase = match fe[4] - fs[4] {
            4 => {
                let w = unsafe { (d.as_ptr().add(fs[4]) as *const u32).read_unaligned() };
                if w != u32::from_le_bytes(*b"view") && w != u32::from_le_bytes(*b"cart") {
                    return Some(rec);
                }
                false
            }
            8 => {
                let w = unsafe { (d.as_ptr().add(fs[4]) as *const u64).read_unaligned() };
                if w != u64::from_le_bytes(*b"purchase") {
                    return Some(rec);
                }
                true
            }
            _ => return Some(rec),
        };
        let mut amount = 0u64;
        let mut category = 0u16;
        if purchase {
            let Some(v) = Self::price(&d[fs[6]..fe[6]]) else { return Some(rec) };
            amount = v;
            category = self.products.category(&d[fs[5]..fe[5]]);
        }
        if ingest - t > LATE_MS {
            return Some(rec);
        }
        rec.pk = self.key(d, fs[1], fe[1], b'u') | (category as u64) << 48;
        rec.tp = (t - self.base) as u64 | amount << 32 | VALID;
        Some(rec)
    }

    /// One well-formed click line: Some only for a valid click, with its
    /// user key in `pk`.
    #[inline(always)]
    fn click(&self, d: &[u8], fs: &[usize; 4], fe: &[usize; 4]) -> Option<Ck2> {
        if fs[0] == fe[0] || fs[1] == fe[1] {
            return None;
        }
        let t = fast::timestamp(d, fs[3], fe[3])?;
        if t < self.click_start || t >= self.window_end {
            return None;
        }
        let c = self.campaigns.index(&d[fs[2]..fe[2]]);
        if c == NOT_A_CAMPAIGN {
            return None;
        }
        let key = self.key(d, fs[0], fe[0], b'k');
        let user = self.key(d, fs[1], fe[1], b'u');
        Some(Ck2 { pk: user, key, t: (t - self.base) as u32, campaign: c, _pad: 0 })
    }
}


fn partition_bits(rows: usize, per_partition: usize) -> u32 {
    let wanted = (rows / per_partition).max(1).next_power_of_two();
    wanted.trailing_zeros().clamp(4, 14)
}

/// Records whose person-table slot was prefetched, resolved in one go.
const BATCH: usize = 32;

/// Lines of one segment, through the window fast path when possible.
/// Calls `f(fs, fe)` for every well-formed line with N fields.
#[inline(always)]
fn lines<const N: usize>(d: &[u8], seg: &Segment, mut f: impl FnMut(&[usize; N], &[usize; N])) {
    let len = d.len();
    let mut pos = first_line(d, seg.start);
    let fast_end = seg.end.min(len.saturating_sub(128));
    let mut fs = [0usize; N];
    let mut fe = [0usize; N];
    while pos < seg.end && pos < len {
        let (end, ok) = match if pos < fast_end { fast::scan_window::<N>(d, pos, &mut fs, &mut fe) } else { fast::Window::Long } {
            fast::Window::Line { end, ok } => (end, ok),
            fast::Window::Long => {
                let (end, n, quotes) = fast::scan::<N>(d, pos, &mut fs, &mut fe);
                (end, n == N && fast::unquote::<N>(d, &mut fs, &mut fe, quotes))
            }
        };
        if ok {
            f(&fs, &fe);
        }
        pos = end + 1;
    }
}

impl Parser<'_> {
    /// Parses one events segment into `out` (partitioned by event key).
    #[inline(never)]
    fn events_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits1: u32, out: &mut Scatter<Ev1>) {
        let mut batch = [Ev1::default(); BATCH];
        let mut n = 0usize;
        let flush = |batch: &[Ev1], out: &mut Scatter<Ev1>| {
            for rec in batch {
                let mut r = *rec;
                r.pk = links.person_of(r.pk & USER_MASK) | (r.pk & !PERSON_MASK);
                out.push(take(fib(r.key), 0, bits1), r);
            }
        };
        lines::<7>(d, seg, |fs, fe| {
            let Some(rec) = self.event(d, fs, fe) else { return };
            if rec.tp & VALID == 0 {
                out.push(take(fib(rec.key), 0, bits1), rec);
                return;
            }
            links.prefetch(rec.pk & USER_MASK);
            batch[n] = rec;
            n += 1;
            if n == BATCH {
                flush(&batch, out);
                n = 0;
            }
        });
        flush(&batch[..n], out);
    }

    /// Parses one clicks segment into `out` (partitioned by person).
    #[inline(never)]
    fn clicks_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits2: u32, out: &mut Scatter<Ck2>) {
        let mut batch = [Ck2::default(); BATCH];
        let mut n = 0usize;
        let flush = |batch: &[Ck2], out: &mut Scatter<Ck2>| {
            for c in batch {
                let mut c = *c;
                c.pk = links.person_of(c.pk);
                out.push(take(fib(c.pk), 0, bits2), c);
            }
        };
        lines::<4>(d, seg, |fs, fe| {
            let Some(c) = self.click(d, fs, fe) else { return };
            links.prefetch(c.pk);
            batch[n] = c;
            n += 1;
            if n == BATCH {
                flush(&batch, out);
                n = 0;
            }
        });
        flush(&batch[..n], out);
    }
}

pub fn run() {
    let started = Instant::now();
    let verbose = std::env::var_os("SESSIONS_VERBOSE").is_some();
    let mut last = started;
    let mut lap = |name: &str| {
        if verbose {
            let now = Instant::now();
            eprintln!(
                "[{:>8.1} ms] {:<10} {:>8.1} ms",
                now.duration_since(started).as_secs_f64() * 1e3,
                name,
                now.duration_since(last).as_secs_f64() * 1e3
            );
            last = now;
        }
    };

    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let out_dir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let threads = thread_count();

    let window = lookup::read_manifest(&data.join("manifest.json"));
    let products = Products::read(&data.join("products.csv"));
    let campaigns = Campaigns::read(&data.join("campaigns.csv"));
    let base = window.start - REACH_MS;
    assert!(window.end - base < u32::MAX as i64, "window longer than this build supports");
    let first_day = window.start.div_euclid(DAY);
    let days = ((window.end - 1).div_euclid(DAY) - first_day + 1) as usize;

    let event_files = csv_files(&data.join("events"));
    let click_files = csv_files(&data.join("clicks"));
    let link_files = csv_files(&data.join("links"));
    let all_files: Vec<&PathBuf> = event_files.iter().chain(&click_files).chain(&link_files).collect();
    let maps: Vec<Mmap> = all_files
        .iter()
        .map(|p| Mmap::open(p).unwrap_or_else(|e| panic!("open {}: {e}", p.display())))
        .collect();
    let (n_ev, n_ck, n_ln) = (event_files.len(), click_files.len(), link_files.len());
    let event_bytes: usize = maps[..n_ev].iter().map(|m| m.bytes().len()).sum();
    let click_bytes: usize = maps[n_ev..n_ev + n_ck].iter().map(|m| m.bytes().len()).sum();
    let event_segments = segments(&maps, 0, n_ev);
    let click_segments = segments(&maps, n_ev, n_ck);
    let link_segments = segments(&maps, n_ev + n_ck, n_ln);
    lap("setup");

    let interner = Interner::new();
    let parser = Parser {
        window_start: window.start,
        window_end: window.end,
        click_start: window.start - REACH_MS,
        base,
        products: &products,
        campaigns: &campaigns,
        interner: &interner,
    };

    // ---- 1. Links.
    let next = AtomicUsize::new(0);
    let pairs: Vec<Vec<(u64, u64)>> = par_run(threads, |_| {
        let mut pairs = Vec::new();
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(seg) = link_segments.get(at) else { break };
            let d = maps[seg.file].bytes();
            lines::<2>(d, seg, |fs, fe| {
                if fs[0] != fe[0] && fs[1] != fe[1] {
                    pairs.push((parser.key(d, fs[0], fe[0], b'u'), parser.key(d, fs[1], fe[1], b'u')));
                }
            });
        }
        pairs
    });
    let odd_after_links = interner.snapshot();
    let link_table = links::build(&pairs, threads, &odd_after_links);
    drop(pairs);
    lap("links");

    // Internal profiling aid: SESSIONS_BENCH=1 scans lines only, =2 also
    // parses event fields; both stop after the parse phase.
    if let Some(mode) = std::env::var("SESSIONS_BENCH").ok().and_then(|v| v.parse::<u32>().ok()) {
        if mode == 0 {
            return;
        }
        let next = AtomicUsize::new(0);
        let work: Vec<&Segment> = event_segments.iter().collect();
        let counts: Vec<(usize, u64)> = par_run(threads, |_| {
            let (mut count, mut acc) = (0usize, 0u64);
            loop {
                let at = next.fetch_add(1, Ordering::Relaxed);
                let Some(seg) = work.get(at) else { break };
                let d = maps[seg.file].bytes();
                lines::<7>(d, seg, |fs, fe| {
                    count += 1;
                    if mode >= 2 {
                        if let Some(rec) = parser.event(d, fs, fe) {
                            acc = acc.wrapping_add(rec.key ^ rec.pk ^ rec.tp);
                        }
                    } else {
                        acc = acc.wrapping_add((fe[6] - fs[0]) as u64);
                    }
                });
            }
            (count, acc)
        });
        lap("bench");
        eprintln!("bench mode {mode}: {} lines, {}", counts.iter().map(|c| c.0).sum::<usize>(), counts.iter().map(|c| c.1).fold(0, u64::wrapping_add));
        return;
    }

    // ---- 2. Parse events and clicks.
    // Rough line counts size the partitions: ~80 bytes per event line, ~45 per click line.
    let est_events = event_bytes / 80 + 1;
    let est_clicks = click_bytes / 45 + 1;
    // Partitions small enough that the dedup and person tables stay in a
    // 1 MiB L2 (the scorers' Cascade Lake cores).
    let bits1 = partition_bits(est_events, 6_144);
    let bits2 = partition_bits(est_events + est_clicks, 8_192);
    let (p1, p2) = (1usize << bits1, 1usize << bits2);
    let work: Vec<&Segment> = event_segments.iter().chain(&click_segments).collect();
    let next = AtomicUsize::new(0);
    let parsed: Vec<(Scatter<Ev1>, Scatter<Ck2>)> = par_run(threads, |_| {
        let mut evs: Scatter<Ev1> = Scatter::new(p1, est_events / threads + 1);
        let mut cks: Scatter<Ck2> = Scatter::new(p2, est_clicks / threads + 1);
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(seg) = work.get(at) else { break };
            let d = maps[seg.file].bytes();
            if seg.file < n_ev {
                parser.events_segment(d, seg, &link_table, bits1, &mut evs);
            } else {
                parser.clicks_segment(d, seg, &link_table, bits2, &mut cks);
            }
        }
        evs.finish();
        cks.finish();
        (evs, cks)
    });
    lap("parse");

    // ---- 3. Dedup: per event ID, the copy with the smallest ingest_ts decides.
    // An open-addressing table of whole records per partition; then one
    // sequential sweep emits the valid winners and clears the table.
    let next = AtomicUsize::new(0);
    let survivors: Vec<Scatter<Ev2>> = par_run(threads, |_| {
        let mut out: Scatter<Ev2> = Scatter::new(p2, est_events / threads + 1);
        let mut table: Vec<Ev1> = Vec::new();
        let mut chunks: Vec<&[Ev1]> = Vec::new();
        loop {
            let p = next.fetch_add(1, Ordering::Relaxed);
            if p >= p1 {
                break;
            }
            chunks.clear();
            for (evs, _) in &parsed {
                evs.chunks(p, &mut chunks);
            }
            let n: usize = chunks.iter().map(|c| c.len()).sum();
            if n == 0 {
                continue;
            }
            let cap = (n * 2).next_power_of_two();
            let tbits = cap.trailing_zeros();
            let mask = cap - 1;
            if table.len() < cap {
                table.resize(cap, Ev1::default());
            }
            for chunk in &chunks {
                for rec in chunk.iter() {
                    let mut h = take(fib(rec.key), bits1, tbits);
                    loop {
                        let slot = unsafe { table.get_unchecked_mut(h) };
                        if slot.key == 0 {
                            *slot = *rec;
                            break;
                        }
                        if slot.key == rec.key {
                            if rec.ingest < slot.ingest {
                                *slot = *rec;
                            }
                            break;
                        }
                        h = (h + 1) & mask;
                    }
                }
            }
            for slot in table[..cap].iter_mut() {
                if slot.key == 0 {
                    continue;
                }
                if slot.tp & VALID != 0 {
                    let ev = Ev2 { pk: slot.pk, t: slot.tp as u32, price: ((slot.tp >> 32) & 0xFF_FFFF) as u32 };
                    out.push(take(fib(slot.pk & PERSON_MASK), 0, bits2), ev);
                }
                slot.key = 0;
            }
        }
        out.finish();
        out
    });
    lap("dedup");

    // ---- 4. People: largest partitions first.
    let odd = interner.snapshot();
    let ctx = person::Ctx { base, first_day, days, campaigns: campaigns.len(), odd: &odd };
    let mut order: Vec<(usize, usize)> = (0..p2)
        .map(|q| {
            let n: usize = survivors.iter().map(|s| s.len(q)).sum::<usize>()
                + parsed.iter().map(|(_, c)| c.len(q)).sum::<usize>();
            (n, q)
        })
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let next = AtomicUsize::new(0);
    let user_dir = out_dir.join("user_daily");
    let campaign_dir = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&user_dir).unwrap_or_else(|e| panic!("create {}: {e}", user_dir.display()));
    std::fs::create_dir_all(&campaign_dir).unwrap_or_else(|e| panic!("create {}: {e}", campaign_dir.display()));
    let outs: Vec<person::Out> = par_run(threads, |t| {
        let mut out = person::Out::new(&ctx);
        let mut scratch = person::Scratch::default();
        let mut evs: Vec<&[Ev2]> = Vec::new();
        let mut cks: Vec<&[Ck2]> = Vec::new();
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(&(_, q)) = order.get(at) else { break };
            evs.clear();
            cks.clear();
            for s in &survivors {
                s.chunks(q, &mut evs);
            }
            for (_, c) in &parsed {
                c.chunks(q, &mut cks);
            }
            person::process(&evs, &cks, bits2, &ctx, &mut scratch, &mut out);
        }
        // Each thread writes its own user_daily file as soon as it is done.
        if !out.rows.is_empty() {
            output::write_user_daily(&user_dir.join(format!("part-{t:03}.parquet")), &out, &ctx, &link_table, &products);
        }
        out
    });
    lap("people");

    // ---- 5. Write the small table (and an empty user_daily file if no rows at all).
    if outs.iter().all(|o| o.rows.is_empty()) {
        output::write_user_daily(&user_dir.join("part-000.parquet"), &outs[0], &ctx, &link_table, &products);
    }
    output::write_campaign_daily(&campaign_dir.join("part-000.parquet"), &outs, &ctx, &campaigns);
    lap("write");
}
