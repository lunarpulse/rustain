//! Infrastructure-level handle for communicating with a running node.
//!
//! This is the wiring counterpart to [`crate::domain::models::TaskHandle`]:
//! `TaskHandle` owns the spawn-time bundle (agent id, status stream, command
//! channel, cancel token, spool metadata) and lives conceptually in the domain
//! boundary, whereas `NodeHandle` is the lightweight, cloneable reference the
//! node tree stores in its side-table keyed by [`AgentId`].
//!
//! # Why infrastructure, not domain?
//! `NodeHandle` holds `tokio` / `tokio-util` runtime types
//! ([`CancellationToken`], [`mpsc::Sender`]). Domain models must remain
//! transport- and runtime-agnostic, so this type lives here.
//!
//! # R1 scope
//! Only the [`NodeHandle::Local`] variant is constructed in R1. The
//! [`NodeHandle::Remote`] variant is defined for forward-compatibility with R2
//! (transport-routed nodes) but is **not** constructed; its presence is pinned
//! by the Epic 14 conformance test so the shape does not regress.
//!
//! [`AgentId`]: crate::domain::models::AgentId
//! [`CancellationToken`]: tokio_util::sync::CancellationToken
//! [`mpsc::Sender`]: tokio::sync::mpsc::Sender

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::models::Op;

/// Infrastructure-level handle for communicating with a running node.
/// Lives in infrastructure (NOT domain) because it holds tokio types.
/// The node tree stores these in a side-table keyed by `AgentId`.
#[derive(Clone)]
pub enum NodeHandle {
    /// An in-process node. Holds the per-agent cancel token (derived from the
    /// parent's token tree) and the bounded command channel that forwards
    /// [`Op`]s to the node's run loop.
    Local {
        cancel_token: CancellationToken,
        command_tx: mpsc::Sender<Op>,
    },
    /// R2: filled with transport identifier for remote nodes.
    /// Defined but NOT constructed in R1 — pinned as unused by conformance test.
    Remote { transport_ref: String },
}

impl NodeHandle {
    /// Send an [`Op`] to the target node.
    ///
    /// For [`NodeHandle::Local`] this is a non-blocking `try_send` on the
    /// command channel — the caller decides how to react to a full/closed
    /// channel via the returned [`NodeHandleError`]. For
    /// [`NodeHandle::Remote`] this always fails: R1 does not route ops to
    /// remote nodes.
    pub fn send_op(&self, op: Op) -> Result<(), NodeHandleError> {
        match self {
            NodeHandle::Local { command_tx, .. } => {
                command_tx.try_send(op).map_err(NodeHandleError::from)
            }
            // R1: remote op routing is not implemented.
            NodeHandle::Remote { .. } => Err(NodeHandleError::RemoteNotSupported),
        }
    }

    /// Request cancellation of the target node.
    ///
    /// For [`NodeHandle::Local`] this cancels the node's derived token, which
    /// propagates through the parent's `CancellationToken` tree. For
    /// [`NodeHandle::Remote`] this is a no-op in R1 — there is no transport
    /// to carry the cancel signal yet.
    pub fn cancel(&self) {
        match self {
            NodeHandle::Local { cancel_token, .. } => cancel_token.cancel(),
            NodeHandle::Remote { transport_ref } => {
                tracing::warn!(
                    transport_ref = %transport_ref,
                    "NodeHandle::cancel() called on a Remote node — no-op in R1 \
                     (remote cancellation arrives with R2 transport)"
                );
            }
        }
    }
}

/// Errors returned by [`NodeHandle`] operations.
#[derive(Debug, Error)]
pub enum NodeHandleError {
    /// R1 does not support sending ops to remote nodes.
    #[error("remote node handles not supported in R1")]
    RemoteNotSupported,

    /// The command channel could not accept the op — either the node has
    /// dropped its receiver (closed) or the bounded channel is saturated
    /// (full). In both cases the op was not delivered.
    #[error("node command channel closed")]
    ChannelClosed,
}

impl<T> From<mpsc::error::TrySendError<T>> for NodeHandleError {
    /// Collapse both `TrySendError` cases (`Full` and `Closed`) into
    /// [`NodeHandleError::ChannelClosed`]: either way the op did not reach the
    /// node's run loop, and R1 callers only need to know delivery failed.
    fn from(_: mpsc::error::TrySendError<T>) -> Self {
        NodeHandleError::ChannelClosed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::Op;

    // --- Compile-time trait proofs ------------------------------------------------

    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_clone<T: Clone>() {}

    #[test]
    fn node_handle_is_send_sync() {
        assert_send_sync::<NodeHandle>();
    }

    #[test]
    fn node_handle_is_clone() {
        assert_clone::<NodeHandle>();
    }

    // --- Local send_op -----------------------------------------------------------

    #[tokio::test]
    async fn local_handle_sends_op() {
        // Keep the receiver alive so the channel is open, not full.
        let (command_tx, mut command_rx) = mpsc::channel::<Op>(8);
        let cancel_token = CancellationToken::new();
        let handle = NodeHandle::Local {
            cancel_token,
            command_tx,
        };

        handle.send_op(Op::Kill).expect("op should be accepted");

        // The op actually landed in the channel.
        let received = command_rx.recv().await;
        assert!(matches!(received, Some(Op::Kill)));
    }

    #[tokio::test]
    async fn local_handle_send_op_reports_channel_closed() {
        // Drop the receiver up front -> try_send fails immediately.
        let (command_tx, command_rx) = mpsc::channel::<Op>(8);
        drop(command_rx);
        let cancel_token = CancellationToken::new();
        let handle = NodeHandle::Local {
            cancel_token,
            command_tx,
        };

        let err = handle
            .send_op(Op::Pause)
            .expect_err("send to closed channel must error");
        assert!(matches!(err, NodeHandleError::ChannelClosed));
    }

    #[tokio::test]
    async fn local_handle_send_op_reports_channel_full() {
        // Capacity 1, never drained -> second send saturates the channel.
        let (command_tx, command_rx) = mpsc::channel::<Op>(1);
        let cancel_token = CancellationToken::new();
        let handle = NodeHandle::Local {
            cancel_token,
            command_tx,
        };

        handle.send_op(Op::Kill).expect("first op fits");
        let err = handle
            .send_op(Op::Resume)
            .expect_err("second op must saturate the bounded channel");
        // Full collapses into ChannelClosed per the From impl.
        assert!(matches!(err, NodeHandleError::ChannelClosed));

        // Keep the receiver alive for the lifetime of the assertions.
        drop(command_rx);
    }

    // --- Local cancel ------------------------------------------------------------

    #[test]
    fn local_handle_cancel_triggers_token() {
        let cancel_token = CancellationToken::new();
        let (command_tx, _command_rx) = mpsc::channel::<Op>(8);
        let handle = NodeHandle::Local {
            cancel_token: cancel_token.clone(),
            command_tx,
        };

        assert!(!cancel_token.is_cancelled(), "token starts uncancelled");
        handle.cancel();
        assert!(
            cancel_token.is_cancelled(),
            "cancel() must cancel the token"
        );
    }

    // --- Remote ------------------------------------------------------------------

    #[test]
    fn remote_send_op_is_not_supported() {
        let handle = NodeHandle::Remote {
            transport_ref: "r2://transport/agent-7".to_string(),
        };

        let err = handle
            .send_op(Op::Kill)
            .expect_err("remote send_op must fail in R1");
        assert!(matches!(err, NodeHandleError::RemoteNotSupported));
    }

    #[test]
    fn remote_cancel_is_a_documented_noop() {
        let handle = NodeHandle::Remote {
            transport_ref: "r2://transport/agent-9".to_string(),
        };
        // Must not panic; R1 has no transport to signal over.
        handle.cancel();
    }

    // --- Clone semantics ---------------------------------------------------------

    #[tokio::test]
    async fn local_handle_clones_share_channel_and_token() {
        let (command_tx, mut command_rx) = mpsc::channel::<Op>(8);
        let cancel_token = CancellationToken::new();
        let handle = NodeHandle::Local {
            cancel_token: cancel_token.clone(),
            command_tx,
        };

        let cloned = handle.clone();

        // Clone shares the same sender: an op sent through the clone reaches
        // the single receiver.
        cloned.send_op(Op::ReportFull).expect("clone can send");
        assert!(matches!(command_rx.recv().await, Some(Op::ReportFull)));

        // Clone shares the same cancel token: cancelling the clone cancels
        // the original's token too.
        cloned.cancel();
        assert!(
            cancel_token.is_cancelled(),
            "cloned handle must share the cancel token"
        );
    }
}
