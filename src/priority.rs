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

impl<T, P> std::fmt::Debug for PooledItem<T, P> 
where
    T: std::fmt::Debug,
    P: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledItem")
            .field("item", &self.item)
            .field("priority", &self.priority)
            .finish()
    }
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

    /// Returns a reference to the item.
    pub fn item(&self) -> &T {
        &self.item
    }

    /// Returns a mutable reference to the item.
    pub fn item_mut(&mut self) -> &mut T {
        &mut self.item
    }

    /// Returns a reference to the priority.
    pub fn priority(&self) -> &P {
        &self.priority
    }

    /// Returns a mutable reference to the priority.
    pub fn priority_mut(&mut self) -> &mut P {
        &mut self.priority
    }
}

#[derive(Debug)]
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
/// This internally uses a [`BinaryHeap`] which is a max-heap. 
/// This means that the item with the highest priority will be returned first.
/// 
/// # Type parameters
/// 
/// * `T`: The type of the items.
/// * `P`: The type of the priority.
#[derive(Debug)]
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

    /// Tries to get the highest priority item that is immediately available from the
    /// [`PriorityPool`]. Returns `None` if the pool is empty.
    /// 
    /// Please note that if the sender is dropped before returning the item to the pool, the item
    /// will be lost and this will be reflected in the pool's length.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// let item = pool.try_get();
    /// assert!(item.is_some());
    /// ```
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

    /// Gets the highest priority item that is available from the [`PriorityPool`]. If there is no
    /// item immediately available, this will wait in a blocking manner until an item is available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// let item = pool.blocking_get();
    /// ```
    /// 
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before returning the item to the pool, which should
    /// never happen.
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

    /// Gets the highest priority item that is available from the [`PriorityPool`]. If there is no
    /// item immediately available, this will `.await` until one becomes available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// futures::executor::block_on(async {
    ///    let item = pool.get().await;
    /// });
    /// ```
    /// 
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before returning the item to the pool, which should
    /// never happen.
    pub fn get(&mut self) -> Get<'_, T, P> {
        Get { ready: &mut self.ready, borrowed: &mut self.borrowed }
    }

    /// Tries to remove the highest priority item that is immediately available from the
    /// [`PriorityPool`]. Returns `None` if the pool is empty.
    /// 
    /// Please note that if the sender is dropped before returning the item to the pool, the item
    /// will be lost and this will be reflected in the pool's length.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// assert_eq!(pool.len(), 1);
    /// 
    /// let item = pool.try_pop();
    /// assert!(item.is_some());
    /// assert_eq!(pool.len(), 0);
    /// ```
    pub fn try_pop(&mut self) -> Option<(T, P)> {
        let ready = &mut self.ready;
        let borrowed = &mut self.borrowed;
        try_recv_from_borrowed(ready, borrowed);
        ready.pop().map(|PriorityItem { item, priority }| (item, priority))
    }

    /// Removes the highest priority item that is available from the [`PriorityPool`]. If there is
    /// no item immediately available, this will wait in a blocking manner until an item is
    /// available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    /// 
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// assert_eq!(pool.len(), 1);
    /// 
    /// let item = pool.blocking_pop();
    /// assert_eq!(pool.len(), 0);
    /// ```
    /// 
    /// # Panic
    /// 
    /// This will panic if the sender is dropped before returning the item to the pool, which should
    /// never happen.
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

    /// Removes the highest priority item that is available from the [`PriorityPool`]. If there is
    /// no item immediately available, this will `.await` until one becomes available.
    /// 
    /// # Example
    /// 
    /// ```
    /// use piscina::PriorityPool;
    ///   
    /// let mut pool = PriorityPool::new();
    /// pool.put("hello", 1);
    /// assert_eq!(pool.len(), 1);
    /// 
    /// futures::executor::block_on(async {
    ///    let item = pool.pop().await;
    ///    assert_eq!(pool.len(), 0);
    /// });
    /// ```
    pub fn pop(&mut self) -> Pop<'_, T, P> {
        Pop { ready: &mut self.ready, borrowed: &mut self.borrowed }
    }
}

pin_project! {
    /// A future that resolves to a [`PooledItem`] from a [`PriorityPool`].
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
    /// A future that resolves to a removed item from a [`PriorityPool`].
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
    fn create_priority_pool() {
        let pool = PriorityPool::<String, u32>::new();
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn create_priority_pool_with_capacity() {
        let pool = PriorityPool::<String, u32>::with_capacity(10);
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn pool_len_does_not_change_after_borrowing_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        assert_eq!(pool.len(), 1);
        let _item = pool.blocking_get();
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn try_get_from_empty_pool_returns_none() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_none!(pool.try_get());
    }

    #[test]
    fn try_get_from_non_empty_pool_returns_some() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        assert_some!(pool.try_get());
    }

    #[test]
    fn try_get_after_dropping_borrowed_item_returns_some() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = pool.blocking_get();
        drop(item);
        assert_some!(pool.try_get());
    }

    #[test]
    fn try_get_after_dropping_borrowed_item_and_putting_new_item_returns_some() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_none!(pool.try_get());
        
        pool.put("hello", 1);
        let item = assert_some!(pool.try_get());
        assert_none!(pool.try_get());

        drop(item);
        assert_some!(pool.try_get());

        pool.put("world", 1);
        assert_some!(pool.try_get());
    }

    #[test]
    fn try_get_returns_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"world");

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"hello");

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"foo");

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"bar");
    }

    #[test]
    fn try_get_after_dropping_borrowed_item_returns_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let world = assert_some!(pool.try_get());
        assert_eq!(world.item(), &"world");

        let hello = assert_some!(pool.try_get());
        assert_eq!(hello.item(), &"hello");

        drop(world);
        let next_item = assert_some!(pool.try_get());
        assert_eq!(next_item.item(), &"world");

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"foo");

        drop(hello);
        let next_item = assert_some!(pool.try_get());
        assert_eq!(next_item.item(), &"hello");

        let item = assert_some!(pool.try_get());
        assert_eq!(item.item(), &"bar");
    }

    #[test]
    fn blocking_get_from_non_empty_pool_returns_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = pool.blocking_get();
        assert_eq!(item.item(), &"hello");
    }

    #[test]
    fn try_pop_from_empty_pool_returns_none() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_none!(pool.try_pop());
    }

    #[test]
    fn try_pop_reduce_len_by_one() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        assert_some!(pool.try_pop());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn try_pop_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "world");

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "hello");

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "foo");

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "bar");
    }

    #[test]
    fn try_pop_after_dropping_borrowed_item_returns_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let world = assert_some!(pool.try_get());
        assert_eq!(world.item(), &"world");

        let hello = assert_some!(pool.try_get());
        assert_eq!(hello.item(), &"hello");

        drop(world);
        let (next_item, _) = assert_some!(pool.try_pop());
        assert_eq!(next_item, "world");

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "foo");

        drop(hello);
        let (next_item, _) = assert_some!(pool.try_pop());
        assert_eq!(next_item, "hello");

        let (item, _) = assert_some!(pool.try_pop());
        assert_eq!(item, "bar");
    }
    
    #[futures_test::test]
    async fn poll_get_from_empty_pool_returns_pending() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_pending!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_from_pool_of_one_item_returns_ready() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"hello");
    }

    #[futures_test::test]
    async fn poll_get_from_exhausted_pool_returns_pending() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"hello");
        assert_pending!(pool.get());
    }

    #[futures_test::test]
    async fn poll_get_after_dropping_borrowed_item_returns_ready() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = assert_ready!(pool.get());
        drop(item);
        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"hello");
    }

    #[futures_test::test]
    async fn poll_get_after_dropping_borrowed_item_and_putting_new_item_returns_ready() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_pending!(pool.get());

        pool.put("hello", 1);
        let item = assert_ready!(pool.get());
        assert_pending!(pool.get());

        drop(item);
        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"hello");

        pool.put("world", 1);
        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"world");
    }

    #[futures_test::test]
    async fn poll_get_returns_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"world");

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"hello");

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"foo");

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"bar");
    }

    #[futures_test::test]
    async fn poll_get_after_dropping_borrowed_item_returns_highest_priority_item() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 5);
        pool.put("world", 6);
        pool.put("foo", 4);
        pool.put("bar", 3);

        let world = assert_ready!(pool.get());
        assert_eq!(world.item(), &"world");

        let hello = assert_ready!(pool.get());
        assert_eq!(hello.item(), &"hello");

        drop(world);
        let next_item = assert_ready!(pool.get());
        assert_eq!(next_item.item(), &"world");

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"foo");

        drop(hello);
        let next_item = assert_ready!(pool.get());
        assert_eq!(next_item.item(), &"hello");

        let item = assert_ready!(pool.get());
        assert_eq!(item.item(), &"bar");
    }

    #[futures_test::test]
    async fn poll_pop_from_empty_pool_returns_pending() {
        let mut pool = PriorityPool::<&str, u32>::new();
        assert_pending!(pool.pop());
    }

    #[futures_test::test]
    async fn poll_pop_from_pool_of_one_item_returns_ready() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let (item, _) = assert_ready!(pool.pop());
        assert_eq!(item, "hello");
    }

    #[futures_test::test]
    async fn poll_pop_reduce_len_by_one() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let (item, _) = assert_ready!(pool.pop());
        assert_eq!(item, "hello");
        assert_eq!(pool.len(), 0);
    }

    #[futures_test::test]
    async fn poll_pop_from_exhausted_pool_returns_pending() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let (item, _) = assert_ready!(pool.pop());
        assert_eq!(item, "hello");
        assert_pending!(pool.pop());
    }

    #[futures_test::test]
    async fn poll_pop_after_dropping_borrowed_item_returns_ready() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let item = assert_ready!(pool.get());
        drop(item);
        let (item, _) = assert_ready!(pool.pop());
        assert_eq!(item, "hello");
    }

    #[futures_test::test]
    async fn poll_pop_after_dropping_removed_item_returns_pending() {
        let mut pool = PriorityPool::<&str, u32>::new();
        pool.put("hello", 1);
        let (item, _) = assert_ready!(pool.pop());
        assert_eq!(item, "hello");
        drop(item);
        assert_pending!(pool.pop());
    }
}