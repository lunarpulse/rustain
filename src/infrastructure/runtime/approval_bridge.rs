//! Story 19.20c: private exact-input delivery and standalone approval effects.

use std::sync::Arc;

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::state::{FeedbackInputState, PendingPermission, TuiState};
use crate::domain::events::AppEvent;
use crate::domain::models::visual::ConfirmationType;
use crate::domain::models::{ApprovalOutcome, ApprovalScope, FocusState, OverlayType, RequestId};
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

fn another_modal_owns_focus(state: &TuiState) -> bool {
    crate::adapters::tui::app::permission_modal_owns_focus(state)
}

fn contains(state: &TuiState, id: &RequestId) -> bool {
    state
        .pending_permission
        .as_ref()
        .is_some_and(|p| &p.id == id)
        || state
            .pending_feedback_input
            .as_ref()
            .is_some_and(|p| &p.pending_permission.id == id)
        || state.permission_reader.review.as_ref() == Some(id)
        || state.permission_queue.queue.iter().any(|p| &p.id == id)
}

/// Pure reducer: exact snapshots enter only from the private bridge, never raw events.
pub fn apply(state: &mut TuiState, event: ApprovalRuntimeEvent, review: Option<PendingReview>) {
    match event {
        ApprovalRuntimeEvent::Requested {
            id,
            source,
            tool,
            risk,
            consent_card,
        } => {
            if contains(state, &id) {
                return;
            }
            let pending = if let Some(review) = review {
                PendingPermission {
                    id: review.id,
                    source: review.source,
                    tool_name: review.tool,
                    input: review.input,
                    risk: review.risk,
                }
            } else if let Some(card) = consent_card {
                // Sender consent retains its existing permission outcomes and card text.
                PendingPermission {
                    id,
                    source,
                    tool_name: tool,
                    input: Arc::new(serde_json::Value::String(card)),
                    risk,
                }
            } else {
                // It was already resolved before the snapshot was acquired.
                return;
            };
            if state.pending_permission.is_some() || another_modal_owns_focus(state) {
                state.permission_queue.push(pending);
            } else {
                state.pending_permission = Some(pending);
                state.focus =
                    FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
            }
            state.needs_redraw = true;
        }
        ApprovalRuntimeEvent::Resolved { id, .. } | ApprovalRuntimeEvent::Cancelled { id, .. } => {
            state.permission_queue.queue.retain(|p| p.id != id);
            let active = state
                .pending_permission
                .as_ref()
                .is_some_and(|p| p.id == id);
            let feedback = state
                .pending_feedback_input
                .as_ref()
                .is_some_and(|p| p.pending_permission.id == id);
            if active {
                state.pending_permission = None;
            }
            if feedback {
                state.pending_feedback_input = None;
            }
            if state.permission_reader.review.as_ref() == Some(&id) {
                state.permission_reader = Default::default();
            }
            if active || feedback {
                advance(state);
            }
            state.needs_redraw = true;
        }
    }
}

/// Keep higher confirmations and feedback ownership; never steal their focus.
pub fn advance(state: &mut TuiState) {
    crate::adapters::tui::app::advance_deferred_permission(state);
}

/// The real key-dispatch effect consumer, shared by the loop and front-door tests.
pub async fn handle_action(state: &mut TuiState, action: &InputAction, runtime: &ApprovalRuntime) {
    match action {
        InputAction::PermissionAllow
        | InputAction::PermissionDeny
        | InputAction::PermissionAlwaysAllow
        | InputAction::PermissionSessionAllow => {
            if let Some(pending) = state.pending_permission.take() {
                // The shown slot remains the pinned RequestId throughout Review.
                let outcome = match action {
                    InputAction::PermissionAllow => ApprovalOutcome::Once,
                    InputAction::PermissionAlwaysAllow => ApprovalOutcome::AlwaysAndSave {
                        scope: ApprovalScope::Tool(pending.tool_name),
                    },
                    InputAction::PermissionSessionAllow => ApprovalOutcome::AlwaysTool {
                        tool_name: pending.tool_name,
                    },
                    _ => ApprovalOutcome::Reject { feedback: None },
                };
                let _ = runtime.resolve(&pending.id, outcome).await;
                advance(state);
            }
        }
        InputAction::PermissionDenyFeedback => {
            if let Some(pending_permission) = state.pending_permission.take() {
                state.pending_feedback_input = Some(FeedbackInputState {
                    buffer: String::new(),
                    cursor: 0,
                    pending_permission,
                });
                state.focus = FocusState::Overlay(OverlayType::Confirmation(
                    ConfirmationType::PermissionFeedback,
                ));
                state.needs_redraw = true;
            }
        }
        InputAction::FeedbackInputChar(c) => {
            if let Some(fi) = state.pending_feedback_input.as_mut()
                && fi.buffer.len() + c.len_utf8() <= 2048
            {
                fi.buffer.push(*c);
                fi.cursor += 1;
                state.needs_redraw = true;
            }
        }
        InputAction::FeedbackInputBackspace => {
            if let Some(fi) = state.pending_feedback_input.as_mut() {
                fi.buffer.pop();
                fi.cursor = fi.cursor.saturating_sub(1);
                state.needs_redraw = true;
            }
        }
        InputAction::FeedbackInputSubmit => {
            if let Some(fi) = state.pending_feedback_input.take() {
                let feedback = (!fi.buffer.is_empty()).then_some(fi.buffer);
                let _ = runtime
                    .resolve(
                        &fi.pending_permission.id,
                        ApprovalOutcome::Reject {
                            feedback: feedback.clone(),
                        },
                    )
                    .await;
                if let Some(feedback) = feedback {
                    let id = RequestId::new().0;
                    state.feedback_blocks.insert(
                        id.clone(),
                        crate::domain::models::FeedbackBlock {
                            id,
                            level: crate::domain::models::FeedbackLevel::Warning,
                            message:
                                crate::domain::services::permission_chain::format_feedback_message(
                                    &feedback,
                                ),
                            actions: vec![],
                        },
                    );
                }
                advance(state);
            }
        }
        InputAction::FeedbackInputCancel => {
            if let Some(fi) = state.pending_feedback_input.take() {
                state.pending_permission = Some(fi.pending_permission);
                state.focus =
                    FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::Permission));
                state.needs_redraw = true;
            }
        }
        _ => {}
    }
}
