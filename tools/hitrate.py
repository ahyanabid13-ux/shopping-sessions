#!/usr/bin/env python3
"""Hit rate of a per-worker direct-mapped user-ID cache, measured on real input order.

Usage: hitrate.py DATA_DIR [--rows N] [--workers 4,8] [--sizes 256,1024,4096,16384]

Emulates how the engine reads the data: the event files, then the click files, cut into 2 MiB
segments that are dealt to the workers in turn; every worker keeps one cache for the whole run. A
lookup hits when the slot chosen by a hash of the complete user ID holds exactly that ID (so a
conflict is a miss, never a wrong answer); every miss replaces the slot. Also prints the share of
rows whose ID the same worker has seen before at all (the limit for a cache of any size).
Rows are the well-formed lines with a non-empty user_id; --rows stops after that many (0 = all).
"""
import argparse
import os
import zlib

SEGMENT = 2 << 20


def rows(data_dir, limit):
    seg_base = 0
    seen = 0
    for folder, fields in (("events", 7), ("clicks", 4)):
        path = os.path.join(data_dir, folder)
        for name in sorted(os.listdir(path)):
            if not name.endswith(".csv"):
                continue
            with open(os.path.join(path, name), "rb") as f:
                data = f.read()
            offset = data.find(b"\n") + 1
            if offset == 0:
                offset = len(data)
            for line in data[offset:].split(b"\n"):
                start = offset
                offset += len(line) + 1
                parts = line.split(b",")
                if len(parts) != fields:
                    continue
                user = parts[1]
                if len(user) >= 2 and user[:1] == b'"' and user[-1:] == b'"':
                    user = user[1:-1]
                if not user or b'"' in user:
                    continue
                yield seg_base + start // SEGMENT, user
                seen += 1
                if limit and seen >= limit:
                    return
            seg_base += len(data) // SEGMENT + 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data_dir")
    ap.add_argument("--rows", type=int, default=3_000_000)
    ap.add_argument("--workers", default="4,8")
    ap.add_argument("--sizes", default="256,1024,4096,16384")
    args = ap.parse_args()
    workers = [int(x) for x in args.workers.split(",")]
    sizes = [int(x) for x in args.sizes.split(",")]
    caches = {(w, s): [[None] * s for _ in range(w)] for w in workers for s in sizes}
    hits = {key: 0 for key in caches}
    seen_before = {w: 0 for w in workers}
    seen_sets = {w: [set() for _ in range(w)] for w in workers}
    total = long_rows = 0
    for segment, user in rows(args.data_dir, args.rows):
        total += 1
        long_rows += len(user) == 33
        h = zlib.crc32(user)
        for w in workers:
            worker = segment % w
            mine = seen_sets[w][worker]
            if user in mine:
                seen_before[w] += 1
            else:
                mine.add(user)
            for s in sizes:
                cache = caches[(w, s)][worker]
                slot = h & (s - 1)
                if cache[slot] == user:
                    hits[(w, s)] += 1
                else:
                    cache[slot] = user
    print(f"{total} rows with a user_id ({long_rows / max(total, 1):.1%} of them 33-byte IDs)")
    for w in workers:
        print(f"{w} workers: seen before by the same worker {seen_before[w] / total:.1%}; hit rate by entries: "
              + "  ".join(f"{s}: {hits[(w, s)] / total:.1%}" for s in sizes))


if __name__ == "__main__":
    main()
