#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-v5). Why is c19ab64 8.13 s under scorer V5?
# Prints sandbox/kernel/mount facts and per-phase timings of the unchanged
# engine on /data (the dev set) and on copies elsewhere. Exits 3; writes
# nothing to /output.
set -u
S=${SCRATCH_DIR:-/scratch}
L="$S/full.log"; : > "$L"
T="/usr/bin/time -f TIME_wall=%e_user=%U_sys=%S_rss=%M_minflt=%R"
run() { # label data extra-env...
  label=$1; d=$2; shift 2
  rm -rf "$S/o"; mkdir -p "$S/o"
  echo "=== RUN $label" >> "$L"
  env DATA_DIR="$d" OUTPUT_DIR="$S/o" SESSIONS_VERBOSE=1 "$@" $T /sessions >> "$L" 2>&1
}
sum() {
  awk -v want="$1" '
    /^=== RUN / { on = ($3 == want); next }
    on && /\] (setup|links|parse|dedup|people|write) / { for (i=1;i<=NF;i++) if ($i ~ /^(setup|links|parse|dedup|people|write)$/) { n=$i; v=$(i+1); for (j=i;j<=NF;j++) if ($j=="sys") s=$(j+1) }; p = p substr(n,1,2) "=" int(v) "/" int(s) " " }
    on && /^TIME_/ { t=$0; gsub(/TIME_/, "", t) }
    END { print want ": " p "| " t }' "$L"
}
run dev1 /data
run dev2 /data
cp -r /data "$S/devcopy" 2>/dev/null
run devscratch "$S/devcopy"
SECONDS0=$(date +%s)
timeout 60 /generate --seed 20 --events 5000000 --clicks 1250000 --output "$S/d5" --threads "$(nproc)" > /dev/null 2>&1
GEN=$(( $(date +%s) - SECONDS0 ))
[ -f "$S/d5/manifest.json" ] && run g5m "$S/d5"
RD=$( { /usr/bin/time -f "%e" cat "$S"/d5/events/*.csv > /dev/null; } 2>&1 | tail -1 )
{
  echo "=== SUMMARY v5"
  echo "kernel: $(uname -r) | $(head -c 90 /proc/version)"
  echo "dmesg: $(dmesg 2>/dev/null | head -2 | tr '\n' ' ' | cut -c1-120)"
  echo "cpu: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | tr -s ' ') nproc $(nproc) thp $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>&1 | cut -c1-40)"
  echo "mounts: $(grep -E ' /data | /scratch | / ' /proc/mounts | awk '{print $2"="$3}' | tr '\n' ' ')"
  echo "gen5m ${GEN}s, cat events ${RD}s, d5 $(du -sh "$S/d5" 2>&1 | cut -f1)"
  for r in dev1 dev2 devscratch g5m; do sum $r; done
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
