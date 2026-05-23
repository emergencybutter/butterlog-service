# Multi-stage Dockerfile for ButterLog Rust Service
# 1. Builder Stage
FROM rust:1-slim AS builder

WORKDIR /usr/src/butterlog-service

# Copy all source files
COPY . .

# Build the release binary
RUN apt-get update && \
    apt-get install -y \
        build-essential \
        libssl-dev \
        pkg-config && \
    cargo build --release

# 2. Runner Stage
FROM debian:bookworm-slim

# Install SSL CA certificates (required for secure outgoing HTTPS requests to Discord API)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/local/bin

# Copy compiled binary from the builder stage
COPY --from=builder /usr/src/butterlog-service/target/release/butterlog-service .

# Cloud Run defaults to port 8080 (the app reads the PORT environment variable)
EXPOSE 8080

# Run the app
CMD ["./butterlog-service"]
