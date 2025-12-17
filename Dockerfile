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
# Touch main.rs and lib.rs to force a rebuild of the application code
RUN touch src/main.rs src/lib.rs
RUN cargo build --release

# Runtime Stage
FROM debian:bookworm-slim
WORKDIR /usr/local/bin

# Install OpenSSL/CA certificates required for reqwest
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/app/target/release/ramsey-worker-rust .

# Set the binary as the entrypoint
CMD ["./ramsey-worker-rust"]
