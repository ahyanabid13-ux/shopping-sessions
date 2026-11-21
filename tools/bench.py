#!/usr/bin/env python3
"""Runs the engine N times on a dataset; prints min/median per phase, wall and CPU.

Usage: bench.py BINARY DATA_DIR [RUNS] [ENV=VALUE ...]
"""
import os, re, resource, shutil, statistics, subprocess, sys, time

binary, data = sys.argv[1], sys.argv[2]
runs = int(sys.argv[3]) if len(sys.argv) > 3 else 5
extra = dict(kv.split("=", 1) for kv in sys.argv[4:])
out = "/tmp/claude-0/bench_out"
phases, walls, cpus = {}, [], []
for _ in range(runs):
    shutil.rmtree(out, ignore_errors=True)
    env = dict(os.environ, DATA_DIR=data, OUTPUT_DIR=out, SESSIONS_VERBOSE="1", **extra)
    r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
    t = time.perf_counter()
    p = subprocess.run([binary], env=env, capture_output=True, text=True)
    walls.append(time.perf_counter() - t)
    r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpus.append((r1.ru_utime - r0.ru_utime, r1.ru_stime - r0.ru_stime))
    if p.returncode != 0:
        sys.exit(p.stderr)
    for m in re.finditer(r"\]\s+(\S+)\s+([0-9.]+) ms", p.stderr):
        phases.setdefault(m.group(1), []).append(float(m.group(2)))
for name, v in phases.items():
    print(f"{name:<10} min {min(v):8.1f}  med {statistics.median(v):8.1f} ms")
print(f"{'wall':<10} min {min(walls)*1e3:8.1f}  med {statistics.median(walls)*1e3:8.1f} ms")
best = min(cpus, key=lambda c: c[0] + c[1])
print(f"{'cpu':<10} min {sum(best)*1e3:8.1f} ms (user {best[0]*1e3:.0f} sys {best[1]*1e3:.0f})  med {statistics.median(u+s for u,s in cpus)*1e3:8.1f} ms")
