# DIAGNOSTIC BRANCH ONLY (diag-60m). Never merge into main.
# Builds the candidate engine (src/), the scored v3 engine (diag/old/src) and
# the organizers' generator, then runs diag/run.sh, which exits 3 on purpose.
FROM rust:1.89-slim-bookworm@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --locked && cp target/release/sessions /sessions
COPY diag/old/src /old/src
RUN cp -r Cargo.toml Cargo.lock .cargo /old/ && cd /old && cargo build --release --locked && cp target/release/sessions /sessions_old
COPY diag/generator /gen
RUN cd /gen && cargo build --release --locked && cp target/release/generate /generate

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends time procps \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /sessions /sessions
COPY --from=build /sessions_old /sessions_old
COPY --from=build /generate /generate
COPY diag/run.sh /run.sh
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/bin/sh", "/run.sh"]
