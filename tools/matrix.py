#!/usr/bin/env python3
"""Differential matrix: the engine against reference outputs.

Usage: matrix.py BIN MATRIX_DIR [--threads 1,2,3,8] [--jobs 3] [--only NAME,...]

Every dataset directory NAME in MATRIX_DIR that has a NAME.ref next to it (the output of
tools/reference.py, or a folder with the two expected CSVs) is run at each thread count with both
line splitters (SESSIONS_PDEP=1 and SESSIONS_PDEP=0) and compared exactly, order-insensitively,
with scripts/compare.py. Prints one line per run and a summary; exits 1 on any mismatch.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent


def one(job):
    binary, data, ref, threads, pdep, scratch = job
    out = tempfile.mkdtemp(prefix=f"{data.name}-{threads}-{pdep}-", dir=scratch)
    try:
        env = dict(os.environ, NCPU=str(threads), SESSIONS_PDEP=str(pdep), DATA_DIR=str(data), OUTPUT_DIR=out)
        env.pop("SESSIONS_VERBOSE", None)
        try:
            run = subprocess.run([binary], env=env, capture_output=True, text=True, timeout=1800)
        except subprocess.TimeoutExpired:
            return job, False, "engine timed out"
        if run.returncode != 0:
            return job, False, f"engine exit {run.returncode}: {run.stderr[-400:]}"
        cmp = subprocess.run([sys.executable, str(HERE / "scripts" / "compare.py"), out, str(ref)],
                             capture_output=True, text=True)
        lines = (cmp.stdout + cmp.stderr).strip().splitlines()
        ok = bool(lines) and lines[-1] == "MATCH"
        return job, ok, "" if ok else "\n".join(lines[-14:])
    finally:
        shutil.rmtree(out, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("matrix_dir")
    ap.add_argument("--threads", default="1,2,3,8")
    ap.add_argument("--jobs", type=int, default=3)
    ap.add_argument("--only", default="")
    args = ap.parse_args()
    root = Path(args.matrix_dir)
    only = {n for n in args.only.split(",") if n}
    datasets = []
    for ref in sorted(root.glob("*.ref")):
        data = root / ref.name[: -len(".ref")]
        if not data.is_dir() or (only and data.name not in only):
            continue
        if not ((ref / "user_daily").is_dir() or (ref / "user_daily.csv").is_file()):
            continue
        datasets.append((data, ref))
    if not datasets:
        sys.exit(f"no datasets with a .ref in {root}")
    scratch = tempfile.mkdtemp(prefix="matrix-", dir=os.environ.get("MATRIX_TMP") or None)
    binary = str(Path(args.binary).resolve())
    jobs = [(binary, data, ref, t, pdep, scratch)
            for data, ref in datasets for t in map(int, args.threads.split(",")) for pdep in (1, 0)]
    failed = 0
    try:
        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            for job, ok, detail in pool.map(one, jobs):
                _, data, _, t, pdep, _ = job
                print(f"{'MATCH ' if ok else 'DIFFER'} {data.name:<8} threads={t} splitter={'pdep' if pdep else 'tzcnt'}", flush=True)
                if not ok:
                    failed += 1
                    print(detail, flush=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    print(f"\n{len(jobs) - failed}/{len(jobs)} comparisons identical over {len(datasets)} datasets")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
