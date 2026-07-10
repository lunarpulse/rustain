//! ToolCall 7-variant FSM and supporting types.
//!
//! See ADR-06-02 for the canonical FSM design.

use serde::{Deserialize, Serialize};

/// A request to execute a tool.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallRequest {
    pub id: String,
    pub tool_name: String,
    pub input: serde_json::Value,
}

/// The result of a successful tool execution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallResult {
    pub output: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

/// Newtype for an approval request identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub String);

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestId {
    /// Generate a fresh 12-character URL-safe ID.
    pub fn new() -> Self {
        Self(nanoid::nanoid!(12))
    }
}

/// Source of an approval request.
///
/// `#[non_exhaustive]` so 14-1 (A2A peer agents) can add a `RemotePeer`
/// variant without breaking match sites.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalSource {
    ForegroundTurn {
        conversation_id: String,
    },
    ForegroundSubagent {
        conversation_id: String,
        parent_tool_call_id: String,
        subagent_type: String,
    },
    AcpSession {
        session_id: String,
        conversation_id: String,
    },
    BackgroundAgent {
        conversation_id: String,
        task_id: String,
        subagent_type: String,
    },
}

impl ApprovalSource {
    /// Return the conversation_id from any variant.
    pub fn conversation_id(&self) -> &str {
        match self {
            ApprovalSource::ForegroundTurn { conversation_id } => conversation_id,
            ApprovalSource::ForegroundSubagent {
                conversation_id, ..
            } => conversation_id,
            ApprovalSource::AcpSession {
                conversation_id, ..
            } => conversation_id,
            ApprovalSource::BackgroundAgent {
                conversation_id, ..
            } => conversation_id,
        }
    }

    /// Return an [`AgentId`] representing the acting agent's scope.
    ///
    /// Used by [`InvocationFingerprint`] to bind an approval to the specific
    /// node that requested it, so an approval for node A cannot replay on node B.
    ///
    /// Multi-component scopes are joined via [`length_prefixed_scope`] rather
    /// than a bare `format!("{}/{}", a, b)` — an unprefixed join lets a `/`
    /// inside `a` or `b` produce the same joined string from two DIFFERENT
    /// component pairs (e.g. `("a/b", "c")` and `("a", "b/c")` both yield
    /// `"a/b/c"`), collapsing two distinct nodes onto one scope and defeating
    /// the "approval for node A cannot replay on node B" guarantee (DD1).
    pub fn scope_agent_id(&self) -> crate::domain::models::agent_id::AgentId {
        match self {
            ApprovalSource::ForegroundTurn { conversation_id } => {
                crate::domain::models::agent_id::AgentId::from_validated(conversation_id.clone())
            }
            ApprovalSource::ForegroundSubagent {
                conversation_id,
                parent_tool_call_id,
                ..
            } => {
                crate::domain::models::agent_id::AgentId::from_validated(length_prefixed_scope(&[
                    conversation_id,
                    parent_tool_call_id,
                ]))
            }
            ApprovalSource::AcpSession {
                conversation_id,
                session_id,
            } => {
                crate::domain::models::agent_id::AgentId::from_validated(length_prefixed_scope(&[
                    conversation_id,
                    session_id,
                ]))
            }
            ApprovalSource::BackgroundAgent {
                conversation_id,
                task_id,
                ..
            } => {
                crate::domain::models::agent_id::AgentId::from_validated(length_prefixed_scope(&[
                    conversation_id,
                    task_id,
                ]))
            }
        }
    }
}

/// Join scope components unambiguously: each component is prefixed with its
/// own decimal byte length before a `:` separator, so no possible byte
/// sequence produces the same joined string from two different component
/// sets (the length itself disambiguates any embedded delimiter).
fn length_prefixed_scope(components: &[&str]) -> String {
    let mut out = String::new();
    for c in components {
        out.push_str(&c.len().to_string());
        out.push(':');
        out.push_str(c);
    }
    out
}

/// 7-variant discriminated-union FSM for a single tool call lifecycle.
///
/// Serialised with `tag = "status", rename_all = "snake_case"` so the
/// wire format is self-describing.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolCall {
    Validating {
        id: String,
        request: ToolCallRequest,
        started_at: i64,
    },
    Scheduled {
        id: String,
        request: ToolCallRequest,
    },
    AwaitingApproval {
        id: String,
        request: ToolCallRequest,
        approval_id: RequestId,
    },
    Executing {
        id: String,
        request: ToolCallRequest,
        started_at: i64,
    },
    Success {
        id: String,
        request: ToolCallRequest,
        result: ToolCallResult,
    },
    Error {
        id: String,
        request: ToolCallRequest,
        error: String,
    },
    Cancelled {
        id: String,
        request: ToolCallRequest,
        reason: String,
    },
}

/// An event emitted every time a `ToolCall` transitions to a new state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCallTransition {
    pub conversation_id: String,
    pub call: ToolCall,
}

#[allow(dead_code)]
impl ToolCall {
    /// Return the tool-use id for this call.
    pub fn id(&self) -> &str {
        match self {
            ToolCall::Validating { id, .. } => id,
            ToolCall::Scheduled { id, .. } => id,
            ToolCall::AwaitingApproval { id, .. } => id,
            ToolCall::Executing { id, .. } => id,
            ToolCall::Success { id, .. } => id,
            ToolCall::Error { id, .. } => id,
            ToolCall::Cancelled { id, .. } => id,
        }
    }

    /// Return the original request.
    pub fn request(&self) -> &ToolCallRequest {
        match self {
            ToolCall::Validating { request, .. } => request,
            ToolCall::Scheduled { request, .. } => request,
            ToolCall::AwaitingApproval { request, .. } => request,
            ToolCall::Executing { request, .. } => request,
            ToolCall::Success { request, .. } => request,
            ToolCall::Error { request, .. } => request,
            ToolCall::Cancelled { request, .. } => request,
        }
    }

    /// True for terminal states (Success, Error, Cancelled).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ToolCall::Success { .. } | ToolCall::Error { .. } | ToolCall::Cancelled { .. }
        )
    }
}

/// Return the TUI status chip string for a given `ToolCall` state.
///
/// Per UX-DR-NEW-05.  Exhaustive match so adding a variant is a
/// compile-time requirement.
pub fn status_chip(call: &ToolCall) -> &'static str {
    match call {
        ToolCall::Validating { .. } => "⋯ Validating",
        ToolCall::Scheduled { .. } => "⧖ Scheduled",
        ToolCall::AwaitingApproval { .. } => "? Awaiting approval",
        ToolCall::Executing { .. } => "● Executing",
        ToolCall::Success { .. } => "✓ Success",
        ToolCall::Error { .. } => "✗ Error",
        ToolCall::Cancelled { .. } => "⊘ Cancelled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> ToolCallRequest {
        ToolCallRequest {
            id: "abc".into(),
            tool_name: "Read".into(),
            input: serde_json::json!({"file_path": "/tmp/x"}),
        }
    }

    #[test]
    fn scope_agent_id_no_slash_collision() {
        // Before the length-prefixed fix, ("a/b", "c") and ("a", "b/c") both
        // joined to "a/b/c" via bare format!("{}/{}"), collapsing two
        // DIFFERENT nodes onto one scope. The length-prefixed join
        // disambiguates them.
        let a = ApprovalSource::ForegroundSubagent {
            conversation_id: "a/b".into(),
            parent_tool_call_id: "c".into(),
            subagent_type: "explore".into(),
        };
        let b = ApprovalSource::ForegroundSubagent {
            conversation_id: "a".into(),
            parent_tool_call_id: "b/c".into(),
            subagent_type: "explore".into(),
        };
        assert_ne!(
            a.scope_agent_id(),
            b.scope_agent_id(),
            "distinct (conversation_id, parent_tool_call_id) pairs sharing a slash \
             must not collide onto the same scope"
        );
    }

    #[test]
    fn roundtrip_validating() {
        let call = ToolCall::Validating {
            id: "abc".into(),
            request: sample_request(),
            started_at: 1714000000,
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Validating { .. }));
        if let ToolCall::Validating { started_at, .. } = back {
            assert_eq!(started_at, 1714000000);
        }
    }

    #[test]
    fn roundtrip_scheduled() {
        let call = ToolCall::Scheduled {
            id: "abc".into(),
            request: sample_request(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Scheduled { .. }));
    }

    #[test]
    fn roundtrip_awaiting_approval() {
        let call = ToolCall::AwaitingApproval {
            id: "abc".into(),
            request: sample_request(),
            approval_id: RequestId("req-1".into()),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::AwaitingApproval { .. }));
    }

    #[test]
    fn roundtrip_executing() {
        let call = ToolCall::Executing {
            id: "abc".into(),
            request: sample_request(),
            started_at: 1714000000,
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Executing { .. }));
    }

    #[test]
    fn roundtrip_success() {
        let call = ToolCall::Success {
            id: "abc".into(),
            request: sample_request(),
            result: ToolCallResult {
                output: "hello".into(),
                is_error: false,
                duration_ms: 42,
            },
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Success { .. }));
    }

    #[test]
    fn roundtrip_error() {
        let call = ToolCall::Error {
            id: "abc".into(),
            request: sample_request(),
            error: "boom".into(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Error { .. }));
    }

    #[test]
    fn roundtrip_cancelled() {
        let call = ToolCall::Cancelled {
            id: "abc".into(),
            request: sample_request(),
            reason: "user-cancel".into(),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, ToolCall::Cancelled { .. }));
    }

    #[test]
    fn is_terminal_table() {
        let req = sample_request();
        assert!(
            !ToolCall::Validating {
                id: "a".into(),
                request: req.clone(),
                started_at: 0
            }
            .is_terminal()
        );
        assert!(
            !ToolCall::Scheduled {
                id: "a".into(),
                request: req.clone()
            }
            .is_terminal()
        );
        assert!(
            !ToolCall::AwaitingApproval {
                id: "a".into(),
                request: req.clone(),
                approval_id: RequestId("req-default".into())
            }
            .is_terminal()
        );
        assert!(
            !ToolCall::Executing {
                id: "a".into(),
                request: req.clone(),
                started_at: 0
            }
            .is_terminal()
        );
        assert!(
            ToolCall::Success {
                id: "a".into(),
                request: req.clone(),
                result: ToolCallResult {
                    output: "o".into(),
                    is_error: false,
                    duration_ms: 0
                }
            }
            .is_terminal()
        );
        assert!(
            ToolCall::Error {
                id: "a".into(),
                request: req.clone(),
                error: "e".into()
            }
            .is_terminal()
        );
        assert!(
            ToolCall::Cancelled {
                id: "a".into(),
                request: req.clone(),
                reason: "r".into()
            }
            .is_terminal()
        );
    }

    #[test]
    fn status_chip_exhaustive() {
        let req = sample_request();
        let cases: Vec<(ToolCall, &'static str)> = vec![
            (
                ToolCall::Validating {
                    id: "a".into(),
                    request: req.clone(),
                    started_at: 0,
                },
                "⋯ Validating",
            ),
            (
                ToolCall::Scheduled {
                    id: "a".into(),
                    request: req.clone(),
                },
                "⧖ Scheduled",
            ),
            (
                ToolCall::AwaitingApproval {
                    id: "a".into(),
                    request: req.clone(),
                    approval_id: RequestId("req-default".into()),
                },
                "? Awaiting approval",
            ),
            (
                ToolCall::Executing {
                    id: "a".into(),
                    request: req.clone(),
                    started_at: 0,
                },
                "● Executing",
            ),
            (
                ToolCall::Success {
                    id: "a".into(),
                    request: req.clone(),
                    result: ToolCallResult {
                        output: "o".into(),
                        is_error: false,
                        duration_ms: 0,
                    },
                },
                "✓ Success",
            ),
            (
                ToolCall::Error {
                    id: "a".into(),
                    request: req.clone(),
                    error: "e".into(),
                },
                "✗ Error",
            ),
            (
                ToolCall::Cancelled {
                    id: "a".into(),
                    request: req.clone(),
                    reason: "r".into(),
                },
                "⊘ Cancelled",
            ),
        ];
        for (call, expected) in cases {
            assert_eq!(status_chip(&call), expected);
        }
    }

    #[test]
    fn id_and_request_helpers() {
        let req = sample_request();
        let call = ToolCall::Executing {
            id: "xyz".into(),
            request: req.clone(),
            started_at: 0,
        };
        assert_eq!(call.id(), "xyz");
        assert_eq!(call.request().tool_name, "Read");
    }

    #[test]
    fn approval_source_serde_roundtrip() {
        let variants = vec![
            ApprovalSource::ForegroundTurn {
                conversation_id: "conv-1".into(),
            },
            ApprovalSource::ForegroundSubagent {
                conversation_id: "conv-2".into(),
                parent_tool_call_id: "tc-1".into(),
                subagent_type: "explore".into(),
            },
            ApprovalSource::BackgroundAgent {
                conversation_id: "conv-3".into(),
                task_id: "task-1".into(),
                subagent_type: "general".into(),
            },
        ];
        for original in variants {
            let json = serde_json::to_string(&original).unwrap();
            let back: ApprovalSource = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&back).unwrap();
            assert_eq!(json, json2, "round-trip mismatch for {:?}", original);
        }
    }
}
