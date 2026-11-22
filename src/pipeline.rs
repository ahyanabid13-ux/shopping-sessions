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
use crate::long::LongTable;
use crate::lookup::{self, Campaigns, Products, NOT_A_CAMPAIGN};
use crate::output;
use crate::person::{self, Ck2, Ev2, CLICK_LONG, REACH_MS};
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
/// In a pending record's `tp`: the event_id is long (`key`, `key2` hold it).
const LONG_EVENT: u64 = 1 << 62;
/// In a pending record's `tp`: the user_id is long (`uhi`, `ulo` hold it).
const LONG_USER: u64 = 1 << 61;
/// In a pending click's `pk`: the user_id is long.
const CK_LONG_USER: u64 = 1 << 62;

/// An event copy whose event_id is long (`e` + 32 hex digits), before dedup.
/// Same fields as `Ev1`, with the 128-bit ID in (hi, lo).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Ev1L {
    hi: u64,
    lo: u64,
    ingest: i64,
    pk: u64,
    tp: u64,
    _pad: u64,
}

/// An event copy as parsed: `key` (or `key`, `key2` when LONG_EVENT), and
/// for a valid copy its user key and category in `pk`, or the category in
/// `pk` and the long user ID in (`uhi`, `ulo`) when LONG_USER.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Pend {
    key: u64,
    key2: u64,
    ingest: i64,
    pk: u64,
    uhi: u64,
    ulo: u64,
    tp: u64,
    _pad: u64,
}

/// Partition (and dedup hash) of a long event ID.
#[inline(always)]
fn long_hash(hi: u64, lo: u64) -> u64 {
    fib(lo ^ fib(hi))
}

/// Person key of the long user ID (hi, lo): its linked person, else its
/// own `LongTable` key, or through the exact interner when the table
/// refuses it.
#[inline(always)]
fn long_person(longs: &LongTable, links: &LinkTable, interner: &Interner, hi: u64, lo: u64) -> u64 {
    match longs.slot(hi, lo) {
        Some(s) => {
            let p = longs.person(s);
            if p != 0 {
                p
            } else {
                ids::ODD | ids::LONG | s as u64
            }
        }
        None => links.person_of(intern_long(interner, hi, lo)),
    }
}

/// The odd key of the long user ID (hi, lo), from its exact bytes (`hex_key`
/// accepts only the canonical form, so they are rebuilt exactly).
#[cold]
#[inline(never)]
fn intern_long(interner: &Interner, hi: u64, lo: u64) -> u64 {
    let mut b = Vec::with_capacity(33);
    ids::write_long(&mut b, b'u', hi, lo);
    interner.intern(&b)
}

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
    longs: &'a LongTable,
}

impl Parser<'_> {
    /// User key of a link side: fast, long (its `LongTable` slot), or interned.
    #[inline(always)]
    fn link_key(&self, d: &[u8], s: usize, e: usize) -> u64 {
        let k = fast::id_key(d, s, e, b'u');
        if k != 0 {
            return k;
        }
        if let Some((hi, lo)) = fast::hex_key(d, s, e, b'u') {
            if let Some(slot) = self.longs.slot(hi, lo) {
                return ids::ODD | ids::LONG | slot as u64;
            }
        }
        self.interner.intern(&d[s..e])
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
    /// copy comes back with VALID set and its user key (and category) in `pk`
    /// (see `Pend` for long IDs).
    #[inline(always)]
    fn event(&self, d: &[u8], fs: &[usize; 7], fe: &[usize; 7]) -> Option<Pend> {
        if fs[0] == fe[0] {
            return None;
        }
        // rules.md: ingest_ts is always valid F1. A line without one cannot be ordered.
        let ingest = fast::timestamp(d, fs[3], fe[3])?;
        let mut rec = Pend { ingest, ..Pend::default() };
        let k = fast::id_key(d, fs[0], fe[0], b'e');
        if k != 0 {
            rec.key = k;
        } else if let Some((hi, lo)) = fast::hex_key(d, fs[0], fe[0], b'e') {
            rec.key = hi;
            rec.key2 = lo;
            rec.tp = LONG_EVENT;
        } else {
            rec.key = self.interner.intern(&d[fs[0]..fe[0]]);
        }
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
        let uk = fast::id_key(d, fs[1], fe[1], b'u');
        if uk != 0 {
            rec.pk = uk;
        } else if let Some((hi, lo)) = fast::hex_key(d, fs[1], fe[1], b'u') {
            rec.uhi = hi;
            rec.ulo = lo;
            rec.tp |= LONG_USER;
        } else {
            rec.pk = self.interner.intern(&d[fs[1]..fe[1]]);
        }
        rec.pk |= (category as u64) << 48;
        rec.tp |= (t - self.base) as u64 | amount << 32 | VALID;
        Some(rec)
    }

    /// One well-formed click line: Some only for a valid click, with its
    /// user key in `pk`, or CK_LONG_USER in `pk` and the long user ID in the
    /// other two values. A long click_id sets CLICK_LONG (see `Ck2`).
    #[inline(always)]
    fn click(&self, d: &[u8], fs: &[usize; 4], fe: &[usize; 4]) -> Option<(Ck2, u64, u64)> {
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
        let mut ck = Ck2 { pk: 0, key: 0, t: (t - self.base) as u32, campaign: c, key2: 0 };
        let k = fast::id_key(d, fs[0], fe[0], b'k');
        if k != 0 {
            ck.key = k;
        } else if let Some((hi, lo)) = fast::hex_key(d, fs[0], fe[0], b'k') {
            ck.key = hi;
            ck.key2 = lo;
            ck.pk = CLICK_LONG;
        } else {
            ck.key = self.interner.intern(&d[fs[0]..fe[0]]);
        }
        let (mut uhi, mut ulo) = (0, 0);
        let uk = fast::id_key(d, fs[1], fe[1], b'u');
        if uk != 0 {
            ck.pk |= uk;
        } else if let Some((hi, lo)) = fast::hex_key(d, fs[1], fe[1], b'u') {
            (uhi, ulo) = (hi, lo);
            ck.pk |= CK_LONG_USER;
        } else {
            ck.pk |= self.interner.intern(&d[fs[1]..fe[1]]);
        }
        Some((ck, uhi, ulo))
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
const LOOKAHEAD: usize = 16;
/// How far ahead of the line being split its bytes are prefetched.
const PREFETCH_AHEAD: usize = 2048;

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
/// records in a ring after their person-table line is prefetched. Copies
/// with a long event or user ID use a second ring of wider records.
struct EventSink<'p, 'a> {
    parser: &'p Parser<'a>,
    d: &'p [u8],
    links: &'p LinkTable,
    bits1: u32,
    bits_l: u32,
    out: &'p mut Scatter<Ev1>,
    out_l: &'p mut Scatter<Ev1L>,
    ring: [Ev1; LOOKAHEAD],
    count: usize,
    wide: [Pend; LOOKAHEAD],
    wide_count: usize,
}

impl EventSink<'_, '_> {
    #[inline(always)]
    fn resolve(links: &LinkTable, bits1: u32, rec: Ev1, out: &mut Scatter<Ev1>) {
        let mut r = rec;
        r.pk = links.person_of(r.pk & USER_MASK) | (r.pk & !PERSON_MASK);
        out.push(take(fib(r.key), 0, bits1), r);
    }

    /// A valid copy from the wide ring, to `out` or (long event ID) `out_l`.
    #[inline(always)]
    fn resolve_wide(parser: &Parser, links: &LinkTable, bits1: u32, bits_l: u32, p: Pend, out: &mut Scatter<Ev1>, out_l: &mut Scatter<Ev1L>) {
        let person = if p.tp & LONG_USER != 0 {
            long_person(parser.longs, links, parser.interner, p.uhi, p.ulo)
        } else {
            links.person_of(p.pk & USER_MASK)
        };
        let pk = person | (p.pk & !PERSON_MASK);
        let tp = p.tp & !(LONG_EVENT | LONG_USER);
        if p.tp & LONG_EVENT != 0 {
            out_l.push(take(long_hash(p.key, p.key2), 0, bits_l), Ev1L { hi: p.key, lo: p.key2, ingest: p.ingest, pk, tp, _pad: 0 });
        } else {
            out.push(take(fib(p.key), 0, bits1), Ev1 { key: p.key, ingest: p.ingest, pk, tp });
        }
    }
}

impl LineSink<7> for EventSink<'_, '_> {
    #[inline(always)]
    fn line(&mut self, fs: &[usize; 7], fe: &[usize; 7]) {
        let Some(p) = self.parser.event(self.d, fs, fe) else { return };
        if p.tp & (VALID | LONG_EVENT | LONG_USER) == VALID {
            let rec = Ev1 { key: p.key, ingest: p.ingest, pk: p.pk, tp: p.tp };
            self.links.prefetch(rec.pk & USER_MASK);
            let slot = unsafe { self.ring.get_unchecked_mut(self.count % LOOKAHEAD) };
            if self.count >= LOOKAHEAD {
                Self::resolve(self.links, self.bits1, *slot, self.out);
            }
            *slot = rec;
            self.count += 1;
            return;
        }
        if p.tp & VALID == 0 {
            // Invalid copies still take part in the dedup (pk = tp = 0).
            if p.tp & LONG_EVENT != 0 {
                let rec = Ev1L { hi: p.key, lo: p.key2, ingest: p.ingest, pk: 0, tp: 0, _pad: 0 };
                self.out_l.push(take(long_hash(p.key, p.key2), 0, self.bits_l), rec);
            } else {
                self.out.push(take(fib(p.key), 0, self.bits1), Ev1 { key: p.key, ingest: p.ingest, pk: 0, tp: 0 });
            }
            return;
        }
        if p.tp & LONG_USER != 0 {
            self.parser.longs.prefetch(p.uhi, p.ulo);
        } else {
            self.links.prefetch(p.pk & USER_MASK);
        }
        let slot = unsafe { self.wide.get_unchecked_mut(self.wide_count % LOOKAHEAD) };
        if self.wide_count >= LOOKAHEAD {
            Self::resolve_wide(self.parser, self.links, self.bits1, self.bits_l, *slot, self.out, self.out_l);
        }
        *slot = p;
        self.wide_count += 1;
    }
}

/// A pending click whose user_id is long.
#[derive(Clone, Copy, Default)]
struct LongCk {
    ck: Ck2,
    uhi: u64,
    ulo: u64,
}

/// Per-segment state of the clicks parse, with the same lookahead rings.
struct ClickSink<'p, 'a> {
    parser: &'p Parser<'a>,
    d: &'p [u8],
    links: &'p LinkTable,
    bits2: u32,
    out: &'p mut Scatter<Ck2>,
    ring: [Ck2; LOOKAHEAD],
    count: usize,
    long_ring: [LongCk; LOOKAHEAD],
    long_count: usize,
}

impl ClickSink<'_, '_> {
    #[inline(always)]
    fn resolve(links: &LinkTable, bits2: u32, c: Ck2, out: &mut Scatter<Ck2>) {
        let mut c = c;
        c.pk = links.person_of(c.pk & USER_MASK) | (c.pk & CLICK_LONG);
        out.push(take(fib(c.pk & PERSON_MASK), 0, bits2), c);
    }

    #[inline(always)]
    fn resolve_long(parser: &Parser, links: &LinkTable, bits2: u32, l: LongCk, out: &mut Scatter<Ck2>) {
        let mut c = l.ck;
        c.pk = long_person(parser.longs, links, parser.interner, l.uhi, l.ulo) | (c.pk & CLICK_LONG);
        out.push(take(fib(c.pk & PERSON_MASK), 0, bits2), c);
    }
}

impl LineSink<4> for ClickSink<'_, '_> {
    #[inline(always)]
    fn line(&mut self, fs: &[usize; 4], fe: &[usize; 4]) {
        let Some((c, uhi, ulo)) = self.parser.click(self.d, fs, fe) else { return };
        if c.pk & CK_LONG_USER == 0 {
            self.links.prefetch(c.pk & USER_MASK);
            let slot = unsafe { self.ring.get_unchecked_mut(self.count % LOOKAHEAD) };
            if self.count >= LOOKAHEAD {
                Self::resolve(self.links, self.bits2, *slot, self.out);
            }
            *slot = c;
            self.count += 1;
            return;
        }
        self.parser.longs.prefetch(uhi, ulo);
        let slot = unsafe { self.long_ring.get_unchecked_mut(self.long_count % LOOKAHEAD) };
        if self.long_count >= LOOKAHEAD {
            Self::resolve_long(self.parser, self.links, self.bits2, *slot, self.out);
        }
        *slot = LongCk { ck: Ck2 { pk: c.pk & !CK_LONG_USER, ..c }, uhi, ulo };
        self.long_count += 1;
    }
}

impl Parser<'_> {
    /// Parses one events segment into `out` (partitioned by event key) and
    /// `out_l` (long event IDs).
    #[inline(never)]
    fn events_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits1: u32, bits_l: u32, out: &mut Scatter<Ev1>, out_l: &mut Scatter<Ev1L>) {
        let mut sink = EventSink {
            parser: self,
            d,
            links,
            bits1,
            bits_l,
            out,
            out_l,
            ring: [Ev1::default(); LOOKAHEAD],
            count: 0,
            wide: [Pend::default(); LOOKAHEAD],
            wide_count: 0,
        };
        lines::<7, _>(d, seg, &mut sink);
        let EventSink { ring, count, wide, wide_count, out, out_l, .. } = sink;
        for i in count.saturating_sub(LOOKAHEAD)..count {
            EventSink::resolve(links, bits1, ring[i % LOOKAHEAD], out);
        }
        for i in wide_count.saturating_sub(LOOKAHEAD)..wide_count {
            EventSink::resolve_wide(self, links, bits1, bits_l, wide[i % LOOKAHEAD], out, out_l);
        }
    }

    /// Parses one clicks segment into `out` (partitioned by person).
    #[inline(never)]
    fn clicks_segment(&self, d: &[u8], seg: &Segment, links: &LinkTable, bits2: u32, out: &mut Scatter<Ck2>) {
        let mut sink = ClickSink {
            parser: self,
            d,
            links,
            bits2,
            out,
            ring: [Ck2::default(); LOOKAHEAD],
            count: 0,
            long_ring: [LongCk::default(); LOOKAHEAD],
            long_count: 0,
        };
        lines::<4, _>(d, seg, &mut sink);
        let ClickSink { ring, count, long_ring, long_count, out, .. } = sink;
        for i in count.saturating_sub(LOOKAHEAD)..count {
            ClickSink::resolve(links, bits2, ring[i % LOOKAHEAD], out);
        }
        for i in long_count.saturating_sub(LOOKAHEAD)..long_count {
            ClickSink::resolve_long(self, links, bits2, long_ring[i % LOOKAHEAD], out);
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

    // Rough line counts size the partitions: ~80 bytes per event line, ~45 per click line.
    let est_events = event_bytes / 80 + 1;
    let est_clicks = click_bytes / 45 + 1;
    let interner = Interner::new();
    let longs = LongTable::new((est_events + est_clicks) / 16);
    let parser = Parser {
        window_start: window.start,
        window_end: window.end,
        click_start: window.start - REACH_MS,
        base,
        products: &products,
        campaigns: &campaigns,
        interner: &interner,
        longs: &longs,
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
                    pairs.push((parser.link_key(d, fs[0], fe[0]), parser.link_key(d, fs[1], fe[1])));
                }
            }));
        }
        pairs
    });
    let odd_after_links = interner.snapshot();
    let link_table = links::build(&pairs, threads, &odd_after_links, &longs);
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
    // Partition counts balance the scatter's staging lines against the size
    // of each partition's dedup and person tables (see `partition_bits`).
    let bits_override = std::env::var("SESSIONS_BITS").ok().and_then(|v| v.parse::<u32>().ok());
    let bits1 = bits_override.unwrap_or_else(|| partition_bits(est_events));
    let bits2 = bits_override.unwrap_or_else(|| partition_bits(est_events + est_clicks));
    if verbose {
        eprintln!("    partitions: events {} / persons {}", 1 << bits1, 1 << bits2);
    }
    // Long event IDs (about 1 in 8) have their own, fewer partitions.
    let bits_l = bits1.saturating_sub(3).max(4);
    let (p1, p2, pl) = (1usize << bits1, 1usize << bits2, 1usize << bits_l);
    let work: Vec<&Segment> = event_segments.iter().chain(&click_segments).collect();
    let next = AtomicUsize::new(0);
    let parsed: Vec<(Scatter<Ev1>, Scatter<Ev1L>, Scatter<Ck2>)> = par_run(threads, |_| {
        let mut evs: Scatter<Ev1> = Scatter::new(p1, est_events / threads + 1);
        let mut evs_l: Scatter<Ev1L> = Scatter::new(pl, est_events / 8 / threads + 1);
        let mut cks: Scatter<Ck2> = Scatter::new(p2, est_clicks / threads + 1);
        loop {
            let at = next.fetch_add(1, Ordering::Relaxed);
            let Some(seg) = work.get(at) else { break };
            let d = maps[seg.file].bytes();
            if seg.file < n_ev {
                parser.events_segment(d, seg, &link_table, bits1, bits_l, &mut evs, &mut evs_l);
            } else {
                parser.clicks_segment(d, seg, &link_table, bits2, &mut cks);
            }
        }
        evs.finish();
        evs_l.finish();
        cks.finish();
        (evs, evs_l, cks)
    });
    lap("parse");

    // ---- 3. Dedup: per event ID, the copy with the smallest ingest_ts decides.
    // An open-addressing table of whole records per partition; then one
    // sequential sweep emits the valid winners and clears the table.
    let next = AtomicUsize::new(0);
    let next_l = AtomicUsize::new(0);
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
            for (evs, _, _) in &parsed {
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
        // The same over the long event IDs' partitions.
        let mut recs: Vec<Ev1L> = Vec::new();
        let mut chunks: Vec<&[Ev1L]> = Vec::new();
        loop {
            let p = next_l.fetch_add(1, Ordering::Relaxed);
            if p >= pl {
                break;
            }
            chunks.clear();
            for (_, evs_l, _) in &parsed {
                evs_l.chunks(p, &mut chunks);
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
                    let mut h = take(long_hash(rec.hi, rec.lo), bits_l, tbits);
                    loop {
                        let i = unsafe { *index.get_unchecked(h) };
                        if i == EMPTY {
                            unsafe { *index.get_unchecked_mut(h) = recs.len() as u32 };
                            recs.push(*rec);
                            break;
                        }
                        let kept = unsafe { recs.get_unchecked_mut(i as usize) };
                        if kept.hi == rec.hi && kept.lo == rec.lo {
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
    let odd = interner.snapshot();
    let ctx = person::Ctx { base, first_day, days, campaigns: campaigns.len(), odd: &odd, longs: &longs };
    let mut order: Vec<(usize, usize)> = (0..p2)
        .map(|q| {
            let n: usize = survivors.iter().map(|s| s.len(q)).sum::<usize>()
                + parsed.iter().map(|(_, _, c)| c.len(q)).sum::<usize>();
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
            for (_, _, c) in &parsed {
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
