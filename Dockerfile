# Stage 1: Build with musl for static linking
FROM rust:slim-bookworm AS builder

RUN apt-get update && apt-get install -y musl-tools && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# Copy source files
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

# Build statically linked release binary
RUN cargo build --release --target x86_64-unknown-linux-musl

# Stage 2: Minimal runtime (binary only)
FROM scratch

# Copy cp binary from busybox for init container use
COPY --from=busybox:uclibc /bin/cp /bin/cp

# Copy binary from builder
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/cm-detector /cm-detector

ENTRYPOINT ["/cm-detector"]
