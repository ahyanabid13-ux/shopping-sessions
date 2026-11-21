#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-60m). Final-scale test on the scorer: generate
# ~60M events in /scratch, time the candidate engine (8 threads) and record its
# peak memory, then run the candidate and the scored v3 engine single-threaded
# and compare their output files byte for byte. Exits 3 on purpose; nothing is
# written to /output. Only a compact summary goes to stderr (the scorer shows
# the last ~1.2 KB).
set -u
S=${SCRATCH_DIR:-/scratch}
L="$S/full.log"; : > "$L"
T="/usr/bin/time -f TIME_wall=%e_user=%U_sys=%S_rss=%M"
run() { # label bin data outdir extra-env...
  label=$1; bin=$2; d=$3; o=$4; shift 4
  rm -rf "$o"; mkdir -p "$o"
  echo "=== RUN $label" >> "$L"
  env DATA_DIR="$d" OUTPUT_DIR="$o" SESSIONS_VERBOSE=1 "$@" $T "$bin" >> "$L" 2>&1
  echo "EXIT $label $?" >> "$L"
}
sum() {
  awk -v want="$1" '
    /^=== RUN / { on = ($3 == want); next }
    on && /\] (links|parse|dedup|people) / { for (i=1;i<=NF;i++) if ($i ~ /^(links|parse|dedup|people)$/) { n=$i; v=$(i+1) }; p = p substr(n,1,2) "=" int(v) " " }
    on && /^TIME_/ { t=$0; gsub(/TIME_|_rss=/, " ", t) }
    on && /^EXIT / { x=$3 }
    END { print want ": " p "|" t " exit=" x }' "$L"
}

$T timeout 120 /generate --seed 60 --events 60000000 --clicks 15000000 --output "$S/d60" --threads "$(nproc)" > /dev/null 2> "$S/gen.err"
if [ -f "$S/d60/manifest.json" ]; then
  run new8a /sessions "$S/d60" "$S/o8"
  run new8b /sessions "$S/d60" "$S/o8"
  run new1 /sessions "$S/d60" "$S/n1" NCPU=1
  run old1 /sessions_old "$S/d60" "$S/v1" NCPU=1
fi
{
  echo "=== SUMMARY 60M"
  echo "gen: $(tail -1 "$S/gen.err" | cut -c1-70) data $(du -sh "$S/d60" 2>&1 | cut -f1)"
  for r in new8a new8b new1 old1; do sum $r; done
  for f in user_daily/part-000.parquet campaign_daily/part-000.parquet; do
    a=$(md5sum "$S/n1/$f" 2>&1 | cut -c1-12); b=$(md5sum "$S/v1/$f" 2>&1 | cut -c1-12)
    if [ "$a" = "$b" ]; then echo "SAME $f $a"; else echo "DIFF $f new=$a old=$b"; fi
  done
  echo "rows-ish: ud $(stat -c %s "$S/n1/user_daily/part-000.parquet" 2>&1) B, cd $(stat -c %s "$S/n1/campaign_daily/part-000.parquet" 2>&1) B"
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
