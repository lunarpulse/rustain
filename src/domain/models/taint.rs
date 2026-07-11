//! Provenance-taint tag and decision type for the permission gate (Story 14.6, AC2).
//!
//! The node owns a monotone integrity bit. Cross-agent ingest and spawn
//! inheritance set it; tool dispatch derives [`ProvenanceTag`] from that local
//! state and the pure gate returns [`TaintDecision`]. The gate never performs
//! live revocation.

/// Provenance/taint tag for the data flowing into a tool call.
///
/// `Default` remains `UserOriginated` for call sites without node context.
///
/// `#[non_exhaustive]` keeps the door open for future provenance classes
/// (e.g. a distinct `ToolOriginated` tag) without breaking exhaustive matches.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProvenanceTag {
    /// Data originated directly from the human user.
    #[default]
    UserOriginated,
    /// Data entered this node from another agent or a tainted parent.
    SelfOriginated,
}

/// Deterministic verdict returned by the provenance-taint gate.
///
/// `RequireApproval` reuses the existing approval runtime. `Deny` remains a
/// test-only anti-vacuity override.
///
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaintDecision {
    /// Provenance policy permits the call.
    Allow,
    /// Tainted context may drive this sink only after explicit approval.
    RequireApproval { reason: String },
    /// Test-only verdict (DD4 Deny-mutant proof).
    #[cfg(any(test, feature = "test-instrumentation"))]
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::tool_call::ApprovalSource;
    use crate::domain::services::permission_chain::taint_gate;
    use serde_json::json;

    #[test]
    fn taint_gate_enforces_the_narrow_sink_matrix() {
        assert!(matches!(
            taint_gate(
                "Bash",
                &json!({"command": "rm -rf /"}),
                None,
                None,
                None,
                ProvenanceTag::SelfOriginated,
            ),
            TaintDecision::RequireApproval { .. }
        ));
        assert_eq!(
            taint_gate(
                "Read",
                &json!({"file_path": "/tmp/x"}),
                None,
                None,
                None,
                ProvenanceTag::SelfOriginated,
            ),
            TaintDecision::Allow
        );
        assert_eq!(
            taint_gate(
                "Bash",
                &json!({"command": "rm -rf /"}),
                None,
                None,
                None,
                ProvenanceTag::UserOriginated,
            ),
            TaintDecision::Allow
        );
    }

    #[test]
    fn taint_gate_uses_source_only_as_context_not_trust() {
        let source = ApprovalSource::ForegroundTurn {
            conversation_id: "conv-1".to_string(),
        };
        assert!(matches!(
            taint_gate(
                "Write",
                &json!({"file_path": "/tmp/x"}),
                Some(&source),
                Some("/tmp/x"),
                Some("mcp__fs"),
                ProvenanceTag::SelfOriginated,
            ),
            TaintDecision::RequireApproval { .. }
        ));
    }

    /// `ProvenanceTag::default()` is available and resolves to the benign variant.
    #[test]
    fn provenance_tag_default_is_benign() {
        assert_eq!(ProvenanceTag::default(), ProvenanceTag::UserOriginated);
    }

    #[test]
    fn taint_decision_is_non_exhaustive_and_constructible() {
        let all_known = [
            TaintDecision::Allow,
            TaintDecision::RequireApproval {
                reason: "tainted".into(),
            },
        ];
        assert_eq!(all_known.len(), 2);
    }
}
