#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-20m). Prints the scorer's environment and the
# engine's per-phase timings on a 20M-event set generated in /scratch, then
# exits non-zero on purpose so this build can never be scored. Nothing is
# written to /output. The scorer shows only the last ~1.2 KB of stderr, so the
# full log goes to a file and a compact summary is printed last.
set -u
S=${SCRATCH_DIR:-/scratch}
L="$S/full.log"
: > "$L"
T="/usr/bin/time -f TIME_wall=%e_user=%U_sys=%S_cpu=%P_rss=%M"

run() { # label, data dir, extra env...
  label=$1; d=$2; shift 2
  rm -rf "$S/o"; mkdir -p "$S/o"
  echo "=== RUN $label" >> "$L"
  env DATA_DIR="$d" OUTPUT_DIR="$S/o" SESSIONS_VERBOSE=1 "$@" $T /sessions >> "$L" 2>&1
}
# One compact line per run: wall per phase (ms) and the time(1) totals.
sum() {
  awk -v want="$1" '
    /^=== RUN / { on = ($3 == want); next }
    on && /\] (links|parse|dedup|people) / { for (i=1;i<=NF;i++) if ($i ~ /^(links|parse|dedup|people)$/) { n=$i; v=$(i+1) }; p = p substr(n,1,2) "=" int(v) " " }
    on && /^TIME_/ { t=$0; gsub(/TIME_|_cpu=[0-9]+%|_rss=/, " ", t) }
    END { print want ": " p "|" t }' "$L"
}

$T timeout 100 /generate --seed 20 --events 20000000 --clicks 5000000 --output "$S/d20" --threads "$(nproc)" > /dev/null 2> "$S/gen.err"
if [ -f "$S/d20/manifest.json" ]; then
  run a "$S/d20"
  run b "$S/d20"
  run c "$S/d20"
  run n4 "$S/d20" NCPU=4
  run n6 "$S/d20" NCPU=6
  run b10 "$S/d20" SESSIONS_BITS=10
  run b12 "$S/d20" SESSIONS_BITS=12
  if command -v perf > /dev/null 2>&1; then
    rm -rf "$S/o"; mkdir -p "$S/o"
    DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" perf stat -x, -e task-clock,cycles,instructions,LLC-load-misses,dTLB-load-misses,branch-misses /sessions > /dev/null 2> "$S/perf.csv"
  fi
fi

{
  echo "=== SUMMARY"
  echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | tr -s ' ')"
  echo "flags: $(grep -m1 '^flags' /proc/cpuinfo | tr ' ' '\n' | grep -xE 'avx512f|avx512vbmi|vpclmulqdq|fsrm' | tr '\n' ' ')"
  echo "topo: $(lscpu 2>/dev/null | grep -E '^(Thread|Core)|L3' | tr -s ' ' | tr '\n' ';')"
  echo "thp: $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>&1) kernel $(uname -r) data $(du -sh "$S/d20" 2>&1 | cut -f1)"
  echo "gen: $(tail -1 "$S/gen.err" | cut -c1-60)"
  for r in a b c n4 n6 b10 b12; do sum $r; done
  [ -f "$S/perf.csv" ] && cut -d, -f1,3 "$S/perf.csv" | tr '\n' ' ' | cut -c1-260
  echo
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
