#!/bin/sh
# Deterministic cost model: instructions and simulated cache misses with a
# Cascade Lake-like hierarchy (32K L1d, 1 MiB L2 modelled as LL... cachegrind
# models two levels; LL is set to a per-core L3 share of 4 MiB).
# Usage: tools/cg.sh BINARY DATA_DIR
BIN=$1; DATA=$2
rm -rf /tmp/claude-0/cg_out
NCPU=1 DATA_DIR=$DATA OUTPUT_DIR=/tmp/claude-0/cg_out SESSIONS_VERBOSE=1 \
  valgrind --tool=cachegrind --cache-sim=yes --I1=32768,8,64 --D1=32768,8,64 --LL=4194304,16,64 \
  --cachegrind-out-file=/tmp/claude-0/cg.out "$BIN" 2>&1 | grep -E "I *refs|D *refs|LLd misses|D1 *misses|LL misses|\] (links|parse|dedup|people)"
