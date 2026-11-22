# DIAGNOSTIC BRANCH ONLY (diag-ucache). Never merge into main.
# Builds the user-cache candidate (src/), the accepted release c19ab64
# (diag/old/src) and the organizers' generator; diag/run.sh times them on the
# scorer and exits 3 on purpose.
FROM rust:1.89-slim-bookworm@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --locked && cp target/release/sessions /cand
COPY diag/old/src /old/src
RUN cp -r Cargo.toml Cargo.lock .cargo /old/ && cd /old && cargo build --release --locked && cp target/release/sessions /base
COPY diag/generator /gen
RUN cd /gen && cargo build --release --locked && cp target/release/generate /generate

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends time \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /cand /cand
COPY --from=build /base /base
COPY --from=build /generate /generate
COPY diag/run.sh /run.sh
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/bin/sh", "/run.sh"]
