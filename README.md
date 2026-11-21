# Shopping Sessions: an exact, parallel engine in Rust

One static binary (no dependencies, no runtime) in an empty Docker image. It
reads `/data`, applies every rule of `rules.md` exactly, and writes
`/output/user_daily/` and `/output/campaign_daily/` as Parquet.

## How it works

The run is five parallel phases. Every phase uses all `NCPU` threads, and the
data flows between phases as cache-sized partitions, never as one big sort.

1. **Links.** Link lines are parsed in parallel and merged with a lock-free
   union-find (CAS on parent pointers, roots always hung under the smaller
   index). Each person is named by its byte-wise smallest ID, found with a
   CAS-min over an integer whose order is the IDs' byte order. The result
   is a read-only table from user ID to person, filled by all threads at
   once: 8-byte slots in 64-byte buckets at most half full, so a lookup
   almost always touches the one cache line that was prefetched for it.
2. **Parse.** Files are memory-mapped and cut into 2 MiB segments.
   - **Lines.** Each line is split from a 128-byte window. AVX2 builds the
     newline, comma and quote bitmasks, `pdep` finds each comma, and `pext`
     checks that every quote wraps a whole field. On CPUs where those two
     instructions are microcoded (AMD Zen 1/2, Hygon; read from CPUID) an
     equivalent tzcnt walk is used instead. The input is prefetched 2 KiB
     ahead, since page-cache pages are scattered 4 KiB frames that the
     hardware prefetcher does not cross.
   - **IDs.** Generator IDs (`e…`, `u…`, `k…`: a letter and up to 8 base-36
     digits) are decoded with SIMD into a dense 43-bit key. Any other string
     is interned, so equality stays exact.
   - **Timestamps.** F1, F2 and F3 are parsed with SIMD/SWAR and a
     calendar table.
   - **Person lookups.** These are prefetched and resolved in batches of 32.
   - **Output.** Event copies are scattered by event ID and clicks by person,
     both through write-combining staging buffers and non-temporal stores
     into chunked, huge-page memory.
3. **Dedup.** For each event-ID partition, an open-addressing table keeps the
   copy with the smallest `ingest_ts`. That copy alone decides validity,
   since dedup comes before validation. One sequential sweep emits the
   valid winners, scattered by person.
4. **People.** For each person partition:
   - Events are grouped by person with a counting sort, then sorted by time
     as 8-byte keys (`t << 32 | purchase`).
   - Sessions split on gaps of more than 30 minutes.
   - A sliding window over the person's clicks gives, per session, the
     credited click (latest; ties go to the byte-wise smallest `click_id`)
     and the set of assisting campaigns.
   - Each thread writes its own `user_daily` Parquet file as soon as it
     finishes; `campaign_daily` is merged at the end.
5. **Parquet.** A small built-in writer: one row group per file and one
   PLAIN page per column, encoded in a single pass over the rows, with RLE
   definition levels and `DATE` and `STRING` logical types. It is read back
   correctly by polars, pyarrow and DuckDB.

Click duplicates are not removed. Copies of a click are identical by the
rules, so they can never change the credited click or the assist set.

## Correctness

- `python3 scripts/run_fixtures.py fixtures -- ./target/release/sessions`
  passes all 36 edge-case fixtures, and also passes through Docker with the
  scorer's flags.
- The 1M-event dev set matches `dev-answer` exactly.
- `tools/reference.py` is a slow, literal Python implementation of
  `rules.md`. It also passes the fixtures and the dev answer. The engine
  matches it exactly on further generated seeds (0.7M to 2M events each).
- Unit tests (`cargo test --release`) fuzz both forms of the SIMD line
  splitter against the scalar CSV rules on 1M random lines
  (`SESSIONS_PDEP=0` forces the tzcnt form in a full run). They also check
  the person table against a hash map. They also check timestamps
  against the reference parser and ID keys for round trips and byte order.

## Build and run

```sh
docker build -t shopping-sessions .
scripts/docker_run.sh /path/to/data /path/to/out   # the scorer's flags
python3 scripts/compare.py /path/to/out dev-answer
```

Without Docker: `cargo build --release`, then
`DATA_DIR=... OUTPUT_DIR=... target/release/sessions`. Set
`SESSIONS_VERBOSE=1` for per-phase timings on stderr.
