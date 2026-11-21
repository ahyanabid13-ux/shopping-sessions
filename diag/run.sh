#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-knobs). Times six builds of the engine that
# differ only in tuning constants, 3 interleaved rounds on 20M events, and
# prints each variant's wall times (sorted) and median. Exits 3 on purpose;
# nothing is written to /output.
set -u
S=${SCRATCH_DIR:-/scratch}
R="$S/res"; : > "$R"
timeout 100 /generate --seed 20 --events 20000000 --clicks 5000000 --output "$S/d20" --threads "$(nproc)" > /dev/null 2>&1
V="base pa1k pa4k la8 la32 seg1m"
if [ -f "$S/d20/manifest.json" ]; then
  for v in $V; do rm -rf "$S/o"; DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" "/v/$v"; done  # warm-up
  for round in 1 2 3; do
    for v in $V; do
      rm -rf "$S/o"; mkdir -p "$S/o"
      w=$( { /usr/bin/time -f "%e" env DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" "/v/$v" > /dev/null; } 2>&1 | tail -1 )
      echo "$v $w" >> "$R"
    done
  done
fi
{
  echo "=== SUMMARY knobs (wall s, 20M, 3 rounds)"
  for v in $V; do
    awk -v v="$v" '$1==v {print $2}' "$R" | sort -n | tr '\n' ' ' | awk -v v="$v" '{printf "%-6s %s median %s\n", v, $0, $2}'
  done
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
