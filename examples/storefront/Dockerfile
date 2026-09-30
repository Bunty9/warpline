# syntax=docker/dockerfile:1.7
# Builds the storefront app and the fraud mock. The build context is the
# repository root (see docker-compose.yml) only because app/Cargo.toml
# patches warpline-core to the path ../../../crates/core. In your own app
# there is no patch, and this file needs only your own sources.

FROM rust:1 AS builder
# examples/rust-toolchain.toml pins the guest toolchain; the app needs no pin.
ENV RUSTUP_TOOLCHAIN=stable
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY examples/storefront/app ./examples/storefront/app
WORKDIR /src/examples/storefront/app
RUN cargo build --release --locked --bin storefront --bin fraud-mock
# Owned by distroless `nonroot` (65532) so a fresh volume is writable.
RUN mkdir -p /out/var/lib/storefront/modules

# wasmtime links libc + libgcc_s, so distroless `cc`, not scratch.
FROM gcr.io/distroless/cc-debian12:nonroot AS storefront
COPY --from=builder /src/examples/storefront/app/target/release/storefront /usr/local/bin/storefront
COPY --from=builder --chown=65532:65532 /out/var/lib/storefront /var/lib/storefront
ENV MODULES_DIR=/var/lib/storefront/modules BIND=0.0.0.0:3000
EXPOSE 3000
ENTRYPOINT ["/usr/local/bin/storefront"]

FROM gcr.io/distroless/cc-debian12:nonroot AS fraud-mock
COPY --from=builder /src/examples/storefront/app/target/release/fraud-mock /usr/local/bin/fraud-mock
ENV BIND=0.0.0.0:3100
EXPOSE 3100
ENTRYPOINT ["/usr/local/bin/fraud-mock"]
