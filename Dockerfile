# Build: only `cargo build --release` runs here. Clippy and the tests run in the
# CI gate and locally, so a newer compiler in this image cannot fail the build
# on a lint that the project does not gate on.
FROM rust:1.91-bookworm AS build
WORKDIR /src

# The dependency layer: a dummy main keeps this build compiling every
# dependency, so a source-only change reuses it and rebuilds just the crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src

COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
# SQLite is compiled in (rusqlite's `bundled`), and the Wayland overlay loads
# libwayland at runtime through dlopen, so the binary needs only glibc and the
# CA bundle. `ldd` on a build of this crate confirms nothing else is linked.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/big_oil /usr/local/bin/big_oil

# Non-root, matching the pod's securityContext; the data volume is made writable
# for this uid by the pod's fsGroup.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/big_oil"]
