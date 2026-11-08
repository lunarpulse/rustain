//! Story 19.20c: private exact-input delivery and standalone approval effects.

use std::sync::Arc;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::decision_queue::{
    self, DecisionEffect, DecisionSubject, PermissionInput, PermissionPayload, Responder,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::{ApprovalOutcome, RequestId};
use crate::domain::services::approval_runtime::{
    ApprovalRuntime, ApprovalRuntimeEvent, PendingReview, ReviewScope,
};
use crate::infrastructure::runtime::event_bus::EventBus;

/// Subscribe synchronously so the first local turn cannot outrun admission.
pub fn spawn(runtime: Arc<ApprovalRuntime>, bus: Arc<EventBus>) -> tokio::task::JoinHandle<()> {
    let rx = runtime.subscribe();
    tokio::spawn(run(runtime, bus, rx))
}

async fn deliver(runtime: &ApprovalRuntime, bus: &EventBus, event: ApprovalRuntimeEvent) {
    let review = match &event {
        ApprovalRuntimeEvent::Requested {
            id,
            consent_card: None,
            ..
        } => runtime.pending_review(id, ReviewScope::AllTools).await,
        _ => None,
    };
    let _ = bus.emit_domain(AppEvent::ApprovalRuntimeEventBridged { event, review });
}

async fn run(
    runtime: Arc<ApprovalRuntime>,
    bus: Arc<EventBus>,
    mut rx: tokio::sync::broadcast::Receiver<ApprovalRuntimeEvent>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match rx.recv().await {
            Ok(event) => deliver(&runtime, &bus, event).await,
            Err(RecvError::Lagged(_)) => {
                // Lost Requested events must not strand a scheduler at approval.
                for pending in runtime.pending_tool_requests(ReviewScope::AllTools).await {
                    deliver(
                        &runtime,
                        &bus,
                        ApprovalRuntimeEvent::Requested {
                            id: pending.id,
                            source: pending.source,
                            tool: pending.tool,
                            risk: pending.risk,
                            consent_card: None,
                        },
                    )
                    .await;
                }
            }
            Err(RecvError::Closed) => break,
        }
    }
}

/// Pure reducer: exact snapshots enter only from the private bridge, never raw events.
/// A request is one item of the session decision queue (keyed by its request id);
/// RU-6 decides whether its frame takes the keyboard (Story 21.6 6-B).
pub fn apply(state: &mut TuiState, event: ApprovalRuntimeEvent, review: Option<PendingReview>) {
    match event {
        ApprovalRuntimeEvent::Requested {
            id,
            source,
            tool,
            risk,
            consent_card,
        } => {
            if decision_queue::holds_request(state, &id) {
                return;
            }
            let payload = if let Some(review) = review {
                PermissionPayload::new(
                    review.id,
                    review.source,
                    review.tool,
                    review.risk,
                    PermissionInput::Ready(review.input),
                )
            } else if let Some(card) = consent_card {
                // Sender consent retains its existing permission outcomes and card text.
                PermissionPayload::new(
                    id,
                    source,
                    tool,
                    risk,
                    PermissionInput::Ready(Arc::new(serde_json::Value::String(card))),
                )
            } else {
                // It was already resolved before the snapshot was acquired.
                return;
            };
            decision_queue::raise_permission(state, payload, Responder::Local);
            state.needs_redraw = true;
        }
        ApprovalRuntimeEvent::Resolved { id, .. } | ApprovalRuntimeEvent::Cancelled { id, .. } => {
            // Our own answer already removed the item; a request settled elsewhere
            // leaves its open frame stale (Close only) or drops it from the queue.
            decision_queue::withdraw_request(state, &id);
            state.needs_redraw = true;
        }
    }
}

/// The real key-dispatch effect consumer, shared by the loop and front-door tests.
/// The frame resolved its item once and left the payload in the queue for this arm.
pub async fn handle_action(state: &mut TuiState, action: &InputAction, runtime: &ApprovalRuntime) {
    let effect = match action {
        InputAction::PermissionAllow => DecisionEffect::PermissionAllowOnce,
        InputAction::PermissionSessionAllow => DecisionEffect::PermissionAllowSession,
        InputAction::PermissionAlwaysAllow => DecisionEffect::PermissionAllowAlways,
        InputAction::PermissionDeny => DecisionEffect::PermissionDeny,
        InputAction::FeedbackInputSubmit => DecisionEffect::PermissionDenyFeedback,
        _ => return,
    };
    let Some(DecisionSubject::Permission(payload)) = state.decisions.take_resolved() else {
        return;
    };
    let Some(outcome) = payload.outcome(effect) else {
        return;
    };
    let feedback = match &outcome {
        ApprovalOutcome::Reject {
            feedback: Some(text),
        } => Some(text.clone()),
        _ => None,
    };
    let _ = runtime
        .resolve(
            &payload.request_id,
            outcome,
            crate::domain::models::PermissionResolution::Operator,
        )
        .await;
    if let Some(feedback) = feedback {
        crate::adapters::tui::notifications::warning(
            state,
            crate::adapters::tui::notifications::NoticeSource::Approval,
            crate::domain::services::permission_chain::format_feedback_message(&feedback),
        );
    }
    state.needs_redraw = true;
}
