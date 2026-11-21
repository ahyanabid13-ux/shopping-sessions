//! The person stage: one partition of people at a time, with all of their
//! surviving events and valid clicks. Groups each person's rows, builds
//! sessions, credits and assists them, and accumulates both output tables.

use crate::ids;
use crate::links::PERSON_MASK;
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

/// A valid click: person key, click key (for ties), time since `base`, campaign index (1..).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Ck2 {
    pub pk: u64,
    pub key: u64,
    pub t: u32,
    pub campaign: u32,
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
}

/// Per-thread accumulators for both tables.
pub struct Out {
    pub rows: Vec<UdRow>,
    /// Credited counts, indexed `campaign * days + day`.
    pub sessions: Vec<u32>,
    pub purchasing: Vec<u32>,
    pub revenue: Vec<u64>,
    /// Assisted counts, indexed `day * campaigns + campaign`.
    pub assisted: Vec<u32>,
}

impl Out {
    pub fn new(ctx: &Ctx) -> Out {
        let cells = ctx.campaigns * ctx.days;
        Out {
            rows: Vec::new(),
            sessions: vec![0; cells],
            purchasing: vec![0; cells],
            revenue: vec![0; cells],
            assisted: vec![0; cells],
        }
    }
}

/// Reusable per-thread buffers.
#[derive(Default)]
pub struct Scratch {
    keys: Vec<u64>,
    ids: Vec<u32>,
    ev_person: Vec<u32>,
    ev_start: Vec<u32>,
    evs: Vec<Ev2>,
    ck_start: Vec<u32>,
    ck_person: Vec<u32>,
    cks: Vec<Ck2>,
    in_window: Vec<u32>,
    position: Vec<u32>,
    active: Vec<u32>,
    category_sums: Vec<(u16, u64)>,
}

/// Processes one partition: `events` and `clicks` are the slices every
/// thread scattered into it.
/// `skip` is the number of top hash bits that chose this partition.
pub fn process(events: &[&[Ev2]], clicks: &[&[Ck2]], skip: u32, ctx: &Ctx, s: &mut Scratch, out: &mut Out) {
    let n_ev: usize = events.iter().map(|x| x.len()).sum();
    if n_ev == 0 {
        return;
    }
    if s.in_window.len() != ctx.campaigns {
        s.in_window = vec![0; ctx.campaigns];
        s.position = vec![0; ctx.campaigns];
    }

    // Local dense person ids.
    let cap = (n_ev * 2).max(16).next_power_of_two();
    let mask = cap - 1;
    let tbits = cap.trailing_zeros();
    s.keys.clear();
    s.keys.resize(cap, 0);
    s.ids.resize(cap, 0);
    s.ev_person.clear();
    s.ev_person.reserve(n_ev);
    s.ev_start.clear();
    let mut persons = 0u32;
    for slice in events {
        for e in slice.iter() {
            let k = e.pk & PERSON_MASK;
            let mut h = take(fib(k), skip, tbits);
            let id = loop {
                let cur = s.keys[h];
                if cur == k {
                    break s.ids[h];
                }
                if cur == 0 {
                    s.keys[h] = k;
                    s.ids[h] = persons;
                    persons += 1;
                    s.ev_start.push(0);
                    break persons - 1;
                }
                h = (h + 1) & mask;
            };
            s.ev_start[id as usize] += 1;
            s.ev_person.push(id);
        }
    }
    // Counts to start offsets (one extra slot for the end).
    let mut acc = 0u32;
    for c in s.ev_start.iter_mut() {
        let n = *c;
        *c = acc;
        acc += n;
    }
    s.ev_start.push(acc);
    s.evs.clear();
    s.evs.resize(n_ev, Ev2::default());
    {
        let mut fill: Vec<u32> = s.ev_start[..persons as usize].to_vec();
        let mut i = 0usize;
        for slice in events {
            for e in slice.iter() {
                let id = s.ev_person[i] as usize;
                s.evs[fill[id] as usize] = *e;
                fill[id] += 1;
                i += 1;
            }
        }
    }

    // Clicks of the people who have events.
    s.ck_start.clear();
    s.ck_start.resize(persons as usize + 1, 0);
    s.ck_person.clear();
    let n_ck: usize = clicks.iter().map(|x| x.len()).sum();
    s.ck_person.reserve(n_ck);
    for slice in clicks {
        for c in slice.iter() {
            let k = c.pk & PERSON_MASK;
            let mut h = take(fib(k), skip, tbits);
            let id = loop {
                let cur = s.keys[h];
                if cur == k {
                    break s.ids[h];
                }
                if cur == 0 {
                    break u32::MAX;
                }
                h = (h + 1) & mask;
            };
            if id != u32::MAX {
                s.ck_start[id as usize] += 1;
            }
            s.ck_person.push(id);
        }
    }
    let mut acc = 0u32;
    for c in s.ck_start.iter_mut() {
        let n = *c;
        *c = acc;
        acc += n;
    }
    s.cks.clear();
    s.cks.resize(acc as usize, Ck2::default());
    {
        let mut fill: Vec<u32> = s.ck_start[..persons as usize].to_vec();
        let mut i = 0usize;
        for slice in clicks {
            for c in slice.iter() {
                let id = s.ck_person[i];
                i += 1;
                if id == u32::MAX {
                    continue;
                }
                s.cks[fill[id as usize] as usize] = *c;
                fill[id as usize] += 1;
            }
        }
    }

    for p in 0..persons as usize {
        let (a, b) = (s.ev_start[p] as usize, s.ev_start[p + 1] as usize);
        let (c, d) = (s.ck_start[p] as usize, s.ck_start[p + 1] as usize);
        let evs = &mut s.evs[a..b];
        evs.sort_unstable_by_key(|e| e.t);
        let cks = &mut s.cks[c..d];
        cks.sort_unstable_by_key(|c| c.t);
        person(
            &s.evs[a..b],
            &s.cks[c..d],
            ctx,
            &mut s.in_window,
            &mut s.position,
            &mut s.active,
            &mut s.category_sums,
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn person(
    evs: &[Ev2],
    cks: &[Ck2],
    ctx: &Ctx,
    in_window: &mut [u32],
    position: &mut [u32],
    active: &mut Vec<u32>,
    category_sums: &mut Vec<(u16, u64)>,
    out: &mut Out,
) {
    let pk = evs[0].pk & PERSON_MASK;
    let days = ctx.days;
    let campaigns = ctx.campaigns;
    let mut row: Option<UdRow> = None;
    let (mut lo, mut hi) = (0usize, 0usize);
    let mut i = 0usize;
    while i < evs.len() {
        let start = evs[i].t;
        let mut last = start;
        let mut j = i;
        let mut revenue = 0u64;
        let mut purchased = false;
        while j < evs.len() && evs[j].t - last <= GAP_MS {
            let e = evs[j];
            last = e.t;
            if e.price != 0 {
                purchased = true;
                revenue += e.price as u64;
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
            for e in &evs[i..j] {
                if e.price != 0 {
                    let cat = (e.pk >> 48) as u16;
                    match category_sums.iter_mut().find(|(c, _)| *c == cat) {
                        Some((_, sum)) => *sum += e.price as u64,
                        None => category_sums.push((cat, e.price as u64)),
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
                if ids::cmp_ids(cks[k].key, cks[best].key, b'k', ctx.odd) == std::cmp::Ordering::Less {
                    best = k;
                }
            }
            cks[best].campaign as usize
        } else {
            0
        };
        let cell = credited * days + day;
        out.sessions[cell] += 1;
        out.purchasing[cell] += purchased as u32;
        out.revenue[cell] += revenue;
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
