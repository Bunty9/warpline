---
title: Publishing warpline to crates.io
status: done — v0.1.0 published 2026-09-28 (manually); releases since 0.2.0 go through trusted publishing
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
5. The workflow: verifies tag == workspace version, extracts the changelog
   section, runs `cargo package --workspace --locked`, exchanges the job's OIDC token
   for a short-lived crates.io token (`rust-lang/crates-io-auth-action`, revoked
   in its post step), publishes only the crates not yet on crates.io, in dependency order
   (`cargo publish -p`, so a failed run can simply be re-run) and creates the GitHub release.
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

Then tag and release without re-running the publish: create the tag on
GitHub with `gh release create vX.Y.Z --notes-file <changelog section>`
after the crates are out. (Pushing the tag also triggers the workflow: it skips crates already on
crates.io and skips the GitHub release if one exists, so a re-run over a
manual publish is a no-op.)

## Open decisions

- **Reserve `warpline`?** A tiny facade crate re-exporting
  `warpline-core` would hold the short name. Not done — a placeholder
  crate is squatting-adjacent; publish it only with real content.
- ~~Should `warpline-host` depend on `warpline-control` unconditionally?~~
  Done in 0.2.0: the `embed-control` cargo feature (default on).
- **Owners.** Add a second crates.io owner (a GitHub team) if the project
  gains maintainers, so the crates can't be orphaned.
