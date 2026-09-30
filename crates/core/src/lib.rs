//! warpline-core — a multi-tenant WebAssembly function runtime you embed in
//! your own app.
//!
//! Hold a [`Runtime`], [`publish`](Runtime::publish) components under
//! `(tenant, function)` names and [`invoke`](Runtime::invoke) them with
//! per-call [`Limits`]. Guests get a tenant-scoped [`KvStore`], logging and
//! allowlisted outbound HTTP; CPU (epoch ticks), memory, output size,
//! per-tenant concurrency and total memory are all capped, and every
//! invocation that reaches a guest is reported to a [`MeterSink`].
//!
//! - [`runtime`] — the [`Runtime`] facade, its config and builder.
//! - [`kv`] — the [`KvStore`] trait and the in-memory [`MemKv`].
//! - [`meter`] — [`Usage`] and the [`MeterSink`] trait.
//! - [`pg`] (feature `postgres`, on by default) — schema-isolated migrations,
//!   bearer-token auth, tenant admin and a batching [`MeterSink`]. Nothing in
//!   this crate reads environment variables or migrates implicitly.
//!
//! For a complete integration (an axum app, a Rust guest, Postgres auth and
//! metering) see [`examples/storefront`](https://github.com/Bunty9/warpline/tree/main/examples/storefront).
//! - [`types`] — [`Limits`], name validation and config validators.

#![warn(missing_debug_implementations)]

mod cache;
mod error;
pub mod kv;
pub mod meter;
#[cfg(feature = "postgres")]
pub mod pg;
mod registry;
pub mod runtime;
mod sandbox;
pub mod types;

pub use cache::digest;
pub use error::{Error, InvokeError, PublishError};
pub use kv::{KvError, KvStore, MemKv};
pub use meter::{MeterSink, Usage};
pub use registry::GC_GRACE_PERIOD;
pub use runtime::{Invocation, Runtime, RuntimeBuilder, RuntimeConfig, Staged};
pub use types::{valid_name, ConfigError, Limits};

pub use bytes::Bytes;
pub use wasmtime;
