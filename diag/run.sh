#!/bin/sh
# DIAGNOSTIC ONLY (branch diag-avx512). On the scorer: run the candidate's
# unit tests (incl. avx512_matches_avx2), time candidate-with-AVX-512,
# candidate-forced-AVX2 and the accepted release on 20M events (3 interleaved
# rounds), and compare candidate vs release outputs byte for byte at 1 thread.
# Exits 3 on purpose; nothing is written to /output.
set -u
S=${SCRATCH_DIR:-/scratch}
R="$S/res"; : > "$R"
cd "$S"
/cand_tests --test-threads 4 > "$S/tests.log" 2>&1; TST=$?
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
    one avx512 /cand
    one avx2 /cand SESSIONS_AVX512=0
    one base /base
  done
  MODE=$(DATA_DIR="$S/d20" OUTPUT_DIR="$S/o" SESSIONS_VERBOSE=1 /cand 2>&1 | grep -o 'masks: [a-z0-9]*')
  rm -rf "$S/c1" "$S/b1"
  NCPU=1 DATA_DIR="$S/d20" OUTPUT_DIR="$S/c1" /cand; NCPU=1 DATA_DIR="$S/d20" OUTPUT_DIR="$S/b1" /base
fi
{
  echo "=== SUMMARY avx512 (20M; wall user sys rssKB)"
  echo "unit tests exit=$TST: $(grep -E 'test result' "$S/tests.log" | cut -c1-60) | $(grep -c 'avx512_matches_avx2 ... ok' "$S/tests.log") avx512-test-ok | candidate $MODE"
  for v in avx512 avx2 base; do
    awk -v v="$v" '$1==v {printf "%s ", $2; u+=$3} END {printf "| user avg %.2f", u/3}' "$R" | awk -v v="$v" '{print v ": " $0}'
  done
  awk '$1=="avx512" {print "rss avx512 " $5}' "$R" | tail -1
  for f in user_daily/part-000.parquet campaign_daily/part-000.parquet; do
    a=$(md5sum "$S/c1/$f" 2>&1 | cut -c1-12); b=$(md5sum "$S/b1/$f" 2>&1 | cut -c1-12)
    if [ "$a" = "$b" ]; then echo "SAME $f $a"; else echo "DIFF $f cand=$a base=$b"; fi
  done
  echo "=== DIAGNOSTIC DONE: exit 3 on purpose"
} >&2
exit 3
