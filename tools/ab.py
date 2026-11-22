#!/usr/bin/env python3
"""Alternating A/B benchmark of two engine binaries on one dataset.

Usage: ab.py BIN_A BIN_B DATA_DIR [ROUNDS] [ENV=VALUE ...]

After one unmeasured warm-up run of each binary, runs A B B A, A B B A, ... (ROUNDS times, default
3, so 2 x ROUNDS timed runs per binary), each with SESSIONS_VERBOSE=1, and reports per binary:
median / min / max wall time (process start to exit), median user and system CPU, peak RSS, and the
median wall time of each phase; then the speed-up of B over A on the medians, next to the run-to-run
spread, so a difference inside the noise is visible as such.
Outputs go to a temporary folder under $BENCH_TMP (default: the system temp dir).
"""
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

PHASE = re.compile(r"\]\s+(\S+)\s+([0-9.]+) ms\s+user\s+([0-9.]+) sys\s+([0-9.]+) ms")


def run(binary, data, out, extra):
    shutil.rmtree(out, ignore_errors=True)
    env = dict(os.environ, DATA_DIR=data, OUTPUT_DIR=out, SESSIONS_VERBOSE="1", **extra)
    start = time.perf_counter()
    child = subprocess.Popen([binary], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    err = child.stderr.read()
    _, status, usage = os.wait4(child.pid, 0)
    wall = time.perf_counter() - start
    child.returncode = os.waitstatus_to_exitcode(status)
    if child.returncode != 0:
        sys.exit(f"{binary} failed ({child.returncode}):\n{err[-2000:]}")
    phases = {m.group(1): float(m.group(2)) for m in PHASE.finditer(err)}
    return {"wall": wall * 1e3, "user": usage.ru_utime * 1e3, "sys": usage.ru_stime * 1e3,
            "rss": usage.ru_maxrss / 1024.0, "phases": phases}


def main():
    a, b, data = sys.argv[1:4]
    rest = sys.argv[4:]
    rounds = int(rest.pop(0)) if rest and "=" not in rest[0] else 3
    extra = dict(kv.split("=", 1) for kv in rest)
    out = tempfile.mkdtemp(prefix="ab-", dir=os.environ.get("BENCH_TMP") or None)
    results = {"A": [], "B": []}
    binaries = {"A": a, "B": b}
    try:
        for name in "AB":
            run(binaries[name], data, os.path.join(out, "o"), extra)
        for _ in range(rounds):
            for name in "ABBA":
                results[name].append(run(binaries[name], data, os.path.join(out, "o"), extra))
    finally:
        shutil.rmtree(out, ignore_errors=True)
    med = {}
    for name in "AB":
        r = results[name]
        walls = [x["wall"] for x in r]
        med[name] = statistics.median(walls)
        spread = (max(walls) - min(walls)) / med[name] * 100
        print(f"{name} {binaries[name]}")
        print(f"   wall  median {med[name]:8.1f}  min {min(walls):8.1f}  max {max(walls):8.1f} ms   spread {spread:4.1f}%   ({len(r)} runs)")
        print(f"   cpu   median user {statistics.median(x['user'] for x in r):8.1f}  sys {statistics.median(x['sys'] for x in r):8.1f}"
              f"  total {statistics.median(x['user'] + x['sys'] for x in r):8.1f} ms   peak RSS {max(x['rss'] for x in r):7.0f} MiB")
        names = list(r[0]["phases"])
        print("   phase " + "  ".join(f"{p} {statistics.median(x['phases'].get(p, 0.0) for x in r):.1f}" for p in names))
    gain = (med["A"] / med["B"] - 1) * 100
    print(f"B vs A: {med['A'] / med['B']:.3f}x  ({gain:+.1f}% faster on median wall; negative means slower)")


if __name__ == "__main__":
    main()
