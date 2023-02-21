//! Yet another implementation of pool of generic items

#[macro_use]
mod cfg;

use std::{
    collections::VecDeque,
    future::Future,
    marker::PhantomData,
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
pub struct PooledItem<'a, T> {
    /// The item borrowed from the pool
    item: ManuallyDrop<T>,

    /// Sender that places the item back into the pool
    sender: ManuallyDrop<Sender<T>>,

    /// lifetime marker
    _lt: PhantomData<&'a ()>,
}

impl<'a, T> Deref for PooledItem<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<'a, T> DerefMut for PooledItem<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

impl<'a, T> Drop for PooledItem<'a, T> {
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

impl<'a, T> PooledItem<'a, T> {
    fn new(item: T, sender: Sender<T>) -> Self {
        Self {
            item: ManuallyDrop::new(item),
            sender: ManuallyDrop::new(sender),
            _lt: PhantomData,
        }
    }
}

/// A pool of generic items
///
/// Internally it maintains two lists: one of ready items and one of borrowed
#[derive(Debug, Default)]
pub struct Pool<T> {
    ready: VecDeque<T>,

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

    /// Loop through all borrowed items and try to receive them. If the sender
    /// has been dropped, the item is no longer retrievable and is removed from
    /// the pool.
    fn try_recv_from_borrowed(&mut self) {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

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
                        log::error!("Item was dropped without being returned to pool");
                    }

                    cfg_tracing! {
                        tracing::error!("Item was dropped without being returned to pool");
                    }
                    false
                }
            }
        });
    }

    /// Tries to get an item from the pool. Returns `None` if there is no item immediately available.
    /// 
    /// Please note that if the sender is dropped before the item is returned to the pool, the
    /// item will be lost and this will be reflected on the pool's length.
    pub fn try_get(&mut self) -> Option<PooledItem<'_, T>> {
        self.try_recv_from_borrowed();
        match self.ready.pop_front() {
            Some(item) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                self.borrowed.push(rx);
                Some(PooledItem::<'_, T>::new(item, tx))
            }
            None => None,
        }
    }

    /// Gets an item from the pool. If there is no item immediately available, the future
    /// will `.await` until one is available.
    ///
    /// # Panic
    ///
    /// This future will panic if any sender is dropped before the item is returned to the pool,
    /// which should not happen.
    pub fn get(&mut self) -> Get<'_, T> {
        Get { pool: self }
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
    /// returned to the pool, which should not happen.
    pub struct Get<'a, T> {
        #[pin]
        pool: &'a mut Pool<T>,
    }
}

impl<'a, T> Future for Get<'a, T> {
    type Output = PooledItem<'a, T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        this.pool.try_recv_from_borrowed();
        if let Some(item) = this.pool.ready.pop_front() {
            let (tx, rx) = futures_channel::oneshot::channel();
            this.pool.borrowed.push(rx);
            return Poll::Ready(PooledItem::<'a, T>::new(item, tx));
        }

        let mut ret = None;
        this.pool.borrowed.retain_mut(|rx| match rx.poll_unpin(cx) {
            Poll::Ready(Ok(item)) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                ret = Some((PooledItem::<'a, T>::new(item, tx), rx));
                false
            },
            Poll::Ready(Err(_canceled)) => {
                // Sender cannot be canceled, so this is unreachable.
                unreachable!("Sender has been dropped, so the item will no longer be returned back to the pool.")
            }
            Poll::Pending => true,
        });

        match ret {
            Some((item, rx)) => {
                this.pool.borrowed.push(rx);
                Poll::Ready(item)
            }
            None => Poll::Pending,
        }
    }
}
