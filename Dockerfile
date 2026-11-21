# DIAGNOSTIC BRANCH ONLY (diag-knobs). Never merge into main.
# Builds the engine six times with different build-time tuning constants, plus
# the organizers' generator; diag/run.sh times each on 20M events and exits 3.
FROM rust:1.89-slim-bookworm@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN mkdir /v \
 && cargo build --release --locked && cp target/release/sessions /v/base \
 && SESSIONS_PREFETCH_AHEAD=1024 cargo build --release --locked && cp target/release/sessions /v/pa1k \
 && SESSIONS_PREFETCH_AHEAD=4096 cargo build --release --locked && cp target/release/sessions /v/pa4k \
 && SESSIONS_LOOKAHEAD=8 cargo build --release --locked && cp target/release/sessions /v/la8 \
 && SESSIONS_LOOKAHEAD=32 cargo build --release --locked && cp target/release/sessions /v/la32 \
 && SESSIONS_SEGMENT=1048576 cargo build --release --locked && cp target/release/sessions /v/seg1m
COPY diag/generator /gen
RUN cd /gen && cargo build --release --locked && cp target/release/generate /generate

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends time \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /v /v
COPY --from=build /generate /generate
COPY diag/run.sh /run.sh
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/bin/sh", "/run.sh"]
