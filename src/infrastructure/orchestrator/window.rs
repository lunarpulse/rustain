//! Opaque symbolic `Window<Handle>` over spoke handles (AC5 / DD4).
//!
//! The window is the **prompt-bound attention view**: compact per-spoke metadata
//! (AgentId + label + status + a relative-ranked salience noun-phrase), NEVER
//! inlined bodies. Full payloads live in the [`ResultStore`](super::result_store)
//! side-table, addressed by `AgentId`; drill is lazy fetch-on-open.
//!
//! ## The type-wall
//!
//! [`Window`] deliberately exposes **no body accessor**. The prompt-build scope
//! can iterate handles and read compact metadata, but cannot inline a full
//! payload body. This is enforced at the TYPE level (not a render rule): a
//! synthesis call that tries to inline a body does not compile (AC5). The
//! trybuild `compile_fail` + `pass`-twin pair (Task 4) proves the only failure
//! cause is the inlining attempt.
//!
//! `Window` is generic over the handle type so R2 can carry richer symbolic
//! references without touching the prompt-build contract. R1 uses
//! [`SpokeHandle`].

use crate::domain::models::agent_id::AgentId;
use crate::domain::models::node_state::NodeState;

/// Compact symbolic reference to a spoke — the window element. Carries NO body
/// (the ≈15× token antidote, AC5). The full payload lives in the
/// [`ResultStore`](super::result_store), fetched lazily on drill by `agent_id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpokeHandle {
    pub agent_id: AgentId,
    pub label: String,
    pub status: NodeState,
    /// Relative-ranked salience noun-phrase — compact metadata only. This is
    /// what enters the coordinator's attention window, not the payload body.
    pub salience: String,
}

/// Opaque window over a slice of spoke handles.
///
/// Has **no body accessor**: the prompt-build scope cannot inline a payload.
/// Construct via [`Window::new`] (crate-scoped — only the executor builds the
/// backing slice); consumers receive a `&Window` and read handles + metadata.
pub struct Window<'a, H> {
    handles: &'a [H],
}

impl<'a, H> Window<'a, H> {
    /// Crate-scoped ctor — only the executor mints a window (it owns the
    /// backing handle slice). External code receives a `&Window`.
    pub(crate) fn new(handles: &'a [H]) -> Self {
        Self { handles }
    }

    /// Number of handles in the window.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    /// `true` when the window holds no handles (honest-empty cohort).
    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// The symbolic handles (AgentId-addressable). Intentionally the ONLY
    /// accessor: compact metadata, never a body. Drill via the `ResultStore`.
    pub fn handles(&self) -> &'a [H] {
        self.handles
    }
}

// NOTE: there is intentionally NO `as_str`, `Deref<Target=str>`, `body()`,
// `payload()`, or any accessor that yields a full payload. Adding one breaks
// the AC5 type-wall and must be rejected in review. There is also NO public
// constructor — `Window::new` is `pub(crate)`, so an external crate cannot
// mint a `Window` at all (the prior `#[doc(hidden)] pub __test_window` escape
// hatch was removed: it punched a hole in the type-wall for every consumer).

#[cfg(test)]
mod tests {
    use super::*;

    fn h(label: &str) -> SpokeHandle {
        SpokeHandle {
            agent_id: AgentId::from_validated(label),
            label: label.into(),
            status: NodeState::Completed,
            salience: format!("{label} salience"),
        }
    }

    #[test]
    fn window_exposes_handles_not_bodies() {
        let handles = vec![h("a"), h("b")];
        let win = Window::new(&handles);
        assert_eq!(win.len(), 2);
        assert!(!win.is_empty());
        assert_eq!(win.handles()[0].label, "a");
        // SpokeHandle has no body field — compile-time guarantee.
    }

    #[test]
    fn empty_window() {
        let handles: Vec<SpokeHandle> = Vec::new();
        let win = Window::new(&handles);
        assert!(win.is_empty());
        assert_eq!(win.len(), 0);
    }
}
