# syntax=docker/dockerfile:1.7

# ---- Stage 1: chef base (cargo-chef for layer-cacheable builds) -------------
FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef
WORKDIR /app

# ---- Stage 2: planner (compute the recipe of dependencies) ------------------
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Stage 3: builder (cook deps, then build the actual workspace) ----------
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cook the dependency layer first — this is the cargo-chef speedup.
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release --bin warpline-host --bin warpline-control

# ---- Stage 4: distroless runtime --------------------------------------------
# NOT scratch: wasmtime links libc + libgcc_s for the cranelift JIT. Distroless
# `cc` ships both, plus ca-certs for outbound HTTPS (the http-out capability,
# wired in Phase 2). Image size: ~70 MiB vs ~5 MiB scratch — the right
# tradeoff for a wasm host where the wasmtime runtime dominates anyway.
FROM gcr.io/distroless/cc-debian12 AS runtime
WORKDIR /app
COPY --from=builder /app/target/release/warpline-host /usr/local/bin/warpline-host
COPY --from=builder /app/target/release/warpline-control /usr/local/bin/warpline-control
COPY migrations /app/migrations
EXPOSE 8080 8081
# Default entrypoint is the host; docker-compose overrides for the control
# plane.
ENTRYPOINT ["/usr/local/bin/warpline-host"]
