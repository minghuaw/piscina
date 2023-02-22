use std::{collections::{VecDeque, BinaryHeap}, task::{Context, Poll}};

use futures_util::FutureExt;
use futures_channel::oneshot::Receiver;

pub trait PoolStore {
    type Item;

    fn push(&mut self, item: Self::Item);
    fn pop(&mut self) -> Option<Self::Item>;
}

impl<T> PoolStore for VecDeque<T> {
    type Item = T;

    fn push(&mut self, item: Self::Item) {
        self.push_back(item)
    }

    fn pop(&mut self) -> Option<Self::Item> {
        self.pop_front()
    }
}

impl<T> PoolStore for BinaryHeap<T>
where
    T: Ord,
{
    type Item = T;

    fn push(&mut self, item: Self::Item) {
        self.push(item)
    }

    fn pop(&mut self) -> Option<Self::Item> {
        self.pop()
    }
}

/// Iterate through all borrowed items and try to receive them. If the sender
/// has been dropped, the item is no longer retrievable and is removed from
/// the pool.
pub(crate) fn try_recv_from_borrowed<P>(ready: &mut P, borrowed: &mut Vec<Receiver<P::Item>>) 
where
    P: PoolStore,
{
    borrowed.retain_mut(|rx| {
        match rx.try_recv() {
            Ok(Some(item)) => {
                ready.push(item);
                false
            }
            Ok(None) => true,
            Err(_) => {
                // Sender has been dropped, so the item is no longer
                // retrievable.
                cfg_log! {
                    log::error!("PooledItem was dropped without being returned to pool. The pool might have been dropped.");
                }

                cfg_tracing! {
                    tracing::error!("PooledItem was dropped without being returned to pool. The pool might have been dropped.");
                }
                false
            }
        }
    });
}

/// Iterate through all borrowed items and try to receive them. If the item is lost
/// (ie. the sender has been dropped before it returned the item), panic.
pub(crate) fn try_recv_from_borrowed_and_panic_if_lost<P>(
    ready: &mut P,
    borrowed: &mut Vec<Receiver<P::Item>>,
) 
where
    P: PoolStore,
{
    borrowed.retain_mut(|rx| match rx.try_recv() {
        Ok(Some(item)) => {
            ready.push(item);
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
pub(crate) fn poll_all_borrowed_and_push_back_ready<P>(
    ready: &mut P,
    borrowed: &mut Vec<Receiver<P::Item>>,
    cx: &mut Context<'_>,
) 
where
    P: PoolStore,
{
    borrowed.retain_mut(|rx| match rx.poll_unpin(cx) {
        Poll::Ready(Ok(item)) => {
            ready.push(item);
            false
        },
        Poll::Ready(Err(_canceled)) => {
            // Sender cannot be canceled, so this is unreachable.
            unreachable!("Sender has been dropped, so the item will no longer be returned back to the pool.")
        }
        Poll::Pending => true,
    });
}
