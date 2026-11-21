#!/bin/sh
# Local A/B benchmark: median per-phase user ms over N runs at NCPU=1.
# Usage: tools/bench_local.sh BIN DATA [RUNS]   (Mac single-thread ns/event tracks the scorer's per-hyperthread cost)
set -eu
bin=$1; data=$2; runs=${3:-7}
out=$(mktemp -d)
for i in $(seq "$runs"); do
  rm -rf "$out/o"
  NCPU=${NCPU:-1} DATA_DIR="$data" OUTPUT_DIR="$out/o" SESSIONS_VERBOSE=1 "$bin" 2>&1 \
    | awk '/\] (links|parse|dedup|people|write) / { for (i=1;i<=NF;i++) if ($i=="user") u=$(i+1); for (i=1;i<=NF;i++) if ($i ~ /^(links|parse|dedup|people|write)$/) n=$i; printf "%s %s\n", n, u }
           /\] write / { for (i=1;i<=NF;i++) if ($i ~ /^\[?[0-9.]+$/ && w=="") w=$i; }
           END {}' > "$out/r$i"
  grep -E '^\[' /dev/null || true
done
for ph in links parse dedup people write; do
  printf "%-7s" "$ph"
  cat "$out"/r* | awk -v p="$ph" '$1==p {print $2}' | sort -n | awk '{a[NR]=$1} END {printf " median %7.1f  min %7.1f\n", a[int((NR+1)/2)], a[1]}'
done
cat "$out"/r* | awk '{s[FILENAME]+=$2} END {}'
for f in "$out"/r*; do awk '{s+=$2} END {print s}' "$f"; done | sort -n | awk '{a[NR]=$1} END {printf "TOTAL  median %7.1f  min %7.1f  (user ms, sum of phases)\n", a[int((NR+1)/2)], a[1]}'
rm -rf "$out"
