#!/bin/sh
# Adversarial differential check: tools/adversarial.py datasets, engine vs
# tools/reference.py, at several thread counts, with exact schema checks
# (scripts/compare.py enforces columns, DATE and string types).
# Usage: tools/advcheck.sh FIRST_SEED LAST_SEED [EVENTS]
# Env: PY (python with polars+pyarrow), WORK (scratch dir), BIN (engine),
#      THREADS (default "1 2 4 8").
# Prints one line per seed; exit 1 on any mismatch or engine failure.
set -u
first=$1; last=$2; n=${3:-20000}
PY=${PY:-python3}; BIN=${BIN:-target/release/sessions}; THREADS=${THREADS:-"1 2 4 8"}
WORK=${WORK:?set WORK}
status=0
seed=$first
while [ "$seed" -le "$last" ]; do
  d="$WORK/adv$seed-$n"
  if [ ! -f "$d/manifest.json" ]; then
    rm -rf "$d"; "$PY" -I tools/adversarial.py "$seed" "$d" "$n"
  fi
  if [ ! -d "$d.ref/user_daily" ]; then
    rm -rf "$d.ref"; "$PY" tools/reference.py "$d" "$d.ref" > /dev/null || { echo "seed $seed: REFERENCE FAILED"; status=1; seed=$((seed + 1)); continue; }
  fi
  line="seed $seed:"
  for t in $THREADS; do
    rm -rf "$d.out"
    if ! NCPU=$t DATA_DIR="$d" OUTPUT_DIR="$d.out" "$BIN" 2> "$d.err"; then
      line="$line t$t=CRASH"; status=1; continue
    fi
    if "$PY" scripts/compare.py "$d.out" "$d.ref" > "$d.cmp" 2>&1 && tail -1 "$d.cmp" | grep -qx MATCH; then
      line="$line t$t=ok"
    else
      line="$line t$t=MISMATCH"; status=1; cp "$d.cmp" "$d.cmp.t$t"
    fi
  done
  echo "$line"
  seed=$((seed + 1))
done
exit $status
