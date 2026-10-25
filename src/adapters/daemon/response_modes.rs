//! Daemon-owned response-mode state.
use crate::domain::models::{Authorship, SendAuthorization};

use std::collections::HashMap;

use tokio::sync::Mutex;
pub(crate) const AUTO_RESPONSE_VISIBLE_DEADLINE_MS: i64 = 1_000;

/// Conservative default template for `notify-and-auto` when the sender override
/// carries no configured response. AC4 (`prd.md:1322`): auto-responses are
/// conservative templates, never LLM text — there is no inference path.
pub(crate) const DEFAULT_AUTO_RESPONSE_TEMPLATE: &str = "Received. Reviewing.";

/// The template an effective `notify-and-auto` policy dispatches: the configured
/// sender override when present and non-blank, else the conservative default.
pub(crate) fn auto_response_template(configured: Option<&String>) -> String {
    configured
        .filter(|response| !response.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_AUTO_RESPONSE_TEMPLATE.to_owned())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoResponseSurface {
    Pending,
    Drafting,
    Sent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AutoResponseDeadlineMiss {
    pub elapsed_ms: i64,
}

/// NFR65's observable deadline, measured from the auto-response decision. The
/// elapsed check comes FIRST: a row or placeholder that only became visible
/// after the deadline is a miss, not a pass — passing a hardcoded `true` here
/// can no longer turn the check vacuous.
pub(crate) fn auto_response_surface(
    clock: &dyn crate::domain::clock::Clock,
    started_at_ms: i64,
    row_visible: bool,
    placeholder_visible: bool,
) -> Result<AutoResponseSurface, AutoResponseDeadlineMiss> {
    let elapsed_ms = clock.wall_now_ms().saturating_sub(started_at_ms);
    if elapsed_ms >= AUTO_RESPONSE_VISIBLE_DEADLINE_MS {
        return Err(AutoResponseDeadlineMiss { elapsed_ms });
    }
    if row_visible {
        return Ok(AutoResponseSurface::Sent);
    }
    if placeholder_visible {
        return Ok(AutoResponseSurface::Drafting);
    }
    Ok(AutoResponseSurface::Pending)
}
#[derive(Clone, Debug)]
pub(crate) struct AutoResponseRetractionPlan {
    pub message: crate::domain::models::ChatMessage,
    pub event: crate::domain::models::RoomEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetractionError {
    DifferentHost,
    NotAutomatic,
    /// ⚠ Refused aloud, while the cross-host `x-rustain-items/retract` answers
    /// an already-marked item with success and no second append (Story 19.16d,
    /// `RetractOutcome::AlreadyMarked`): a local keypress on a spent control is
    /// an operator error worth telling, a repeated wire call is an ordinary
    /// retry. Same first-mark-wins rule, two front doors.
    AlreadyRetracted,
    InvalidTarget,
}

pub(crate) fn plan_auto_response_retraction(
    message: &crate::domain::models::ChatMessage,
    target_seq: u64,
    same_host_trusted_local: bool,
    retracted_at_ms: i64,
) -> Result<AutoResponseRetractionPlan, RetractionError> {
    if !same_host_trusted_local {
        return Err(RetractionError::DifferentHost);
    }
    if target_seq == 0 {
        return Err(RetractionError::InvalidTarget);
    }
    if message.send_authorization != crate::domain::models::SendAuthorization::Automatic {
        return Err(RetractionError::NotAutomatic);
    }
    if message.retracted_at_ms.is_some() {
        return Err(RetractionError::AlreadyRetracted);
    }
    let mut message = message.clone();
    message.retracted_at_ms = Some(retracted_at_ms);
    Ok(AutoResponseRetractionPlan {
        message,
        event: crate::domain::models::RoomEvent::AutoResponseRetracted {
            target_seq,
            retracted_at_ms,
        },
    })
}

pub(crate) const DRAFTING_PLACEHOLDER: &str = "[drafting...]";
pub(crate) const DRAFT_INTERRUPTED_NOTICE: &str = "[draft interrupted] The daemon restarted while this reply was being drafted; nothing was sent.";
pub(crate) const AWAITING_RESPONSE_PLACEHOLDER: &str = "[awaiting your response]";
pub(crate) const DRAFT_APPROVAL_PREFIX: &str =
    "[y] Approve  [e] Edit  [n] Reject  [write] My own\n";

pub(crate) const RAP_ONE_WAY_REFUSAL: &str = "RAP carries messages one way; no reply was sent.";

pub(crate) fn rap_one_way_notice(mode: crate::domain::models::ResponseMode) -> String {
    format!(
        "[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your {} response setting does not apply here.",
        mode.as_str()
    )
}

#[cfg(test)]
#[test]
fn rap_one_way_copy_pins_every_mode() {
    use crate::domain::models::ResponseMode;
    for mode in [
        ResponseMode::NotifyAndWait,
        ResponseMode::NotifyAndDraft,
        ResponseMode::NotifyAndAuto,
    ] {
        assert_eq!(
            rap_one_way_notice(mode),
            format!(
                "[no reply over RAP] RAP carries messages one way: nothing can be sent back, so your {} response setting does not apply here.",
                mode.as_str()
            )
        );
    }
    assert_eq!(
        RAP_ONE_WAY_REFUSAL,
        "RAP carries messages one way; no reply was sent."
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DraftState {
    Drafting {
        placeholder: String,
    },
    Ready {
        content: String,
    },
    Sent {
        content: String,
        authorship: Authorship,
        send_authorization: SendAuthorization,
        status: Option<crate::domain::models::StatusStatement>,
    },
    Rejected,
}

/// Controller phase without copying its potentially large draft/answer body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DraftPhase {
    Drafting,
    Ready,
    Settled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DraftResolution {
    Approve,
    Edit(String),
    Reject,
    WriteOwn(String),
    ShareStatus(crate::domain::models::StatusStatement),
}

#[derive(Default)]
pub(crate) struct PendingDraftController {
    states: Mutex<HashMap<String, DraftState>>,
}

impl PendingDraftController {
    pub(crate) async fn begin(&self, id: impl Into<String>) -> bool {
        let mut states = self.states.lock().await;
        let id = id.into();
        if states.contains_key(&id) {
            return false;
        }
        states.insert(
            id,
            DraftState::Drafting {
                placeholder: DRAFTING_PLACEHOLDER.to_owned(),
            },
        );
        true
    }

    pub(crate) async fn complete(&self, id: &str, content: impl Into<String>) -> bool {
        let mut states = self.states.lock().await;
        let Some(state @ DraftState::Drafting { .. }) = states.get_mut(id) else {
            return false;
        };
        *state = DraftState::Ready {
            content: content.into(),
        };
        true
    }

    pub(crate) async fn state(&self, id: &str) -> Option<DraftState> {
        self.states.lock().await.get(id).cloned()
    }

    pub(crate) async fn phase(&self, id: &str) -> Option<DraftPhase> {
        self.states.lock().await.get(id).map(|state| match state {
            DraftState::Drafting { .. } => DraftPhase::Drafting,
            DraftState::Ready { .. } => DraftPhase::Ready,
            DraftState::Sent { .. } | DraftState::Rejected => DraftPhase::Settled,
        })
    }

    pub(crate) async fn resolve(
        &self,
        id: &str,
        resolution: DraftResolution,
    ) -> Option<DraftState> {
        let mut states = self.states.lock().await;
        let state = states.get_mut(id)?;
        match state {
            DraftState::Sent { .. } | DraftState::Rejected => return Some(state.clone()),
            DraftState::Drafting { .. } => return None,
            DraftState::Ready { .. } => {}
        }
        let DraftState::Ready { content } = state else {
            unreachable!("settled and drafting states returned above")
        };
        let next = match resolution {
            DraftResolution::Approve => DraftState::Sent {
                content: std::mem::take(content),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            },
            DraftResolution::Edit(edited) => DraftState::Sent {
                content: edited,
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            },
            DraftResolution::Reject => DraftState::Rejected,
            DraftResolution::WriteOwn(human) => DraftState::Sent {
                content: human,
                authorship: Authorship::HumanWritten,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            },
            DraftResolution::ShareStatus(statement) => DraftState::Sent {
                content: statement.text(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: Some(statement),
            },
        };
        *state = next;
        Some(state.clone())
    }

    /// Evict a pending entry whose durable card never landed (or was rolled
    /// back). Without this a transient persistence failure would block every
    /// retry with "pending draft already exists".
    pub(crate) async fn abandon(&self, id: &str) {
        self.states.lock().await.remove(id);
    }

    pub(crate) async fn restore_ready(&self, id: &str, content: String) -> bool {
        let mut states = self.states.lock().await;
        let Some(state @ (DraftState::Sent { .. } | DraftState::Rejected)) = states.get_mut(id)
        else {
            return false;
        };
        *state = DraftState::Ready { content };
        true
    }
}

/// A draft row that is still awaiting operator resolution carries exactly one
/// of these two shapes; anything else (sent content, a rejected card) is
/// settled and must never be reconstructed into a fresh `Ready` state — that
/// is what makes restart/re-attach resolution idempotent (AC3).
pub(crate) fn pending_draft_content(persisted: &str) -> Option<String> {
    if persisted == AWAITING_RESPONSE_PLACEHOLDER {
        Some(String::new())
    } else {
        persisted
            .strip_prefix(DRAFT_APPROVAL_PREFIX)
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoResponseSurface, DraftResolution, DraftState, PendingDraftController, RetractionError,
        auto_response_surface, plan_auto_response_retraction,
    };
    use crate::domain::models::{Authorship, SendAuthorization};

    #[tokio::test]
    async fn draft_controller_buffers_and_preserves_agent_authorship_after_edit() {
        let controller = PendingDraftController::default();
        assert!(controller.begin("task-1").await);
        assert_eq!(
            controller.state("task-1").await,
            Some(DraftState::Drafting {
                placeholder: "[drafting...]".to_owned(),
            })
        );

        assert!(controller.complete("task-1", "generated answer").await);
        assert_eq!(
            controller
                .resolve("task-1", DraftResolution::Edit("edited answer".to_owned()))
                .await,
            Some(DraftState::Sent {
                content: "edited answer".to_owned(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            })
        );
        assert_eq!(
            controller.resolve("task-1", DraftResolution::Reject).await,
            Some(DraftState::Sent {
                content: "edited answer".to_owned(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            }),
            "a settled resolution is idempotent"
        );
    }

    #[tokio::test]
    async fn only_blank_composer_resolution_clears_agent_authorship() {
        let controller = PendingDraftController::default();
        assert!(controller.begin("task-2").await);
        assert!(controller.complete("task-2", "generated").await);
        assert_eq!(
            controller
                .resolve(
                    "task-2",
                    DraftResolution::WriteOwn("human response".to_owned()),
                )
                .await,
            Some(DraftState::Sent {
                content: "human response".to_owned(),
                authorship: Authorship::HumanWritten,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            })
        );
    }

    #[tokio::test]
    async fn approved_draft_records_agent_composition_and_human_send_decision() {
        let controller = PendingDraftController::default();
        assert!(controller.begin("approved").await);
        assert!(controller.complete("approved", "generated").await);
        assert_eq!(
            controller
                .resolve("approved", DraftResolution::Approve)
                .await,
            Some(DraftState::Sent {
                content: "generated".to_owned(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: None,
            })
        );
    }

    #[tokio::test]
    async fn reject_settles_without_content() {
        let controller = PendingDraftController::default();
        assert!(controller.begin("task-3").await);
        assert!(controller.complete("task-3", "must stay local").await);
        assert_eq!(
            controller.resolve("task-3", DraftResolution::Reject).await,
            Some(DraftState::Rejected)
        );
    }

    #[test]
    fn retraction_plan_is_same_host_once_and_preserves_the_row() {
        let mut row = crate::domain::models::ChatMessage {
            id: "auto-row".to_owned(),
            role: crate::domain::models::MessageRole::Assistant,
            content: "keep this content".to_owned(),
            authorship: crate::domain::models::Authorship::AgentComposed,
            send_authorization: crate::domain::models::SendAuthorization::Automatic,
            ..Default::default()
        };
        assert!(matches!(
            plan_auto_response_retraction(&row, 7, false, 123),
            Err(RetractionError::DifferentHost)
        ));

        let plan =
            plan_auto_response_retraction(&row, 7, true, 123).expect("same-host auto row retracts");
        assert_eq!(plan.message.id, "auto-row");
        assert_eq!(plan.message.content, "keep this content");
        assert_eq!(plan.message.retracted_at_ms, Some(123));
        assert_eq!(
            plan.event,
            crate::domain::models::RoomEvent::AutoResponseRetracted {
                target_seq: 7,
                retracted_at_ms: 123,
            }
        );

        row.retracted_at_ms = Some(123);
        assert!(matches!(
            plan_auto_response_retraction(&row, 7, true, 456),
            Err(RetractionError::AlreadyRetracted)
        ));
        row.send_authorization = crate::domain::models::SendAuthorization::HumanDecided;
        row.retracted_at_ms = None;
        assert!(matches!(
            plan_auto_response_retraction(&row, 7, true, 456),
            Err(RetractionError::NotAutomatic)
        ));
    }

    #[test]
    fn k03_retraction_authorization_is_independent_of_authorship() {
        use crate::domain::models::{Authorship, ChatMessage, SendAuthorization};
        let mut row = ChatMessage {
            authorship: Authorship::Unknown,
            send_authorization: SendAuthorization::Automatic,
            ..Default::default()
        };
        assert!(
            plan_auto_response_retraction(&row, 7, true, 123).is_ok(),
            "Unknown authorship + Automatic authorization must remain retractable"
        );
        row.authorship = Authorship::AgentComposed;
        for authorization in [SendAuthorization::Unknown, SendAuthorization::HumanDecided] {
            row.send_authorization = authorization;
            assert!(
                matches!(
                    plan_auto_response_retraction(&row, 7, true, 123),
                    Err(RetractionError::NotAutomatic)
                ),
                "AgentComposed + {authorization:?} must refuse retraction with NotAutomatic"
            );
        }
    }

    #[test]
    fn nfr65_deadline_is_observable_at_999_and_1001_ms() {
        let clock = crate::domain::clock::MockClock::at_wall_ms(999);
        assert_eq!(
            auto_response_surface(&clock, 0, false, false),
            Ok(AutoResponseSurface::Pending)
        );
        assert_eq!(
            auto_response_surface(&clock, 0, false, true),
            Ok(AutoResponseSurface::Drafting)
        );
        assert_eq!(
            auto_response_surface(&clock, 0, true, false),
            Ok(AutoResponseSurface::Sent)
        );
        clock.set_wall_anchor_ms(1_001);
        assert!(
            auto_response_surface(&clock, 0, false, false).is_err(),
            "1001ms with neither row nor placeholder breaches NFR65"
        );
        assert!(
            auto_response_surface(&clock, 0, true, false).is_err(),
            "a row first visible after the deadline is a miss, not a pass"
        );
        assert!(
            auto_response_surface(&clock, 0, false, true).is_err(),
            "a placeholder first visible after the deadline is a miss, not a pass"
        );
    }

    #[test]
    fn auto_template_prefers_the_configured_override() {
        assert_eq!(
            super::auto_response_template(Some(&"On it.".to_owned())),
            "On it."
        );
        assert_eq!(
            super::auto_response_template(Some(&"   ".to_owned())),
            super::DEFAULT_AUTO_RESPONSE_TEMPLATE
        );
        assert_eq!(
            super::auto_response_template(None),
            super::DEFAULT_AUTO_RESPONSE_TEMPLATE
        );
    }

    #[test]
    fn only_pending_rows_reconstruct() {
        assert_eq!(
            super::pending_draft_content(super::AWAITING_RESPONSE_PLACEHOLDER),
            Some(String::new())
        );
        assert_eq!(
            super::pending_draft_content(&format!("{}draft body", super::DRAFT_APPROVAL_PREFIX)),
            Some("draft body".to_owned())
        );
        assert_eq!(super::pending_draft_content("already sent"), None);
        assert_eq!(
            super::pending_draft_content("[draft rejected]\nsomething"),
            None
        );
    }

    #[tokio::test]
    async fn story19_24_share_status_is_human_decided_agent_composed_and_immutable() {
        let controller = PendingDraftController::default();
        let statement = crate::domain::models::StatusStatement::nothing();
        assert!(controller.begin("status").await);
        assert!(controller.complete("status", "").await);
        let sent = controller
            .resolve("status", DraftResolution::ShareStatus(statement.clone()))
            .await
            .unwrap();
        assert_eq!(
            sent,
            DraftState::Sent {
                content: "No status shared.".to_owned(),
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::HumanDecided,
                status: Some(statement),
            }
        );
        assert_eq!(
            controller.resolve("status", DraftResolution::Reject).await,
            Some(sent)
        );
    }
}
