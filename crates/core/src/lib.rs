//! warpline-core — runtime building blocks for the warpline multi-tenant
//! WASM function host.
//!
//! Modules:
//! - [`runtime`] — `Engine` builder, [`runtime::invoke`], host-import
//!   registration.
//! - [`kv`] — [`kv::KvStore`] trait + in-memory implementation, scoped per
//!   tenant by [`runtime::HostCtx`].
//! - [`meter`] — Postgres-backed per-invocation metering writer.
//! - [`cache`] — content-hash `.cwasm` cache on local disk.
//! - [`types`] — host-side request/response envelopes mirroring
//!   `wit/warpline.wit`.

pub mod cache;
pub mod kv;
pub mod meter;
pub mod runtime;
pub mod types;
