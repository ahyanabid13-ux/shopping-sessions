#!/bin/sh
# DIAGNOSTIC ONLY. Where does e648b78 spend its time on the scorer?
# Generates a 20M-event V5 dataset in /scratch, runs the unchanged engine on
# it and prints a compact per-phase summary (wall/sys ms). Exits 3 on purpose;
# writes nothing to /output.
set -u
S=${SCRATCH_DIR:-/scratch}
L="$S/full.log"; : > "$L"
T="/usr/bin/time -f TIME_w=%e_u=%U_s=%S_rssMB=%M_minflt=%R_majflt=%F"
run() { # label data [env...]
  label=$1; d=$2; shift 2
  rm -rf "$S/o"; mkdir -p "$S/o"
  echo "=== RUN $label" >> "$L"
  env DATA_DIR="$d" OUTPUT_DIR="$S/o" SESSIONS_VERBOSE=1 "$@" $T /sessions >> "$L" 2>&1
}
sum() {
  awk -v want="$1" '
    /^=== RUN / { on = ($3 == want); next }
    on && /\] (setup|links|parse|dedup|people|write) / { for (i=1;i<=NF;i++) if ($i ~ /^(setup|links|parse|dedup|people|write)$/) { n=$i; v=$(i+1); for (j=i;j<=NF;j++) { if ($j=="user") u=$(j+1); if ($j=="sys") s=$(j+1) } }; p = p substr(n,1,2) "=" int(v) "/" int(u) "/" int(s) " " }
    on && /^TIME_/ { t=$0; gsub(/TIME_/, "", t) }
    END { print want ": " p "| " t }' "$L"
}
G0=$(date +%s)
timeout 100 /generate --seed 20 --events 20000000 --clicks 5000000 --output "$S/d20" --threads "$(nproc)" > /dev/null 2>&1
GEN=$(( $(date +%s) - G0 ))
cat "$S"/d20/*/*.csv > /dev/null 2>&1
if [ -f "$S/d20/manifest.json" ]; then
  run a8 "$S/d20"
  run b8 "$S/d20"
  run c8 "$S/d20"
  run t4 "$S/d20" NCPU=4
  run t16 "$S/d20" NCPU=16
fi
{
  echo "=== SUMMARY e648b78 (phase=wall/user/sys ms)"
  echo "cpu:$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | tr -s ' ' | cut -c1-44) nproc $(nproc) NCPU ${NCPU:-unset}"
  echo "thp: $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>&1 | cut -c1-30) defrag: $(cat /sys/kernel/mm/transparent_hugepage/defrag 2>&1 | cut -c1-50)"
  echo "mounts: $(grep -E ' /data | /scratch | /output ' /proc/mounts | awk '{print $2"="$3}' | tr '\n' ' ') gen ${GEN}s $(du -sh "$S/d20" 2>&1 | cut -f1)"
  for r in a8 b8 c8 t4 t16; do sum $r; done
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
