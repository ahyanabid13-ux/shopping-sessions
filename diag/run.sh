#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-ucache). 20M events: candidate with user cache
# 0 / 1024 / 4096 entries and the accepted release, 3 interleaved rounds, and
# a byte comparison (1 thread) of candidate (1024) vs release. Exits 3.
set -u
S=${SCRATCH_DIR:-/scratch}
R="$S/res"; : > "$R"
timeout 100 /generate --seed 20 --events 20000000 --clicks 5000000 --output "$S/d20" --threads "$(nproc)" > /dev/null 2>&1
one() { # label bin env...
  label=$1; bin=$2; shift 2
  rm -rf "$S/o"; mkdir -p "$S/o"
  r=$( { env DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" "$@" /usr/bin/time -f "%e %U %S %M" "$bin" > /dev/null; } 2>&1 | tail -1 )
  echo "$label $r" >> "$R"
}
if [ -f "$S/d20/manifest.json" ]; then
  one warm /cand; one warm /base
  for round in 1 2 3; do
    one uc0 /cand SESSIONS_UCACHE=0
    one uc1k /cand SESSIONS_UCACHE=1024
    one uc4k /cand SESSIONS_UCACHE=4096
    one base /base
  done
  rm -rf "$S/c1" "$S/b1"
  SESSIONS_UCACHE=1024 NCPU=1 DATA_DIR="$S/d20" OUTPUT_DIR="$S/c1" /cand; NCPU=1 DATA_DIR="$S/d20" OUTPUT_DIR="$S/b1" /base
fi
{
  echo "=== SUMMARY ucache (20M; wall s x3 | avg user s)"
  for v in uc0 uc1k uc4k base; do
    awk -v v="$v" '$1==v {printf "%s ", $2; u+=$3} END {printf "| user %.2f", u/3}' "$R" | awk -v v="$v" '{print v ": " $0}'
  done
  for f in user_daily/part-000.parquet campaign_daily/part-000.parquet; do
    a=$(md5sum "$S/c1/$f" 2>&1 | cut -c1-12); b=$(md5sum "$S/b1/$f" 2>&1 | cut -c1-12)
    if [ "$a" = "$b" ]; then echo "SAME $f $a"; else echo "DIFF $f cand=$a base=$b"; fi
  done
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
