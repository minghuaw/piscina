//! A simple generic pool that supports both sync and async.
//!
//! [![crate_version](https://img.shields.io/crates/v/piscina.svg?style=flat)](https://crates.io/crates/piscina)
//! [![docs_version](https://img.shields.io/badge/docs-latest-blue.svg?style=flat)](https://docs.rs/fe2o3-amqp/latest/piscina/)
//!
//! This crate provides a simple generic pool that uses channels instead of locks and supports both sync and async
//! without dependence on any runtime.
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
//! Non-async example:
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
//! drop(item1);
//! let item3 = pool.try_get();
//! assert!(item3.is_some());
//! ```
//!
//! Async example:
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
//!     drop(item1);
//!     let item3 = pool.get().await;
//! });
//! ```
//!
//! # Safety
//!
//! Instead of using an `Option`, this crate uses unsafe code `ManuallyDrop` in `PooledItem`.
//! And the only use of unsafe code `ManuallyDrop::take()` occurs when `PooledItem` is dropped.

#[macro_use]
mod cfg;

use std::{
    collections::VecDeque,
    future::Future,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll},
};

use futures_channel::oneshot::{Receiver, Sender};
use futures_util::FutureExt;
use pin_project_lite::pin_project;

/// An item that is borrowed from the [`Pool`]
///
/// Dropping this item will return it to the pool.
pub struct PooledItem<T> {
    /// The item borrowed from the pool
    item: ManuallyDrop<T>,

    /// Sender that places the item back into the pool
    sender: ManuallyDrop<Sender<T>>,
}

impl<T> Deref for PooledItem<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<T> DerefMut for PooledItem<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

impl<T> Drop for PooledItem<T> {
    fn drop(&mut self) {
        // # SAFETY:
        //
        // This is the only place where the ownerships of the item and sender
        // are taken and then transferred back to the pool, and thus the only
        // unsafe access to the memory.
        unsafe {
            let item = ManuallyDrop::take(&mut self.item);
            let sender = ManuallyDrop::take(&mut self.sender);

            if sender.send(item).is_err() {
                cfg_log! {
                    log::error!("Failed to return item to pool");
                }

                cfg_tracing! {
                    tracing::error!("Failed to return item to pool");
                }
            }
        }
    }
}

impl<T> PooledItem<T> {
    fn new(item: T, sender: Sender<T>) -> Self {
        Self {
            item: ManuallyDrop::new(item),
            sender: ManuallyDrop::new(sender),
        }
    }

    /// Get a reference to the item
    pub fn item(&self) -> &T {
        &self.item
    }

    /// Get a mutable reference to the item
    pub fn item_mut(&mut self) -> &mut T {
        &mut self.item
    }
}

/// Iterate through all borrowed items and try to receive them. If the sender
/// has been dropped, the item is no longer retrievable and is removed from
/// the pool.
fn try_recv_from_borrowed<T>(ready: &mut VecDeque<T>, borrowed: &mut Vec<Receiver<T>>) {
    borrowed.retain_mut(|rx| {
        match rx.try_recv() {
            Ok(Some(item)) => {
                ready.push_back(item);
                false
            }
            Ok(None) => true,
            Err(_) => {
                // Sender has been dropped, so the item is no longer
                // retrievable.
                cfg_log! {
                    log::error!("Item was dropped without being returned to pool. The pool might have been dropped.");
                }

                cfg_tracing! {
                    tracing::error!("Item was dropped without being returned to pool. The pool might have been dropped.");
                }
                false
            }
        }
    });
}

/// Iterate through all borrowed items and try to receive them. If the item is lost
/// (ie. the sender has been dropped before it returned the item), panic.
fn try_recv_from_borrowed_and_panic_if_lost<T>(
    ready: &mut VecDeque<T>,
    borrowed: &mut Vec<Receiver<T>>,
) {
    borrowed.retain_mut(|rx| match rx.try_recv() {
        Ok(Some(item)) => {
            ready.push_back(item);
            false
        },
        Ok(None) => true,
        Err(_) => {
            // Sender cannot be canceled, so this is unreachable.
            unreachable!("Sender has been dropped, so the item will no longer be returned back to the pool.")
        },
    });
}

/// Iterate and poll all borrowed items and retain the ones that are still pending
/// and push the ones that are ready back into the ready queue.
fn poll_all_borrowed_and_push_back_ready<T>(
    ready: &mut VecDeque<T>,
    borrowed: &mut Vec<Receiver<T>>,
    cx: &mut Context<'_>,
) {
    borrowed.retain_mut(|rx| match rx.poll_unpin(cx) {
        Poll::Ready(Ok(item)) => {
            ready.push_back(item);
            false
        },
        Poll::Ready(Err(_canceled)) => {
            // Sender cannot be canceled, so this is unreachable.
            unreachable!("Sender has been dropped, so the item will no longer be returned back to the pool.")
        }
        Poll::Pending => true,
    });
}

/// A pool of generic items
///
/// Internally it maintains two lists: one of ready items and one of borrowed
#[derive(Debug, Default)]
pub struct Pool<T> {
    /// List of ready items
    ready: VecDeque<T>,

    /// List of borrowed items
    borrowed: Vec<Receiver<T>>,
}

impl<T> Pool<T> {
    /// Creates an empty pool
    pub fn new() -> Self {
        Self {
            ready: VecDeque::new(),
            borrowed: Vec::new(),
        }
    }

    /// Creates an empty pool with the specified capacity for both the ready and borrowed lists.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ready: VecDeque::with_capacity(capacity),
            borrowed: Vec::with_capacity(capacity),
        }
    }

    /// Returns the number of all items, whether ready or borrowed.
    pub fn len(&self) -> usize {
        self.ready.len() + self.borrowed.len()
    }

    /// Returns `true` if the pool contains no items.
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.borrowed.is_empty()
    }

    /// Shrinks the capacity of both the ready and borrowed lists as much as
    /// possible.
    pub fn shrink_to_fit(&mut self) {
        self.ready.shrink_to_fit();
        self.borrowed.shrink_to_fit();
    }

    /// Tries to get an item from the pool. Returns `None` if there is no item immediately available.
    ///
    /// Please note that if the sender is dropped before the item is returned to the pool, the
    /// item will be lost and this will be reflected on the pool's length.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// let item = pool.try_get();
    /// assert!(item.is_some());
    /// ```
    pub fn try_get(&mut self) -> Option<PooledItem<T>> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        try_recv_from_borrowed(ready, borrowed);
        match self.ready.pop_front() {
            Some(item) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                self.borrowed.push(rx);
                Some(PooledItem::new(item, tx))
            }
            None => None,
        }
    }

    /// Gets an item from the pool. If there is no item immediately available, this will wait
    /// in a blocking manner until an item is available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// let item = pool.blocking_get();
    /// ```
    ///
    /// # Panic
    ///
    /// This will panic if the sender is dropped before the item is returned to the pool, which
    /// should never happen.
    pub fn blocking_get(&mut self) -> PooledItem<T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

        loop {
            try_recv_from_borrowed_and_panic_if_lost(ready, borrowed);

            match ready.pop_front() {
                Some(item) => {
                    let (tx, rx) = futures_channel::oneshot::channel();
                    borrowed.push(rx);
                    return PooledItem::new(item, tx);
                }
                None => std::thread::yield_now(),
            }
        }
    }

    /// Gets an item from the pool. If there is no item immediately available, the future
    /// will `.await` until one is available.
    ///
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// futures::executor::block_on(async {
    ///     let item = pool.get().await;
    /// });
    /// ```
    /// 
    /// # Panic
    ///
    /// This future will panic if any sender is dropped before the item is returned to the pool,
    /// which should never happen.
    pub fn get(&mut self) -> Get<'_, T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        Get { ready, borrowed }
    }

    /// Puts an item into the pool
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// ```
    pub fn put(&mut self, item: T) {
        self.ready.push_back(item);
    }

    /// Tries to pop an item from the pool. Returns `None` if there is no item immediately available.
    /// 
    /// Please note that if the sender is dropped before the item is returned to the pool, the
    /// item will be lost and this will be reflected on the pool's length.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// assert_eq!(pool.len(), 1);
    /// let item = pool.try_pop();
    /// assert!(item.is_some());
    /// assert_eq!(pool.len(), 0);
    /// ```
    pub fn try_pop(&mut self) -> Option<T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        try_recv_from_borrowed(ready, borrowed);
        self.ready.pop_front()
    }

    /// Pops an item from the pool. If there is no item immediately available, this will wait
    /// in a blocking manner until an item is available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// assert_eq!(pool.len(), 1);
    /// let item = pool.blocking_pop();
    /// assert_eq!(pool.len(), 0);
    /// ```
    /// 
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before the item is returned to the pool, which
    /// should never happen.
    pub fn blocking_pop(&mut self) -> T {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

        loop {
            try_recv_from_borrowed_and_panic_if_lost(ready, borrowed);

            match ready.pop_front() {
                Some(item) => return item,
                None => std::thread::yield_now(),
            }
        }
    }

    /// Pops an item from the pool. If there is no item immediately available, this will wait
    /// in a blocking manner until an item is available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::Pool;
    /// 
    /// let mut pool = Pool::new();
    /// pool.put(1);
    /// assert_eq!(pool.len(), 1);
    /// futures::executor::block_on(async {
    ///    let item = pool.pop().await;
    ///    assert_eq!(pool.len(), 0);
    /// });
    /// ```
    /// 
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before the item is returned to the pool, which
    /// should never happen.
    pub fn pop(&mut self) -> Pop<'_, T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        Pop { ready, borrowed }
    }
}

pin_project! {
    /// A future that resolves to a [`PooledItem`]
    ///
    /// # Panic
    ///
    /// This future will panic if the sender is dropped before the item is
    /// returned to the pool, which should never happen.
    pub struct Get<'a, T> {
        #[pin]
        ready: &'a mut VecDeque<T>,

        #[pin]
        borrowed: &'a mut Vec<Receiver<T>>,
    }
}

impl<'a, T> Future for Get<'a, T> {
    type Output = PooledItem<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let ready = &mut this.ready;
        let borrowed = &mut this.borrowed;

        poll_all_borrowed_and_push_back_ready(ready, borrowed, cx);

        match ready.pop_front() {
            Some(item) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                borrowed.push(rx);
                Poll::Ready(PooledItem::new(item, tx))
            }
            None => Poll::Pending,
        }
    }
}

pin_project! {
    pub struct Pop<'a, T> {
        #[pin]
        ready: &'a mut VecDeque<T>,

        #[pin]
        borrowed: &'a mut Vec<Receiver<T>>,
    }
}

impl<'a, T> Future for Pop<'a, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let ready = &mut this.ready;
        let borrowed = &mut this.borrowed;

        poll_all_borrowed_and_push_back_ready(ready, borrowed, cx);

        match ready.pop_front() {
            Some(item) => Poll::Ready(item),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_ready {
        ($fut:expr) => {{
            let mut fut = $fut;
            let pinned = std::pin::Pin::new(&mut fut);
            match futures_util::poll!(pinned) {
                std::task::Poll::Ready(val) => val,
                std::task::Poll::Pending => panic!("expected Ready, got Pending"),
            }
        }};
    }

    macro_rules! assert_pending {
        ($fut:expr) => {{
            let mut fut = $fut;
            let pinned = std::pin::Pin::new(&mut fut);
            match futures_util::poll!(pinned) {
                std::task::Poll::Ready(_) => panic!("expected Pending, got Ready"),
                std::task::Poll::Pending => {}
            }
        }};
    }

    macro_rules! assert_some {
        ($opt:expr) => {{
            match $opt {
                Some(val) => val,
                None => panic!("expected Some, got None"),
            }
        }};
    }

    macro_rules! assert_none {
        ($opt:expr) => {{
            match $opt {
                Some(_) => panic!("expected None, got Some"),
                None => {}
            }
        }};
    }

    #[test]
    fn create_pool_with_capacity() {
        let pool = Pool::<i32>::with_capacity(10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn pool_len_does_not_change_after_borrowing_item() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_eq!(pool.len(), 1);

        let item = assert_some!(pool.try_get());
        assert_eq!(pool.len(), 1);

        drop(item);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn try_get_from_empty_pool_returns_none() {
        let mut pool = Pool::<i32>::new();
        assert_none!(pool.try_get());
    }

    #[test]
    fn try_get_from_non_empty_pool_returns_some() {
        let mut pool = Pool::<i32>::new();
        pool.put(1);
        assert_some!(pool.try_get());
    }

    #[test]
    fn try_get_from_exhausted_pool_returns_none() {
        let mut pool = Pool::new();
        pool.put(1);
        let _item = assert_some!(pool.try_get());
        assert_none!(pool.try_get());
    }

    #[test]
    fn try_get_after_dropping_borrowed_item_returns_some() {
        let mut pool = Pool::new();
        pool.put(1);
        let item = assert_some!(pool.try_get());
        drop(item);
        assert_some!(pool.try_get());
    }

    #[test]
    fn try_get_after_dropping_borrowed_item_and_putting_new_item_returns_some() {
        let mut pool = Pool::new();
        assert_none!(pool.try_get());

        pool.put(1);
        let item = assert_some!(pool.try_get());
        assert_none!(pool.try_get());

        drop(item);
        assert_some!(pool.try_get());

        pool.put(2);
        assert_some!(pool.try_get());
    }

    #[test]
    fn blocking_get_from_non_empty_pool_returns_item() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_eq!(*pool.blocking_get(), 1);
    }

    #[test]
    fn try_pop_from_empty_pool_returns_none() {
        let mut pool = Pool::<i32>::new();
        assert_none!(pool.try_pop());
    }

    #[test]
    fn try_pop_reduce_len_by_one() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_eq!(pool.len(), 1);
        assert_some!(pool.try_pop());
        assert_eq!(pool.len(), 0);
    }

    #[futures_test::test]
    async fn poll_get_from_empty_pool_returns_pending() {
        let mut pool = Pool::<i32>::new();
        assert_pending!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_from_pool_of_one_item_returns_ready() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_ready!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_from_exhausted_pool_returns_pending() {
        let mut pool = Pool::new();
        pool.put(1);
        let _item = assert_ready!(pool.get());
        assert_pending!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_after_dropping_borrowed_item_returns_ready() {
        let mut pool = Pool::new();
        pool.put(1);
        let item = assert_ready!(pool.get());
        drop(item);
        assert_ready!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_after_dropping_borrowed_item_and_putting_new_item_returns_ready() {
        let mut pool = Pool::new();
        assert_pending!(pool.get());

        pool.put(1);
        let item = assert_ready!(pool.get());
        assert_pending!(pool.get());

        drop(item);
        assert_ready!(pool.get());

        pool.put(2);
        assert_ready!(pool.get());
    }

    #[futures_test::test]
    async fn poll_pop_from_empty_pool_returns_pending() {
        let mut pool = Pool::<i32>::new();
        assert_pending!(pool.pop());
    }

    #[futures_test::test]
    async fn poll_pop_reduce_len_by_one() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_eq!(pool.len(), 1);
        assert_ready!(pool.pop());
        assert_eq!(pool.len(), 0);
    }

    #[futures_test::test]
    async fn poll_pop_from_pool_of_one_item_returns_ready() {
        let mut pool = Pool::new();
        pool.put(1);
        assert_ready!(pool.pop());
    }

    #[futures_test::test]
    async fn poll_pop_from_exhausted_pool_returns_pending() {
        let mut pool = Pool::new();
        pool.put(1);
        let _item = assert_ready!(pool.pop());
        assert_pending!(pool.pop());
    }

    #[futures_test::test]
    async fn poll_pop_after_dropping_borrowed_item_returns_pending() {
        let mut pool = Pool::new();
        pool.put(1);
        let item = assert_ready!(pool.pop());
        drop(item);
        assert_pending!(pool.pop());
    }
}
