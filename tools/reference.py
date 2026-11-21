#!/usr/bin/env python3
"""Slow, literal reference implementation of rules.md.

Written for clarity, not speed: every rule is applied in the order rules.md
states it, on plain Python values. It is the oracle the fast engine is checked
against on many generated seeds.

Usage: reference.py DATA_DIR OUTPUT_DIR   (or env DATA_DIR / OUTPUT_DIR)
Writes OUTPUT_DIR/user_daily/part-0.parquet and OUTPUT_DIR/campaign_daily/part-0.parquet.
"""
import datetime
import json
import os
import sys

import pyarrow as pa
import pyarrow.parquet as pq

DAY_MS = 86_400_000
GAP_MS = 1_800_000
REACH_MS = 7 * DAY_MS
LATE_MS = 86_400_000
I64_MAX = 2**63 - 1


def days_from_civil(y, m, d):
    y = y - 1 if m <= 2 else y
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def is_leap(y):
    return y % 4 == 0 and (y % 100 != 0 or y % 400 == 0)


def days_in_month(y, m):
    if m == 2:
        return 29 if is_leap(y) else 28
    return 30 if m in (4, 6, 9, 11) else 31


def all_digits(b):
    return len(b) > 0 and all(48 <= c <= 57 for c in b)


def parse_ts(b):
    """Return epoch milliseconds (an int, possibly huge) or None for a bad timestamp."""
    if b is None:
        return None
    if all_digits(b):  # F3
        return int(b)
    n = len(b)
    if n == 20:
        if b[19] != ord("Z"):
            return None
        offset_min = 0
    elif n == 25:
        if b[19] not in (ord("+"), ord("-")) or b[22] != ord(":"):
            return None
        hh, mm = b[20:22], b[23:25]
        if not (all_digits(hh) and all_digits(mm)):
            return None
        oh, om = int(hh), int(mm)
        if oh > 23 or om > 59:
            return None
        offset_min = oh * 60 + om
        if b[19] == ord("-"):
            offset_min = -offset_min
    else:
        return None
    if b[4] != ord("-") or b[7] != ord("-") or b[10] != ord("T") or b[13] != ord(":") or b[16] != ord(":"):
        return None
    parts = (b[0:4], b[5:7], b[8:10], b[11:13], b[14:16], b[17:19])
    if not all(all_digits(p) for p in parts):
        return None
    y, mo, d, h, mi, s = (int(p) for p in parts)
    if not (1 <= mo <= 12) or not (1 <= d <= days_in_month(y, mo)) or h > 23 or mi > 59 or s > 59:
        return None
    local = ((days_from_civil(y, mo, d) * 24 + h) * 60 + mi) * 60 + s
    return (local - offset_min * 60) * 1000


def parse_line(line, nfields):
    """Return the list of values (None for null) or None when the line is malformed."""
    if line == b"":
        return None
    raw = line.split(b",")
    if len(raw) != nfields:
        return None
    out = []
    for field in raw:
        if b'"' in field:
            if len(field) >= 2 and field[0] == 34 and field[-1] == 34 and b'"' not in field[1:-1]:
                field = field[1:-1]
            else:
                return None
        out.append(field if field != b"" else None)
    return out


def read_lines(folder):
    """Every data line (header skipped) of every *.csv file in folder."""
    if not os.path.isdir(folder):
        return
    for name in sorted(os.listdir(folder)):
        path = os.path.join(folder, name)
        if not name.endswith(".csv") or not os.path.isfile(path):
            continue
        with open(path, "rb") as f:
            data = f.read()
        lines = data.split(b"\n")
        if data.endswith(b"\n"):
            lines.pop()
        yield from lines[1:]


def read_lookup(path):
    rows = {}
    for line in read_lines_file(path):
        f = parse_line(line, 2)
        if f is None or f[0] is None:
            continue
        rows[f[0]] = f[1]
    return rows


def read_lines_file(path):
    with open(path, "rb") as f:
        data = f.read()
    lines = data.split(b"\n")
    if data.endswith(b"\n"):
        lines.pop()
    return lines[1:]


def main():
    data_dir = sys.argv[1] if len(sys.argv) > 2 else os.environ["DATA_DIR"]
    out_dir = sys.argv[2] if len(sys.argv) > 2 else os.environ["OUTPUT_DIR"]

    with open(os.path.join(data_dir, "manifest.json")) as f:
        manifest = json.load(f)
    ws = parse_ts(manifest["window_start"].encode())
    we = parse_ts(manifest["window_end"].encode())
    assert ws is not None and we is not None

    products = read_lookup(os.path.join(data_dir, "products.csv"))
    campaigns = read_lookup(os.path.join(data_dir, "campaigns.csv"))

    # ---- Linked IDs: union-find over every ID in a valid link line.
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for line in read_lines(os.path.join(data_dir, "links")):
        f = parse_line(line, 2)
        if f is None or f[0] is None or f[1] is None:
            continue
        a, b = f
        parent.setdefault(a, a)
        parent.setdefault(b, b)
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[ra] = rb
    name_of_root = {}
    for x in parent:
        r = find(x)
        if r not in name_of_root or x < name_of_root[r]:
            name_of_root[r] = x

    def person(uid):
        if uid in parent:
            return name_of_root[find(uid)]
        return uid

    # ---- Events: parse, dedup on event_id (smallest ingest_ts), then validate.
    kept = {}
    for line in read_lines(os.path.join(data_dir, "events")):
        f = parse_line(line, 7)
        if f is None or f[0] is None:
            continue
        ingest = parse_ts(f[3])
        if ingest is None:
            raise SystemExit(f"invalid ingest_ts {f[3]!r}: rules.md says this never happens")
        prev = kept.get(f[0])
        if prev is None or ingest < prev[0]:
            kept[f[0]] = (ingest, f)

    by_person = {}
    for ingest, f in kept.values():
        _, user, ts_raw, _, etype, product, price = f
        if user is None or product is None:
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (ws <= ts < we):
            continue
        if etype not in (b"view", b"cart", b"purchase"):
            continue
        amount = 0
        if etype == b"purchase":
            if price is None or not all_digits(price):
                continue
            amount = int(price)
            if not (1 <= amount <= 10_000_000):
                continue
        if ingest - ts > LATE_MS:
            continue
        category = products.get(product, b"unknown")
        by_person.setdefault(person(user), []).append((ts, etype == b"purchase", amount, category))

    # ---- Clicks: parse, dedup on click_id, validate.
    clicks_by_id = {}
    for line in read_lines(os.path.join(data_dir, "clicks")):
        f = parse_line(line, 4)
        if f is None or f[0] is None:
            continue
        clicks_by_id.setdefault(f[0], f)
    clicks_of = {}
    for click_id, user, campaign, ts_raw in clicks_by_id.values():
        if user is None:
            continue
        ts = parse_ts(ts_raw)
        if ts is None or not (ws - REACH_MS <= ts < we):
            continue
        if campaign not in campaigns:
            continue
        clicks_of.setdefault(person(user), []).append((ts, click_id, campaign))

    # ---- Sessions, attribution, aggregation.
    user_daily = {}
    camp_daily = {}

    def camp_row(cid, day):
        key = (cid, day)
        if key not in camp_daily:
            camp_daily[key] = [0, 0, 0, 0]
        return camp_daily[key]

    for pid, events in by_person.items():
        events.sort(key=lambda e: e[0])
        sessions = []
        last_ts = None
        for e in events:
            if last_ts is None or e[0] - last_ts > GAP_MS:
                sessions.append([])
            sessions[-1].append(e)
            last_ts = e[0]
        pclicks = clicks_of.get(pid, [])
        for s in sessions:
            start = s[0][0]
            day = datetime.date(1970, 1, 1) + datetime.timedelta(days=start // DAY_MS)
            revenue = sum(e[2] for e in s if e[1])
            purchased = any(e[1] for e in s)
            row = user_daily.setdefault((pid, day), [0, 0, 0, {}])
            row[0] += 1
            row[1] += len(s)
            row[2] += revenue
            for e in s:
                if e[1]:
                    row[3][e[3]] = row[3].get(e[3], 0) + e[2]
            window = [c for c in pclicks if c[0] <= start and start - c[0] <= REACH_MS]
            if window:
                best = min(window, key=lambda c: (-c[0], c[1]))
                credited = best[2]
            else:
                credited = b"organic"
            r = camp_row(credited, day)
            r[0] += 1
            r[1] += 1 if purchased else 0
            r[2] += revenue
            for cid in {c[2] for c in window}:
                camp_row(cid, day)[3] += 1

    os.makedirs(os.path.join(out_dir, "user_daily"), exist_ok=True)
    os.makedirs(os.path.join(out_dir, "campaign_daily"), exist_ok=True)
    cols = {"user_id": [], "day": [], "sessions": [], "events": [], "revenue_cents": [], "top_category": []}
    for (pid, day), (ns, ne, rev, cats) in user_daily.items():
        top = None
        if cats:
            top = min(cats.items(), key=lambda kv: (-kv[1], kv[0]))[0].decode()
        cols["user_id"].append(pid.decode())
        cols["day"].append(day)
        cols["sessions"].append(ns)
        cols["events"].append(ne)
        cols["revenue_cents"].append(rev)
        cols["top_category"].append(top)
    pq.write_table(pa.table({
        "user_id": pa.array(cols["user_id"], pa.string()),
        "day": pa.array(cols["day"], pa.date32()),
        "sessions": pa.array(cols["sessions"], pa.int64()),
        "events": pa.array(cols["events"], pa.int64()),
        "revenue_cents": pa.array(cols["revenue_cents"], pa.int64()),
        "top_category": pa.array(cols["top_category"], pa.string()),
    }), os.path.join(out_dir, "user_daily", "part-0.parquet"))

    ccols = {k: [] for k in ("campaign_id", "channel", "day", "sessions", "purchasing_sessions",
                             "revenue_cents", "assisted_sessions")}
    for (cid, day), (ns, npurch, rev, nassist) in camp_daily.items():
        ccols["campaign_id"].append(cid.decode())
        ccols["channel"].append("organic" if cid == b"organic" else campaigns[cid].decode())
        ccols["day"].append(day)
        ccols["sessions"].append(ns)
        ccols["purchasing_sessions"].append(npurch)
        ccols["revenue_cents"].append(rev)
        ccols["assisted_sessions"].append(nassist)
    pq.write_table(pa.table({
        "campaign_id": pa.array(ccols["campaign_id"], pa.string()),
        "channel": pa.array(ccols["channel"], pa.string()),
        "day": pa.array(ccols["day"], pa.date32()),
        "sessions": pa.array(ccols["sessions"], pa.int64()),
        "purchasing_sessions": pa.array(ccols["purchasing_sessions"], pa.int64()),
        "revenue_cents": pa.array(ccols["revenue_cents"], pa.int64()),
        "assisted_sessions": pa.array(ccols["assisted_sessions"], pa.int64()),
    }), os.path.join(out_dir, "campaign_daily", "part-0.parquet"))


if __name__ == "__main__":
    main()
