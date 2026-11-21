//! Writes `user_daily/` (one Parquet file per thread) and `campaign_daily/`.

use std::path::Path;

use crate::ids;
use crate::links::{LinkTable, LINKED};
use crate::lookup::{Campaigns, Products};
use crate::parquet::{self, Column};
use crate::person::{self, NO_CATEGORY};

pub fn write_user_daily(path: &Path, out: &person::Out, ctx: &person::Ctx, links: &LinkTable, products: &Products) {
    let n = out.rows.len();
    let mut names: Vec<u8> = Vec::with_capacity(n * 10);
    let mut name_ends: Vec<u32> = Vec::with_capacity(n);
    let mut days: Vec<i32> = Vec::with_capacity(n);
    let mut sessions: Vec<i32> = Vec::with_capacity(n);
    let mut events: Vec<i32> = Vec::with_capacity(n);
    let mut revenue: Vec<i64> = Vec::with_capacity(n);
    let mut cats: Vec<u8> = Vec::with_capacity(n * 6);
    let mut cat_ends: Vec<u32> = Vec::with_capacity(n);
    let mut present: Vec<bool> = Vec::with_capacity(n);
    for r in &out.rows {
        let user_key = if r.pk & LINKED != 0 { links.names[(r.pk & !LINKED) as usize] } else { r.pk };
        ids::write_id(&mut names, user_key, b'u', ctx.odd);
        name_ends.push(names.len() as u32);
        days.push((ctx.first_day + r.day as i64) as i32);
        sessions.push(r.sessions as i32);
        events.push(r.events as i32);
        revenue.push(r.revenue as i64);
        if r.top != NO_CATEGORY {
            cats.extend_from_slice(&products.categories[r.top as usize]);
            present.push(true);
        } else {
            present.push(false);
        }
        cat_ends.push(cats.len() as u32);
    }
    let columns = [
        Column::Str { name: "user_id", values: &names, ends: &name_ends },
        Column::I32 { name: "day", values: &days, date: true },
        Column::I32 { name: "sessions", values: &sessions, date: false },
        Column::I32 { name: "events", values: &events, date: false },
        Column::I64 { name: "revenue_cents", values: &revenue },
        Column::OptStr { name: "top_category", values: &cats, ends: &cat_ends, present: &present },
    ];
    parquet::write_file(path, &columns).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
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
