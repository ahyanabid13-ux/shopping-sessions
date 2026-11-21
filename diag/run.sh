#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-20m). Prints the scorer's environment and the
# engine's per-phase timings on the dev set and on a 20M-event set generated in
# /scratch, then exits non-zero on purpose so this build can never be scored.
# Nothing is written to /output.
set -u
S=${SCRATCH_DIR:-/scratch}
log() { echo "$@" >&2; }
T="/usr/bin/time -f TIME_wall=%e_user=%U_sys=%S_cpu=%P_maxrss_kb=%M_minflt=%R_majflt=%F_vcsw=%w_ivcsw=%c"

log "=== ENV"
log "uname: $(uname -r)  nproc: $(nproc)  NCPU=${NCPU:-} MEM_GB=${MEM_GB:-}"
grep -m1 'model name' /proc/cpuinfo >&2
grep -m1 '^flags' /proc/cpuinfo | tr ' ' '\n' | grep -xE 'avx2|bmi2|avx512f|avx512bw|avx512vbmi|avx512vl|vpclmulqdq|erms|fsrm' | tr '\n' ' ' >&2; log ""
lscpu 2>/dev/null | grep -E 'Thread|Core|Socket|NUMA node\(|L1d|L2|L3|MHz' >&2
log "thp: $(cat /sys/kernel/mm/transparent_hugepage/enabled 2>&1) defrag: $(cat /sys/kernel/mm/transparent_hugepage/defrag 2>&1)"
log "cgroup cpu.max: $(cat /sys/fs/cgroup/cpu.max 2>&1) cpuset: $(cat /sys/fs/cgroup/cpuset.cpus.effective 2>&1) mem.max: $(cat /sys/fs/cgroup/memory.max 2>&1)"
log "affinity: $(taskset -pc $$ 2>&1)"
grep -E ' /data | /scratch | /output ' /proc/mounts >&2
log "data: $(du -sh /data 2>&1 | cut -f1)  meminfo: $(grep -E 'MemTotal|MemAvailable' /proc/meminfo | tr -s ' ' | tr '\n' ' ')"
log "perf_event_paranoid: $(cat /proc/sys/kernel/perf_event_paranoid 2>&1)"

run() { # label, data dir, extra env...
  label=$1; d=$2; shift 2
  rm -rf "$S/o"; mkdir -p "$S/o"
  log "=== RUN $label"
  env DATA_DIR="$d" OUTPUT_DIR="$S/o" SESSIONS_VERBOSE=1 "$@" $T /sessions 2>&1 | grep -vE '^\s+write_user_daily\[' >&2
}

run dev-1 /data
run dev-2 /data

log "=== GEN 20M"
$T timeout 100 /generate --seed 20 --events 20000000 --clicks 5000000 --output "$S/d20" --threads "$(nproc)" >/dev/null 2>"$S/gen.err"
tail -2 "$S/gen.err" >&2
log "d20: $(du -sh "$S/d20" 2>&1 | cut -f1) events files: $(ls "$S/d20/events" 2>/dev/null | wc -l)"

if [ -f "$S/d20/manifest.json" ]; then
  run d20-a "$S/d20"
  run d20-b "$S/d20"
  run d20-c "$S/d20"
  run d20-ncpu4 "$S/d20" NCPU=4
  run d20-pdep0 "$S/d20" SESSIONS_PDEP=0
  if command -v perf >/dev/null 2>&1; then
    log "=== PERF STAT d20"
    rm -rf "$S/o"; mkdir -p "$S/o"
    DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" perf stat -e task-clock,cycles,instructions,cache-misses,LLC-load-misses,dTLB-load-misses,page-faults,context-switches,cpu-migrations /sessions 2>&1 | tail -16 >&2
  fi
fi
log "=== DIAGNOSTIC DONE: exiting 3 on purpose (no output written)"
exit 3
