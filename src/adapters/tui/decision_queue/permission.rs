//! The permission family (Story 21.6 slice 6-B, A2, A8, A12, A14): a tool call
//! waiting on the operator, and an a2a sender's consent request.
//!
//! Both hosts hold the same item: the local `ApprovalRuntimeEvent::Requested`
//! stream and the attached `DaemonFrame::ApprovalRequest` family raise it, the
//! request id is its source identity, and the payload that used to live in
//! `pending_permission`, `PermissionQueue`, `AttachedApprovals` and the attached
//! consent queue moves here. The Review reader and the Deny + feedback field are
//! sub-contexts of the frame, never separate owners of the keyboard.

use std::sync::Arc;

use super::{
    APPROVAL_NO_LONGER_PENDING, Arrival, DecisionDraft, DecisionEffect, DecisionKey, DecisionKind,
    DecisionSubject, Raised, Responder, raise,
};
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::tool_block::display_tool_name;
use crate::domain::models::tool_call::{ApprovalSource, RequestId};
use crate::domain::models::{ApprovalOutcome, ApprovalScope, ToolRisk};

/// The longest feedback the field takes (the legacy mini-input's bound).
pub const FEEDBACK_MAX_BYTES: usize = 2048;

/// The exact input a permission decides on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionInput {
    /// The complete exact pending value.
    Ready(Arc<serde_json::Value>),
    /// Attached writer only: the exact input is still arriving in parts;
    /// `summary` is the daemon's one-line per-tool summary.
    Receiving { summary: String },
}

/// One tool call awaiting the operator's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPayload {
    pub request_id: RequestId,
    pub source: ApprovalSource,
    pub tool_name: String,
    pub risk: ToolRisk,
    pub input: PermissionInput,
    /// The Review reader is open (a sub-context: Esc returns to the request).
    pub reviewing: bool,
    /// The Deny + feedback field (DecisionCompose), present while it is open.
    pub feedback: Option<TextField>,
}

impl PermissionPayload {
    #[must_use]
    pub fn new(
        request_id: RequestId,
        source: ApprovalSource,
        tool_name: String,
        risk: ToolRisk,
        input: PermissionInput,
    ) -> Self {
        Self {
            request_id,
            source,
            tool_name,
            risk,
            input,
            reviewing: false,
            feedback: None,
        }
    }

    /// True once the complete exact input is held (Review needs it).
    #[must_use]
    pub fn ready(&self) -> bool {
        matches!(self.input, PermissionInput::Ready(_))
    }

    /// The outcome the approval runtime (or the attached `ApprovalResponse`) takes
    /// for `effect`; `None` for an effect that is not a permission answer.
    #[must_use]
    pub fn outcome(&self, effect: DecisionEffect) -> Option<ApprovalOutcome> {
        Some(match effect {
            DecisionEffect::PermissionAllowOnce => ApprovalOutcome::Once,
            DecisionEffect::PermissionAllowSession => ApprovalOutcome::AlwaysTool {
                tool_name: self.tool_name.clone(),
            },
            DecisionEffect::PermissionAllowAlways => ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool(self.tool_name.clone()),
            },
            DecisionEffect::PermissionDeny => ApprovalOutcome::Reject { feedback: None },
            DecisionEffect::PermissionDenyFeedback => ApprovalOutcome::Reject {
                feedback: self
                    .feedback
                    .as_ref()
                    .map(TextField::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned),
            },
            _ => return None,
        })
    }
}

/// An a2a sender's request awaiting consent (attached writer only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentPayload {
    pub request_id: RequestId,
    pub tool: String,
    pub input_preview: String,
}

impl ConsentPayload {
    /// The `ApprovalResponse` outcome for `effect`; `None` for a non-consent effect.
    #[must_use]
    pub fn outcome(&self, effect: DecisionEffect) -> Option<ApprovalOutcome> {
        Some(match effect {
            DecisionEffect::ConsentOnce => ApprovalOutcome::Once,
            DecisionEffect::ConsentAlways => ApprovalOutcome::AlwaysAndSave {
                scope: ApprovalScope::Tool(self.tool.clone()),
            },
            DecisionEffect::ConsentDecline => ApprovalOutcome::Reject { feedback: None },
            _ => return None,
        })
    }
}

fn key_of(kind: DecisionKind, id: &RequestId) -> DecisionKey {
    DecisionKey {
        kind,
        source: id.0.clone(),
        conversation_id: None,
    }
}

/// True when the queue already holds the request (a redelivered event is no new arrival).
#[must_use]
pub fn holds_request(state: &TuiState, id: &RequestId) -> bool {
    state
        .decisions
        .contains_key(&key_of(DecisionKind::Permission, id))
        || state
            .decisions
            .contains_key(&key_of(DecisionKind::Consent, id))
}

/// Raise a tool permission (Automatic arrival: RU-6 decides whether it takes focus).
pub fn raise_permission(
    state: &mut TuiState,
    payload: PermissionPayload,
    responder: Responder,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Permission, "")
        .source(payload.request_id.0.clone())
        .summary(display_tool_name(&payload.tool_name).into_owned())
        .responder(responder)
        .subject(DecisionSubject::Permission(Box::new(payload)));
    raise(state, draft, Arrival::Automatic)
}

/// Raise a sender's consent request (attached writer, Automatic arrival).
pub fn raise_consent(state: &mut TuiState, payload: ConsentPayload) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Consent, "")
        .source(payload.request_id.0.clone())
        .summary(payload.tool.clone())
        .responder(Responder::Attached)
        .subject(DecisionSubject::Consent(Box::new(payload)));
    raise(state, draft, Arrival::Automatic)
}

/// The complete exact input of an attached request arrived: Review and the
/// Ready body are now available. False when the request is not queued.
pub fn complete_input(state: &mut TuiState, id: &RequestId, value: serde_json::Value) -> bool {
    let key = key_of(DecisionKind::Permission, id);
    let Some(item) = state.decisions.item_mut(&key) else {
        return false;
    };
    let DecisionSubject::Permission(payload) = &mut item.subject else {
        return false;
    };
    payload.input = PermissionInput::Ready(Arc::new(value));
    state.needs_redraw = true;
    true
}

/// The request settled elsewhere (resolved, cancelled, closed by the daemon): its
/// item leaves the queue, or, when its frame is open, turns stale with the
/// source's copy and waits for Close (A7). False when nothing was queued.
pub fn withdraw_request(state: &mut TuiState, id: &RequestId) -> bool {
    let mut found = false;
    for kind in [DecisionKind::Permission, DecisionKind::Consent] {
        found |= super::withdraw_key(state, &key_of(kind, id), APPROVAL_NO_LONGER_PENDING);
    }
    found
}

/// Withdraw every permission item (a turn cancelled or rewound: nothing can
/// answer them any more), returning the request ids so the caller can cancel
/// them with the runtime. Stale-on-open applies as for any withdrawal.
pub fn drain_permissions(state: &mut TuiState) -> Vec<RequestId> {
    let ids: Vec<RequestId> = state
        .decisions
        .items()
        .filter(|item| item.kind == DecisionKind::Permission)
        .filter_map(|item| match &item.subject {
            DecisionSubject::Permission(payload) => Some(payload.request_id.clone()),
            _ => None,
        })
        .collect();
    for id in &ids {
        super::withdraw_key(
            state,
            &key_of(DecisionKind::Permission, id),
            APPROVAL_NO_LONGER_PENDING,
        );
    }
    ids
}
