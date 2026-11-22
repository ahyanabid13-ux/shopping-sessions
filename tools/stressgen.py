#!/usr/bin/env python3
"""Adversarial test-data generator for the shopping-sessions pipeline.

Usage: stressgen.py OUT_DIR --profile {ids,dialect,time,skew} --seed N [--scale K]

Writes one complete dataset in the /data layout (manifest.json, products.csv,
campaigns.csv, events/*.csv, clicks/*.csv, links/*.csv). Standard library only.
Deterministic from (profile, seed, scale): every random choice comes from one
random.Random seeded with a string (the random module hashes str seeds with
SHA-512), Python's hash() is never used, and no output order comes from
iterating a set.

Profiles:
  ids      opaque-ID shapes at the boundaries of fast paths (short base-36,
           32 hex, near-hex, case, prefixes, whitespace, long IDs), drawn so
           that equal and nearly equal IDs recur in links, naming, dedup and
           same-millisecond click tie-breaks
  dialect  the CSV dialect of rules.md under stress: quoting, every kind of
           malformed line, \\r, exact line lengths, odd files, a >5 MB file
           with engineered positions at each MiB boundary, exact-size files
  time     timestamp forms, bad forms, window edges, late cutoff, gaps,
           midnight, 7-day click reach, 29 February 2028 (even seeds)
  skew     a 600-ID chain, a 2,000-person shared device, a 20,000-event whale,
           a 5,000-click user, thousands of single-event people, event_ids
           copied 50 times, repeated / reversed / self links
"""
import argparse
import json
import os
import random

B36 = "0123456789abcdefghijklmnopqrstuvwxyz"
HEX = "0123456789abcdef"
HEXU = "0123456789ABCDEF"
DIG = "0123456789"
SEC = 1000
MIN = 60 * SEC
HOUR = 60 * MIN
DAY = 24 * HOUR
GAP = 30 * MIN
REACH = 7 * DAY
MIB = 1 << 20
N_BAD = 36  # number of bad timestamp spellings in Gen.bad_ts
ODD_CHARS = ["g", "z", "/", ":", "`", " ", "é"]

EV_HDR = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents"
CK_HDR = "click_id,user_id,campaign_id,ts"
LK_HDR = "user_id_a,user_id_b"

ODD_SHAPES = ["b9", "b10", "b31", "b32", "b33", "hex_up1", "hex_upall", "hex31", "hex33",
              "nonhex", "zero", "zero16a", "zero16b", "fff", "wrongp", "noprefix", "digits",
              "upperp", "space", "tab", "cr", "one", "long", "utf8"]
ALL_SHAPES = ["short", "hex"] + ODD_SHAPES


# ---------------------------------------------------------------- calendar

def days_from_civil(y, m, d):
    y = y - 1 if m <= 2 else y
    era = y // 400
    yoe = y - era * 400
    doy = (153 * ((m + 9) % 12) + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def civil_from_days(z):
    z += 719468
    era = z // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    y = yoe + era * 400
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + 3 if mp < 10 else mp - 9
    return (y + (1 if m <= 2 else 0), m, d)


def fmt_local(sec):
    days, rem = divmod(sec, 86400)
    y, m, d = civil_from_days(days)
    return "%04d-%02d-%02dT%02d:%02d:%02d" % (y, m, d, rem // 3600, rem // 60 % 60, rem % 60)


def f1(ms):
    assert ms % SEC == 0, ms
    return fmt_local(ms // SEC) + "Z"


def f2(ms, off_min, neg_zero=False):
    assert ms % SEC == 0, ms
    s = fmt_local(ms // SEC + off_min * 60)
    sign = "-" if (off_min < 0 or neg_zero) else "+"
    a = abs(off_min)
    return s + "%s%02d:%02d" % (sign, a // 60, a % 60)


# ---------------------------------------------------------------- generator

class Gen:
    def __init__(self, profile, seed, scale):
        self.profile = profile
        self.seed = seed
        self.scale = scale
        self.r = random.Random("adversarial/%s/%d/%d" % (profile, seed, scale))
        self.ev, self.ck, self.lk = [], [], []
        self.used = {"e": set(), "k": set(), "u": set()}
        self.pending = {"e": [], "k": [], "u": []}
        self.persons = []
        self.pairs = []
        self.cfg = dict(quote=0.12, malformed=0.003, bad_ts=0.005, late=0.01, dup_ev=0.03,
                        dup_ck=0.02, bad_type=0.002, bad_price=0.05, unknown_prod=0.005,
                        empty_user=0.002, unknown_camp=0.005, odd=0.04, hex=0.125,
                        family=0.0, family_use=0.0, crlf=0.0, stray_cr=0.0, link_upper=0.0005)

    # ---- basics
    def rs(self, alphabet, n):
        r = self.r
        return "".join(r.choice(alphabet) for _ in range(n))

    def window(self, leap=False):
        r = self.r
        if leap:
            start_day = days_from_civil(2028, 2, 29) - r.randint(0, 13)
        else:
            start_day = r.randint(days_from_civil(2025, 1, 1), days_from_civil(2028, 12, 18))
        self.ws = start_day * DAY
        self.we = self.ws + 14 * DAY

    def catalog(self):
        r = self.r
        cats = ["apparel", "books", "Books", "electronics", "garden", "home", "toys", "toys_kids",
                "beauty", "sports", "épicerie", "zz_misc", "Toys"]
        self.products = []
        for i in range(1, 181):
            self.products.append(("p%d" % i, r.choice(cats)))
        self.products.append(("P1x", "Books"))
        self.products.append(("p_" + "0" * 20, "books"))
        self.prod_ids = [p for p, _ in self.products]
        chans = ["search", "social", "email", "display", "affiliate"]
        self.campaigns = [("c_%d" % i, r.choice(chans)) for i in range(1, 61)]
        self.campaigns += [("c_01", "email"), ("c_007", "search"), ("c_1000000", "display")]
        self.camp_ids = [c for c, _ in self.campaigns]
        self.whale_camps = r.sample(self.camp_ids, 4)

    def pick_product(self):
        r = self.r
        if r.random() < 0.6:
            return self.prod_ids[min(int(r.expovariate(1 / 12.0)), len(self.prod_ids) - 1)]
        return r.choice(self.prod_ids)

    def pick_camp(self, dirt=True):
        r = self.r
        if dirt and r.random() < self.cfg["unknown_camp"]:
            return r.choice(["c_999", "C_1", "c_1 ", " c_2", "c_0", "c_", "c_1\t", "organic"])
        if r.random() < 0.3:
            return r.choice(self.whale_camps)
        return r.choice(self.camp_ids)

    # ---- IDs
    def pick_shape(self):
        r = self.r
        x = r.random()
        if x < self.cfg["odd"]:
            return r.choice(ODD_SHAPES)
        if x < self.cfg["odd"] + self.cfg["hex"]:
            return "hex"
        return "short"

    def rid(self, p, shape):
        r, rs = self.r, self.rs
        if shape == "short":
            return p + rs(B36, r.randint(1, 8))
        if shape in ("b9", "b10", "b31", "b32", "b33"):
            return p + rs(B36, int(shape[1:]))
        if shape == "hex":
            return p + rs(HEX, 32)
        if shape == "hex_up1":
            h = list(rs(HEX, 32))
            h[r.randrange(32)] = r.choice("ABCDEF")
            return p + "".join(h)
        if shape == "hex_upall":
            h = list(rs(HEXU, 32))
            h[r.randrange(32)] = r.choice("ABCDEF")
            return p + "".join(h)
        if shape == "hex31":
            return p + rs(HEX, 31)
        if shape == "hex33":
            return p + rs(HEX, 33)
        if shape == "nonhex":
            h = list(rs(HEX, 32))
            h[r.choice([0, 15, 16, r.randrange(1, 31), 31])] = r.choice(ODD_CHARS)
            return p + "".join(h)
        if shape == "zero":
            return p + "0" * 32
        if shape == "zero16a":
            return p + "0" * 16 + rs(HEX, 16)
        if shape == "zero16b":
            return p + rs(HEX, 16) + "0" * 16
        if shape == "fff":
            return p + r.choice(["f" * 32, "F" * 32, "0" * 31 + "1", "f" * 31 + "e"])
        if shape == "wrongp":
            q = r.choice([c for c in "euk" if c != p])
            return q + (rs(B36, r.randint(1, 8)) if r.random() < 0.6 else rs(HEX, 32))
        if shape == "noprefix":
            return rs(B36, r.randint(1, 8)) if r.random() < 0.6 else rs(HEX, 32)
        if shape == "digits":
            return rs(DIG, r.choice([1, 2, 5, 8, 13, 19, 20, 32]))
        if shape == "upperp":
            return p.upper() + (rs(B36, r.randint(1, 8)) if r.random() < 0.6 else rs(HEX, 32))
        if shape in ("space", "tab", "cr"):
            ch = {"space": " ", "tab": "\t", "cr": "\r"}[shape]
            v = p + (rs(B36, r.randint(1, 8)) if r.random() < 0.6 else rs(HEX, 32))
            k = r.choice([0, len(v), r.randint(1, len(v) - 1) if len(v) > 1 else 1])
            return v[:k] + ch + v[k:]
        if shape == "one":
            return r.choice(B36 + "UEK~_")
        if shape == "long":
            return p + rs(B36, r.randint(99, 299))
        if shape == "utf8":
            return (p + rs(B36, r.randint(1, 4)) + r.choice(["é", "中", "\U0001f600", "ÿ"])
                    + rs(B36, r.randint(0, 3)))
        raise ValueError(shape)

    def family(self, p):
        """IDs that are nearly equal across the short / 32-hex / odd boundaries."""
        r, rs = self.r, self.rs
        h = rs(HEX, 32)
        k = r.randrange(16)
        if k == 0:
            return [p + h, p + rs(HEX, 16) + h[16:]]
        if k == 1:
            return [p + h, p + h[:16] + rs(HEX, 16)]
        if k == 2:
            return [p + h, p + h[:31] + r.choice([c for c in HEX if c != h[31]])]
        if k == 3:
            return [p + h, p + h[:8] + rs(HEX, 24)]
        if k == 4:
            s = rs(HEX, r.randint(1, 8))
            return [p + s, p + s + "0" * (32 - len(s))]
        if k == 5:
            return r.choice([[p + "00000000", p + "0" * 32], [p + "0" * 32, p + "0" * 31 + "1"],
                             [p + "0", p + "0" * 32], [p + "f" * 32, p + "f" * 31 + "e"]])
        if k == 6:
            h2 = h[:5] + r.choice("abcdef") + h[6:]
            return [p + h2, p + h2.upper(), p.upper() + h2,
                    p + h2[:5] + h2[5].upper() + h2[6:]]
        if k == 7:
            return [p + h[:31], p + h, p + h + r.choice(HEX)]
        if k == 8:
            s = rs(B36, 9)
            return [p + s, p + s + r.choice(B36), p + s + rs(B36, 22), p + s + rs(B36, 23)]
        if k == 9:
            s = rs(B36, r.randint(1, 8))
            return [p + s, " " + p + s, p + s + " ", p + s + "\t"]
        if k == 10:
            L = rs(B36, r.randint(150, 290))
            return [p + L, p + L[:-1] + r.choice([c for c in B36 if c != L[-1]])]
        if k == 11:
            s = rs(B36, r.randint(1, 6))
            return [p + s + "z", p + s + "é", p + s + "~"]
        if k == 12:
            q = [c for c in "euk" if c != p]
            return [q[0] + h, q[1] + h, p + h, h]
        if k == 13:
            return [p + h, p + h[:16], p + h[16:]]
        if k == 14:
            s = rs(B36, r.randint(1, 7))
            return [p + s, p + s + "\r", p + s + "0"]
        return [p + h, p + h[:-1] + "g", p + "g" + h[1:], p + h[:16] + ":" + h[17:]]

    def take_family(self, p):
        for _ in range(200):
            fam = self.family(p)
            if len(set(fam)) == len(fam) and not any(v in self.used[p] for v in fam):
                for v in fam:
                    self.used[p].add(v)
                return fam
        raise RuntimeError("no fresh family")

    def claim(self, p, values):
        """Mark explicit IDs used; return False if any was already taken."""
        if len(set(values)) != len(values) or any(v in self.used[p] for v in values):
            return False
        for v in values:
            self.used[p].add(v)
        return True

    def new_id(self, p, shape=None):
        r, c = self.r, self.cfg
        pend = self.pending[p]
        if shape is None and pend and r.random() < c["family_use"]:
            v = pend.pop(r.randrange(len(pend)))
            if v not in self.used[p]:
                self.used[p].add(v)
                return v
        for _ in range(400):
            if shape is None and r.random() < c["family"]:
                fam = self.family(p)
                if len(set(fam)) != len(fam) or any(v in self.used[p] for v in fam):
                    continue
                self.used[p].add(fam[0])
                pend.extend(fam[1:])
                return fam[0]
            v = self.rid(p, shape or self.pick_shape())
            if v not in self.used[p]:
                self.used[p].add(v)
                return v
        return self.new_id(p, "long")

    # ---- CSV rendering
    def ser(self, vals, q):
        r = self.r
        out = []
        for v in vals:
            v = "" if v is None else v
            assert "," not in v and '"' not in v and "\n" not in v, repr(v)
            out.append('"' + v + '"' if q and r.random() < q else v)
        return ",".join(out)

    def malform(self, vals):
        r = self.r
        n = len(vals)
        vals = ["" if v is None else v for v in vals]
        k = r.randrange(15)
        if k == 0:
            return self.ser(vals[:-1], self.cfg["quote"])
        if k == 1:
            return self.ser(vals + [r.choice(["", "x", vals[-1]])], self.cfg["quote"])
        if k == 2:
            i = r.randrange(n)
            v = vals[i]
            j = r.randint(1, len(v) - 1) if len(v) > 1 else 0
            vals[i] = None
            parts = self.ser(vals, 0).split(",")
            parts[i] = (v[:j] + '"' + v[j:]) if v else '"'
            return ",".join(parts)
        if k in (3, 4):
            i = r.randrange(n)
            parts = self.ser(vals, 0).split(",")
            parts[i] = ('"' + parts[i]) if k == 3 else (parts[i] + '"')
            return ",".join(parts)
        if k == 5:
            i = r.randrange(n)
            parts = self.ser(vals, 0).split(",")
            parts[i] = '"'
            return ",".join(parts)
        if k == 6:
            return ""
        if k == 7:
            return "," * r.choice([n - 1, n, n - 2 if n > 2 else n, 2 * n])
        if k == 8:
            i = r.randrange(n)
            parts = self.ser(vals, 0).split(",")
            parts[i] = '"' + parts[i] + '"x'
            return ",".join(parts)
        if k == 9:
            i = r.randrange(n)
            parts = self.ser(vals, 0).split(",")
            parts[i] = '""' + parts[i] + '""'
            return ",".join(parts)
        if k == 10:
            parts = self.ser(vals, 0).split(",")
            i = r.randrange(n - 1)
            return ",".join(parts[:i] + ['"' + parts[i] + "," + parts[i + 1] + '"'] + parts[i + 2:])
        if k == 11:
            return r.choice(["\r", " ", '"', '""', "\t", '"' + '","' * (n - 1) + '"'])
        if k == 12:
            i = r.randrange(n)
            parts = self.ser(vals, 0).split(",")
            parts[i] = 'x"' + parts[i] + '"'
            return ",".join(parts)
        if k == 13:
            parts = self.ser(vals, 0).split(",")
            parts[-1] = '"' + parts[-1] + '"\r'
            return ",".join(parts)
        return self.ser(vals, 0) + ',"'

    def render(self, vals, dirt=True, q=None, no_cr=()):
        r, c = self.r, self.cfg
        if dirt and r.random() < c["malformed"]:
            return self.malform(vals)
        vals = list(vals)
        if dirt and c["stray_cr"] and r.random() < c["stray_cr"]:
            i = r.choice([j for j in range(len(vals)) if j not in no_cr])
            v = vals[i] or ""
            k = r.randint(0, len(v))
            vals[i] = v[:k] + "\r" + v[k:]
        s = self.ser(vals, c["quote"] if q is None else q)
        if dirt and c["crlf"] and r.random() < c["crlf"]:
            s += "\r"
        return s

    # ---- timestamps
    def pick_off(self):
        r = self.r
        if r.random() < 0.08:
            return r.choice([0, 23 * 60 + 59, -(23 * 60 + 59), 13 * 60, -12 * 60, 5 * 60 + 45])
        return r.randint(-40, 48) * 15

    def f3(self, t, zeros=None):
        r = self.r
        if zeros is None:
            zeros = r.choice([1, 2, 3, 6, 7, 12, 30]) if r.random() < 0.05 else 0
        return "0" * zeros + str(t)

    def ts_str(self, t, dirt=True):
        r = self.r
        if dirt and r.random() < self.cfg["bad_ts"]:
            return self.bad_ts(t)
        if t % SEC:
            return self.f3(t)
        x = r.random()
        if x < 0.58:
            return f1(t)
        if x < 0.68:
            if r.random() < 0.05:
                return f2(t, 0, neg_zero=True)
            return f2(t, self.pick_off())
        return self.f3(t)

    def bad_ts(self, t, k=None):
        r = self.r
        s = t // SEC * SEC
        a = f1(s)
        y = int(a[:4])
        nl = y if y % 4 else 2027
        opts = [
            lambda: None,
            lambda: a[:-1] + ".%03dZ" % r.randint(0, 999),
            lambda: a[:-1] + ".000Z",
            lambda: a[:-1] + "z",
            lambda: a[:10] + "t" + a[11:],
            lambda: a[:10] + " " + a[11:],
            lambda: a[:-1],
            lambda: str(t) + ".0",
            lambda: "1.79e12",
            lambda: "-" + str(t),
            lambda: "+" + str(t),
            lambda: " " + str(t),
            lambda: str(t) + " ",
            lambda: a[:5] + "13" + a[7:],
            lambda: a[:5] + "00" + a[7:],
            lambda: a[:8] + "00" + a[10:],
            lambda: a[:8] + "32" + a[10:],
            lambda: a[:5] + "02-30" + a[10:],
            lambda: "%04d-02-29" % nl + a[10:],
            lambda: a[:5] + "04-31" + a[10:],
            lambda: a[:11] + "24" + a[13:],
            lambda: a[:14] + "60" + a[16:],
            lambda: a[:17] + "60" + a[19:],
            lambda: a[:-1] + "+24:00",
            lambda: a[:-1] + "+05:60",
            lambda: a[:-1] + "+0200",
            lambda: a[:-1] + "-24:00",
            lambda: a[:-1] + "+5:00",
            lambda: a + "+00:00",
            lambda: "".join(chr(0x660 + int(ch)) for ch in str(t)),
            lambda: "２" + a[1:],
            lambda: a + "\r",
            lambda: a[:16] + "Z",
            lambda: a[:5] + a[6:],
            lambda: a.replace("-", "/", 2),
            lambda: str(t)[:6] + "٣" + str(t)[7:],
        ]
        if k is None:
            k = r.randrange(len(opts))
        return opts[k % len(opts)]()

    # ---- rows
    def pick_uid(self, ids):
        r = self.r
        return ids[0] if r.random() < 0.5 else r.choice(ids)

    def pick_tpp(self, dirt=True):
        r, c = self.r, self.cfg
        x = r.random()
        etype = "view" if x < 0.62 else ("cart" if x < 0.82 else "purchase")
        product = self.pick_product()
        if etype == "purchase":
            price = str(r.randint(100, 50000))
            if r.random() < 0.03:
                price = "0" * r.randint(1, 3) + price
            if r.random() < 0.01:
                price = r.choice(["1", "10000000", "0010000000"])
        else:
            price = r.choice([None, None, None, None, "0", "-5", "abc", "19.99", str(r.randint(1, 999))])
        if dirt:
            if r.random() < c["bad_type"]:
                etype = r.choice(["Purchase", "VIEW", "cart ", " view", "click", "purchase\t",
                                  "vi ew", "Cart", "purchases", "vie"])
            if etype == "purchase" and r.random() < c["bad_price"]:
                price = r.choice(["19.99", "-500", "0", None, "10000001", "+100", "1e3",
                                  "２５０", "٢٥٠", " 100", "100 ",
                                  "99999999999999999999", "00000000", "0x10"])
            if r.random() < c["unknown_prod"]:
                product = r.choice([None, "p_missing%d" % r.randint(1, 99), "P1", "p1 ", " p2",
                                    "p0", "unknown"])
        return etype, product, price

    def pick_ingest(self, t, dirt=True):
        r, c = self.r, self.cfg
        base = -(-t // SEC) * SEC
        x = r.random()
        if dirt and x < c["late"]:
            return base + DAY + r.choice([0, 0, SEC, SEC, r.randint(2, 4 * 86400) * SEC])
        if x < c["late"] + 0.03:
            return base - r.randint(1, 7200) * SEC
        return base + r.choice([0, r.randint(0, 120), r.randint(0, 7200), r.randint(0, 80000)]) * SEC

    def add_event(self, uid, t, eid=None, etype=None, product=None, price=None, ingest=None,
                  ts=None, dirt=True, dup=None, keep_type=False):
        r, c = self.r, self.cfg
        if eid is None:
            eid = self.new_id("e")
        if not keep_type:
            etype, product, price = self.pick_tpp(dirt)
        if ingest is None:
            ingest = self.pick_ingest(t, dirt)
        if ts is None:
            ts = self.ts_str(t, dirt)
        if dirt and r.random() < c["empty_user"]:
            uid = r.choice([None, uid.upper()])
        vals = [eid, uid, ts, f1(ingest), etype, product, price]
        s = self.render(vals, dirt, no_cr=(3,))
        self.ev.append(s)
        if dup or (dirt and r.random() < c["dup_ev"]):
            self.dup_event(vals, ingest, t, s, n=dup if isinstance(dup, int) else None)
        return vals

    def dup_event(self, vals, ingest, t, s, n=None):
        """Copies of one event_id. Copies with equal ingest_ts are byte-identical."""
        r = self.r
        n = n or r.randint(1, 3)
        used_ing = [ingest]
        for _ in range(n):
            mode = r.random()
            if mode < 0.15:
                self.ev.append(s)  # identical copy, same ingest
                continue
            earlier = r.random() < 0.3
            while True:
                ing = ingest + (-1 if earlier else 1) * r.randint(1, 600) * SEC * r.choice([1, 1, 2, 30])
                if ing not in used_ing:
                    break
            used_ing.append(ing)
            v = list(vals)
            v[3] = f1(ing)
            alt = r.random()
            if alt < 0.2 and self.persons:
                v[1] = self.pick_uid(r.choice(self.persons))
            elif alt < 0.3:
                v[2] = self.bad_ts(t)
            elif alt < 0.4:
                v[4], v[5], v[6] = "purchase", self.pick_product(), r.choice(["0", "19.99", None, "777"])
            elif alt < 0.5:
                v[2] = self.ts_str(t + r.choice([-1, 1]) * r.randint(1, 3 * HOUR), False)
            elif alt < 0.55:
                v[1] = None
            elif alt < 0.6:
                self.ev.append(self.malform(v))
                continue
            self.ev.append(self.render(v, True, no_cr=(3,)))

    def add_click(self, uid, t, camp=None, cid=None, dirt=True, ts=None, dup=None):
        r, c = self.r, self.cfg
        if cid is None:
            cid = self.new_id("k")
        if camp is None:
            camp = self.pick_camp(dirt)
        if ts is None:
            ts = self.ts_str(t, dirt)
        if dirt and r.random() < c["empty_user"]:
            uid = None
        vals = [cid, uid, camp, ts]
        ncopy = dup if dup is not None else (r.randint(1, 2) if dirt and r.random() < c["dup_ck"] else 0)
        if not ncopy:
            self.ck.append(self.render(vals, dirt))
            return
        # copies have identical values: same \r decisions, quoting differs
        if dirt and c["stray_cr"] and r.random() < c["stray_cr"] * 5:
            i = r.randrange(4)
            vals[i] = (vals[i] or "") + "\r"
        tail = "\r" if dirt and c["crlf"] and r.random() < c["crlf"] else ""
        for _ in range(ncopy + 1):
            self.ck.append(self.ser(vals, max(c["quote"], 0.3)) + tail)

    def add_link(self, a, b, dirt=True):
        r, c = self.r, self.cfg
        if r.random() < 0.5:
            a, b = b, a
        self.pairs.append((a, b))
        if dirt:
            if r.random() < c["empty_user"]:
                if r.random() < 0.5:
                    a = None
                else:
                    b = None
            if r.random() < c["link_upper"] and a:
                a = a.upper()
        self.lk.append(self.render([a, b], dirt))
        if dirt and r.random() < 0.08:
            if r.random() < 0.5:
                a, b = b, a
            self.lk.append(self.render([a, b], dirt))

    def link_tree(self, ids):
        for i in range(1, len(ids)):
            self.add_link(ids[i], ids[self.r.randrange(i)])
        if self.r.random() < 0.01:
            x = self.r.choice(ids)
            self.add_link(x, x)

    def link_chain(self, ids):
        order = list(ids)
        self.r.shuffle(order)
        for i in range(1, len(order)):
            self.add_link(order[i - 1], order[i])

    # ---- people and activity
    def n_ids(self):
        r = self.r
        x = r.random()
        if x < 0.45:
            return 1
        if x < 0.75:
            return 2
        if x < 0.88:
            return 3
        return r.randint(4, 8)

    def n_events(self):
        return min(int(self.r.expovariate(1 / 4.0)), 30) + 1

    def gap(self):
        r = self.r
        x = r.random()
        if x < 0.08:
            return 0
        if x < 0.72:
            g = r.randint(1, 300) * SEC
            return g + (r.randint(0, 999) if r.random() < 0.3 else 0)
        if x < 0.88:
            return r.randint(300, 1790) * SEC + (r.randint(0, 999) if r.random() < 0.3 else 0)
        if x < 0.93:
            return GAP
        if x < 0.97:
            return GAP + 1
        return r.randint(31, 300) * MIN

    def pick_starts(self, n, min_gap=0):
        r = self.r
        out = []
        tries = 0
        while len(out) < n and tries < n * 50:
            tries += 1
            s = r.randint(self.ws, self.we - 1)
            if r.random() < 0.7:
                s = s // SEC * SEC
            if min_gap and any(abs(s - o) < min_gap for o in out):
                continue
            out.append(s)
        out.sort()
        return out

    def session_clicks(self, ids, s):
        r = self.r
        for _ in range(r.randint(1, 3)):
            if r.random() < 0.08:
                t = s + r.choice([-REACH, -REACH - 1, 0, 1, -REACH + 1, -1])
            else:
                t = s - r.randint(0, REACH)
                if r.random() < 0.6:
                    t = t // SEC * SEC
            self.add_click(self.pick_uid(ids), t)

    def tie_clicks(self, ids, s):
        """2-4 clicks by one person at the same millisecond, odd click_id shapes, distinct
        campaigns, timed to be the latest qualifying clicks of the session starting at s."""
        r = self.r
        T = s - r.choice([0, 0, r.randint(1, 7200) * SEC, r.randint(1, 7200) * SEC + r.randint(1, 999)])
        fam = self.take_family("k")
        if len(fam) > 4:
            fam = r.sample(fam, 4)
        if r.random() < 0.3:
            fam.append(self.new_id("k", r.choice(ALL_SHAPES)))
        camps = r.sample(self.camp_ids, len(fam))
        for cid, camp in zip(fam, camps):
            self.add_click(self.pick_uid(ids), T, camp=camp, cid=cid, dirt=False,
                           dup=1 if r.random() < 0.1 else 0)
        if r.random() < 0.5:
            self.add_click(self.pick_uid(ids), T - r.randint(1, 3 * DAY))

    def activity(self, ids, nsess, click_p=0.6, tie=False, nev=None):
        starts = self.pick_starts(nsess, 8 * HOUR if tie else 0)
        for s in starts:
            t = s
            k = nev() if nev else self.n_events()
            for i in range(k):
                if i:
                    t += self.gap() if not tie else min(self.gap(), GAP)
                self.add_event(self.pick_uid(ids), t)
            if tie:
                self.tie_clicks(ids, s)
            elif self.r.random() < click_p:
                self.session_clicks(ids, s)
        return starts

    def ordinary(self, n, tie_p=0.0, nsess=(1, 12)):
        r = self.r
        for _ in range(n):
            ids = [self.new_id("u") for _ in range(self.n_ids())]
            self.persons.append(ids)
            self.link_tree(ids)
            if r.random() < 0.05:
                self.add_link(self.new_id("u"), r.choice(ids))
            self.activity(ids, r.randint(*nsess), tie=r.random() < tie_p)

    def rand_t(self, click=False):
        r = self.r
        lo = self.ws - (REACH if click else 0)
        t = r.randint(lo, self.we - 1)
        return t // SEC * SEC if r.random() < 0.6 else t

    # ---- filler / padding lines (for exact sizes in the dialect profile)
    def simple_uid(self):
        for _ in range(50):
            ids = self.r.choice(self.persons)
            u = self.r.choice(ids)
            if len(u.encode()) <= 12 and "\r" not in u:
                return u
        return "u" + self.rs(B36, 5)

    def filler_event(self, q=None, last_plain=True):
        r = self.r
        t = self.rand_t()
        etype, product, price = self.pick_tpp(False)
        vals = [self.new_id("e"), self.simple_uid(), self.ts_str(t, False), f1(self.pick_ingest(t, False)),
                etype, product, price]
        s = self.ser(vals[:-1], self.cfg["quote"] if q is None else q)
        return s + "," + (price or "")

    def pad_event(self, n):
        """A valid view line of exactly n bytes (no newline); the junk price is ignored."""
        t = self.rand_t()
        vals = [self.new_id("e", "short"), self.simple_uid(), self.ts_str(t, False),
                f1(self.pick_ingest(t, False)), "view", self.pick_product(), ""]
        base = self.ser(vals, 0)
        k = n - len(base.encode())
        assert k >= 0, (n, base)
        return base + self.rs(B36, k)

    def filler_click(self, q=None):
        t = self.rand_t(click=True)
        return self.ser([self.new_id("k"), self.simple_uid(), self.pick_camp(False), self.ts_str(t, False)],
                        self.cfg["quote"] if q is None else q)

    def pad_click(self, n):
        t = self.rand_t(click=True)
        rest = "," + self.simple_uid() + "," + self.pick_camp(False) + "," + self.ts_str(t, False)
        k = n - len(rest.encode()) - 1
        assert k >= 1, n
        while True:
            cid = "k" + self.rs(B36, k)
            if cid not in self.used["k"]:
                self.used["k"].add(cid)
                return cid + rest

    def filler_link(self, q=None):
        r = self.r
        x = r.random()
        if x < 0.4 and self.pairs:
            a, b = r.choice(self.pairs)
        elif x < 0.7:
            a = r.choice(r.choice(self.persons))
            b = a
        else:
            a = r.choice(r.choice(self.persons))
            b = self.new_id("u")
        if r.random() < 0.5:
            a, b = b, a
        return self.ser([a, b], self.cfg["quote"] if q is None else q)

    def pad_link(self, n):
        a = self.simple_uid()
        k = n - len(a.encode()) - 2
        assert k >= 1, n
        while True:
            b = "u" + self.rs(B36, k)
            if b not in self.used["u"]:
                self.used["u"].add(b)
                return a + "," + b


# ---------------------------------------------------------------- profiles

def profile_ids(g):
    r, s = g.r, g.scale
    g.cfg.update(odd=0.30, hex=0.15, family=0.15, family_use=0.5, dup_ev=0.10, dup_ck=0.04,
                 quote=0.15, malformed=0.002)
    g.window()
    g.catalog()

    # People whose name is designed to sit on a shape boundary.
    def named(kind):
        h = g.rs(HEX, 32)
        sh = g.rs(HEX, r.randint(1, 8))
        sb = g.rs(B36, r.randint(1, 8))
        if kind == 0:   # short name, strict prefix of a 32-hex member
            return ["u" + sh, "u" + sh + "0" * (32 - len(sh)), "u" + g.rs(HEX, 32)], None
        if kind == 1:   # 32-hex name
            return ["u" + "0" * 16 + g.rs(HEX, 16), "u" + g.rs("123456789", 1) + g.rs(B36, 4),
                    "u" + g.rs("abc", 1) + g.rs(HEX, 31)], None
        if kind == 2:   # upper-case name
            return ["U" + sb, "u" + sb, "u" + h], None
        if kind == 3:   # name appears in no event
            x = r.choice([" u" + sb, "\tu" + sb, g.rs(DIG, r.randint(1, 20)), "A" + sb, "E" + h,
                          "U" + h, "e" + sb, "k" + h, "!" + sb])
            return [x, "u" + sb + "q", "u" + h], 0
        if kind == 4:   # the two smallest differ only at the end
            if r.random() < 0.5:
                h2 = "00" + h[2:]
                return ["u" + h2, "u" + h2[:31] + r.choice([c for c in HEX if c != h2[31]]),
                        "u" + g.rs("123456789", 1) + g.rs(B36, 3)], None
            L = "u0" + g.rs(B36, r.randint(120, 280))
            return [L, L[:-1] + r.choice([c for c in B36 if c != L[-1]]), "u" + h], None
        if kind == 5:   # signed-byte trap: z (0x7a) < e-acute (0xc3)
            return ["u" + sb + "z", "u" + sb + "é", "u" + sb + "éa"], None
        if kind == 6:   # whitespace variants
            return ["u" + sb + "\t", "u" + sb, "u" + sb + " ", " u" + sb], None
        if kind == 7:   # case variants of one hex string
            h2 = h[:3] + r.choice("abcdef") + h[4:]
            return ["u" + h2, "u" + h2.upper(), "U" + h2], None
        if kind == 8:   # carriage returns
            return ["u" + sb + "\r", "u" + sb + "\r0", "u" + sb + "0"], None
        if kind == 9:   # digits-only and wrong prefix
            return [g.rs(DIG, r.randint(1, 12)), "e" + sb, "u" + sb, "k" + h], None
        if kind == 10:  # one-character ID
            return [r.choice(B36 + "U~"), "u" + sb, "u" + h], None
        if kind == 11:  # hex 31/32/33 of one string
            return ["u" + h[:31], "u" + h, "u" + h + "0"], None
        # long IDs sharing a 32-hex prefix
        return ["u" + h + g.rs(B36, r.randint(100, 250)), "u" + h, "u" + h[:16]], None

    for i in range(170 * s):
        for _ in range(50):
            ids, hidden = named(i % 13)
            if g.claim("u", ids):
                break
        else:
            continue
        r.shuffle(ids) if hidden is None else None
        g.link_chain(ids)
        act = ids[1:] if hidden == 0 else ids
        g.persons.append(act)
        g.activity(act, r.randint(2, 9), tie=r.random() < 0.6)

    # People made of one ID family (nearly equal IDs that ARE one person).
    for _ in range(220 * s):
        ids = g.take_family("u") + [g.new_id("u") for _ in range(r.randint(0, 2))]
        g.persons.append(ids)
        g.link_chain(ids)
        g.activity(ids, r.randint(1, 8), tie=r.random() < 0.5)

    # Ordinary people; new_id hands out families whose siblings land in OTHER people.
    g.ordinary(1100 * s, tie_p=0.35)

    # Duplicated events for event_ids of every shape.
    for shape in ALL_SHAPES:
        for _ in range(10 * s):
            eid = g.new_id("e", shape)
            g.add_event(g.pick_uid(r.choice(g.persons)), g.rand_t(), eid=eid, dup=r.randint(2, 4))
    # Distinct event_ids that share a half / all but one digit: never merged.
    for _ in range(400 * s):
        fam = g.take_family("e")
        for eid in fam:
            g.add_event(g.pick_uid(r.choice(g.persons)), g.rand_t(), eid=eid,
                        dup=r.randint(1, 2) if r.random() < 0.3 else None)


def profile_time(g):
    r, s = g.r, g.scale
    g.cfg.update(odd=0.06, hex=0.125, family=0.03, family_use=0.3, bad_ts=0.03, late=0.03)
    g.window(leap=(g.seed % 2 == 0))
    g.catalog()
    ws, we = g.ws, g.we

    def solo(n_ids=None):
        ids = [g.new_id("u") for _ in range(n_ids or g.n_ids())]
        g.persons.append(ids)
        g.link_tree(ids)
        return ids

    def one(t, ts, ingest=None, etype="view", price=None, ids=None, click=None):
        ids = ids or solo()
        g.add_event(g.pick_uid(ids), t, ts=ts, ingest=ingest, etype=etype,
                    product=g.pick_product(), price=price, keep_type=True, dirt=False)
        if click is not None:
            g.add_click(g.pick_uid(ids), click, dirt=False)
        return ids

    def all_forms(t):
        out = [g.f3(t, 0), g.f3(t, r.randint(1, 30))]
        if t % SEC == 0:
            out += [f1(t), f2(t, 0, neg_zero=True), f2(t, 0), f2(t, 23 * 60 + 59), f2(t, -(23 * 60 + 59)),
                    f2(t, r.randint(-40, 48) * 15)]
        return out

    g.ordinary(1500 * s)

    for _ in range(s):
        # Event window edges, every form.
        for t in (ws, ws - 1, ws - SEC, ws + 1, we - 1, we - SEC, we, we + SEC, ws - DAY, we + DAY):
            for ts in all_forms(t):
                one(t, ts, click=t - r.randint(0, 3 * DAY) if r.random() < 0.5 else None)
        # Click window edges: sessions at ws and at we-1 / we-1s.
        for st in (ws, ws + SEC):
            for ct in (ws - REACH, ws - REACH - 1, ws - REACH + 1, ws - REACH - SEC):
                for ts in all_forms(ct):
                    ids = one(st, f1(st) if st % SEC == 0 else g.f3(st))
                    g.add_click(g.pick_uid(ids), ct, ts=ts, dirt=False)
        for st in (we - 1, we - SEC):
            for ct in (we, we - 1, we - SEC, we + 1):
                if ct > we:
                    continue
                for ts in all_forms(ct):
                    ids = one(st, g.f3(st) if st % SEC else f1(st))
                    g.add_click(g.pick_uid(ids), ct, ts=ts, dirt=False)

        # Late cutoff.
        for _ in range(120):
            base = g.rand_t() // SEC * SEC
            if base >= we - SEC:
                continue
            for ing, t, form in ((base + DAY, base, "f1"), (base + DAY + SEC, base, "f1"),
                                 (base + DAY, base, "f3"), (base + DAY + SEC, base, "f2"),
                                 (base + DAY, base - 1, "f3"), (base + DAY, base + 1, "f3"),
                                 (base + DAY - SEC, base - 1, "f3"), (base + 2 * DAY, base, "f1"),
                                 (base - 2 * DAY, base, "f1"), (base - SEC, base - 1, "f3")):
                if not (ws <= t < we):
                    continue
                ts = f1(t) if form == "f1" else (f2(t, g.pick_off()) if form == "f2" else g.f3(t))
                one(t, ts, ingest=ing, etype=r.choice(["view", "purchase"]), price=str(r.randint(1, 9999)))

        # 30-minute gaps with runs of equal timestamps around them.
        for _ in range(250):
            ids = solo(r.randint(1, 4))
            t = g.rand_t()
            for _ in range(r.randint(3, 8)):
                for _ in range(r.randint(1, 4)):
                    etype = r.choice(["view", "view", "cart", "purchase"])
                    g.add_event(g.pick_uid(ids), t, etype=etype, product=g.pick_product(),
                                price=str(r.randint(1, 50000)), keep_type=True, dirt=False)
                t += r.choice([GAP, GAP, GAP + 1, GAP + 1, GAP - 1, 0, GAP + SEC, 2 * SEC])
            if r.random() < 0.6:
                g.session_clicks(ids, t)

        # Sessions crossing midnight, starting just before it (also F2 with a date change).
        for d in range(15):
            for _ in range(8):
                mid = ws + d * DAY
                ids = solo()
                t = mid - r.choice([1, SEC, 30 * SEC, 10 * MIN, 29 * MIN, GAP, GAP + 1]) - (
                    r.randint(0, 999) if r.random() < 0.3 else 0)
                for _ in range(r.randint(2, 6)):
                    g.add_event(g.pick_uid(ids), t)
                    t += r.choice([SEC, 10 * MIN, 29 * MIN + 59 * SEC + 999, GAP, 1])
                if r.random() < 0.7:
                    g.session_clicks(ids, mid - 1)

        # Clicks at the 7-day reach and the session start.
        for _ in range(250):
            ids = solo()
            st = g.rand_t()
            if r.random() < 0.3:
                st = st // DAY * DAY  # session at midnight
            g.add_event(g.pick_uid(ids), st, ts=g.ts_str(st, False))
            g.add_event(g.pick_uid(ids), st + r.randint(1, 20) * MIN)
            offs = r.sample([-REACH, -REACH - 1, -REACH + 1, 0, 1, -1, -REACH - SEC, SEC], r.randint(2, 4))
            camps = r.sample(g.camp_ids, len(offs))
            for o, camp in zip(offs, camps):
                ct = st + o
                g.add_click(g.pick_uid(ids), ct, camp=camp, ts=r.choice(all_forms(ct)), dirt=False)

        # F3 extremes: valid huge leading-zero spellings of in-window instants, and junk.
        for _ in range(40):
            t = g.rand_t()
            for width in (19, 20, 21, 25, 40, 64, 100):
                one(t, str(t).rjust(width, "0"))
            g.add_click(g.pick_uid(one(t, f1(t // SEC * SEC))), t - SEC, ts=str(t - SEC).rjust(33, "0"),
                        dirt=False)
        for ts in ("9223372036854775807", "9223372036854775808", "18446744073709551615",
                   "18446744073709551616", "1" + "0" * 30, "0", "1", "86400000", "0" * 25,
                   str(ws - 1).rjust(30, "0"), str(we).rjust(30, "0"), "9" * 40,
                   str(ws).rjust(20, "0"), str(we - 1).rjust(20, "0")):
            for _ in range(3):
                t = ws if ts.lstrip("0") == str(ws) else g.rand_t()
                one(t, ts)
                ids = one(g.rand_t(), f1(g.rand_t() // SEC * SEC))
                g.add_click(g.pick_uid(ids), t, ts=ts, dirt=False)

        # Every bad form, one person each, on events and on clicks.
        for _ in range(6):
            for k in range(N_BAD):
                t = g.rand_t() // SEC * SEC
                one(t, g.bad_ts(t, k))
                t2 = min(t + HOUR, we - SEC)
                ids = one(t2, f1(t2))
                g.add_click(g.pick_uid(ids), t, ts=g.bad_ts(t, k), dirt=False)

        # 29 February 2028 (inside the window on even seeds) and impossible leap days.
        feb29 = days_from_civil(2028, 2, 29) * DAY
        for _ in range(60):
            t = feb29 + r.randint(0, DAY - 1) // SEC * SEC
            forms = [f1(t), f2(t, r.choice([60, 180, 330, 600, 720])), f2(t, r.choice([-60, -300, -600])),
                     g.f3(t)]
            one(t, r.choice(forms), click=t - r.randint(0, REACH) // SEC * SEC)
            st = feb29 - r.randint(1, 20) * MIN
            ids = solo()
            for k in range(r.randint(2, 5)):
                tt = st + k * r.randint(5, 25) * MIN
                g.add_event(g.pick_uid(ids), tt, ts=r.choice([f1(tt), f2(tt, 120), f2(tt, -480)]), dirt=False)
            st = feb29 + DAY - r.randint(1, 20) * MIN
            ids = solo()
            for k in range(r.randint(2, 5)):
                tt = st + k * r.randint(5, 25) * MIN
                g.add_event(g.pick_uid(ids), tt, ts=r.choice([f1(tt), f2(tt, 120), f2(tt, -480)]), dirt=False)
        for ts in ("2027-02-29T10:00:00Z", "2026-02-29T10:00:00Z", "2025-02-29T00:00:00Z",
                   "2100-02-29T12:00:00Z", "2028-02-30T00:00:00Z", "2028-02-29T24:00:00Z",
                   "2028-02-29T23:59:60Z", "2028-02-29T10:00:00+24:00", "2000-02-29T10:00:00Z",
                   "2028-03-01T00:30:00+01:00", "2028-02-28T23:30:00-01:00"):
            for _ in range(4):
                one(feb29 + 10 * HOUR, ts)


def profile_skew(g):
    r, s = g.r, g.scale
    g.cfg.update(odd=0.10, hex=0.125, family=0.03, family_use=0.3)
    g.window()
    g.catalog()
    g.ordinary(1200 * s)

    # One person: a 600-ID chain mixing shapes, used evenly.
    chain = [g.new_id("u", r.choice(ALL_SHAPES) if r.random() < 0.3 else None) for _ in range(600)]
    g.link_chain(chain)
    for _ in range(300):
        a, b = r.sample(chain, 2)
        g.add_link(a, b)
    g.persons.append(chain)
    for st in g.pick_starts(300):
        t = st
        for _ in range(r.randint(5, 15)):
            g.add_event(r.choice(chain), t)
            t += g.gap()
        if r.random() < 0.6:
            g.session_clicks(chain[r.randrange(600):], st)

    # A shared device joining 2,000 otherwise separate people.
    dev = g.new_id("u", "hex")
    for _ in range(2000):
        ids = [g.new_id("u") for _ in range(r.randint(1, 3))]
        g.link_tree(ids)
        g.add_link(dev, r.choice(ids))
        g.persons.append(ids)
        g.activity(ids, r.randint(1, 3), click_p=0.5, nev=lambda: r.randint(1, 5))
    for _ in range(30):
        g.add_event(dev, g.rand_t())

    # A whale: 20,000 events in overlapping bursts with many equal timestamps.
    whale = [g.new_id("u") for _ in range(5)]
    g.link_tree(whale)
    g.persons.append(whale)
    for _ in range(400):
        bs = g.rand_t()
        offs = sorted(r.randint(0, 45 * MIN) for _ in range(15))
        for _ in range(50):
            g.add_event(g.pick_uid(whale), min(bs + r.choice(offs), g.we + HOUR))
    for _ in range(300):
        g.add_click(g.pick_uid(whale), g.rand_t(click=True))

    # A user with 5,000 clicks (bursts at the same millisecond included).
    clicky = [g.new_id("u") for _ in range(2)]
    g.link_tree(clicky)
    g.persons.append(clicky)
    g.activity(clicky, 60, click_p=0.0)
    n = 0
    while n < 5000:
        t = g.rand_t(click=True)
        k = r.randint(1, 4) if r.random() < 0.2 else 1
        for _ in range(k):
            g.add_click(g.pick_uid(clicky), t)
        n += k

    # Thousands of people with exactly one event.
    for _ in range(5000 * s):
        u = g.new_id("u")
        g.persons.append([u])
        g.add_event(u, g.rand_t())
        if r.random() < 0.3:
            g.add_click(u, g.rand_t(click=True))

    # event_ids copied 50 times.
    for _ in range(200 * s):
        eid = g.new_id("e", r.choice(ALL_SHAPES))
        g.add_event(g.pick_uid(r.choice(g.persons)), g.rand_t(), eid=eid, dup=49)

    # Link lines repeated, reversed and self-linked.
    pairs = list(g.pairs)
    for _ in range(4000 * s):
        a, b = r.choice(pairs)
        x = r.random()
        if x < 0.4:
            g.lk.append(g.ser([a, b], 0.2))
        elif x < 0.8:
            g.lk.append(g.ser([b, a], 0.2))
        else:
            g.lk.append(g.ser([a, a], 0.2))
    for _ in range(1000 * s):
        x = r.choice(r.choice(g.persons))
        g.lk.append(g.ser([x, x], 0.2))


# ---------------------------------------------------------------- writing

def write_bytes(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def zipf_split(r, lines, nfiles):
    w = [1.0 / (i + 1) for i in range(nfiles)]
    r.shuffle(w)
    tot = sum(w)
    out, i = [], 0
    for k in range(nfiles):
        n = len(lines) - i if k == nfiles - 1 else int(round(len(lines) * w[k] / tot))
        out.append(lines[i:i + n])
        i += n
    return out


def write_parts(g, out, folder, header, lines, nmin, nmax):
    r = g.r
    lines = list(lines)
    r.shuffle(lines)
    chunks = zipf_split(r, lines, r.randint(nmin, nmax))
    chunks += [[] for _ in range(r.randint(1, 2))]
    r.shuffle(chunks)
    for i, ch in enumerate(chunks):
        data = "".join(x + "\n" for x in [header] + ch).encode()
        write_bytes(os.path.join(out, folder, "part-%05d.csv" % i), data)


def write_common(g, out):
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "manifest.json"), "w") as f:
        json.dump({"window_start": f1(g.ws), "window_end": f1(g.we)}, f, indent=2)
        f.write("\n")
    write_bytes(os.path.join(out, "products.csv"),
                ("product_id,category\n" + "".join("%s,%s\n" % pc for pc in g.products)).encode())
    write_bytes(os.path.join(out, "campaigns.csv"),
                ("campaign_id,channel\n" + "".join("%s,%s\n" % cc for cc in g.campaigns)).encode())


class FB:
    """A file being built byte by byte."""

    def __init__(self, header):
        self.parts = []
        self.n = 0
        if header is not None:
            self.add(header + "\n")

    def add(self, s):
        b = s.encode()
        self.parts.append(b)
        self.n += len(b)

    def line(self, s):
        self.add(s + "\n")

    def data(self):
        return b"".join(self.parts)


class Src:
    def __init__(self, lines, filler):
        self.lines = lines
        self.filler = filler

    def take(self):
        return self.lines.pop() if self.lines else self.filler()


def exact_file(header, size, src, pad, newline=True, first=None):
    fb = FB(header)
    if first:
        fb.line(first)
    while True:
        s = src.take()
        if size - fb.n - len(s.encode()) - 1 < 400:
            src.lines.append(s)
            break
        fb.line(s)
    rest = size - fb.n
    if newline:
        fb.line(pad(rest - 1))
    else:
        fb.add(pad(rest))
    d = fb.data()
    assert len(d) == size and (d.endswith(b"\n") == newline), (len(d), size)
    return d


def long_lines(g, kind, lens):
    out = []
    for n in lens:
        if kind == "events":
            out.append(g.pad_event(n))
        elif kind == "clicks":
            out.append(g.pad_click(n))
        else:
            out.append(g.pad_link(n))
    return out


def assemble_dialect(g, out):
    r = g.r
    lens = [126, 127, 128, 129, 255, 256, 257, 4095, 4096, 4097, 8192, 12000]
    specs = {
        "events": (EV_HDR, g.ev, g.filler_event, g.pad_event, 7),
        "clicks": (CK_HDR, g.ck, g.filler_click, g.pad_click, 4),
        "links": (LK_HDR, g.lk, g.filler_link, g.pad_link, 2),
    }
    for folder, (hdr, lines, filler, pad, nf) in specs.items():
        r.shuffle(lines)
        src = Src(lines, filler)
        files = {}
        longs = long_lines(g, folder, lens)
        bad_longs = []
        for n in (128, 256, 257, 5000):
            v = pad(n - 1)
            bad_longs.append(v[: len(v) // 2] + '"' + v[len(v) // 2:])  # n bytes, a quote mid-field
        r.shuffle(longs)

        # many tiny files, with the last line valid / malformed / unterminated / long
        for i in range(40 if folder != "links" else 25):
            fb = FB(hdr)
            body = [src.take() for _ in range(r.randint(1, 3))]
            x = r.random()
            if x < 0.15:
                body.append(g.malform([None] * nf) if r.random() < 0.3 else bad_longs[i % len(bad_longs)])
            elif x < 0.3:
                body.append(longs[i % len(longs)])
            if r.random() < 0.2:
                body.insert(0, longs[(i + 3) % len(longs)])
            for j, s in enumerate(body):
                if j == len(body) - 1 and r.random() < 0.3:
                    fb.add(s)  # no trailing newline
                else:
                    fb.line(s)
            files["tiny-%02d.csv" % i] = fb.data()

        # odd files
        files["empty.csv"] = b""
        files["only-newline.csv"] = b"\n"
        files["header-only.csv"] = (hdr + "\n").encode()
        files["header-only-nonl.csv"] = hdr.encode()
        fb = FB(None)
        fb.add(hdr + "\r\n")
        for _ in range(30):
            # fresh rows only: a \r on one copy of a duplicated row would make copies differ
            fb.add(filler() + "\r\n")
        files["crlf.csv"] = fb.data()
        fb = FB(None)
        fb.line(",".join('"%s"' % h for h in hdr.split(",")))
        for _ in range(30):
            s = src.take()
            fb.line(s)
            x = r.random()
            if x < 0.2:
                fb.line("")
            elif x < 0.3:
                fb.line("," * (nf - 1))
            elif x < 0.35:
                fb.line("\r")
            elif x < 0.4:
                fb.line("," * nf)
        files["quoted-header-blank-lines.csv"] = fb.data()
        fb = FB(hdr)
        for _ in range(20):
            fb.line(src.take())
        fb.add(g.malform([None] * nf) or '"')
        files["last-malformed-nonl.csv"] = fb.data()
        fb = FB(hdr)
        for _ in range(20):
            fb.line(src.take())
        fb.add(src.take())
        files["last-valid-nonl.csv"] = fb.data()
        fb = FB(hdr)
        fb.line(longs[0])
        for _ in range(20):
            fb.line(src.take())
        fb.add(longs[1])
        files["long-first-last-nonl.csv"] = fb.data()
        # Rows that must be ignored: not *.csv.
        ign = "".join(filler() + "\n" for _ in range(20))
        files["notes.txt"] = (hdr + "\n" + ign).encode()
        files["part-99999.csv.bak"] = (hdr + "\n" + ign).encode()
        # exact sizes (a 64 KiB file ending exactly at the boundary, with and without \n)
        files["exact-64k-nl.csv"] = exact_file(hdr, 65536, src, pad, True)
        files["exact-64k-nonl.csv"] = exact_file(hdr, 65536, src, pad, False)

        if folder == "events":
            files["exact-2m-nonl.csv"] = exact_file(hdr, 2 * MIB, src, pad, False, first=longs[2])
            files["big.csv"] = big_events(g, src)
        elif folder == "clicks":
            files["exact-2m-nl.csv"] = exact_file(hdr, 2 * MIB, src, pad, True, first=longs[2])
        else:
            files["exact-256k-nl.csv"] = exact_file(hdr, 256 * 1024, src, pad, True)

        # the rest, in medium files, long lines first / middle / last
        rest = list(src.lines)
        src.lines = []
        nmid = 6 if folder != "links" else 3
        chunks = zipf_split(r, rest, nmid)
        li = 0
        for i, ch in enumerate(chunks):
            ch = list(ch)
            for pos in ("first", "mid", "last"):
                if li < len(longs) + len(bad_longs):
                    v = (longs + bad_longs)[li]
                    li += 1
                    if pos == "first":
                        ch.insert(0, v)
                    elif pos == "last":
                        ch.append(v)
                    else:
                        ch.insert(len(ch) // 2, v)
            files["part-%05d.csv" % i] = "".join(x + "\n" for x in [hdr] + ch).encode()
        for name, data in files.items():
            write_bytes(os.path.join(out, folder, name), data)


def big_events(g, src):
    """A >6 MB events file with an engineered position at each MiB boundary."""
    r = g.r
    fb = FB(EV_HDR)
    fb.line(g.pad_event(127))

    def plain():
        return g.filler_event(q=0.2)

    targets = []
    # (boundary, line, offset of the boundary inside line+"\n", check)
    x = plain()
    targets.append((1 * MIB, x, len(x.encode()), "nl"))
    eid = "e" + g.rs(B36, 80)
    g.used["e"].add(eid)
    base = g.filler_event(q=0)
    x = '"' + eid + '"' + base[base.find(","):]
    targets.append((2 * MIB, x, 41, "inq"))
    base = g.filler_event(q=0)
    x = '"' + base
    targets.append((3 * MIB, x, 3, "mal"))
    x = plain()
    targets.append((4 * MIB, x, 0, "start"))
    eid = "e" + g.rs(B36, 20)
    g.used["e"].add(eid)
    base = g.filler_event(q=0)
    x = '"' + eid + '"' + base[base.find(","):]
    targets.append((5 * MIB, x, 0, "quote"))
    base = g.filler_event(q=0)
    x = base + "\r"
    targets.append((6 * MIB, x, len(x.encode()), "cr"))
    x = g.pad_event(6000)
    targets.append((6 * MIB + MIB // 2, x, 3001, "long"))
    for B, line, off, _ in targets:
        start = B - off
        while True:
            s = src.take()
            if start - fb.n - len(s.encode()) - 1 < 400:
                src.lines.append(s)
                break
            fb.line(s)
        fb.line(g.pad_event(start - fb.n - 1))
        assert fb.n == start
        fb.line(line)
    while fb.n < 6 * MIB + 800 * 1024:
        fb.line(src.take())
    fb.line(g.pad_event(128))
    d = fb.data()
    for B, line, off, kind in targets:
        if kind == "nl":
            assert d[B:B + 1] == b"\n"
        elif kind == "inq":
            assert d[d.rfind(b"\n", 0, B) + 1:][:1] == b'"' and d[B:B + 1] not in (b'"', b",")
        elif kind == "start":
            assert d[B - 1:B] == b"\n"
        elif kind == "quote":
            assert d[B - 1:B + 1] == b'\n"'
        elif kind == "cr":
            assert d[B - 1:B + 1] == b"\r\n"
    return d


def profile_dialect(g, out):
    r, s = g.r, g.scale
    g.cfg.update(odd=0.04, hex=0.125, family=0.02, family_use=0.3, quote=0.35, malformed=0.025,
                 crlf=0.02, stray_cr=0.004)
    g.window()
    g.catalog()
    g.ordinary(1500 * s)
    for _ in range(200 * s):
        # quoted empty fields and quoted nulls on otherwise valid rows
        ids = r.choice(g.persons)
        t = g.rand_t()
        vals = [g.new_id("e"), g.pick_uid(ids), g.ts_str(t, False), f1(g.pick_ingest(t, False)),
                r.choice(["view", "cart"]), g.pick_product(), None]
        g.ev.append(g.ser(vals, 1.0))
        g.ev.append(",".join('"%s"' % (v or "") if i != 6 else '""' for i, v in enumerate(
            [g.new_id("e")] + vals[1:])))
    write_common(g, out)
    assemble_dialect(g, out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("out")
    ap.add_argument("--profile", required=True, choices=["ids", "dialect", "time", "skew"])
    ap.add_argument("--seed", type=int, required=True)
    ap.add_argument("--scale", type=int, default=1)
    a = ap.parse_args()
    if os.path.exists(a.out) and os.listdir(a.out):
        raise SystemExit("%s exists and is not empty" % a.out)
    g = Gen(a.profile, a.seed, a.scale)
    if a.profile == "dialect":
        profile_dialect(g, a.out)
        return
    {"ids": profile_ids, "time": profile_time, "skew": profile_skew}[a.profile](g)
    write_common(g, a.out)
    write_parts(g, a.out, "events", EV_HDR, g.ev, 6, 18)
    write_parts(g, a.out, "clicks", CK_HDR, g.ck, 3, 9)
    write_parts(g, a.out, "links", LK_HDR, g.lk, 2, 6)


if __name__ == "__main__":
    main()
