# ---- build stage ----
FROM rust:1-bookworm AS builder

# musl target: fully static binary, no runtime libc/libssl needed.
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /build

# musl-tools: musl-gcc linker. perl/make: compile vendored OpenSSL from source.
RUN apt-get update && apt-get install -y --no-install-recommends \
    musl-tools perl make \
    && rm -rf /var/lib/apt/lists/*

# Copy only manifests first so dependency builds are cached across source changes.
COPY Cargo.toml Cargo.lock ./
COPY crates/castbell/Cargo.toml crates/castbell/Cargo.toml
COPY crates/castbell-client/Cargo.toml crates/castbell-client/Cargo.toml

# Create stub source so `cargo build` resolves the workspace deps without needing
# the real source yet (keeps the dependency layer cached).
RUN mkdir -p crates/castbell/src crates/castbell-client/src \
    && echo "fn main() {}" > crates/castbell/src/main.rs \
    && echo "fn main() {}" > crates/castbell-client/src/main.rs
RUN cargo build --release --target x86_64-unknown-linux-musl -p castbell

# Now copy the real source and build the actual binary.
COPY crates/ crates/
RUN touch crates/castbell/src/main.rs crates/castbell-client/src/main.rs \
    && cargo build --release --target x86_64-unknown-linux-musl -p castbell

# ---- runtime stage ----
# distroless/static: ~2 MB, just ca-certs + /etc/passwd. No shell, no libc.
FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/castbell /usr/local/bin/castbell

USER nonroot
EXPOSE 8080

ENTRYPOINT ["castbell"]
CMD ["--listen", "0.0.0.0:8080"]
