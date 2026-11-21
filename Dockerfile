# DIAGNOSTIC BRANCH ONLY (diag-20m). Never merge into main.
# Builds the engine exactly as main does, plus the organizers' generator, and
# runs diag/run.sh, which prints timings and exits non-zero on purpose.
FROM rust:1.89-slim-bookworm@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --locked && cp target/release/sessions /sessions
COPY diag/generator /gen
RUN cd /gen && cargo build --release --locked && cp target/release/generate /generate

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends time util-linux procps linux-perf \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /sessions /sessions
COPY --from=build /generate /generate
COPY diag/run.sh /run.sh
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/bin/sh", "/run.sh"]
