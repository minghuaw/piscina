#![deny(missing_docs, missing_debug_implementations)]
#![warn(clippy::unused_async)]

//! A simple generic pool that supports both sync and async.
//!
//! [![crate_version](https://img.shields.io/crates/v/piscina.svg?style=flat)](https://crates.io/crates/piscina)
//! [![docs_version](https://img.shields.io/badge/docs-latest-blue.svg?style=flat)](https://docs.rs/piscina/latest/piscina/)
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
//! Non-async [`Pool`] example:
//!
//! ```rust
//! use piscina::Pool;
//!
//! let mut pool = Pool::new();
//! pool.put(1);
//! pool.put(2);
//!
//! let item1 = pool.try_get();
//! assert!(item1.is_some());
//!
//! let item2 = pool.blocking_get();
//!
//! let none = pool.try_get();
//! assert!(none.is_none());
//!
//! drop(item1); // Return the item to the pool by dropping it
//! let item3 = pool.try_get();
//! assert!(item3.is_some());
//! ```
//!
//! Async [`Pool`] example:
//!
//! ```rust
//! use piscina::Pool;
//!
//! futures::executor::block_on(async {
//!     let mut pool = Pool::new();
//!     pool.put(1);
//!     pool.put(2);
//!
//!     let item1 = pool.get().await;
//!     let item2 = pool.get().await;
//!
//!     let none = pool.try_get();
//!     assert!(none.is_none());
//!
//!     drop(item1); // Return the item to the pool by dropping it
//!     let item3 = pool.get().await;
//! });
//! ```
//!
//! Non-async [`PriorityPool`] example:
//! 
//! ```
//! use piscina::PriorityPool;
//! 
//! let mut pool = PriorityPool::new();
//! pool.put("hello", 1);
//! pool.put("world", 2); // Larger value means higher priority.
//! 
//! // Get the highest priority item.
//! let item = pool.try_get();
//! assert!(item.is_some());
//! let item = item.unwrap();
//! assert_eq!(item.item(), &"world");
//! 
//! let another_item = pool.try_get();
//! assert!(another_item.is_some());
//! let another_item = another_item.unwrap();
//! assert_eq!(another_item.item(), &"hello");
//! 
//! // The pool is now empty.
//! let empty_item = pool.try_get();
//! assert!(empty_item.is_none());
//! 
//! drop(another_item); // Return the item back to the pool by dropping it.
//! let hello = pool.try_get();
//! assert!(hello.is_some());
//! assert_eq!(hello.unwrap().item(), &"hello");
//! ```
//! 
//! Async [`PriorityPool`] example:
//! 
//! ```
//! use piscina::PriorityPool;
//! 
//! let mut pool = PriorityPool::new();
//! pool.put("hello", 1);
//! pool.put("world", 2); // Larger value means higher priority.
//! 
//! futures::executor::block_on(async {
//!     // Get the highest priority item.
//!     let world = pool.get().await;
//!     assert_eq!(world.item(), &"world"); 
//!
//!     let hello = pool.get().await;
//!     assert_eq!(hello.item(), &"hello");
//! 
//!     // The pool is now empty.
//!     drop(hello); // Return the item back to the pool by dropping it.
//!     let item = pool.get().await;
//!     assert_eq!(item.item(), &"hello"); 
//! });
//! ```
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