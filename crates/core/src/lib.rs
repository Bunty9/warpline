//! warpline-core — runtime building blocks for the warpline multi-tenant
//! WASM function host.
//!
//! Modules:
//! - [`runtime`] — `Engine`/`Linker` builders, `wasmtime::component::bindgen!`
//!   host bindings for `wit/warpline.wit`, the epoch ticker, and
//!   [`runtime::invoke`].
//! - [`kv`] — [`kv::KvStore`] trait + in-memory implementation, scoped per
//!   tenant by [`types::HostCtx`].
//! - [`meter`] — Postgres-backed per-invocation metering writer.
//! - [`cache`] — content-hash `.cwasm` cache on local disk, now over
//!   `wasmtime::component::Component`.
//! - [`registry`] — pointer files mapping `(tenant, func)` to a content
//!   digest, plus the in-memory `Component` LRU built on top of `cache`.
//! - [`auth`] — bearer-token auth, per-tenant config, and the
//!   optional-Postgres bootstrap shared by `warpline-host` and
//!   `warpline-control`.
//! - [`types`] — [`types::HostCtx`] and [`types::TenantLimiter`], the
//!   per-invocation state attached to every `Store`.

pub mod auth;
pub mod cache;
pub mod kv;
pub mod meter;
pub mod registry;
pub mod runtime;
pub mod types;
