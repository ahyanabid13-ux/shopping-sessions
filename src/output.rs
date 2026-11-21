//! Writes `user_daily/` (one Parquet file per thread) and `campaign_daily/`.

use std::path::Path;

use crate::ids;
use crate::links::{LinkTable, LINKED};
use crate::lookup::{Campaigns, Products};
use crate::parquet::{self, Chunk, Column, Kind};
use crate::person::{self, NO_CATEGORY};

pub fn write_user_daily(path: &Path, out: &person::Out, ctx: &person::Ctx, links: &LinkTable, products: &Products) {
    let rows = &out.rows;
    let n = rows.len();
    // Each column's PLAIN page body, built in one pass over the rows.
    let mut user: Vec<u8> = Vec::with_capacity(n * 14);
    let mut days: Vec<u8> = Vec::with_capacity(n * 4);
    let mut sessions: Vec<u8> = Vec::with_capacity(n * 4);
    let mut events: Vec<u8> = Vec::with_capacity(n * 4);
    let mut revenue: Vec<u8> = Vec::with_capacity(n * 8);
    let mut levels = parquet::Levels::new(n);
    let mut cats: Vec<u8> = Vec::with_capacity(n * 12);
    let encoded_categories: Vec<Vec<u8>> = products
        .categories
        .iter()
        .map(|c| {
            let mut v = Vec::new();
            parquet::put_str(&mut v, c);
            v
        })
        .collect();
    let mut name_buf: Vec<u8> = Vec::with_capacity(64);
    // A person's rows are consecutive: its name is written once.
    let mut previous = u64::MAX;
    for (i, r) in rows.iter().enumerate() {
        if r.pk != previous {
            previous = r.pk;
            let user_key = if r.pk & LINKED != 0 { links.names[(r.pk & !LINKED) as usize] } else { r.pk };
            name_buf.clear();
            ids::write_id(&mut name_buf, user_key, b'u', ctx.odd);
        }
        parquet::put_str(&mut user, &name_buf);
        days.extend_from_slice(&((ctx.first_day + r.day as i64) as i32).to_le_bytes());
        sessions.extend_from_slice(&(r.sessions as i32).to_le_bytes());
        events.extend_from_slice(&(r.events as i32).to_le_bytes());
        revenue.extend_from_slice(&(r.revenue as i64).to_le_bytes());
        if r.top != NO_CATEGORY {
            levels.set(i);
            cats.extend_from_slice(&encoded_categories[r.top as usize]);
        }
    }
    let chunks = [
        Chunk { name: "user_id", kind: Kind::Str, parts: vec![&user] },
        Chunk { name: "day", kind: Kind::Date, parts: vec![&days] },
        Chunk { name: "sessions", kind: Kind::I32, parts: vec![&sessions] },
        Chunk { name: "events", kind: Kind::I32, parts: vec![&events] },
        Chunk { name: "revenue_cents", kind: Kind::I64, parts: vec![&revenue] },
        Chunk { name: "top_category", kind: Kind::OptStr, parts: vec![levels.bytes(), &cats] },
    ];
    parquet::write_chunks(path, n, &chunks).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

pub fn write_campaign_daily(path: &Path, outs: &[person::Out], ctx: &person::Ctx, campaigns: &Campaigns) {
    let cells = ctx.campaigns * ctx.days;
    let mut sessions = vec![0u64; cells];
    let mut purchasing = vec![0u64; cells];
    let mut revenue = vec![0u64; cells];
    let mut assisted = vec![0u64; cells];
    for o in outs {
        for c in 0..ctx.campaigns {
            for d in 0..ctx.days {
                let cell = c * ctx.days + d;
                sessions[cell] += o.sessions[cell] as u64;
                purchasing[cell] += o.purchasing[cell] as u64;
                revenue[cell] += o.revenue[cell];
                assisted[cell] += o.assisted[d * ctx.campaigns + c] as u64;
            }
        }
    }
    let mut ids_col: Vec<u8> = Vec::new();
    let mut id_ends: Vec<u32> = Vec::new();
    let mut channel_col: Vec<u8> = Vec::new();
    let mut channel_ends: Vec<u32> = Vec::new();
    let (mut days, mut s_col, mut p_col, mut r_col, mut a_col) = (vec![], vec![], vec![], vec![], vec![]);
    for c in 0..ctx.campaigns {
        for d in 0..ctx.days {
            let cell = c * ctx.days + d;
            if sessions[cell] == 0 && assisted[cell] == 0 {
                continue;
            }
            ids_col.extend_from_slice(&campaigns.ids[c]);
            id_ends.push(ids_col.len() as u32);
            channel_col.extend_from_slice(&campaigns.channels[c]);
            channel_ends.push(channel_col.len() as u32);
            days.push((ctx.first_day + d as i64) as i32);
            s_col.push(sessions[cell] as i64);
            p_col.push(purchasing[cell] as i64);
            r_col.push(revenue[cell] as i64);
            a_col.push(assisted[cell] as i64);
        }
    }
    let columns = [
        Column::Str { name: "campaign_id", values: &ids_col, ends: &id_ends },
        Column::Str { name: "channel", values: &channel_col, ends: &channel_ends },
        Column::I32 { name: "day", values: &days, date: true },
        Column::I64 { name: "sessions", values: &s_col },
        Column::I64 { name: "purchasing_sessions", values: &p_col },
        Column::I64 { name: "revenue_cents", values: &r_col },
        Column::I64 { name: "assisted_sessions", values: &a_col },
    ];
    parquet::write_file(path, &columns).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}
