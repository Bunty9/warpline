# warpline adoption report: from 0.2 to a drop-in for popular stacks

Date: 2026-10-05. Subject: warpline-core / -host / -control 0.2.0 (wasmtime 49, Component Model, WASI p2), https://github.com/Bunty9/warpline.
Inputs: five research reports (01 landscape, 02 use cases, 03 stack integration, 04 technical gaps, 05 distribution). I checked their code claims against the repo and re-verified the high-stakes web claims. Claims I could not confirm are dropped or marked **unverified**. The companion sequencing is in [`docs/plans/2026-10-05-roadmap-to-1.0.md`](../plans/2026-10-05-roadmap-to-1.0.md).

## 1. Executive summary

- **Slot.** warpline is the only maintained, Component-Model-native **Rust library** I found that ships a multi-tenant control surface: per-tenant CPU, memory and egress caps, admission control, API-key auth and Postgres metering. Extism is a library but uses core modules (wasmtime-wasi `p1`) and has no tenant layer. Spin and wasmCloud are platforms. Position warpline as "self-hosted Shopify Functions / Workers for Platforms that you embed", not as a Spin competitor.
- **Top use cases.** SaaS custom actions and event hooks, commerce/checkout hooks, and AI-agent tool (MCP) hosting score highest, followed by rules/policy engines and webhook transformation. All of them are short, hot-path, deny-by-default calls, which is what warpline is built for.
- **The shipped binary is not production-safe yet,** and none of the fixes is large:
  - KV is in-memory and lost on restart, because `warpline-host` never calls `.kv()`.
  - Global concurrency is about 16 at the default limits, and the binary has no knob to raise it.
  - `/healthz` returns "ok" unconditionally.
  - `http-out` cannot send headers, so a guest cannot call an authenticated API.
- **The biggest adoption blocker is guest DX, not host glue.**
  - The guest contract is a bespoke `list<u8> -> list<u8>` world that is not published to any registry.
  - Only Rust guests are documented.
  - There are no secrets or config, and no CLI or templates.
- **Host-side "drop-in" is cheap.**
  - Rust: a thin `warpline-axum`/tower crate, since `Runtime` is already `Clone` and `InvokeError::http_status` exists.
  - Every other language: sidecar plus an OpenAPI spec and thin TS/Python clients. Native bindings are not worth it.
- **Standards timing.**
  - WASI 0.3 went final on 2026-06-11, and Wasmtime 46+ enables it by default. warpline links p2 only.
  - wasi:http is the only Phase 3 proposal among those relevant here. keyvalue and config are Phase 2, logging is Phase 1.
  - So: do the custom-world fixes now, add wasi:http later, and do not adopt keyvalue/config as the contract.
- **0.2.1 (in progress):**
  - wasmtime 49.0.2, which covers RUSTSEC-2026-0321..0327 (8 GHSAs in total).
  - Prebuilt binaries for 4 targets, SHA256SUMS and attestations, binstall metadata, and a GHCR multi-arch image.
- **Demand is the largest unknown.** Plugin sandboxes have proven demand (Extism has 5.8k stars). A tenant-aware Component Model embedder does not have proven demand yet. Run a demand test alongside 0.2.2/0.3.

## 2. Where warpline fits

| | **warpline 0.2** | **Spin 4.x** (Akamai/CNCF) | **wasmCloud 2.x** (CNCF Incubating) | **Extism 1.30** (Dylibso) | **Cloudflare Workers for Platforms** |
|---|---|---|---|---|---|
| Shape | Rust **library** plus two optional binaries | Platform and CLI (`spin up`, SpinKube) | Distributed platform, `wash`, k8s operator | **Library**: host SDKs in about 15 languages | Proprietary SaaS |
| Wasm model | Component Model, WASI p2, custom world `warpline:host/handler` (`handle(list<u8>) -> list<u8>`) | Component Model, `wasi:http`, P3 (WASIp3 handler since 4.0) | Component Model; WASI P3 on by default since 2.5 (Wasmtime 46) | Core modules; `wasmtime-wasi` `p1` (main pins wasmtime 48) | V8 isolates; Wasm via JS, no Component Model |
| Tenancy, metering | Per-tenant CPU (epoch), memory, in-flight, egress allowlist, admission budget, API keys, Postgres metering and quotas | Per-app; no tenant billing | Per-workload; not metered per tenant | Per-plugin timeout, memory and HTTP allowlist; no tenants or metering | Dispatch namespaces, per-script limits, billed per request and per CPU-ms |
| Standard interfaces | None (custom kv/log/http-out) | wasi:http, wasi:keyvalue, wasi:config, variables, SQL, Redis | wasi:http, keyvalue, config, logging, messaging | Own host-function ABI | Workers bindings (KV, DO, R2, D1) |
| Guest languages | Rust (documented) | Rust, JS/TS, Python, Go | Rust, Go, TS, Python | About 16 PDKs | JS/TS first; Rust, C, Go via Wasm; Python |
| Distribution | crates.io; binaries and GHCR in 0.2.1 | Installer, OCI, SpinKube Helm | OCI, Helm and operator, binaries | Language packages | SaaS |
| Latest (verified) | 0.2.0 | v4.2.1 (2026-09-30), 6.5k stars | v2.10.3 (2026-10-02), 2.4k stars | v1.30.0 (2026-06-04), 5.8k stars | $25/mo plan: 20M requests, 60M CPU-ms and 1,000 scripts included, then $0.30/M requests, $0.02/M CPU-ms, $0.02 per script |
| Relationship to warpline | Competes only for "in-house FaaS" (U9) | Competes for U9; reference for guest DX | Closest library peer; differentiate on Component Model and tenancy | The commercial reference for U1–U3; warpline's pitch is self-hosted, embeddable and Rust |

Differentiators (from report 01, checked against the code):

| Claim | Verdict |
|---|---|
| Library-first, Component-Model-native | **Real, strongest.** No maintained peer found. |
| Per-tenant caps and fail-fast admission | Real but replicable. The value is tested defaults. |
| Postgres auth and metering | Real but narrow; it ties warpline to Postgres. |
| Content-addressed `.cwasm` cache with compat hash | Real, table stakes. |
| 0.26 ms cold start | Real but measured with an echo micro-benchmark. No HTTP-path bench yet. |
| "~1800x cheaper than CF Workers" | **Not credible. Remove it** (PROGRESS.md already calls it non-load-bearing). |
| Custom WIT world | Double-edged: simple, but not portable to Spin or wasmCloud. |

Dead or stalled adjacent projects: NGINX Unit is archived (repo read-only; last push 2025-10-08). Lunatic and Suborbital are stalled. Fermyon was acquired by Akamai (announced 2025-12-01).

## 3. Top use cases

Fit and demand are report 02's judgement on a 1–5 scale. I kept them, but cross-checked each "needs" column against the code. Gap codes: **S** secrets/config, **T** triggers, **J** JS/Python guests, **W** standard worlds such as wasi:http, **O** observability, versions and audit, **M** deterministic fuel metering, **L** streaming or large payloads, **R** durable or long-running, **P** permission manifests and signing, **H** `http-out` headers (added by me; the reports missed it), **K** durable KV (added by me).

| Rank | Use case (name used in roadmap) | Fit | Demand | What it needs from warpline | Evidence |
|---|---|---|---|---|---|
| 1 | **SaaS hooks**: custom code actions and event hooks in a B2B SaaS (HubSpot/Zapier-style) | 4 | 5 | K, H, S, J, typed or fallible `handle`, per-tenant logs, event fan-out (T) | HubSpot custom code limits of 20 s and 128 MB (per report 02; **unverified**) |
| 2 | **Commerce hooks**: checkout, discount and shipping functions (Shopify-Functions-style) | 5 | 4 | M (fuel), typed contract, K, versions and rollout (O), J | Shopify: 256 kB binary, 10,000 kB memory, 512 kB stack, 11M instructions, 128 kB in / 20 kB out (shopify.dev, verified). `examples/storefront` already implements this. |
| 3 | **Agent tools**: AI-agent tool and MCP-server hosting | 4 | 5 | H, S (host-injected credentials), MCP front-end mapping `tools/call` to `invoke`, J, W | Microsoft Wassette; Cloudflare Code Mode |
| 4 | **Policy engine**: rules, pricing, eligibility and policy decisions | 4 | 4 | M, versioning, audit and shadow mode (O), typed contract | OPA compiles Rego to Wasm (core ABI, needs an adapter) |
| 5 | **Workflow code steps**: replacing n8n/Zapier/Pipedream Code nodes | 3 | 5 | J (decisive), S, H, K, longer wall time (R) | n8n CVE-2026-1470 (CVSS 9.9, expression-engine AST sandbox escape via `with`) and CVE-2026-0863 (8.5, Python Code-node escape), both verified |
| 6 | **LLM code execution**: running model-written snippets ("Code Mode" pattern) | 3 | 5 | Prebuilt JS/Python interpreter guest (J), M, bindings injection (S), virtual FS (W) | E2B $21M Series A (Jul 2025); Modal $355M Series C at $4.65B (May 2026). Both verified. |
| 7 | **Webhook transforms**: webhook and payload transformation | 5 | 3 | J, S, H, webhook ingress with signature checks (needs headers) | — |
| 8 | **Template rendering**: notification and template rendering | 4 | 3 | J (Liquid/Handlebars guest), S | — |
| 8 | **In-house FaaS**: internal multi-tenant FaaS platform | 4 | 3 | T (cron), W (wasi:http), multi-node registry, O, K | American Express runs wasmCloud internally (report 02, secondary source) |
| 10 | **CMS plugins**: CMS and commerce plugin hosting | 3 | 3 | J, T, D | — |

Poor fits, which should not drive the roadmap: API-gateway plugins (needs proxy-wasm or in-process streaming), stream transforms, database UDFs, game mods, CI steps, UI plugins and heavy compute. Salesforce Functions (EOL 2025-01-31) is the cautionary tale for heavy compute.

Gap frequency across the top 10: **J** 9, **S** 8, **H/K** 7 each (from my code check: any guest that calls an authenticated API or keeps state), **T** 6, **O** 5, **M** 3, **W** 3. J, S, H and K are the leverage points.

## 4. Stack-by-stack drop-in assessment

Effort: S ≤ 2 days, M ≈ 1 week, L ≥ 2 weeks (single maintainer).

| Stack | Recommended integration | Exists today | Missing | Effort |
|---|---|---|---|---|
| **Rust: axum, Loco** | `warpline-tower` (`Service<InvokeRequest>`) plus `warpline-axum` (state, `TenantAuth` extractor, `IntoResponse for InvokeError`, mountable invoke router) | `Runtime` is `Clone` (`Arc<Inner>`); `InvokeError::http_status`; `parse_bearer`; `examples/storefront` (copy-paste axum) | The crates themselves; an `Authenticate` trait (auth is a concrete Postgres struct); per-invoke spans | S (+M for the auth trait) |
| Rust: actix, Poem, Salvo, Rocket | Examples on `warpline-tower`, no crates until asked | Works today via `web::Data<Runtime>` and similar; all run on tokio | Smoke test on actix's current-thread workers | S each |
| Rust: Pingora | Run `warpline-host` as an upstream; example only | — | Doc | S |
| **Node/TS** (Express, Fastify, Next.js) | `@warpline/client` (fetch-based, typed errors, retry on 429/503); Fastify and Express helpers | HTTP API on :8080/:8081 | OpenAPI spec, client, examples. napi-rs only after demand. | S (client) |
| **Python** (FastAPI, Django) | `warpline` on PyPI (httpx client, FastAPI `Depends`, Django middleware) | HTTP API | Client package. PyO3 later, if ever. | S |
| Go, Java/Spring, .NET, Ruby, PHP, Elixir | Clients generated from OpenAPI 3.1 plus a thin hand-written error and retry layer | HTTP API | OpenAPI spec; per-language packages only on request | M (spec plus generation), S each after |
| **Docker / compose** | GHCR `ghcr.io/bunty9/warpline` (amd64, arm64, distroless, non-root) | Dockerfile and compose (source build) | Published image: **in progress for 0.2.1** | in progress |
| Bare metal / VM | Release tarballs, `cargo binstall`, systemd unit and guide | `cargo install` | Tarballs and binstall **in progress for 0.2.1**; systemd unit | S |
| Fly / Render / Railway | Templates | `fly.toml` (one machine, one volume) | Render and Railway templates | S each |
| **Kubernetes** | Helm chart (host plus control, probes, ServiceMonitor, PVC; **single replica** until a blob registry exists) | Nothing | Chart; real `/readyz`; drain | M |
| **Guest: Rust** | `warpline-guest` crate (embedded WIT plus `export_handler!`) | README wit-bindgen snippet with a relative WIT path | Crate; WIT published via `wkg` | S |
| **Guest: JS/TS** | `jco componentize` template plus `@warpline/guest` types | Nothing | Template, CI build, a documented size and memory floor (StarlingMonkey engine is MBs; the reports' "about 8 MB" figure is **unverified**) | M |
| **Guest: Python** | `componentize-py` template | Nothing | Template; check against the 16 MiB upload cap and 64 MiB memory default | M |
| Guest: Go | One toolchain (TinyGo wasip2 or componentize-go), CI-pinned | Nothing | Template plus CI | M |
| Guest: .NET, Zig, MoonBit, C | "Preview" or "community" docs only | — | — | — |
| **Spin / wasmCloud components** | Second ABI: `wasi:http` incoming handler (p2, then p3) detected at publish | Rejected with 422 (does not match the handler world) | wasi:http host via `wasmtime-wasi-http` with the warpline egress hook | L |

## 5. Technical gaps (consolidated, de-duplicated)

**Verified against the code** (repo HEAD 21404e6, working tree):

| # | Gap | Evidence | Effort | Breaking |
|---|---|---|---|---|
| G1 | **Binary uses volatile in-memory KV.** Data is lost on restart and diverges across nodes. | `crates/host/src/main.rs:69` `Runtime::builder(cfg).meter(meter).build()`, no `.kv()`; `runtime.rs:141` defaults to `MemKv::new()`. The storefront already has a `PgKv`. | S–M | No |
| G2 | **Global concurrency is about 16 at default limits.** | `runtime.rs:75` budget is 1024 MiB; `runtime.rs:454` weight is `mem_cap_bytes.div_ceil(MIB)`; default cap is 64 MiB (`types.rs:123`, migration `DEFAULT 67108864`). 1024/64 = 16, regardless of cores or actual use. The 32-per-tenant cap (`:76`) is unreachable at defaults. The binary exposes **no env var** for the budget. | S (env knob) / M (rework) | Default change only |
| G3 | **`/healthz` is unconditional** on both services. | `host/src/lib.rs:91`, `control/src/lib.rs:85` `get(\|\| async { "ok" })` | S | No |
| G4 | **`http-out` cannot set request headers**, so a guest cannot send `Authorization`. | `warpline.wit`: `record request { url, method, body }` | S–M (new WIT version) | Additive world |
| G5 | Guest contract is untyped and infallible; the host forwards only the body and always answers 200. | `warpline.wit` `handle: func(list<u8>) -> list<u8>`; `host/src/lib.rs:169,192` | S (with G4) | New WIT version |
| G6 | KV is `get`/`put` only: no delete, list, increment/CAS or TTL. The storefront's lost-increment bug comes from this. | `warpline.wit`, `kv.rs` | M | Trait methods need defaults |
| G7 | No per-tenant config or secrets; WASI env is empty. | `types.rs` (`WasiCtxBuilder::new().build()`) | M | No |
| G8 | WIT is unpublished, there are no guest SDKs, CLI or local test harness, and only Rust is documented. | README "Writing a guest" | M–L | No |
| G9 | No `wasi:http` world; p2-only linker. | `sandbox.rs` (`p2::add_to_linker_async`); wasmtime-wasi features `["p2"]` | L | No (additive ABI) |
| G10 | No versions, rollback or canary. Pointer-based GC can delete a blob a DB row still references (Known issue 2). | `registry.rs` GC; one `functions` row | M–L | Pointer format (reader-compatible) |
| G11 | No key revocation, rotation, scopes or audit log. The same key invokes **and** publishes. | No DELETE/revoke anywhere; `control/src/lib.rs` | M | Backfill |
| G12 | Auth has no circuit breaker during a Postgres outage (2 s per uncached key). | Known issue 1 | S | No |
| G13 | No traces. Metrics live only in the host binary, so embedders get none. | No `opentelemetry` dependency | M | No |
| G14 | Single-node registry (local directory, no trait); no single-flight on cold compile; the warm path does a `spawn_blocking` pointer read on every invoke. | README roadmap; `registry.rs` | L / M | No (registry is private) |
| G15 | No pooling allocator; stack sizes and engine posture are implicit; CPU limit is epoch-based (`consume_fuel(false)`, `sandbox.rs:222-223`), not deterministic. | `sandbox.rs` | M / S / S–M | No (fuel forces recompiles) |
| G16 | Upload hardening is missing: no `wasmparser` limits, compile is in-process with no timeout, `.cwasm` has no integrity check, file mode 0644. | `runtime.rs:286-310`, `cache.rs` | M / S–M | No |
| G17 | No per-tenant egress rate, concurrency or byte quotas; one reqwest pool for all tenants. | `sandbox.rs` fetch | M | No |
| G18 | `pub use wasmtime;` makes every monthly wasmtime major a semver-major for embedders. | `core/src/lib.rs:44` | S | **Yes** (do before 1.0) |
| G19 | Meter table is one row per invocation, never partitioned or rolled up. | `0001_warpline.sql` | M | Additive |
| G20 | No cron or queue triggers. | — | M (cron) / L (queue) | No |
| G21 | No `Runtime::drain`; no readiness flip on SIGTERM. | `host/src/main.rs` | S | No |

**Contradictions between the reports and how I resolved them:**

| Topic | Conflict | Resolution |
|---|---|---|
| Concurrency | 02 says "32 in flight per tenant"; 04 says "about 16 global". | Both are in the code. 16 binds first at defaults (G2). |
| wasmtime pin | 04 says Cargo.lock has 49.0.1. | The working tree's Cargo.lock now has 49.0.2: the 0.2.1 work in progress. |
| Spin 4.0 | 01 says "2026-06-15, stabilised WASIp3". | GitHub API: v4.0.0 shipped **2026-04-20** on Wasmtime 43 with a WASIp3 (RC) HTTP handler. It predates WASI 0.3 final, so 01's date is wrong. |
| wasmCloud P3 default | 01 says 2.6; 04 says 2.5. | The 2026-07-01 community meeting says "**2.5** shipped with Wasmtime 46 and WASI P3 on by default". 2.6 refers to the `implements` feature. |
| Extism wasmtime | 01 says the release notes give 43 and Cargo.toml gives 48. | Main branch `runtime/Cargo.toml` pins 48 with `p1`. v1.30.0 is the latest tag (2026-06-04). Both can be true. |
| WASI proposal phases | 01: messaging Phase 1. 04: could not verify logging. | WASI `docs/Proposals.md`: http **Phase 3**; keyvalue, config and messaging **Phase 2**; logging and observe **Phase 1**. |
| 49.0.2 advisories | 04 says 8 advisories; the brief says RUSTSEC-0321..0327 (7). | The release lists 8 GHSAs. RustSec has 0321–0324 under `wasmtime-wasi` and 0325–0327 under `wasmtime`. The 8th (wasi:http zero-timeout panic) is in `wasmtime-wasi-http`, which warpline does not depend on. |
| cargo-dist | 05 says v0.33.0 on 2026-09-10. | GitHub API: published 2026-09-11 UTC. It is active, but 05's "hand-roll, don't adopt" verdict stands. |

## 6. Distribution

**In progress, shipping in 0.2.1** (commit 21404e6 on `release/binaries`; README "Install" already documents it):

| Item | Detail |
|---|---|
| Targets | `x86_64`/`aarch64-unknown-linux-gnu` (built on ubuntu-22.04 for a glibc 2.35 floor), `x86_64`/`aarch64-apple-darwin` |
| Artifacts | `warpline-vX.Y.Z-<target>.tar.gz` (both binaries plus licenses and README), combined `SHA256SUMS` |
| Provenance | `actions/attest-build-provenance` on every archive and on SHA256SUMS; verify with `gh attestation verify` |
| binstall | `[package.metadata.binstall]` in the host and control crates; resolves from 0.2.1 onward |
| Image | `ghcr.io/bunty9/warpline`, linux/amd64 and arm64, distroless non-root, built by COPYing the attested tarballs (no QEMU, byte-identical binaries) |
| Ordering | preflight → build (×4) → crates.io → GitHub release (draft, upload, publish) → image. Every step is idempotent. |
| Security | wasmtime 49.0.2 (RUSTSEC-2026-0321..0327) |

**Next, in order:**
1. A release-checklist line that runs `cargo binstall --dry-run` against the tag.
2. A systemd unit and bare-metal guide (S).
3. A Helm chart, after `/readyz` (M).
4. Render and Railway templates (S).
5. An SBOM (`cargo cyclonedx`) when someone asks.

**Not now:**
- musl: wasmtime Tier 3 and dynamic-only.
- Windows: untested server.
- Homebrew tap, curl|sh installer, Nix, AUR, Scoop.
- cargo-dist: it would own `release.yml` and undo the existing hardening.
- cosign on top of attestations: duplicate.

## 7. Risks

| Risk | Facts (verified) | Mitigation |
|---|---|---|
| **WASI 0.3 churn** | 0.3.0 final 2026-06-11; 0.3.1 2026-08-11; 0.3.2 planned 2026-10-13; patch releases every two months. Guest toolchains (Rust, Go, JS, Python, C) still "in progress" for P3. | Keep p2 as the stable guest target through 0.4. Add wasi:http through `wasmtime-wasi-http`, which carries both p2 and p3 modules. Classify ABIs at publish so p3 is additive. Do not invent a streaming ABI. |
| **wasmtime cadence** | A new major on the 20th of every month. LTS = version divisible by 12, supported 24 months. Others are supported 2 months. **48 is LTS; 49 (the pin) is not.** 49 support ends around 2026-11-20. The next LTS is 60, around 2027-08. | Through 0.x, follow the monthly train with a CI job that bumps it and runs the `.cwasm` recompile path. At 1.0, pin an LTS (60 if 1.0 lands after Aug 2027) and drop `pub use wasmtime` first. |
| **Guest toolchain churn** | ComponentizeJS is being rewritten (QuickJS-NG/wit-dylib); `wit-bindgen-go` is deprecated in favour of go-modules/componentize-go; componentize-dotnet is still a preview. | Pin toolchain versions in the templates. A CI matrix builds "hello" in each tier-1 language and uploads it to a host container. |
| **Single maintainer** | All work is serial. Roadmap effort assumes one person. | Strict YAGNI list (see roadmap). Prefer extension crates over core growth. Do not ship per-language native bindings. |
| **Unproven demand** | Extism's 5.8k stars show demand for plugin sandboxes. No public evidence that anyone wants a tenant-aware Component Model embedder; warpline itself has 0 stars. | Run a demand test before 0.4. Talk to 5–10 SaaS teams with customer hooks; track crates.io downloads and GHCR pulls. Gate 0.4+ scope on what they ask for. |
| Shared-process isolation | The 49.0.2 advisories include GC-heap corruption and native stack overflow classes. One escape exposes every tenant. | Track advisories within days (0.2.1 shows the process works). Add seccomp/Landlock and child-process compile before 1.0. Hyperlight stays post-1.0 (its own repo says it is not production-grade). |

## 8. Sources

All of these survived verification; I fetched the first group myself on 2026-10-05.

Verified directly:
- https://bytecodealliance.org/articles/WASI-0.3 (0.3.0 ratified 2026-06-11; Wasmtime 46 ships it with CM async on by default)
- https://wasi.dev/roadmap ; https://wasi.dev/releases/wasi-p3 (0.3.1 2026-08-11, 0.3.2 2026-10-13, two-month cadence, Wasmtime 46+)
- https://github.com/WebAssembly/WASI/blob/main/docs/Proposals.md (proposal phases)
- https://github.com/WebAssembly/wasi-keyvalue (Phase 2; last changelog entry 2024-03-29) ; https://github.com/WebAssembly/wasi-config ; https://github.com/WebAssembly/wasi-logging
- https://docs.wasmtime.dev/stability-release.html (monthly majors, LTS divisible by 12, 24 vs 2 months)
- https://github.com/bytecodealliance/wasmtime/releases/tag/v49.0.2 (2026-10-02, 8 GHSAs)
- https://github.com/rustsec/advisory-db (RUSTSEC-2026-0321..0324 under wasmtime-wasi, 0325..0327 under wasmtime)
- https://github.com/spinframework/spin/releases (v4.0.0 2026-04-20, v4.2.1 2026-09-30)
- https://github.com/wasmCloud/wasmCloud/releases (v2.10.3 2026-10-02) ; https://wasmcloud.com/community/2026-07-01-community-meeting/
- https://github.com/extism/extism (5,784 stars; v1.30.0 2026-06-04) ; https://raw.githubusercontent.com/extism/extism/main/runtime/Cargo.toml
- https://github.com/nginx/unit (archived; last push 2025-10-08)
- https://github.com/axodotdev/cargo-dist/releases/tag/v0.33.0 (2026-09-11 UTC)
- https://shopify.dev/docs/api/functions/latest ; https://shopify.dev/docs/apps/build/functions/network-access
- https://developers.cloudflare.com/cloudflare-for-platforms/workers-for-platforms/platform/pricing/
- https://orca.security/resources/blog/cve-2026-1470-n8n-rce-sandbox-escape/ ; https://thehackernews.com/2026/01/two-high-severity-n8n-flaws-allow.html ; https://research.jfrog.com/post/achieving-remote-code-execution-on-n8n-via-sandbox-escape/
- https://modal.com/blog/modal-series-c ; https://www.finsmes.com/2026/05/modal-raises-355m-in-series-c-funding-at-post-money-valuation-of-4-65-billion.html
- https://www.vestbee.com/insights/articles/e2-b-secures-21-m (E2B $21M Series A, Jul 2025)
- https://www.globenewswire.com/news-release/2025/12/01/3196978/0/en/Akamai-Technologies-Announces-Acquisition-of-Function-as-a-Service-Company-Fermyon.html

Used from the reports (primary pages, not contradicted):
- https://docs.wasmtime.dev/stability-tiers.html ; https://github.com/cargo-bins/cargo-binstall/blob/main/SUPPORT.md ; https://github.blog/changelog/2025-08-07-arm64-hosted-runners-for-public-repositories-are-now-generally-available/ ; https://github.com/actions/runner-images/issues/13046
- https://docs.rs/wasmtime-wasi-http/latest/wasmtime_wasi_http/ ; https://docs.wasmtime.dev/examples-fast-instantiation.html ; https://docs.wasmtime.dev/api/wasmtime/struct.PoolingAllocationConfig.html
- https://github.com/bytecodealliance/wasm-pkg-tools ; https://github.com/bytecodealliance/jco ; https://github.com/bytecodealliance/componentize-py ; https://github.com/bytecodealliance/go-modules ; https://github.com/bytecodealliance/componentize-dotnet ; https://wasmcloud.com/docs/wash/developer-guide/language-support/ ; https://wasmcloud.com/community/2026-03-18-community-meeting/
- https://spinframework.dev/v3/cli-reference ; https://spinframework.dev/v3/dynamic-configuration ; https://www.spinkube.dev/docs/topics/architecture/ ; https://wasmcloud.com/docs/kubernetes-operator/
- https://extism.org/docs/concepts/host-sdk/ ; https://extism.org/docs/concepts/manifest/
- https://developers.cloudflare.com/workers/versions-and-deployments/gradual-deployments/ ; https://blog.cloudflare.com/code-mode-mcp/
- https://opensource.microsoft.com/blog/2025/08/06/introducing-wassette-webassembly-based-tools-for-ai-agents/
- https://github.com/hyperlight-dev/hyperlight-wasm ; https://www.cncf.io/projects/hyperlight/
- https://www.salesforceben.com/salesforce-to-retire-functions-elastic-services/
- https://www.openpolicyagent.org/docs/wasm ; https://docs.redpanda.com/current/develop/data-transforms/
- https://docs.stripe.com/api/v2/billing-meter-stream
- https://github.com/WebAssembly/WASI/issues/646 (wasi:otel)

**Dropped as unverifiable or wrong:**
- Spin 4.0 date of 2026-06-15 (wrong).
- Wasmtime "CVE-2026-104855" (malformed ID).
- Dylibso headcount.
- Golem paid GA.
- Daytona $24M Series A.
- E2B "88% of Fortune 100" and "1B+ sandbox launches by Aug 2026" (secondary blog only).
- JS-component cold start of 1–4 s.
- Fastly Component Model status.

**Kept but marked unverified:**
- HubSpot custom-code limits.
- StarlingMonkey "about 8 MB" per component.
- The fuel overhead of "10–30%".
