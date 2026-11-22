//! The whole run, phase by phase. Every phase runs on all threads and hands
//! its per-thread results to the next one.
//!
//! 1. links:   parse link lines, union-find, person table
//! 2. parse:   events (scattered by event ID for dedup) and clicks (scattered by person)
//! 3. dedup:   keep each event's earliest-ingested copy, drop invalid ones, scatter by person
//! 4. people:  sessions, attribution, assists, both tables' accumulators
//! 5. write:   Parquet

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
const SEGMENT: usize = build_knob(option_env!("SESSIONS_SEGMENT"), 2 << 20);
/// User keys use the low 47 bits of a pending record's `pk`.
const USER_MASK: u64 = (1 << ids::KEY_BITS) - 1;

/// A tuning constant, overridable at build time only (for A/B builds on the
/// scorer); unset means the default, so normal builds are unaffected.
const fn build_knob(v: Option<&str>, default: usize) -> usize {
    let Some(v) = v else { return default };
    let b = v.as_bytes();
    let mut n = 0usize;
    let mut i = 0;
    while i < b.len() {
        assert!(b[i].is_ascii_digit(), "build knob must be digits");
        n = n * 10 + (b[i] - b'0') as usize;
        i += 1;
    }
    assert!(n > 0, "build knob must be positive");
    n
}

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


/// Partition count (as bits) for `rows` records. Each thread stages one
/// cache line per partition, and each partition's tables grow as rows /
/// partitions: their sum is smallest near sqrt(rows / 3.2) partitions
/// (measured best: 2048 at 20M events; 4096 at 60M).
fn partition_bits(rows: usize) -> u32 {
    let ideal = ((rows as f64 / 3.2).sqrt()).max(1.0);
    (ideal.log2().round() as u32).clamp(4, 13)
}

/// How many parsed records later a prefetched person-table line is read.
const LOOKAHEAD: usize = build_knob(option_env!("SESSIONS_LOOKAHEAD"), 16);
/// How far ahead of the line being split its bytes are prefetched.
const PREFETCH_AHEAD: usize = build_knob(option_env!("SESSIONS_PREFETCH_AHEAD"), 2048);

/// Receives every well-formed line of a segment. A trait method marked
/// `inline(always)` (unlike a closure, which stable Rust cannot mark) keeps
/// the per-line work inside the line loop instead of behind a call.
trait LineSink<const N: usize> {
    fn line(&mut self, fs: &[usize; N], fe: &[usize; N]);
}

/// Cold paths (links, bench modes) still pass closures.
struct FnSink<F>(F);

impl<const N: usize, F: FnMut(&[usize; N], &[usize; N])> LineSink<N> for FnSink<F> {
    #[inline(always)]
    fn line(&mut self, fs: &[usize; N], fe: &[usize; N]) {
        (self.0)(fs, fe)
    }
}

/// Lines of one segment, through the window fast path when possible.
/// Calls `sink.line(fs, fe)` for every well-formed line with N fields.
#[inline(always)]
fn lines<const N: usize, S: LineSink<N>>(d: &[u8], seg: &Segment, sink: &mut S) {
    if PDEP_FAST.load(Ordering::Relaxed) {
        lines_with::<N, true, S>(d, seg, sink)
    } else {
        lines_with::<N, false, S>(d, seg, sink)
    }
}

/// Whether the window splitter may use pdep/pext (see `fast::pdep_is_fast`).
static PDEP_FAST: AtomicBool = AtomicBool::new(true);

#[inline(always)]
fn lines_with<const N: usize, const PDEP: bool, S: LineSink<N>>(d: &[u8], seg: &Segment, sink: &mut S) {
    let len = d.len();
    let mut pos = first_line(d, seg.start);
    let fast_end = seg.end.min(len.saturating_sub(128));
    let mut fs = [0usize; N];
    let mut fe = [0usize; N];
    while pos < seg.end && pos < len {
        // Page-cache pages are scattered 4 KiB frames: the hardware
        // prefetcher stops at each page boundary, so fetch well ahead.
        unsafe {
            let ahead = d.as_ptr().wrapping_add(pos + PREFETCH_AHEAD) as *const i8;
            std::arch::x86_64::_mm_prefetch(ahead, std::arch::x86_64::_MM_HINT_T0);
            std::arch::x86_64::_mm_prefetch(ahead.wrapping_add(64), std::arch::x86_64::_MM_HINT_T0);
        }
        let (end, ok) = match if pos < fast_end { fast::scan_window::<N, PDEP>(d, pos, &mut fs, &mut fe) } else { fast::Window::Long } {
            fast::Window::Line { end, ok } => (end, ok),
            fast::Window::Long => {
                let (end, n, quotes) = fast::scan::<N>(d, pos, &mut fs, &mut fe);
                (end, n == N && fast::unquote::<N>(d, &mut fs, &mut fe, quotes))
            }
        };
        if ok {
            sink.line(&fs, &fe);
        }
        pos = end + 1;
    }
}

/// Per-segment state of the events parse: valid copies wait LOOKAHEAD
/// records in a ring after their person-table line is prefetched.
struct EventSink<'p, 'a> {
    parser: &'p Parser<'a>,
    d: &'p [u8],
    links: &'p LinkTable,
    bits1: u32,
    out: &'p mut Scatter<Ev1>,
    ring: [Ev1; LOOKAHEAD],
    count: usize,
}

impl EventSink<'_, '_> {
    #[inline(always)]
    fn resolve(links: &LinkTable, bits1: u32, rec: Ev1, out: &mut Scatter<Ev1>) {
        let mut r = rec;
        r.pk = links.person_of(r.pk & USER_MASK) | (r.pk & !PERSON_MASK);
        out.push(take(fib(r.key), 0, bits1), r);
    }
}

impl LineSink<7> for EventSink<'_, '_> {
    #[inline(always)]
    fn line(&mut self, fs: &[usize; 7], fe: &[usize; 7]) {
        let Some(rec) = self.parser.event(self.d, fs, fe) else { return };
        if rec.tp & VALID == 0 {
            self.out.push(take(fib(rec.key), 0, self.bits1), rec);
            return;
        }
        self.links.prefetch(rec.pk & USER_MASK);
        let slot = unsafe { self.ring.get_unchecked_mut(self.count % LOOKAHEAD) };
        if self.count >= LOOKAHEAD {
            Self::resolve(self.links, self.bits1, *slot, self.out);
        }
        *slot = rec;
        self.count += 1;
    }
}

/// Per-segment state of the clicks parse, with the same lookahead ring.
struct ClickSink<'p, 'a> {
    parser: &'p Parser<'a>,
    d: &'p [u8],
    links: &'p LinkTable,
    bits2: u32,
    out: &'p mut Scatter<Ck2>,
    ring: [Ck2; LOOKAHEAD],
    count: usize,
}

impl ClickSink<'_, '_> {
    #[inline(always)]
    fn resolve(links: &LinkTable, bits2: u32, c: Ck2, out: &mut Scatter<Ck2>) {
        let mut c = c;
        c.pk = links.person_of(c.pk);
        out.push(take(fib(c.pk), 0, bits2), c);
    }
}

impl LineSink<4> for ClickSink<'_, '_> {
    #[inline(always)]
    fn line(&mut self, fs: &[usize; 4], fe: &[usize; 4]) {
        let Some(c) = self.parser.click(self.d, fs, fe) else { return };
        self.links.prefetch(c.pk);
        let slot = unsafe { self.ring.get_unchecked_mut(self.count % LOOKAHEAD) };
        if self.count >= LOOKAHEAD {
            Self::resolve(self.links, self.bits2, *slot, self.out);
        }
        *slot = c;
        self.count += 1;
    }
}

impl Parser<'_> {
    /// Parses one events segment into `out` (partitioned by event key).
    #[inline(never)]
    fn events_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits1: u32, out: &mut Scatter<Ev1>) {
        let mut sink = EventSink { parser: self, d, links, bits1, out, ring: [Ev1::default(); LOOKAHEAD], count: 0 };
        lines::<7, _>(d, seg, &mut sink);
        let EventSink { ring, count, out, .. } = sink;
        for i in count.saturating_sub(LOOKAHEAD)..count {
            EventSink::resolve(links, bits1, ring[i % LOOKAHEAD], out);
        }
    }

    /// Parses one clicks segment into `out` (partitioned by person).
    #[inline(never)]
    fn clicks_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits2: u32, out: &mut Scatter<Ck2>) {
        let mut sink = ClickSink { parser: self, d, links, bits2, out, ring: [Ck2::default(); LOOKAHEAD], count: 0 };
        lines::<4, _>(d, seg, &mut sink);
        let ClickSink { ring, count, out, .. } = sink;
        for i in count.saturating_sub(LOOKAHEAD)..count {
            ClickSink::resolve(links, bits2, ring[i % LOOKAHEAD], out);
        }
    }
}

pub fn run() {
    let started = Instant::now();
    let verbose = std::env::var_os("SESSIONS_VERBOSE").is_some();
    let mut last = started;
    let mut last_cpu = crate::util::cpu_times();
    let mut lap = |name: &str| {
        if verbose {
            let now = Instant::now();
            let cpu = crate::util::cpu_times();
            eprintln!(
                "[{:>8.1} ms] {:<10} {:>8.1} ms   user {:>7.1} sys {:>6.1} ms",
                now.duration_since(started).as_secs_f64() * 1e3,
                name,
                now.duration_since(last).as_secs_f64() * 1e3,
                (cpu.0 - last_cpu.0) * 1e3,
                (cpu.1 - last_cpu.1) * 1e3
            );
            last = now;
            last_cpu = cpu;
        }
    };

    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let out_dir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let threads = thread_count();
    let pdep = std::env::var("SESSIONS_PDEP").map_or_else(|_| fast::pdep_is_fast(), |v| v != "0");
    PDEP_FAST.store(pdep, Ordering::Relaxed);

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
    if verbose {
        eprintln!("    {threads} threads, window splitter: {}", if pdep { "pdep" } else { "tzcnt" });
    }

    // Since V5 about one in eight distinct IDs is long (interned): size the
    // interner's tables for that share of the lines up front.
    let input_bytes: usize = maps.iter().map(|m| m.bytes().len()).sum();
    let interner = Interner::new(input_bytes / 40 / 8 + 1024);
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
            lines::<2, _>(d, seg, &mut FnSink(|fs: &[usize; 2], fe: &[usize; 2]| {
                if fs[0] != fe[0] && fs[1] != fe[1] {
                    pairs.push((parser.key(d, fs[0], fe[0], b'u'), parser.key(d, fs[1], fe[1], b'u')));
                }
            }));
        }
        pairs
    });
    let odd_after_links = interner.view();
    let link_table = links::build(&pairs, threads, &odd_after_links);
    // The view holds every interner shard's lock: release it before parsing.
    drop(odd_after_links);
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
                lines::<7, _>(d, seg, &mut FnSink(|fs: &[usize; 7], fe: &[usize; 7]| {
                    count += 1;
                    if mode >= 2 {
                        if let Some(rec) = parser.event(d, fs, fe) {
                            acc = acc.wrapping_add(rec.key ^ rec.pk ^ rec.tp);
                        }
                    } else {
                        acc = acc.wrapping_add((fe[6] - fs[0]) as u64);
                    }
                }));
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
    // Partition counts balance the scatter's staging lines against the size
    // of each partition's dedup and person tables (see `partition_bits`).
    let bits_override = std::env::var("SESSIONS_BITS").ok().and_then(|v| v.parse::<u32>().ok());
    let bits1 = bits_override.unwrap_or_else(|| partition_bits(est_events));
    let bits2 = bits_override.unwrap_or_else(|| partition_bits(est_events + est_clicks));
    if verbose {
        eprintln!("    partitions: events {} / persons {}", 1 << bits1, 1 << bits2);
    }
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
        // Distinct event IDs of the partition, densely, with an index table
        // (u32, EMPTY when free) to find them by key.
        const EMPTY: u32 = u32::MAX;
        let mut recs: Vec<Ev1> = Vec::new();
        let mut index: Vec<u32> = Vec::new();
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
            assert!(n < EMPTY as usize, "partition too large");
            let cap = (n * 2).next_power_of_two();
            let tbits = cap.trailing_zeros();
            let mask = cap - 1;
            if index.len() < cap {
                index.resize(cap, EMPTY);
            }
            recs.clear();
            recs.reserve(n);
            for chunk in &chunks {
                for rec in chunk.iter() {
                    let mut h = take(fib(rec.key), bits1, tbits);
                    loop {
                        let i = unsafe { *index.get_unchecked(h) };
                        if i == EMPTY {
                            unsafe { *index.get_unchecked_mut(h) = recs.len() as u32 };
                            recs.push(*rec);
                            break;
                        }
                        let kept = unsafe { recs.get_unchecked_mut(i as usize) };
                        if kept.key == rec.key {
                            if rec.ingest < kept.ingest {
                                *kept = *rec;
                            }
                            break;
                        }
                        h = (h + 1) & mask;
                    }
                }
            }
            for r in &recs {
                if r.tp & VALID != 0 {
                    let ev = Ev2 { pk: r.pk, t: r.tp as u32, price: ((r.tp >> 32) & 0xFF_FFFF) as u32 };
                    out.push(take(fib(r.pk & PERSON_MASK), 0, bits2), ev);
                }
            }
            index[..cap].fill(EMPTY);
        }
        out.finish();
        out
    });
    lap("dedup");

    // ---- 4. People: largest partitions first.
    let odd = interner.view();
    let ctx = person::Ctx { base, first_day, days, campaigns: campaigns.len(), odd: &odd };
    let mut order: Vec<(usize, usize)> = (0..p2)
        .map(|q| {
            let n: usize = survivors.iter().map(|s| s.len(q)).sum::<usize>()
                + parsed.iter().map(|(_, c)| c.len(q)).sum::<usize>();
            (n, q)
        })
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    if verbose {
        let total: usize = order.iter().map(|o| o.0).sum();
        eprintln!("    partitions {}: largest {:?}, total {total}", order.len(), &order.iter().take(6).map(|o| o.0).collect::<Vec<_>>());
    }
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
            let tw = Instant::now();
            output::write_user_daily(&user_dir.join(format!("part-{t:03}.parquet")), &out, &ctx, &link_table, &products);
            if verbose {
                eprintln!("    write_user_daily[{t}]: {} rows, {:.1} ms", out.rows.len(), tw.elapsed().as_secs_f64() * 1e3);
            }
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
    if verbose {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines().filter(|l| l.starts_with("VmHWM") || l.starts_with("VmRSS")) {
                eprintln!("{line}");
            }
        }
    }
    // Every output file is written and flushed (write_chunks flushes and
    // returns its errors). Exit now: dropping gigabytes of buffers one by one
    // only delays the exit the scorer is timing; the kernel frees them anyway.
    crate::util::exit_now();
}

#[cfg(test)]
mod knob_tests {
    #[test]
    fn defaults_when_unset() {
        if option_env!("SESSIONS_SEGMENT").is_none() {
            assert_eq!(super::SEGMENT, 2 << 20);
        }
        if option_env!("SESSIONS_LOOKAHEAD").is_none() {
            assert_eq!(super::LOOKAHEAD, 16);
        }
        if option_env!("SESSIONS_PREFETCH_AHEAD").is_none() {
            assert_eq!(super::PREFETCH_AHEAD, 2048);
        }
        assert_eq!(super::build_knob(Some("4096"), 1), 4096);
        assert_eq!(super::build_knob(None, 7), 7);
    }
}
