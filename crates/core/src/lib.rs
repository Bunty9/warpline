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
//! - [`types`] — [`types::HostCtx`] and [`types::TenantLimiter`], the
//!   per-invocation state attached to every `Store`.

pub mod cache;
pub mod kv;
pub mod meter;
pub mod runtime;
pub mod types;
