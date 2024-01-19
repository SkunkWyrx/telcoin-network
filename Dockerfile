FROM rust:1.75-slim-bookworm as builder

WORKDIR /usr/src/telcoin-network

RUN apt-get update \
    && apt-get install -y build-essential cmake libclang-15-dev \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo* ./
COPY bin/ ./bin/
COPY crates/ ./crates/

# must move the resulting binary out of the cache for subsequent build steps
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=./target \
    cargo build --release \
    && mv ./target/release/telcoin-network /tmp/


# Production Image
FROM debian:bookworm-slim

# Create a non-root user
RUN useradd -ms /bin/bash nonroot
USER nonroot

COPY --from=builder /tmp/telcoin-network /usr/local/bin/telcoin

CMD ["telcoin", "node"]
