#!/usr/bin/env python3
"""Adversarial dataset generator for differential testing against tools/reference.py.

Usage: adversarial.py SEED OUT_DIR [EVENTS]

Unlike the official generator, every dataset here is dense with the cases the
rules single out, while staying inside the input guarantees of rules.md:
- ingest_ts is always valid F1; copies of an event with equal ingest_ts are
  identical; copies of a click are identical (up to quoting);
- products.csv and campaigns.csv are clean, IDs unique;
- no value contains a comma, a quote or a newline.

Cases covered (each drawn per row with seed-dependent weights):
- IDs: generator-like (e/u/k + base36, up to 8 digits), and odd IDs (upper case,
  leading/trailing spaces, tabs, \r, multibyte UTF-8, long, digits-only, other
  prefixes, 9+ base36 digits), quoted or not.
- Links: long chains (hundreds of IDs, random order), self links, repeats both
  ways round, empty sides, malformed lines, IDs that never appear in rows,
  upper-cased variants that sort before lower-case ones.
- Events: duplicates (1-3 extra copies) with later ingest, conflicting users,
  invalid earliest copy (bad ts / type / user / product / price), later valid
  copy that must NOT rescue; equal-ingest identical copies; late at exactly
  24 h and 24 h + 1 s; ingest before ts; every timestamp form and bad form.
- Window edges: ts exactly window_start, window_end - 1, window_end; clicks at
  window_start - 7 d exactly and 1 ms earlier.
- Sessions: gaps of exactly 1,800,000 and 1,800,001 ms, equal timestamps,
  midnight crossings.
- Clicks: exactly 7 d before a session start, 7 d + 1 ms, at the start, 1 ms
  after, same-millisecond ties between click IDs that differ only in byte
  order (including odd IDs), unknown / case-variant campaigns, quoted copies.
- Prices: leading zeros, 0, 1, 10,000,000, 10,000,001, empty, signed,
  decimal, non-ASCII digits, '\r' suffix. Types in other cases / padded.
- Skew: one "whale" person can hold a large share of events.
- Files: header-only files, many small files, one empty-line-heavy file.
"""
import datetime
import json
import os
import random
import sys

DAY = 86_400_000
GAP = 1_800_000
REACH = 7 * DAY
B36 = "0123456789abcdefghijklmnopqrstuvwxyz"


def b36(n):
    s = ""
    while True:
        s = B36[n % 36] + s
        n //= 36
        if n == 0:
            return s


def f1(ms):
    d = datetime.datetime.fromtimestamp(ms // 1000, tz=datetime.timezone.utc)
    return d.strftime("%Y-%m-%dT%H:%M:%SZ")


def f2(ms, off_min):
    d = datetime.datetime.fromtimestamp(ms // 1000 + off_min * 60, tz=datetime.timezone.utc)
    sign = "+" if off_min >= 0 else "-"
    a = abs(off_min)
    return d.strftime("%Y-%m-%dT%H:%M:%S") + f"{sign}{a // 60:02d}:{a % 60:02d}"


class Gen:
    def __init__(self, seed, n_events):
        self.r = random.Random(seed)
        self.n = n_events
        r = self.r
        year = r.choice([2025, 2026, 2027, 2028])
        # Some seeds put Feb 29 2028 inside the window.
        if r.random() < 0.3:
            start = datetime.datetime(2028, 2, r.randint(20, 28), tzinfo=datetime.timezone.utc)
        else:
            start = datetime.datetime(year, r.randint(1, 12), r.randint(1, 28), tzinfo=datetime.timezone.utc)
        self.ws = int(start.timestamp()) * 1000
        self.we = self.ws + r.randint(3, 14) * DAY
        self.cs = self.ws - REACH
        self.odd_rate = r.choice([0.02, 0.1, 0.3])
        self.quote_rate = r.choice([0.0, 0.2, 0.6])

    # ---- IDs -------------------------------------------------------------
    def gen_id(self, prefix, i):
        r = self.r
        if r.random() < self.odd_rate:
            kind = r.randrange(10)
            base = prefix + b36(i)
            if kind == 0:
                return base.upper()
            if kind == 1:
                return " " + base
            if kind == 2:
                return base + " "
            if kind == 3:
                return base + "\t"
            if kind == 4:
                return base + "é"
            if kind == 5:
                return prefix + b36(i).rjust(12, "0")  # longer than 8 digits
            if kind == 6:
                return str(i)  # digits only, no prefix
            if kind == 7:
                return "x" + b36(i)  # another prefix
            if kind == 8:
                return base + "\r"
            return prefix + "_" + b36(i)
        return prefix + b36(i)

    def q(self, v):
        if v is None:
            return ""
        if self.r.random() < self.quote_rate:
            return '"' + v + '"'
        return v

    def line(self, fields):
        return ",".join(self.q(f) for f in fields)

    # ---- timestamps ------------------------------------------------------
    def ts_text(self, ms):
        r = self.r
        k = r.random()
        if k < 0.5:
            return f1(ms) if ms % 1000 == 0 else str(ms)
        if k < 0.65:
            if ms % 1000 != 0:
                return str(ms)
            return f2(ms, r.choice([-600, -330, -1, 0, 1, 345, 720, 23 * 60 + 59, -(23 * 60 + 59)]))
        if k < 0.97:
            return str(ms).rjust(r.choice([13, 14, 16]), "0")
        return "-00:00"  # placeholder replaced below

    def bad_ts(self, ms):
        r = self.r
        s = f1(ms - ms % 1000)
        return r.choice([
            "", s.lower(), s.replace("T", " "), s[:-1], s[:-1] + ".000Z", s[:-1] + "+0200",
            s[:-1] + "+24:00", s[:-1] + "+05:60", "1.79e12", "-5", str(ms) + ".0",
            s[:5] + "13" + s[7:], s[:5] + "02-30" + s[10:], s[:11] + "24" + s[13:],
            s[:17] + "60Z", "2027-02-29T00:00:00Z", "٢٥٠", "１２３", s + "\r",
            "99999999999999999999999",
        ])

    def ts_field(self, ms):
        t = self.ts_text(ms)
        if t == "-00:00":
            if ms % 1000 == 0:
                return f1(ms)[:-1] + "-00:00"
            return str(ms)
        return t

    # ---- build -----------------------------------------------------------
    def build(self, out):
        r = self.r
        os.makedirs(out, exist_ok=True)
        for sub in ("events", "clicks", "links"):
            os.makedirs(os.path.join(out, sub), exist_ok=True)
        with open(os.path.join(out, "manifest.json"), "w") as f:
            json.dump({"window_start": f1(self.ws), "window_end": f1(self.we)}, f)
        n_products = r.randint(5, 300)
        cats = ["c" + str(i) for i in range(r.randint(1, 25))] + ["Unknown", "UNKNOWN", "unknown_", " sp"]
        products = ["p" + str(i) for i in range(n_products)]
        with open(os.path.join(out, "products.csv"), "w") as f:
            f.write("product_id,category\n")
            for p in products:
                f.write(f"{p},{r.choice(cats)}\n")
        campaigns = ["c_" + str(i) for i in range(r.randint(1, 60))]
        with open(os.path.join(out, "campaigns.csv"), "w") as f:
            f.write("campaign_id,channel\n")
            for c in campaigns:
                f.write(f"{c},{r.choice(['search', 'social', 'email', 'display', 'affiliate'])}\n")

        # Persons: each has 1..k IDs; some get long chains; one whale.
        n_persons = max(3, self.n // r.choice([5, 20, 60]))
        persons = []
        next_id = 1
        for p in range(n_persons):
            k = 1 if r.random() < 0.5 else r.randint(2, 6)
            if r.random() < 0.01:
                k = r.randint(50, 400)
            ids = []
            for _ in range(k):
                ids.append(self.gen_id("u", next_id))
                next_id += 1
            persons.append(ids)
        links = []
        for ids in persons:
            order = ids[:]
            r.shuffle(order)
            for i in range(1, len(order)):
                a, b = (order[i], order[r.randrange(i)]) if r.random() < 0.7 else (order[i], order[i - 1])
                links.append((a, b) if r.random() < 0.5 else (b, a))
                if r.random() < 0.1:
                    links.append((b, a))  # repeat, other way round
            if r.random() < 0.05:
                links.append((ids[0], ids[0]))  # self link
            if r.random() < 0.05:
                extra = self.gen_id("u", next_id)
                next_id += 1
                links.append((ids[0], extra))  # linked, never used in a row
        # Shared device: one ID linking several persons into one.
        for _ in range(r.randint(0, 3)):
            dev = "u" + b36(10**9 + r.randrange(10**6))
            for ids in r.sample(persons, min(len(persons), r.randint(2, 40))):
                links.append((dev, r.choice(ids)))
        link_lines = []
        for a, b in links:
            if r.random() < 0.01:
                a = ""
            if r.random() < 0.005:
                b = b.upper()
            link_lines.append(self.line([a, b]))
        for _ in range(max(1, len(links) // 200)):
            link_lines.append(r.choice(['u1,u2,u3', 'u1', '"u1,u2', 'u1",u2', '', 'u"1,u2']))

        whale = r.randrange(n_persons) if r.random() < 0.6 else None
        whale_share = r.choice([0.1, 0.3, 0.6])

        events, clicks = [], []
        eid = 1
        cid = 1
        span = self.we - self.ws
        session_starts = []
        while len(events) < self.n:
            p = whale if whale is not None and r.random() < whale_share else r.randrange(n_persons)
            ids = persons[p]
            # A burst of events forming sessions with adversarial gaps.
            t = self.ws + r.randrange(span)
            if r.random() < 0.05:
                t = r.choice([self.ws, self.we - 1, self.we, self.ws - 1, self.ws + r.randrange(1000)])
            burst = r.randint(1, 8)
            session_starts.append((p, t))
            for _ in range(burst):
                uid = ids[0] if r.random() < 0.5 else r.choice(ids)
                self.add_event(events, eid, uid, t, products)
                eid += 1
                step = r.choice([0, 0, 1, 1000, 60_000, 600_000, GAP, GAP, GAP + 1, GAP - 1, 3 * GAP, DAY])
                t += step
                if r.random() < 0.05:
                    session_starts.append((p, t))
        # Clicks around session starts, including every boundary.
        for p, s in session_starts:
            if r.random() < 0.5:
                continue
            ids = persons[p]
            for _ in range(r.randint(1, 4)):
                off = r.choice([0, 1, -1, REACH, REACH + 1, REACH - 1, r.randrange(REACH), r.randrange(DAY), -60_000])
                ct = s - off
                if r.random() < 0.02:
                    ct = r.choice([self.cs, self.cs - 1, self.we, self.we - 1])
                camp = r.choice(campaigns)
                if r.random() < 0.03:
                    camp = r.choice([camp.upper(), camp + " ", "c_999999", "", "organic"])
                kid = self.gen_id("k", cid)
                cid += 1
                uid = r.choice(ids)
                if r.random() < 0.01:
                    uid = ""
                ts = self.ts_field(ct) if r.random() > 0.01 else self.bad_ts(ct)
                if r.random() < 0.003:
                    kid = ""
                row = [kid, uid, camp, ts]
                clicks.append(self.line(row))
                if r.random() < 0.05:
                    clicks.append(self.line(row))  # duplicate copy, quoted differently
                if r.random() < 0.15:
                    # Same-millisecond tie with a sibling ID differing in byte order.
                    # Own namespace ("s" prefix) so a sibling never reuses a click ID.
                    # "-" never occurs in base 36, so base + suffix is unique.
                    base = "s" + b36(cid) + "-"
                    cid += 1
                    for suffix in r.sample(["", "0", "a", "Z", "_", " ", "é"], r.randint(2, 4)):
                        clicks.append(self.line([base + suffix, r.choice(ids), r.choice(campaigns), ts]))
        for _ in range(max(1, len(clicks) // 300)):
            clicks.append(r.choice(['k1,u1,c_1', 'k1,u1,c_1,2026-01-01T00:00:00Z,x', '"k1,u1,c_1,1', '']))

        self.write_files(out, "events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", events)
        self.write_files(out, "clicks", "click_id,user_id,campaign_id,ts", clicks)
        self.write_files(out, "links", "user_id_a,user_id_b", link_lines)

    def add_event(self, events, eid, uid, t, products):
        r = self.r
        key = self.gen_id("e", eid)
        typ = r.choices(["view", "cart", "purchase"], [6, 2, 2])[0]
        prod = r.choice(products)
        price = str(r.randint(1, 50_000))
        if r.random() < 0.05:
            prod = r.choice(["", "P0", "p99999", prod + " "])
        if typ == "purchase" and r.random() < 0.2:
            price = r.choice(["0250", "0", "1", "10000000", "10000001", "", "-5", "+5", "1.0", "٢٥٠", "100\r", " 100"])
        if r.random() < 0.02:
            typ = r.choice(["Purchase", "VIEW", "cart ", " view", "", "buy"])
        if r.random() < 0.005:
            uid = ""
        lag = r.choice([0, 1000, 60_000, DAY, DAY + 1000, DAY - 1000, -60_000, 5 * DAY])
        ingest = (t // 1000) * 1000 + lag
        ingest -= ingest % 1000
        ts = self.ts_field(t) if r.random() > 0.01 else self.bad_ts(t)
        row = [key, uid, ts, f1(ingest), typ, prod, price]
        events.append(self.line(row))
        roll = r.random()
        if roll < 0.08:
            # Later copies: may differ in anything (only the earliest counts).
            later = ingest
            for _ in range(r.randint(1, 3)):
                # Strictly increasing: copies with equal ingest_ts must be identical.
                later += r.choice([1000, 60_000, 600_000])
                other = row[:]
                other[3] = f1(later)
                if r.random() < 0.3:
                    other[1] = r.choice([uid, "", self.gen_id("u", r.randrange(1, 10_000))])
                if r.random() < 0.3:
                    other[2] = self.ts_field(t + r.choice([0, 1000, GAP + 1]))
                if r.random() < 0.2:
                    other[4] = r.choice(["view", "purchase", "Purchase"])
                events.append(self.line(other))
        elif roll < 0.10:
            # Earliest copy invalid, later copy valid: the event must be dropped.
            bad = row[:]
            bad[2] = self.bad_ts(t)
            bad[3] = f1(ingest - 1000)
            events.append(self.line(bad))
        elif roll < 0.11:
            events.append(self.line(row))  # identical copy, equal ingest

    def write_files(self, out, sub, header, rows):
        r = self.r
        r.shuffle(rows)
        n_files = r.randint(1, 8)
        cuts = sorted(r.randrange(len(rows) + 1) for _ in range(n_files - 1))
        bounds = [0] + cuts + [len(rows)]
        for i in range(n_files):
            chunk = rows[bounds[i]:bounds[i + 1]]
            with open(os.path.join(out, sub, f"part-{i:05d}.csv"), "w", newline="") as f:
                f.write(header + "\n")
                for line in chunk:
                    f.write(line + "\n")
        with open(os.path.join(out, sub, f"part-{n_files:05d}.csv"), "w") as f:
            f.write(header + "\n")  # header only


def main():
    seed = int(sys.argv[1])
    out = sys.argv[2]
    n = int(sys.argv[3]) if len(sys.argv) > 3 else 20_000
    Gen(seed, n).build(out)


if __name__ == "__main__":
    main()
