//! Priority pool.

use std::{collections::BinaryHeap, mem::ManuallyDrop, ops::{Deref, DerefMut}, future::Future, task::{Poll, Context}, pin::Pin};

use futures_channel::oneshot::{Sender, Receiver};
use pin_project_lite::pin_project;

use crate::internal::{try_recv_from_borrowed, try_recv_from_borrowed_and_panic_if_lost, poll_all_borrowed_and_push_back_ready};

/// An item that is borrowed from a [`PriorityPool`].
pub struct PooledItem<T, P> {
    /// The item.
    item: ManuallyDrop<T>,

    /// The priority of the item.
    priority: ManuallyDrop<P>,

    /// The sender that is used to return the item to the pool.
    sender: ManuallyDrop<Sender<PriorityItem<T, P>>>,
}

impl<T, P> Deref for PooledItem<T, P> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<T, P> DerefMut for PooledItem<T, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.item
    }
}

impl<T, P> Drop for PooledItem<T, P> {
    fn drop(&mut self) {
        // # SAFETY:
        //
        // This is the only place where we are taking the ownership of the
        // priority, item, and sender. We are then sending the priority and item
        // back to the pool.
        unsafe {
            let priority = ManuallyDrop::take(&mut self.priority);
            let item = ManuallyDrop::take(&mut self.item);
            let sender = ManuallyDrop::take(&mut self.sender);

            let value = PriorityItem::new(item, priority);
            if sender.send(value).is_err() {
                cfg_log! {
                    log::error!("PooledItem was dropped without being returned to pool. The pool might have been dropped.");
                }

                cfg_tracing! {
                    tracing::error!("PooledItem was dropped without being returned to pool. The pool might have been dropped.");
                }
            }
        }
    }
}

impl<T, P> PooledItem<T, P> {
    fn new(item: T, priority: P, sender: Sender<PriorityItem<T, P>>) -> Self {
        Self { 
            item: ManuallyDrop::new(item),
            priority: ManuallyDrop::new(priority), 
            sender: ManuallyDrop::new(sender),
        }
    }
}

struct PriorityItem<T, P> {
    priority: P,
    item: T,
}

impl<T, P> PartialEq for PriorityItem<T, P>
where
    P: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.priority.eq(&other.priority)
    }
}

impl<T, P> Eq for PriorityItem<T, P>
where
    P: Eq,
{ }

impl<T, P> PartialOrd for PriorityItem<T, P>
where
    P: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.priority.partial_cmp(&other.priority)
    }
}

impl<T, P> Ord for PriorityItem<T, P>
where
    P: Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

impl<T, P> PriorityItem<T, P> {
    fn new(item: T, priority: P) -> Self {
        Self { item, priority }
    }
}

/// A pool that can be used to borrow items with a priority.
/// 
/// # Type parameters
/// 
/// * `T`: The type of the items.
/// * `P`: The type of the priority.
pub struct PriorityPool<T, P> 
where
    P: Ord,
{
    ready: BinaryHeap<PriorityItem<T, P>>,
    borrowed: Vec<Receiver<PriorityItem<T, P>>>,
}

impl<T, P> PriorityPool<T, P> 
where
    P: Ord,
{
    /// Creates a new [`PriorityPool`].
    pub fn new() -> Self {
        Self { ready: BinaryHeap::new(), borrowed: Vec::new() }
    }

    /// Creates a new [`PriorityPool`] with the given capacity that is used for both the ready and borrowed queues.
    pub fn with_capacity(capacity: usize) -> Self {
        Self { ready: BinaryHeap::with_capacity(capacity), borrowed: Vec::with_capacity(capacity) }
    }

    /// Returns the number of items in the pool.
    pub fn len(&self) -> usize {
        self.ready.len() + self.borrowed.len()
    }

    /// Returns `true` if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.borrowed.is_empty()
    }

    /// Shrinks the capacity of the pool as much as possible.
    pub fn shrink_to_fit(&mut self) {
        self.ready.shrink_to_fit();
        self.borrowed.shrink_to_fit();
    }

    /// Puts an item `T` with the given priority `P` into the pool.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// ```
    pub fn put(&mut self, item: T, priority: P) {
        self.ready.push(PriorityItem::new(item, priority));
    }

    pub fn try_get(&mut self) -> Option<PooledItem<T, P>> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        try_recv_from_borrowed(ready, borrowed);
        match ready.pop() {
            Some(PriorityItem { priority, item }) => {
                let (sender, receiver) = futures_channel::oneshot::channel();
                borrowed.push(receiver);
                Some(PooledItem::new(item, priority, sender))
            },
            None => None,
        }
    }

    pub fn blocking_get(&mut self) -> PooledItem<T, P> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

        loop {
            try_recv_from_borrowed_and_panic_if_lost(ready, borrowed);

            match ready.pop() {
                Some(PriorityItem { item, priority }) => {
                    let (tx, rx) = futures_channel::oneshot::channel();
                    borrowed.push(rx);
                    return PooledItem::new(item, priority, tx);
                },
                None => std::thread::yield_now(),
            }
        }
    }

    pub fn get(&mut self) -> Get<'_, T, P> {
        Get { ready: &mut self.ready, borrowed: &mut self.borrowed }
    }

    pub fn try_pop(&mut self) -> Option<(T, P)> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        try_recv_from_borrowed(ready, borrowed);
        ready.pop().map(|PriorityItem { item, priority }| (item, priority))
    }

    pub fn blocking_pop(&mut self) -> (T, P) {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;

        loop {
            try_recv_from_borrowed_and_panic_if_lost(ready, borrowed);

            match ready.pop() {
                Some(PriorityItem { item, priority }) => return (item, priority),
                None => std::thread::yield_now(),
            }
        }
    }

    pub fn pop(&mut self) -> Pop<'_, T, P> {
        Pop { ready: &mut self.ready, borrowed: &mut self.borrowed }
    }
}

pin_project! {
    pub struct Get<'a, T, P> {
        #[pin]
        ready: &'a mut BinaryHeap<PriorityItem<T, P>>,
        #[pin]
        borrowed: &'a mut Vec<Receiver<PriorityItem<T, P>>>,
    }
}

impl<'a, T, P> Future for Get<'a, T, P> 
where
    P: Ord,
{
    type Output = PooledItem<T, P>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let ready = &mut this.ready;
        let borrowed = &mut this.borrowed;

        poll_all_borrowed_and_push_back_ready(**ready, borrowed, cx);

        match ready.pop() {
            Some(PriorityItem { item, priority }) => {
                let (tx, rx) = futures_channel::oneshot::channel();
                borrowed.push(rx);
                Poll::Ready(PooledItem::new(item, priority, tx))
            },
            None => Poll::Pending,
        }
    }
}

pin_project! {
    pub struct Pop<'a, T, P> {
        #[pin]
        ready: &'a mut BinaryHeap<PriorityItem<T, P>>,
        #[pin]
        borrowed: &'a mut Vec<Receiver<PriorityItem<T, P>>>,
    }
}

impl<'a, T, P> Future for Pop<'a, T, P>
where
    P: Ord,
{
    type Output = (T, P);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        let ready = &mut this.ready;
        let borrowed = &mut this.borrowed;

        poll_all_borrowed_and_push_back_ready(**ready, borrowed, cx);

        match ready.pop() {
            Some(PriorityItem { item, priority, .. }) => Poll::Ready((item, priority)),
            None => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    #[test]
    fn create_priority_pool_with_capacity() {
        let pool = PriorityPool::<String, u32>::with_capacity(10);
        assert_eq!(pool.len(), 0);
    }

}