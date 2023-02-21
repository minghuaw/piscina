//! Yet another implementation of pool of generic items.
//! 
//! # Features
//! 
//! - `log`: Enables logging using the `log` crate
//! - `tracing`: Enables logging using the `tracing` crate
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

            if let Err(_) = sender.send(item) {
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
}

/// Loop through all borrowed items and try to receive them. If the sender
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

    /// Creates an empty pool with the specified capacity for both the ready and borrowed lists
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            ready: VecDeque::with_capacity(capacity),
            borrowed: Vec::with_capacity(capacity),
        }
    }

    /// Returns the number of ready and borrowed items. The first value is the
    /// number of ready items, and the second value is the number of borrowed
    /// items.
    pub fn len(&self) -> (usize, usize) {
        (self.ready.len(), self.borrowed.len())
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
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before the item is returned to the pool, which
    /// should never happen.
    pub fn blocking_get(&mut self) -> PooledItem<T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

        loop {
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

            match ready.pop_front() {
                Some(item) => {
                    let (tx, rx) = futures_channel::oneshot::channel();
                    borrowed.push(rx);
                    return PooledItem::new(item, tx);
                },
                None => std::thread::yield_now(),
            }
        }
    }

    /// Gets an item from the pool. If there is no item immediately available, the future
    /// will `.await` until one is available.
    ///
    /// # Panic
    ///
    /// This future will panic if any sender is dropped before the item is returned to the pool,
    /// which should never happen.
    pub fn get(&mut self) -> Get<'_, T> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        Get { 
            ready,
            borrowed,
        }
    }

    /// Puts an item into the pool
    pub fn put(&mut self, item: T) {
        self.ready.push_back(item);
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

        match ready.pop_front() {
            Some(item) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                borrowed.push(rx);
                return Poll::Ready(PooledItem::new(item, tx));
            }
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_ready {
        ($fut:expr) => {
            {
                let mut fut = $fut;
                let pinned = std::pin::Pin::new(&mut fut);
                match futures_util::poll!(pinned) {
                    std::task::Poll::Ready(val) => val,
                    std::task::Poll::Pending => panic!("expected Ready, got Pending"),
                }
            }
        };
    }
    
    macro_rules! assert_pending {
        ($fut:expr) => {
            {
                let mut fut = $fut;
                let pinned = std::pin::Pin::new(&mut fut);
                match futures_util::poll!(pinned) {
                    std::task::Poll::Ready(_) => panic!("expected Pending, got Ready"),
                    std::task::Poll::Pending => {}
                }
            }
        };
    }

    macro_rules! assert_some {
        ($opt:expr) => {
            {
                match $opt {
                    Some(val) => val,
                    None => panic!("expected Some, got None"),
                }
            }
        };
    }

    macro_rules! assert_none {
        ($opt:expr) => {
            {
                match $opt {
                    Some(_) => panic!("expected None, got Some"),
                    None => {}
                }
            }
        };
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
}