---
title: Publishing warpline to crates.io
status: ready (not yet published)
date: 2026-09-28
---

# Publishing warpline to crates.io

## What gets published

| Crate              | Kind      | Depends on                     | Install / use                          |
| ------------------ | --------- | ------------------------------ | -------------------------------------- |
| `warpline-core`    | library   | —                              | `cargo add warpline-core`              |
| `warpline-control` | lib + bin | `warpline-core`                | `cargo install warpline-control`       |
| `warpline-host`    | lib + bin | `warpline-core`, `warpline-control` (embedded mode) | `cargo install warpline-host` |

`examples/hello-wasm` and `examples/test-guest` stay `publish = false`.
All four names (`warpline`, `warpline-core`, `warpline-host`,
`warpline-control`) were unclaimed on crates.io as of 2026-09-28.

## Done in preparation

- Shared metadata in `[workspace.package]`: `license = "MIT OR Apache-2.0"`,
  `repository`, `homepage`, `keywords`, `rust-version`. Each crate adds its
  own `description`, `categories`, and `readme = "../../README.md"`
  (crates.io resolves the README's relative links against `repository`);
  `warpline-core` sets `documentation` to docs.rs.
- `LICENSE-MIT` / `LICENSE-APACHE` at the root, symlinked into each crate so
  the tarballs ship both texts.
- `wit/` and `migrations/` moved into `crates/core/`: `bindgen!` and
  `sqlx::migrate!` read them at compile time, and a crates.io tarball only
  contains the crate's own directory. Migration checksums are
  content-based, so existing databases are unaffected by the move.
- Path dependencies carry `version = "0.1.0"`, so they resolve from the
  registry once published.
- No `sqlx::query!` macros, so docs.rs builds need neither a database nor
  offline query data.
- CI job `package` runs `cargo package --workspace`, which builds each
  crate from its extracted tarball — the same check `cargo publish` runs.

## Release checklist

1. `main` green in CI (including the `package` job).
2. Decide the version. 0.x signals an unstable API; `warpline-core`'s
   public surface (`invoke`, `HostCtx`, `registry`, `auth`) will still move.
   Bump `[workspace.package] version` **and** the three path-dep
   `version = "..."` entries together.
3. Update `PROGRESS.md` / README if the release changes behaviour.
4. Dry run, in dependency order:
   ```bash
   cargo package --workspace          # builds all three from tarballs
   cargo publish -p warpline-core --dry-run
   ```
   (`--dry-run` for `control`/`host` fails until `warpline-core` exists on
   the registry; `cargo package --workspace` covers them.)
5. Log in once: `cargo login` with a crates.io token scoped to
   `publish-new` + `publish-update` for `warpline*`.
6. Publish in order — each must be indexed before the next resolves it:
   ```bash
   cargo publish -p warpline-core
   cargo publish -p warpline-control
   cargo publish -p warpline-host
   ```
   (Recent cargo also accepts `cargo publish --workspace`, which orders and
   waits automatically.)
7. Tag and release: `git tag -a v0.1.0 -m "warpline 0.1.0" && git push origin v0.1.0`,
   then a GitHub release with the PROGRESS.md highlights.
8. After publishing: add crates.io / docs.rs badges and an "Install"
   section (`cargo install warpline-host warpline-control`) to the README;
   confirm the docs.rs build succeeded.

Publishing is permanent — a version can be yanked but never deleted or
re-uploaded. Double-check the version and metadata before step 6.

## Open decisions

- **Reserve `warpline`?** A tiny facade crate re-exporting
  `warpline-core` would hold the short name. Not done — a placeholder
  crate is squatting-adjacent; publish it only with real content.
- **Should `warpline-host` depend on `warpline-control` unconditionally?**
  It does today, for `WARPLINE_EMBED_CONTROL`. A cargo feature (default on)
  would let library users of `warpline_host` skip it. Low priority.
- **Owners.** Add a second crates.io owner (a GitHub team) if the project
  gains maintainers, so the crates can't be orphaned.
