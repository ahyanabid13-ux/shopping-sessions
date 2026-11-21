#!/bin/sh
# Differential check: engine vs tools/reference.py on a generated seed.
# Usage: tools/diffcheck.sh SEED EVENTS CLICKS
# Env: PY (python with polars+pyarrow), GEN (generate binary), WORK (scratch dir),
#      BIN (engine, default target/release/sessions), THREADS (default "1 2 3 8").
# Prints one line per thread count: MATCH or MISMATCH. Exit 1 on any mismatch.
set -eu
seed=$1; events=$2; clicks=$3
PY=${PY:-python3}; BIN=${BIN:-target/release/sessions}; THREADS=${THREADS:-"1 2 3 8"}
WORK=${WORK:?set WORK}; GEN=${GEN:?set GEN}
d="$WORK/seed$seed-$events"
if [ ! -f "$d/manifest.json" ]; then
  "$GEN" --seed "$seed" --events "$events" --clicks "$clicks" --output "$d" >/dev/null 2>&1
fi
if [ ! -d "$d.ref/user_daily" ]; then
  rm -rf "$d.ref"; "$PY" tools/reference.py "$d" "$d.ref" >/dev/null
fi
status=0
for t in $THREADS; do
  rm -rf "$d.out"
  NCPU=$t DATA_DIR="$d" OUTPUT_DIR="$d.out" "$BIN"
  if "$PY" scripts/compare.py "$d.out" "$d.ref" 2>&1 | tail -1 | grep -qx MATCH; then
    echo "seed $seed events $events threads $t: MATCH"
  else
    echo "seed $seed events $events threads $t: MISMATCH"; status=1
  fi
done
exit $status
