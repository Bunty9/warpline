//! warpline-core — runtime building blocks for the warpline multi-tenant
//! WASM function host.
//!
//! Modules:
//! - [`runtime`] — `Engine`/`Linker` builders, `wasmtime::component::bindgen!`
//!   host bindings for `crates/core/wit/warpline.wit`, the epoch ticker, and
//!   [`runtime::invoke`].
//! - [`kv`] — [`kv::KvStore`] trait + in-memory implementation, scoped per
//!   tenant by [`types::HostCtx`].
//! - [`meter`] — [`Usage`] and the [`MeterSink`] trait invocations report to.
//! - [`cache`] — content-hash `.cwasm` cache on local disk, now over
//!   `wasmtime::component::Component`.
//! - [`registry`] — pointer files mapping `(tenant, func)` to a content
//!   digest, plus the in-memory `Component` LRU built on top of `cache`.
//! - `pg` (feature `postgres`, on by default) — everything Postgres:
//!   schema-isolated migrations, bearer-token auth, tenant admin and the
//!   batching [`MeterSink`]. Nothing in this crate reads environment
//!   variables or migrates implicitly.
//! - [`types`] — [`types::HostCtx`] and [`types::TenantLimiter`], the
//!   per-invocation state attached to every `Store`.

#![warn(missing_debug_implementations)]

pub mod cache;
pub mod kv;
pub mod meter;
#[cfg(feature = "postgres")]
pub mod pg;
pub mod registry;
pub mod runtime;
pub mod types;

pub use meter::{MeterSink, Usage};
pub use types::Limits;
