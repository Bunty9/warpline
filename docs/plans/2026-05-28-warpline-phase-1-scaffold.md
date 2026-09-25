---
title: warpline Phase 1 — Scaffold + Compile
status: draft
date: 2026-05-28
related:
    - ../specs/2026-05-28-warpline-design.md
    - ../../../projects-l3-l4.md
    - ../../../backend-cloud-roadmap.md
---

# warpline Phase 1 — Scaffold + Compile

> **Goal:** lay down the workspace skeleton — `warpline-core` library
> (`Engine` builder, host-import stubs, KV trait, `.cwasm` cache, metering
> writer), the `warpline-host` axum invoke server, the `warpline-control`
> upload + compile server, the `examples/hello-wasm` guest module (out of
> workspace), Postgres migrations, Dockerfile + compose + fly.toml, and
> the CI matrix — so that `cargo check --workspace` is green on the host
> platform and the file structure is ready for Phase 2 to fill in the
> real wit-bindgen-generated `handle` glue + the live host imports.
>
> Phase 1 does **not** implement: the wit-bindgen export bindings, the
> live kv/log/http-out host fns (currently stubbed — see `// NOTE(phase-1):`
> markers in `crates/core/src/runtime.rs`), the metering write on the
> invoke hot path, the S3-backed `.cwasm` cache, or the
> Cloudflare-Workers comparison bench harness.

**Spec source:** [`../specs/2026-05-28-warpline-design.md`](../specs/2026-05-28-warpline-design.md).

## File inventory (checklist)

- [x] `Cargo.toml` — workspace root, members
      `[crates/core, crates/host, crates/control]`; `examples/hello-wasm`
      excluded by design (wasm32 target conflict). Workspace deps pinned
      from `backend-cloud-roadmap.md` § 3.
- [x] `rust-toolchain.toml` — `channel = "stable"` + clippy + rustfmt.
- [x] `deny.toml` — minimal `cargo-deny` config (advisories deny,
      license allowlist for MIT/Apache/BSD/ISC/MPL/Unicode/CC0).
- [x] `.gitignore` — Rust + `*.wasm` + `*.cwasm` + `modules/` + `.env`.
- [x] `wit/warpline.wit` — capability surface (`kv`, `log`, `http-out`,
      world `handler`).
- [x] `crates/core/Cargo.toml` + `src/lib.rs` — re-exports `runtime`,
      `kv`, `meter`, `cache`, `types`.
- [x] `crates/core/src/types.rs` — `HttpReq`, `HttpResp`, `HostCtx`,
      `TenantLimiter` (impl `ResourceLimiter`).
- [x] `crates/core/src/runtime.rs` — `build_engine`, `invoke`,
      `register_host_fns`. Sync `func_wrap` substitution for kv + log;
      http-out is a Phase-2 stub.
- [x] `crates/core/src/kv.rs` — `KvStore` trait + `MemKv` impl
      (tokio RwLock<HashMap>).
- [x] `crates/core/src/cache.rs` — `load_or_compile` content-hash
      `.cwasm` cache (sha2 + hex).
- [x] `crates/core/src/meter.rs` — `record` Postgres insert.
- [x] `crates/host/Cargo.toml` + `src/main.rs` — axum server, port
      `:8080`, `POST /tenants/{t}/functions/{f}/invoke`. In-process
      module registry (stub-populated).
- [x] `crates/control/Cargo.toml` + `src/main.rs` — axum server, port
      `:8081`, `POST /tenants/{t}/functions/{f}` multipart upload.
      Calls `cache::load_or_compile`, persists under
      `./modules/{tenant}/{fn}/`.
- [x] `examples/hello-wasm/Cargo.toml` — `crate-type = ["cdylib"]`,
      standalone (not in workspace).
- [x] `examples/hello-wasm/src/lib.rs` — `#[no_mangle] pub extern "C" fn
      handle()` returning a static greeting via `handle_ptr` /
      `handle_len` helpers. `// TODO(phase-2): wit-bindgen` marker.
- [x] `examples/hello-wasm/README.md` — build instructions
      (`rustup target add wasm32-wasip1` + `cargo build --target
      wasm32-wasip1 --release`).
- [x] `migrations/0001_init.sql` — `tenants`, `functions`, `meter`
      tables.
- [x] `Dockerfile` — multi-stage `cargo-chef`, distroless `cc-debian12`
      final (NOT scratch — wasmtime needs libc).
- [x] `docker-compose.yml` — postgres + minio + warpline-host +
      warpline-control. Migrations mounted into both services.
- [x] `fly.toml` — single region `sin` (multi-tenant single host is
      the point per spec).
- [x] `.github/workflows/ci.yml` — stable + beta matrix; fmt, clippy
      (`-D warnings`), nextest, cargo-deny, criterion non-blocking,
      plus `build-wasm-example` job that installs `wasm32-wasip1` and
      builds `examples/hello-wasm`.
- [x] `README.md` — problem, ASCII arch diagram, stack table, upload +
      invoke curl examples, bench targets, "Design tradeoffs" section
      (epoch over fuel, `.cwasm` content-hash cache, deny-by-default
      http-out allowlist), dual MIT/Apache license.
- [x] `docs/specs/2026-05-28-warpline-design.md` — full P6 spec lifted
      from `projects-l3-l4.md`.
- [x] `docs/plans/2026-05-28-warpline-phase-1-scaffold.md` — this plan.
- [x] `PROGRESS.md` — per-sprint tracker, P6 bench targets recorded.

## Exit criteria

1. **`cargo check --workspace` passes** from the project root with no
   environment prerequisites. The workspace excludes
   `examples/hello-wasm` by design, so this check works without
   `rustup target add wasm32-wasip1` being installed.
2. **`cargo build --target wasm32-wasip1 --release` inside
   `examples/hello-wasm/` produces a `.wasm` artefact** at
   `examples/hello-wasm/target/wasm32-wasip1/release/hello_wasm.wasm`.
   (Verified in CI by the `build-wasm-example` job; not part of the
   host-platform `cargo check` exit criterion.)
3. **`cargo run -p warpline-host` binds `:8080`** and responds `200 OK`
   to `GET /healthz`. The invoke path returns `404 NOT FOUND` for
   unknown (tenant, fn) tuples — module-registry population is a
   Phase-2 task.

## Out of scope (deferred to later phases)

- wit-bindgen-generated `handle` export glue (host + guest sides).
- Real `kv`, `log`, `http-out` host fn bodies (currently stubs).
- Metering write on the invoke hot path.
- Tenant auth (API keys / mTLS) on the control plane.
- Per-tenant outbound allowlist enforcement.
- S3 / MinIO `.cwasm` cache backend (currently local fs only).
- Bench harness vs Cloudflare Workers (target: cost per million
  invocations, instances/sec/core).
- Component Model migration from preview1.
- Hyperlight integration for sub-millisecond ephemeral instances.

## Verification recipe

```bash
cd warpline
cargo check --workspace                                    # exit-criterion 1
# (optional, requires wasm32-wasip1 target installed)
(cd examples/hello-wasm && cargo build --target wasm32-wasip1 --release)
# exit-criterion 2
cargo run -p warpline-host                                 # exit-criterion 3
```
