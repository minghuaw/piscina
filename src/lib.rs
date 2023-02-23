#![deny(missing_docs, missing_debug_implementations)]
#![warn(clippy::unused_async)]

//! A simple generic pool that supports both sync and async.
//!
//! [![crate_version](https://img.shields.io/crates/v/piscina.svg?style=flat)](https://crates.io/crates/piscina)
//! [![docs_version](https://img.shields.io/badge/docs-latest-blue.svg?style=flat)](https://docs.rs/fe2o3-amqp/latest/piscina/)
//!
//! Two types of pools are provided:
//! 
//! - [`Pool`]: A simple pool that supports both sync and async.
//! - [`PriorityPool`]: A pool of items with priorities and supports both sync and async.
//! 
//! This crate internally utilizes `futures_channel::oneshot` and is thus lock-free.
//!
//! # Features
//!
//! ```toml
//! default = []
//! ```
//!
//! The features below are related to logging errors, which should be unreachable.
//!
//! - `log`: Enables logging using the `log` crate.
//! - `tracing`: Enables logging using the `tracing` crate.
//!
//! # Examples
//!
//! Please see [`Pool`] and [`PriorityPool`] for examples.
//!
//! # Safety
//!
//! Instead of using an `Option`, this crate uses unsafe code `ManuallyDrop` in `PooledItem`.
//! And the only use of unsafe code `ManuallyDrop::take()` occurs when `PooledItem` is dropped.

#[macro_use]
mod cfg;

pub(crate) mod internal;

pub mod pool;
pub mod priority;

pub use pool::Pool;
pub use priority::PriorityPool;

#[cfg(test)]
pub(crate) mod test_util;