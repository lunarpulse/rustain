//! The reply route a queued decision owns (Story 21.6 6-C): the oneshot sender of a
//! turn that waits for the answer, held by the queue item instead of a `TuiState` slot.
//!
//! The handle is shared by clones of the item (the queue clones payloads for the frame
//! and the list); the first `send` or `abandon` consumes the sender, every later one is a
//! no-op, so an answer reaches the waiting turn exactly once.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::oneshot;

/// A shared, take-once oneshot sender.
pub struct ReplyHandle<T>(Arc<Mutex<Option<oneshot::Sender<T>>>>);

impl<T> ReplyHandle<T> {
    #[must_use]
    pub fn new(tx: oneshot::Sender<T>) -> Self {
        Self(Arc::new(Mutex::new(Some(tx))))
    }

    /// Answer the waiting turn. False when the answer was already sent, the handle was
    /// abandoned or the receiver is gone (nothing was delivered).
    pub fn send(&self, value: T) -> bool {
        self.0
            .lock()
            .take()
            .is_some_and(|tx| tx.send(value).is_ok())
    }

    /// True once the answer was sent, the sender dropped or the receiver closed: nobody
    /// waits for this decision any more.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0
            .lock()
            .as_ref()
            .is_none_or(oneshot::Sender::is_closed)
    }

    /// Drop the sender: the waiting turn gets a `RecvError` instead of an answer.
    pub fn abandon(&self) {
        drop(self.0.lock().take());
    }
}

impl<T> Clone for ReplyHandle<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> std::fmt::Debug for ReplyHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplyHandle")
    }
}

impl<T> PartialEq for ReplyHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<T> Eq for ReplyHandle<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_a_reply_handle_answers_once_and_reports_closure() {
        let (tx, mut rx) = oneshot::channel::<String>();
        let handle = ReplyHandle::new(tx);
        let twin = handle.clone();
        assert_eq!(handle, twin, "clones share one sender");
        assert!(!handle.is_closed());
        assert!(twin.send("first".into()));
        assert!(!handle.send("second".into()), "a second answer is dropped");
        assert!(handle.is_closed());
        assert_eq!(rx.try_recv().unwrap(), "first");
        assert_eq!(format!("{handle:?}"), "ReplyHandle");
    }

    #[test]
    fn story21_6_abandoning_or_a_gone_receiver_closes_the_handle() {
        let (tx, rx) = oneshot::channel::<u8>();
        let handle = ReplyHandle::new(tx);
        drop(rx);
        assert!(handle.is_closed(), "receiver gone");
        assert!(!handle.send(1), "nothing is delivered to a gone receiver");

        let (tx, mut rx) = oneshot::channel::<u8>();
        let handle = ReplyHandle::new(tx);
        handle.abandon();
        assert!(handle.is_closed());
        assert!(rx.try_recv().is_err(), "the waiting turn gets RecvError");
    }

    #[test]
    fn story21_6_distinct_handles_are_not_equal() {
        let (a, _ra) = oneshot::channel::<u8>();
        let (b, _rb) = oneshot::channel::<u8>();
        assert_ne!(ReplyHandle::new(a), ReplyHandle::new(b));
    }
}
