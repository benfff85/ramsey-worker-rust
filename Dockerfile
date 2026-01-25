# Build Stage
FROM rust:latest AS builder
WORKDIR /usr/src/app

# Create a clean dependency layer
COPY Cargo.toml Cargo.lock ./
RUN mkdir src
RUN echo "fn main() {}" > src/main.rs
RUN echo "" > src/lib.rs
RUN cargo build --release

# Build the actual application
RUN rm -rf src
COPY src src
COPY .cargo .cargo
# Touch main.rs and lib.rs to force a rebuild of the application code
RUN touch src/main.rs src/lib.rs
# TARGET_CPU can be overridden at build time: --build-arg TARGET_CPU=apple-m4
# Options: generic (default), apple-m4, neoverse-v2 (Graviton3/4), etc.
ARG TARGET_CPU=generic
ENV RUSTFLAGS="-C target-cpu=${TARGET_CPU}"
RUN cargo build --release

# Runtime Stage - use trixie to match glibc 2.38 from rust:latest
FROM debian:trixie-slim
WORKDIR /usr/local/bin

# Install OpenSSL/CA certificates required for reqwest
RUN apt-get update && apt-get install -y --no-install-recommends \
    openssh-client \
    iproute2 \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*


COPY --from=builder /usr/src/app/target/release/ramsey-worker-rust .

# Spawn WORKER_COUNT processes (default: 1) to utilize multi-core containers
CMD ["sh", "-c", "for i in $(seq 1 ${WORKER_COUNT:-1}); do ./ramsey-worker-rust & done; wait"]
