# Build Stage
FROM rust:latest AS builder
WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

# Runtime Stage
FROM debian:bookworm-slim
WORKDIR /usr/local/bin

# Install OpenSSL/CA certificates required for reqwest
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/app/target/release/ramsey-worker-rust .

# Set the binary as the entrypoint
CMD ["./ramsey-worker-rust"]
