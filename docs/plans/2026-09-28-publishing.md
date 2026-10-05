---
title: Publishing warpline to crates.io
status: done — v0.1.0 published 2026-09-28 (manually); releases since 0.2.0 go through trusted publishing; prebuilt binaries and a GHCR image from 0.2.1
date: 2026-09-28
---

# Publishing warpline to crates.io

## What gets published

| Crate              | Kind      | Depends on                     | Install / use                          |
| ------------------ | --------- | ------------------------------ | -------------------------------------- |
| `warpline-core`    | library   | —                              | `cargo add warpline-core`              |
| `warpline-control` | lib + bin | `warpline-core`                | `cargo install warpline-control`, `cargo binstall warpline-control` |
| `warpline-host`    | lib + bin | `warpline-core`, `warpline-control` (embedded mode) | `cargo install warpline-host`, `cargo binstall warpline-host` |

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
- Path dependencies carry the workspace version (`version = "X.Y.Z"`), so they resolve from the
  registry once published.
- No `sqlx::query!` macros, so docs.rs builds need neither a database nor
  offline query data.
- CI job `package` runs `cargo package --workspace`, which builds each
  crate from its extracted tarball — the same check `cargo publish` runs.

## Release process (trusted publishing, since 0.2.0)

Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`. No long-lived
token exists: each crate has a crates.io trusted publisher configured for
repository `Bunty9/warpline`, workflow `release.yml`, environment `release`
(a GitHub environment that only allows `v*` tags).

1. `main` green in CI (including `package` and `example`).
2. Bump `[workspace.package] version` **and** the path-dep
   `version = "..."` entries in `crates/*/Cargo.toml` together.
3. Add a `## [X.Y.Z] - date` section to `CHANGELOG.md` (it becomes the
   GitHub release notes).
4. Release via a PR merged to `main`, then tag `main`: `git tag vX.Y.Z && git push origin vX.Y.Z`.
5. The workflow, in job order (a failure stops everything after it):
   - `preflight`: tag == workspace version, tag on `main`, changelog section
     extracted, `cargo package --workspace --locked`.
   - `build`: matrix of `x86_64-unknown-linux-gnu` (ubuntu-22.04),
     `aarch64-unknown-linux-gnu` (ubuntu-22.04-arm), `aarch64-apple-darwin`
     and `x86_64-apple-darwin` (both on macos-latest; the latter is
     cross-compiled). No build cache. Each job uploads
     `warpline-vX.Y.Z-<target>.tar.gz` (a `warpline-vX.Y.Z-<target>/`
     directory holding both binaries, both licenses and the README).
   - `publish` (environment `release`): exchanges the job's OIDC token for a
     short-lived crates.io token (`rust-lang/crates-io-auth-action`, revoked
     in its post step) and publishes only the crates not yet on crates.io, in
     dependency order (`cargo publish -p`, so a failed run can simply be
     re-run).
   - `release`: verifies all four archives, writes `SHA256SUMS`, attests the
     archives (`actions/attest-build-provenance`), creates or reuses a draft
     release, uploads with `--clobber`, checks the assets and publishes it.
   - `docker`: builds `Dockerfile.release` (prebuilt binaries on distroless
     `cc-debian12:nonroot`) for linux/amd64 and linux/arm64, pushes
     `ghcr.io/bunty9/warpline` with `X.Y.Z`, `X.Y`, `latest` and `sha-*`
     tags and attests the image.
   - Fly keeps building the `runtime-root` target of the root `Dockerfile`;
     there is no release image for it.
6. Confirm the docs.rs builds succeeded.

Publishing is permanent: a version can be yanked but never deleted or
re-uploaded. A failed run after publish (for example the GitHub release step)
must be finished by hand, never by re-tagging.

### Manual fallback

If the workflow itself is broken, publish from a laptop:

```bash
cargo package --workspace
cargo login                      # token scoped to publish-new + publish-update for warpline*
cargo publish --workspace        # or, in order: warpline-core, warpline-control, warpline-host
```

Then push the tag: the workflow skips crates already on crates.io and
builds, uploads and publishes the GitHub release as usual, so a run over a
manual publish only adds the release assets and image.

### Dry run

`workflow_dispatch` (input `dry_run`, default true) runs only `preflight`
(tag checks skipped, version read from Cargo metadata) and `build`, and keeps
the archives as workflow artifacts. `publish`, `release` and `docker` run on
tag pushes only; the `release` environment only allows `v*` tags, so a
dispatch can never publish. Use it to exercise the four-target build,
especially the cross-compiled `x86_64-apple-darwin` one (if `aws-lc-sys`
fails there, move that row to `macos-15-intel`).

### Prebuilt binaries and binstall

`[package.metadata.binstall]` in `warpline-host` and `warpline-control`
points at the release archive and the `bin-dir` inside it. It reaches
crates.io only with the next publish, so `cargo binstall` resolves archives
for 0.2.1 and later; older versions build from source. Between the crates.io
publish and the release being published (a few minutes) binstall falls back
to a source build. Smoke-check on the first release with
`cargo binstall --dry-run warpline-host`.

## Open decisions

- **Reserve `warpline`?** A tiny facade crate re-exporting
  `warpline-core` would hold the short name. Not done — a placeholder
  crate is squatting-adjacent; publish it only with real content.
- ~~Should `warpline-host` depend on `warpline-control` unconditionally?~~
  Done in 0.2.0: the `embed-control` cargo feature (default on).
- **Owners.** Add a second crates.io owner (a GitHub team) if the project
  gains maintainers, so the crates can't be orphaned.
