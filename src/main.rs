//! Shopping Sessions: reads the click-stream CSVs in $DATA_DIR and writes the
//! exact `user_daily` and `campaign_daily` Parquet tables of rules.md to
//! $OUTPUT_DIR. Exit code 0 means both tables are complete.

mod csv;
mod fast;
mod ids;
mod links;
mod lookup;
mod micro;
mod output;
mod parquet;
mod person;
mod pipeline;
mod scatter;
mod ts;
mod util;

#[global_allocator]
static GLOBAL: util::HugeAlloc = util::HugeAlloc;

fn main() {
    if let Some(path) = std::env::var_os("SESSIONS_MICRO") {
        micro::run(path.to_str().expect("utf-8 path"));
        return;
    }
    pipeline::run();
}
