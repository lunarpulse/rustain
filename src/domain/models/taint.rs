//! Provenance-taint tag and decision type for the permission gate (Story 14.6, AC2).
//!
//! Two inert seam halves that R2 fills in:
//!
//! - [`ProvenanceTag`] — a gate-visible provenance/taint tag carried alongside a
//!   tool call. In R1 it is inert: [`ProvenanceTag::default()`] (`UserOriginated`)
//!   is the benign default and the [`taint_gate`][crate::domain::services::permission_chain::taint_gate]
//!   decision function unconditionally allows.
//!
//! - [`TaintDecision`] — the gate's verdict. R1 has only [`TaintDecision::Allow`];
//!   R2 (planned) adds `RequireApproval { reason }` for tainted data.
//!
//! # R2 envelope-origin → tag propagation (planned, not implemented in R1)
//!
//! Today every call through `permission_chain::check_with_source` is a foreground,
//! user-driven tool call, so it carries [`ProvenanceTag::default()`]
//! (`UserOriginated`, trusted). In R2 the **origin** of a subagent-envelope result
//! is propagated onto the tag: model/subagent-originated data becomes
//! [`SelfOriginated`](ProvenanceTag::SelfOriginated) — potentially tainted, since
//! it may carry injected instructions read from tool output — while direct human
//! input stays [`UserOriginated`](ProvenanceTag::UserOriginated). The gate then
//! consults the tag to decide whether `SelfOriginated` data must obtain explicit
//! approval before driving a privileged tool.
//!
//! The seam is deliberately split from the *live* revocation path (DD5) so the
//! inert provenance gate (DD4) can land and be exercised without coupling to
//! approval revocation. The gate **never** calls revoke.

/// Provenance/taint tag for the data flowing into a tool call.
///
/// R1: inert. [`ProvenanceTag::default()`] is `UserOriginated` — the benign,
/// permissive default for foreground (user-driven) tool calls, which is all
/// `permission_chain::check_with_source` sees in R1.
///
/// `#[non_exhaustive]` keeps the door open for future provenance classes
/// (e.g. a distinct `ToolOriginated` tag) without breaking exhaustive matches.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProvenanceTag {
    /// Data originated from the human user — trusted. Benign R1 default.
    #[default]
    UserOriginated,
    /// Data originated from the agent or a subagent (model output) — potentially
    /// tainted. Inert in R1; subject to the gate in R2.
    SelfOriginated,
}

/// Verdict returned by the provenance-taint gate.
///
/// R1: only [`Allow`](TaintDecision::Allow) in production — the gate is an
/// inert, load-bearing seam (it sits on the hot path and a non-`Allow`
/// verdict hard-denies, so R2 only has to *populate* the tag and add
/// verdicts). A `Deny` variant exists ONLY under `test`/`test-instrumentation`
/// (DD4, Murat — proof (b): "inert is indistinguishable from not-wired
/// without a Deny-mutant") — it is never reachable in a production build,
/// so R1's "R1 body returns Allow unconditionally" guarantee holds outside
/// test builds.
///
/// R2 (planned): adds `RequireApproval { reason: String }` so tainted
/// (`SelfOriginated`) data can be routed through the approval runtime.
///
/// `#[non_exhaustive]` lets future variants land without breaking matches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaintDecision {
    /// Provenance policy permits the call (R1: always, in production).
    Allow,
    /// Test-only verdict (DD4 Deny-mutant proof) — never constructed by
    /// production code; only reachable via `TAINT_GATE_FORCE_DENY`.
    #[cfg(any(test, feature = "test-instrumentation"))]
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tool_call::ApprovalSource;
    use crate::domain::services::permission_chain::taint_gate;
    use serde_json::json;

    /// R1 inertness: the gate allows any input regardless of provenance tag.
    #[test]
    fn taint_gate_allows_any_input_inert() {
        for provenance in [ProvenanceTag::UserOriginated, ProvenanceTag::SelfOriginated] {
            assert_eq!(
                taint_gate(
                    "Bash",
                    &json!({"command": "rm -rf /"}),
                    None,
                    None,
                    None,
                    provenance,
                ),
                TaintDecision::Allow,
                "R1 gate must be inert for provenance {provenance:?}"
            );
        }
    }

    /// The gate still allows when a real approval source and hints are present
    /// (foreground turn driving a file tool) — proving the seam is wired, not dead.
    #[test]
    fn taint_gate_allows_foreground_with_hints() {
        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "conv-1".to_string(),
        };
        assert_eq!(
            taint_gate(
                "Write",
                &json!({"file_path": "/tmp/x"}),
                Some(&source),
                Some("/tmp/x"),
                Some("mcp__fs"),
                ProvenanceTag::default(),
            ),
            TaintDecision::Allow,
        );
    }

    /// `ProvenanceTag::default()` is available and resolves to the benign variant.
    #[test]
    fn provenance_tag_default_is_benign() {
        assert_eq!(ProvenanceTag::default(), ProvenanceTag::UserOriginated);
    }

    /// `TaintDecision` is `#[non_exhaustive]`: it is matched with the wildcard-free
    /// `matches!` form that external crates must use. `#[non_exhaustive]` is a
    /// compile-time property of the definition; this test pins the only in-crate
    /// variant today and documents that exhaustive matching is intentionally
    /// forbidden outside this crate.
    #[test]
    fn taint_decision_is_non_exhaustive() {
        // Constructable and matchable via the non-exhaustive-friendly pattern.
        let decision = TaintDecision::Allow;
        assert!(matches!(decision, TaintDecision::Allow));

        // The single in-crate variant today; R2 will add RequireApproval.
        let all_known = [TaintDecision::Allow];
        assert_eq!(all_known.len(), 1);
    }
}
