FROM rust:1.83-slim AS builder

WORKDIR /build

# Copy manifests first for layer caching.
COPY Cargo.toml Cargo.lock* ./

# Create a dummy main.rs so `cargo build` downloads and compiles dependencies.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release || true

# Now copy real source and rebuild.
COPY src/ src/
# Touch main.rs to invalidate the dummy build.
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/acp-gateway /usr/local/bin/acp-gateway
COPY policies/ /etc/acp-gateway/policies/

ENTRYPOINT ["acp-gateway"]
