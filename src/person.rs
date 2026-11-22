//! The person stage: one partition of people at a time, with all of their
//! surviving events and valid clicks. Groups each person's rows, builds
//! sessions, credits and assists them, and accumulates both output tables.

use crate::ids;
use crate::links::PERSON_MASK;
use crate::long::LongTable;
use crate::ts::DAY;
use crate::util::{fib, take};

/// A surviving event: person key (low 48 bits) with the category above it,
/// time since `base`, and the price (0 unless a purchase).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Ev2 {
    pub pk: u64,
    pub t: u32,
    pub price: u32,
}

/// A valid click: person key, click key (for ties), time since `base`,
/// campaign index (1..). 32 bytes for the scatter's cache-line staging.
/// A long click ID (`k` + 32 hex digits) sets CLICK_LONG in `pk` and keeps
/// its value in `key` (first 16 digits) and `key2`; otherwise `key2` is 0.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Ck2 {
    pub pk: u64,
    pub key: u64,
    pub t: u32,
    pub campaign: u32,
    pub key2: u64,
}

/// In `Ck2::pk`, above the person key (which uses the low 44 bits).
pub const CLICK_LONG: u64 = 1 << 63;

/// Whether click `a`'s click_id is byte-wise less than click `b`'s.
fn click_id_less(a: &Ck2, b: &Ck2, odd: &[Vec<u8>]) -> bool {
    match (a.pk & CLICK_LONG != 0, b.pk & CLICK_LONG != 0) {
        (true, true) => (a.key, a.key2) < (b.key, b.key2),
        (false, false) => ids::cmp_ids(a.key, b.key, b'k', odd) == std::cmp::Ordering::Less,
        _ => {
            let write = |c: &Ck2, out: &mut Vec<u8>| {
                if c.pk & CLICK_LONG != 0 {
                    ids::write_long(out, b'k', c.key, c.key2);
                } else {
                    ids::write_id(out, c.key, b'k', odd);
                }
            };
            let (mut sa, mut sb) = (Vec::with_capacity(40), Vec::with_capacity(40));
            write(a, &mut sa);
            write(b, &mut sb);
            sa < sb
        }
    }
}

pub const NO_CATEGORY: u16 = u16::MAX;
pub const GAP_MS: u32 = 1_800_000;
pub const REACH_MS: i64 = 7 * DAY;

/// One user_daily row; the person's name is resolved when writing.
#[derive(Clone, Copy)]
pub struct UdRow {
    pub pk: u64,
    pub revenue: u64,
    pub sessions: u32,
    pub events: u32,
    pub day: u16,
    pub top: u16,
}

pub struct Ctx<'a> {
    /// Epoch ms that every `t` counts from (window start minus 7 days).
    pub base: i64,
    /// Day number (since 1970-01-01) of the window's first day.
    pub first_day: i64,
    pub days: usize,
    pub campaigns: usize,
    pub odd: &'a [Vec<u8>],
    pub longs: &'a LongTable,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Credited {
    pub sessions: u32,
    pub purchasing: u32,
    pub revenue: u64,
}

/// Per-thread accumulators for both tables.
pub struct Out {
    pub rows: Vec<UdRow>,
    /// Credited sessions, purchasing sessions and revenue, indexed
    /// `campaign * days + day`: one 16-byte cell per session update.
    pub credited: Vec<Credited>,
    /// Assisted counts, indexed `day * campaigns + campaign`.
    pub assisted: Vec<u32>,
}

impl Out {
    pub fn new(ctx: &Ctx) -> Out {
        let cells = ctx.campaigns * ctx.days;
        Out {
            rows: Vec::new(),
            credited: vec![Credited::default(); cells],
            assisted: vec![0; cells],
        }
    }
}

/// Reusable per-thread buffers.
#[derive(Default)]
pub struct Scratch {
    /// Person table: (person key, local id, epoch). A slot whose epoch is not
    /// the current partition's is empty, so the table is never cleared.
    table: Vec<(u64, u32, u32)>,
    epoch: u32,
    ev_person: Vec<u32>,
    ev_start: Vec<u32>,
    /// Person key of each local id.
    person_keys: Vec<u64>,
    fill: Vec<u32>,
    /// Each person's events as `t << 32 | purchase`, purchase being 0 or
    /// 1 + an index into `purchases`.
    keys: Vec<u64>,
    purchases: Vec<(u32, u16)>,
    ck_start: Vec<u32>,
    ck_person: Vec<u32>,
    cks: Vec<Ck2>,
    in_window: Vec<u32>,
    position: Vec<u32>,
    active: Vec<u32>,
    category_sums: Vec<(u16, u64)>,
}

/// Turns counts into start offsets, with one extra entry for the end.
#[inline]
fn offsets(counts: &mut Vec<u32>) -> u32 {
    let mut acc = 0u32;
    for c in counts.iter_mut() {
        let n = *c;
        *c = acc;
        acc += n;
    }
    counts.push(acc);
    acc
}

/// Starts a new epoch on a person table of at least `cap` slots: every
/// slot stamped with an older epoch counts as empty.
fn begin_epoch(s: &mut Scratch, cap: usize) {
    if s.table.len() < cap || s.epoch == u32::MAX {
        s.table.clear();
        s.table.resize(cap.max(s.table.len()), (0, 0, 0));
        s.epoch = 0;
    }
    s.epoch += 1;
}

/// The local id of person key `k` in the first `cap` slots of the table,
/// inserting it as `next` when absent; the bool says whether it was new.
#[inline(always)]
fn find_or_insert(s: &mut Scratch, cap: usize, skip: u32, k: u64, next: u32) -> (u32, bool) {
    let mask = cap - 1;
    let epoch = s.epoch;
    let mut h = take(fib(k), skip, cap.trailing_zeros());
    loop {
        let slot = unsafe { s.table.get_unchecked_mut(h) };
        if slot.2 != epoch {
            *slot = (k, next, epoch);
            return (next, true);
        }
        if slot.0 == k {
            return (slot.1, false);
        }
        h = (h + 1) & mask;
    }
}

/// Processes one partition: `events` and `clicks` are the slices every
/// thread scattered into it. `skip` is the number of top hash bits that
/// chose this partition.
pub fn process(events: &[&[Ev2]], clicks: &[&[Ck2]], skip: u32, ctx: &Ctx, s: &mut Scratch, out: &mut Out) {
    let n_ev: usize = events.iter().map(|x| x.len()).sum();
    if n_ev == 0 {
        return;
    }
    if s.in_window.len() != ctx.campaigns {
        s.in_window = vec![0; ctx.campaigns];
        s.position = vec![0; ctx.campaigns];
    }

    // Local dense person ids, from an epoch-stamped table sized for the
    // persons (not the events) and doubled whenever its load passes 2/3.
    let mut cap = (n_ev / 8).max(64).next_power_of_two();
    begin_epoch(s, cap);
    s.ev_person.clear();
    s.ev_person.reserve(n_ev);
    s.ev_start.clear();
    s.person_keys.clear();
    let first_row = out.rows.len();
    let mut persons = 0u32;
    for slice in events {
        for e in slice.iter() {
            let k = e.pk & PERSON_MASK;
            let (id, new) = find_or_insert(s, cap, skip, k, persons);
            if new {
                persons += 1;
                s.ev_start.push(0);
                s.person_keys.push(k);
                if persons as usize * 3 > cap * 2 {
                    cap *= 2;
                    begin_epoch(s, cap);
                    for i in 0..s.person_keys.len() {
                        let key = s.person_keys[i];
                        find_or_insert(s, cap, skip, key, i as u32);
                    }
                }
            }
            unsafe { *s.ev_start.get_unchecked_mut(id as usize) += 1 };
            s.ev_person.push(id);
        }
    }
    let tbits = cap.trailing_zeros();
    let mask = cap - 1;
    let epoch = s.epoch;
    let table = &s.table[..cap];
    offsets(&mut s.ev_start);

    // Group the events by person as sort keys; purchases keep their price and
    // category on the side.
    s.purchases.clear();
    s.keys.clear();
    s.keys.reserve(n_ev);
    unsafe { s.keys.set_len(n_ev) };
    s.fill.clear();
    s.fill.extend_from_slice(&s.ev_start[..persons as usize]);
    {
        let mut i = 0usize;
        for slice in events {
            for e in slice.iter() {
                let id = unsafe { *s.ev_person.get_unchecked(i) } as usize;
                let tag = if e.price != 0 {
                    s.purchases.push((e.price, (e.pk >> 48) as u16));
                    s.purchases.len() as u64
                } else {
                    0
                };
                unsafe {
                    let at = s.fill.get_unchecked_mut(id);
                    *s.keys.get_unchecked_mut(*at as usize) = (e.t as u64) << 32 | tag;
                    *at += 1;
                }
                i += 1;
            }
        }
    }

    // Clicks of the people who have events.
    s.ck_start.clear();
    s.ck_start.resize(persons as usize, 0);
    s.ck_person.clear();
    let n_ck: usize = clicks.iter().map(|x| x.len()).sum();
    s.ck_person.reserve(n_ck);
    for slice in clicks {
        for c in slice.iter() {
            let k = c.pk & PERSON_MASK;
            let mut h = take(fib(k), skip, tbits);
            let id = loop {
                let slot = unsafe { table.get_unchecked(h) };
                if slot.2 != epoch {
                    break u32::MAX;
                }
                if slot.0 == k {
                    break slot.1;
                }
                h = (h + 1) & mask;
            };
            if id != u32::MAX {
                unsafe { *s.ck_start.get_unchecked_mut(id as usize) += 1 };
            }
            s.ck_person.push(id);
        }
    }
    let kept = offsets(&mut s.ck_start) as usize;
    s.cks.clear();
    s.cks.reserve(kept);
    unsafe { s.cks.set_len(kept) };
    s.fill.clear();
    s.fill.extend_from_slice(&s.ck_start[..persons as usize]);
    {
        let mut i = 0usize;
        for slice in clicks {
            for c in slice.iter() {
                let id = unsafe { *s.ck_person.get_unchecked(i) };
                i += 1;
                if id == u32::MAX {
                    continue;
                }
                unsafe {
                    let at = s.fill.get_unchecked_mut(id as usize);
                    *s.cks.get_unchecked_mut(*at as usize) = *c;
                    *at += 1;
                }
            }
        }
    }

    for p in 0..persons as usize {
        let (a, b) = (s.ev_start[p] as usize, s.ev_start[p + 1] as usize);
        let (c, d) = (s.ck_start[p] as usize, s.ck_start[p + 1] as usize);
        s.keys[a..b].sort_unstable();
        if d - c > 1 {
            s.cks[c..d].sort_unstable_by_key(|c| c.t);
        }
        person(
            p as u32,
            &s.keys[a..b],
            &s.purchases,
            &s.cks[c..d],
            ctx,
            &mut s.in_window,
            &mut s.position,
            &mut s.active,
            &mut s.category_sums,
            out,
        );
    }
    // The rows of this partition carry local ids: give them person keys.
    for r in &mut out.rows[first_row..] {
        r.pk = s.person_keys[r.pk as usize];
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn person(
    id: u32,
    keys: &[u64],
    purchases: &[(u32, u16)],
    cks: &[Ck2],
    ctx: &Ctx,
    in_window: &mut [u32],
    position: &mut [u32],
    active: &mut Vec<u32>,
    category_sums: &mut Vec<(u16, u64)>,
    out: &mut Out,
) {
    // Rows carry the local id until the partition is done (see `process`).
    let pk = id as u64;
    let days = ctx.days;
    let campaigns = ctx.campaigns;
    let mut row: Option<UdRow> = None;
    let (mut lo, mut hi) = (0usize, 0usize);
    let mut i = 0usize;
    while i < keys.len() {
        let start = (keys[i] >> 32) as u32;
        let mut last = start;
        let mut j = i;
        let mut revenue = 0u64;
        let mut purchased = false;
        while j < keys.len() {
            let t = (keys[j] >> 32) as u32;
            if t - last > GAP_MS {
                break;
            }
            last = t;
            let tag = keys[j] as u32;
            if tag != 0 {
                purchased = true;
                revenue += purchases[tag as usize - 1].0 as u64;
            }
            j += 1;
        }
        let day = ((ctx.base + start as i64).div_euclid(DAY) - ctx.first_day) as usize;
        debug_assert!(day < days);

        // user_daily: sessions are in start order, so days never go back.
        match &mut row {
            Some(r) if r.day as usize == day => {}
            _ => {
                if let Some(r) = row.take() {
                    out.rows.push(finish_row(r, category_sums));
                }
                row = Some(UdRow { pk, revenue: 0, sessions: 0, events: 0, day: day as u16, top: NO_CATEGORY });
            }
        }
        let r = row.as_mut().unwrap();
        r.sessions += 1;
        r.events += (j - i) as u32;
        r.revenue += revenue;
        if purchased {
            for &k in &keys[i..j] {
                let tag = k as u32;
                if tag != 0 {
                    let (price, cat) = purchases[tag as usize - 1];
                    match category_sums.iter_mut().find(|(c, _)| *c == cat) {
                        Some((_, sum)) => *sum += price as u64,
                        None => category_sums.push((cat, price as u64)),
                    }
                }
            }
        }

        // Clicks in [start - 7 days, start].
        while hi < cks.len() && cks[hi].t <= start {
            let c = cks[hi].campaign as usize;
            if in_window[c] == 0 {
                position[c] = active.len() as u32;
                active.push(c as u32);
            }
            in_window[c] += 1;
            hi += 1;
        }
        while lo < hi && (cks[lo].t as i64) + REACH_MS < start as i64 {
            let c = cks[lo].campaign as usize;
            in_window[c] -= 1;
            if in_window[c] == 0 {
                let at = position[c] as usize;
                let moved = active.pop().unwrap();
                if moved as usize != c {
                    active[at] = moved;
                    position[moved as usize] = at as u32;
                }
            }
            lo += 1;
        }
        let credited = if lo < hi {
            let latest = cks[hi - 1].t;
            let mut best = hi - 1;
            let mut k = hi - 1;
            while k > lo && cks[k - 1].t == latest {
                k -= 1;
                if click_id_less(&cks[k], &cks[best], ctx.odd) {
                    best = k;
                }
            }
            cks[best].campaign as usize
        } else {
            0
        };
        let cell = credited * days + day;
        let c = &mut out.credited[cell];
        c.sessions += 1;
        c.purchasing += purchased as u32;
        c.revenue += revenue;
        let assisted = &mut out.assisted[day * campaigns..(day + 1) * campaigns];
        for &c in active.iter() {
            assisted[c as usize] += 1;
        }
        i = j;
    }
    if let Some(r) = row.take() {
        out.rows.push(finish_row(r, category_sums));
    }
    for &c in active.iter() {
        in_window[c as usize] = 0;
    }
    active.clear();
}

#[inline]
fn finish_row(mut r: UdRow, category_sums: &mut Vec<(u16, u64)>) -> UdRow {
    let mut best: Option<(u16, u64)> = None;
    for &(cat, sum) in category_sums.iter() {
        best = match best {
            Some((bc, bs)) if bs > sum || (bs == sum && bc < cat) => Some((bc, bs)),
            _ => Some((cat, sum)),
        };
    }
    r.top = best.map_or(NO_CATEGORY, |(c, _)| c);
    category_sums.clear();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_ids_order_like_bytes() {
        let interner = ids::Interner::new();
        let mut ids_: Vec<Vec<u8>> = vec![
            b"k0".to_vec(),
            b"k0a1b".to_vec(),
            b"k0a1b2c3d".to_vec(),
            b"kzzzzzzzz".to_vec(),
            b"kf".to_vec(),
            b"kg".to_vec(),
            b"K0A1B".to_vec(),
            b"k0a1b2c3d0000000000000000000000000".to_vec(),
            b"k0A1B2c3d000000000000000000000000".to_vec(),
        ];
        for head in ["0a1b", "0a1b2c3d", "0a1b2c3d1", "0a1b2c3e", "f", "ffffffff", "1", ""] {
            let mut v = format!("k{head}").into_bytes();
            v.resize(33, b'0');
            ids_.push(v.clone());
            *v.last_mut().unwrap() = b'7';
            ids_.push(v);
        }
        let cks: Vec<Ck2> = ids_
            .iter()
            .map(|s| {
                let k = ids::fast_key(s, b'k');
                if k != 0 {
                    return Ck2 { key: k, ..Default::default() };
                }
                if let Some((hi, lo)) = crate::fast::hex_key(s, 0, s.len(), b'k') {
                    return Ck2 { pk: CLICK_LONG | 12345, key: hi, key2: lo, ..Default::default() };
                }
                Ck2 { key: interner.intern(s), ..Default::default() }
            })
            .collect();
        assert!(cks.iter().filter(|c| c.pk & CLICK_LONG != 0).count() >= 14);
        let odd = interner.snapshot();
        for i in 0..cks.len() {
            for j in 0..cks.len() {
                assert_eq!(click_id_less(&cks[i], &cks[j], &odd), ids_[i] < ids_[j], "{:?} {:?}", ids_[i], ids_[j]);
            }
        }
    }
}
