//! A spawned task that is aborted when its handle is dropped.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::task::{JoinError, JoinHandle};

/// Owns a spawned task; dropping the guard aborts it.
///
/// Awaiting the guard (or `&mut guard`, e.g. in a `select!` loop) yields the task's result
/// once; after that it stays pending forever.
pub(crate) struct TaskGuard<T> {
    /// `None` once the result has been taken.
    handle: Option<JoinHandle<T>>,
}

impl<T: Send + 'static> TaskGuard<T> {
    pub(crate) fn spawn(fut: impl Future<Output = T> + Send + 'static) -> Self {
        TaskGuard {
            handle: Some(tokio::spawn(fut)),
        }
    }
}

impl<T> TaskGuard<T> {
    /// The task has finished (its result may already have been taken).
    pub(crate) fn is_done(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Aborts the task and waits until it is gone (its resources dropped).
    pub(crate) async fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }
}

impl<T> Future for TaskGuard<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(handle) = self.handle.as_mut() else {
            return Poll::Pending;
        };
        let result = ready!(Pin::new(handle).poll(cx));
        self.handle = None;
        Poll::Ready(result)
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}
