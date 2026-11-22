# Shopping Sessions: a single static Rust binary in an empty image.
#
# Build stage: the Rust toolchain pinned by digest; no crates are
# downloaded (the engine has no dependencies), so the build needs no network
# beyond the base image.
FROM rust:1.89-slim-bookworm@sha256:d7fc7de78bb8c1469933aeecbf801314d30d7d6e9f0578bba4cfa285bfa37fe6 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
RUN cargo build --release --locked \
 && strip target/release/sessions \
 && mkdir -p /out && cp target/release/sessions /out/sessions

# Run stage: nothing but the binary. The scorer runs it as uid 1000 with a
# read-only root, /data read-only, and /output and /scratch writable.
FROM scratch
COPY --from=build /out/sessions /sessions
ENV DATA_DIR=/data OUTPUT_DIR=/output
CMD ["/sessions"]
