//! Decision kinds, payloads and the per-kind policy table (Story 21.6 A2, A3).
//!
//! One row per kind: title, deferrability, the meaning of `Esc` and the rail.
//! Every rail key text is generated from the registry's rows of the kind's
//! context (a child of `Context::Decision`), so a key the rail paints is a key
//! the handler honours (Story 21.3).

use super::artifact::{PendingArtifactCard, PendingPeerAdd};
use super::confirm::{ExportOverwritePayload, ForkPayload, RewindPayload};
use super::delegation::{AgentKillPayload, PendingDelegationCard, PendingSpawnGate};
use super::memory::{ConsolidationPayload, PendingForgetCard};
use super::permission::{ConsentPayload, PermissionPayload};
use super::plan::{
    CancelPlanPayload, PlanApprovalPayload, PlanCardPayload, PlanDeviationPayload,
    SkipCascadePayload,
};
use super::question::QuestionPayload;
use super::retract::TeamRetractPayload;
use super::skill_trust::SkillTrustPayload;
use super::{DecisionItem, keys};
use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::table_permission::PermissionCmd;
use crate::adapters::tui::keymap::{self, Context};
use crate::domain::models::ImageAttachment;
use crate::domain::models::visual::DeleteConfirmTarget;

/// The decision kinds migrated so far (21-1's interim kinds, 6-B's permission family,
/// 6-C's cards).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionKind {
    Recovery,
    Carryover,
    Budget,
    ContextWarning,
    LargeImage,
    DeleteConversations,
    /// A tool call waits for the operator (both hosts).
    Permission,
    /// An a2a sender's request waits for the operator's consent (attached writer).
    Consent,
    /// The agent asked the user a question (AskUserQuestion).
    Question,
    /// A workspace skill waits for the user's trust (model- or slash-driven).
    SkillTrust,
    /// A plan waits for approval (the plan-mode overlay's successor).
    PlanApproval,
    /// The inline plan card waits for approve / edit / reject.
    PlanCard,
    /// A running plan deviated and needs reapproval.
    PlanDeviation,
    /// Skipping a task has dependants: skip them too, continue anyway or cancel.
    SkipCascade,
    /// Cancelling a running plan waits for confirmation.
    CancelPlan,
    /// A task has a delegation suggestion.
    Delegation,
    /// A `/fanout` above the learned threshold waits for confirmation.
    SpawnGate,
    /// Killing an agent and its subtree waits for confirmation.
    AgentKill,
    /// A memory consolidation waits for promote-all / decline (both hosts).
    Consolidation,
    /// `/memory forget` waits for the operator to choose what to purge.
    Forget,
    /// `/artifact apply` waits for its confirmed workspace write.
    ArtifactApply,
    /// `/artifact resolve` waits for its confirmed report.
    ArtifactResolve,
    /// `/peer add` waits for the pin confirm.
    PeerAdd,
    /// `/team retract` (or the auto-response retract) waits for confirm (both hosts).
    TeamRetract,
    /// A fork waits for confirm.
    Fork,
    /// A rewind waits for confirm.
    Rewind,
    /// An export whose target exists waits for the overwrite decision.
    ExportOverwrite,
    /// An agent's proposed message waits for Send / Review / Dismiss (own tab only).
    TeamProposal,
    /// A peer's pending response (a draft to approve, or a request to answer) waits (attached).
    RecipientDraftApproval,
    /// An MCP task's input-request ticket waits for the operator's answer (Accumulate).
    Ticket,
    /// A peer's reply was observed (Accumulate).
    ReplyArrived,
}

/// What `Esc` does on a kind (RU-5, LR-3, A7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscPolicy {
    /// Set the item aside: it stays queued and counted, never decided.
    Defer,
    /// The kind's safe default is a real, existing effect.
    Resolve(DecisionEffect),
}

impl DecisionKind {
    /// The registry context whose rows (and `Decision` parent rows) resolve this
    /// kind's keys (its default sub-state: a question's field closed, a skill's trust
    /// prompt rather than its reader).
    #[must_use]
    pub fn context(self) -> Context {
        match self {
            Self::Permission => Context::Permission,
            Self::Consent => Context::Consent,
            Self::Question => Context::QuestionIdle,
            Self::SkillTrust => Context::SkillTrust,
            Self::PlanApproval => Context::PlanApproval,
            Self::PlanCard | Self::PlanDeviation => Context::PlanDecision,
            Self::SkipCascade => Context::SkipCascade,
            Self::CancelPlan
            | Self::AgentKill
            | Self::Consolidation
            | Self::ArtifactApply
            | Self::ArtifactResolve
            | Self::PeerAdd
            | Self::TeamRetract
            | Self::Fork => Context::DecisionConfirm,
            Self::Forget => Context::Forget,
            Self::Rewind => Context::Rewind,
            Self::ExportOverwrite => Context::ExportOverwrite,
            Self::Delegation => Context::Delegation,
            Self::SpawnGate => Context::SpawnGate,
            Self::TeamProposal => Context::TeamProposal,
            Self::RecipientDraftApproval => Context::RecipientDraft,
            Self::Ticket => Context::Ticket,
            Self::ReplyArrived => Context::ReplyArrived,
            _ => Context::Decision,
        }
    }

    /// Frame title (also the breadcrumb kind).
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Recovery => "Session recovery",
            Self::Carryover => "Carry over context",
            Self::Budget => "Daily budget",
            Self::ContextWarning => "Context window",
            Self::LargeImage => "Large image",
            Self::DeleteConversations => "Delete conversations",
            Self::Permission => "Permission",
            Self::Consent => "Peer consent",
            Self::Question => "Question",
            Self::SkillTrust => "Skill trust",
            Self::PlanApproval => "Plan approval",
            Self::PlanCard => "Plan card",
            Self::PlanDeviation => "Plan deviation",
            Self::SkipCascade => "Skip cascade",
            Self::CancelPlan => "Cancel plan",
            Self::Delegation => "Delegate task",
            Self::SpawnGate => "Spawn gate",
            Self::AgentKill => "Kill agent",
            Self::Consolidation => "Memory consolidation",
            Self::Forget => "Forget memory",
            Self::ArtifactApply => "Apply artifact",
            Self::ArtifactResolve => "Resolve artifact",
            Self::PeerAdd => "Add peer",
            Self::TeamRetract => "Retract",
            Self::Fork => "Fork",
            Self::Rewind => "Rewind",
            Self::ExportOverwrite => "Export overwrite",
            Self::TeamProposal => "Team proposal",
            Self::RecipientDraftApproval => "Recipient draft",
            Self::Ticket => "Ticket",
            Self::ReplyArrived => "Reply arrived",
        }
    }

    /// The stack frame's title: `Decision: <kind title>`.
    #[must_use]
    pub fn frame_title(self) -> &'static str {
        match self {
            Self::Recovery => "Decision: Session recovery",
            Self::Carryover => "Decision: Carry over context",
            Self::Budget => "Decision: Daily budget",
            Self::ContextWarning => "Decision: Context window",
            Self::LargeImage => "Decision: Large image",
            Self::DeleteConversations => "Decision: Delete conversations",
            Self::Permission => "Decision: Permission",
            Self::Consent => "Decision: Peer consent",
            Self::Question => "Decision: Question",
            Self::SkillTrust => "Decision: Skill trust",
            Self::PlanApproval => "Decision: Plan approval",
            Self::PlanCard => "Decision: Plan card",
            Self::PlanDeviation => "Decision: Plan deviation",
            Self::SkipCascade => "Decision: Skip cascade",
            Self::CancelPlan => "Decision: Cancel plan",
            Self::Delegation => "Decision: Delegate task",
            Self::SpawnGate => "Decision: Spawn gate",
            Self::AgentKill => "Decision: Kill agent",
            Self::Consolidation => "Decision: Memory consolidation",
            Self::Forget => "Decision: Forget memory",
            Self::ArtifactApply => "Decision: Apply artifact",
            Self::ArtifactResolve => "Decision: Resolve artifact",
            Self::PeerAdd => "Decision: Add peer",
            Self::TeamRetract => "Decision: Retract",
            Self::Fork => "Decision: Fork",
            Self::Rewind => "Decision: Rewind",
            Self::ExportOverwrite => "Decision: Export overwrite",
            Self::TeamProposal => "Decision: Team proposal",
            Self::RecipientDraftApproval => "Decision: Recipient draft",
            Self::Ticket => "Decision: Ticket",
            Self::ReplyArrived => "Decision: Reply arrived",
        }
    }

    /// `Esc` on the kind: defer, or resolve its safe default (Recovery continues,
    /// Delete cancels).
    #[must_use]
    pub fn esc(self) -> EscPolicy {
        match self {
            Self::Recovery => EscPolicy::Resolve(DecisionEffect::RecoveryContinue),
            Self::DeleteConversations => EscPolicy::Resolve(DecisionEffect::CancelDelete),
            _ => EscPolicy::Defer,
        }
    }

    /// True when `Esc` means "Later" (the item stays pending, never destructive).
    #[must_use]
    pub fn deferrable(self) -> bool {
        self.esc() == EscPolicy::Defer
    }

    /// The painted rail: `(key, label)` pairs, exactly the keys the handler honours. The
    /// key text is generated from the rows of the kind's context (Story 21.3 AC15). A
    /// kind with sub-states paints its default one here; [`rail_of`] paints an item's own.
    #[must_use]
    pub fn rail(self) -> Vec<(String, &'static str)> {
        let key = |command: OverlayCmd| keymap::render::rail(Context::Decision, command);
        match self {
            Self::Recovery => vec![
                (
                    format!(
                        "[{}]",
                        keymap::render::rail_pair(
                            Context::Decision,
                            OverlayCmd::PaneEnter,
                            OverlayCmd::PaneEsc
                        )
                    ),
                    "Continue",
                ),
                (key(OverlayCmd::PaneNo), "New session"),
            ],
            Self::Carryover => vec![
                (key(OverlayCmd::PaneYes), "Carry over a summary"),
                (key(OverlayCmd::PaneNo), "Start fresh without it"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::Budget => vec![
                (key(OverlayCmd::PaneYes), "Continue"),
                (key(OverlayCmd::PaneSwitch), "Switch model"),
                (key(OverlayCmd::PanePause), "Pause until tomorrow"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::ContextWarning => vec![
                // Letters fold case in the pane's resolver; the rail paints the capital.
                (key(OverlayCmd::PaneCompact).to_uppercase(), "Compact"),
                (key(OverlayCmd::PaneNo), "Start fresh"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::LargeImage => vec![
                (key(OverlayCmd::PaneYes), "Attach anyway"),
                (key(OverlayCmd::PaneNo), "Cancel"),
                (key(OverlayCmd::PaneEsc), "Later"),
            ],
            Self::DeleteConversations => vec![
                (key(OverlayCmd::PaneYes), "Delete"),
                (key(OverlayCmd::PaneNo), "Cancel"),
                (key(OverlayCmd::PaneEsc), "Cancel"),
            ],
            Self::Permission => {
                let permission =
                    |command: PermissionCmd| keymap::render::rail(Context::Permission, command);
                vec![
                    (permission(PermissionCmd::AllowOnce), "Allow"),
                    (permission(PermissionCmd::AllowSession), "Session"),
                    (permission(PermissionCmd::AllowAlways), "Always"),
                    (permission(PermissionCmd::Deny), "Deny"),
                    (
                        permission(PermissionCmd::DenyWithFeedback),
                        "Deny + feedback",
                    ),
                    (permission(PermissionCmd::Review), "Review"),
                ]
            }
            Self::Consent => {
                let consent = |command: CardCmd| keymap::render::rail(Context::Consent, command);
                vec![
                    (consent(CardCmd::Yes), "Allow once"),
                    (consent(CardCmd::Always), "Always allow"),
                    (consent(CardCmd::No), "Decline"),
                ]
            }
            Self::Question => {
                let idle = |command: CardCmd| keymap::render::rail(Context::QuestionIdle, command);
                vec![
                    (idle(CardCmd::Answer), "Answer"),
                    (idle(CardCmd::Skip), "Skip question"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::SkillTrust => {
                let trust = |command: CardCmd| keymap::render::rail(Context::SkillTrust, command);
                vec![
                    (trust(CardCmd::Yes), "Trust"),
                    (trust(CardCmd::No), "Decline"),
                    (trust(CardCmd::Inspect), "Inspect"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::PlanApproval => {
                let plan = |command: CardCmd| keymap::render::rail(Context::PlanApproval, command);
                vec![
                    (plan(CardCmd::Yes), "Approve"),
                    (plan(CardCmd::Always), "Approve + auto-edit"),
                    (plan(CardCmd::No), "Reject"),
                    (plan(CardCmd::Edit), "Edit"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::PlanCard | Self::PlanDeviation => {
                let plan = |command: CardCmd| keymap::render::rail(Context::PlanDecision, command);
                // The deviation card kept its captured wording.
                let approve = if matches!(self, Self::PlanDeviation) {
                    "Approve revised"
                } else {
                    "Approve"
                };
                vec![
                    (plan(CardCmd::Yes), approve),
                    (plan(CardCmd::Edit), "Edit"),
                    (plan(CardCmd::No), "Reject"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::SkipCascade => {
                let skip = |command: CardCmd| keymap::render::rail(Context::SkipCascade, command);
                vec![
                    (skip(CardCmd::CascadeSkip), "Skip cascade"),
                    (skip(CardCmd::Continue), "Continue anyway"),
                    (skip(CardCmd::No), "Cancel skip"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::CancelPlan | Self::AgentKill => {
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                // The cancel-plan card kept its captured wording.
                let (yes, no) = if matches!(self, Self::CancelPlan) {
                    ("Cancel all", "Keep running")
                } else {
                    ("Confirm", "Decline")
                };
                vec![
                    (confirm(CardCmd::Yes), yes),
                    (confirm(CardCmd::No), no),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::Delegation => {
                let delegation =
                    |command: CardCmd| keymap::render::rail(Context::Delegation, command);
                vec![
                    (delegation(CardCmd::Delegate), "Delegate"),
                    (delegation(CardCmd::RunLocal), "Run locally"),
                    (delegation(CardCmd::CancelPlan), "Cancel plan"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::SpawnGate => {
                let gate = |command: CardCmd| keymap::render::rail(Context::SpawnGate, command);
                vec![
                    (gate(CardCmd::Confirm), "Confirm"),
                    (gate(CardCmd::Cap), "Cap"),
                    (gate(CardCmd::No), "Cancel"),
                    (
                        keymap::render::rail_arrows(
                            Context::SpawnGate,
                            CardCmd::Left,
                            CardCmd::Right,
                        ),
                        "adjust",
                    ),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::Consolidation => {
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                // The consolidation card kept its captured wording.
                vec![
                    (confirm(CardCmd::Yes), "promote all"),
                    (confirm(CardCmd::No), "decline"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::Forget => {
                let forget = |command: CardCmd| keymap::render::rail(Context::Forget, command);
                vec![
                    (
                        keymap::render::rail_arrows(Context::Forget, CardCmd::Up, CardCmd::Down),
                        "navigate",
                    ),
                    (forget(CardCmd::Toggle), "toggle"),
                    (forget(CardCmd::Yes), "forget selected"),
                    (forget(CardCmd::No), "cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::ArtifactApply | Self::ArtifactResolve => {
                use crate::adapters::tui::widgets::apply_card::{
                    APPLY_CARD_BINDINGS, RESOLVE_CARD_BINDINGS,
                };
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                let labels = if matches!(self, Self::ArtifactApply) {
                    &APPLY_CARD_BINDINGS
                } else {
                    &RESOLVE_CARD_BINDINGS
                };
                vec![
                    (confirm(CardCmd::Yes), labels[0].label),
                    (confirm(CardCmd::No), labels[1].label),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::PeerAdd => {
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                vec![
                    (confirm(CardCmd::Yes), "Confirm and pin"),
                    (confirm(CardCmd::No), "Cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::TeamRetract => {
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                vec![
                    (confirm(CardCmd::Yes), "Retract"),
                    (confirm(CardCmd::No), "Cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::Fork => {
                let confirm =
                    |command: CardCmd| keymap::render::rail(Context::DecisionConfirm, command);
                vec![
                    (confirm(CardCmd::Yes), "Fork"),
                    (confirm(CardCmd::No), "Cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::Rewind => {
                let rewind = |command: CardCmd| keymap::render::rail(Context::Rewind, command);
                vec![
                    (rewind(CardCmd::Yes), "Rewind"),
                    (rewind(CardCmd::ForkInstead), "Fork instead"),
                    (rewind(CardCmd::No), "Cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::ExportOverwrite => {
                let export =
                    |command: CardCmd| keymap::render::rail(Context::ExportOverwrite, command);
                vec![
                    (export(CardCmd::Yes), "Overwrite"),
                    (export(CardCmd::No), "Cancel"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::TeamProposal => {
                let proposal =
                    |command: CardCmd| keymap::render::rail(Context::TeamProposal, command);
                vec![
                    (proposal(CardCmd::Yes), "Send"),
                    (proposal(CardCmd::Review), "Review first"),
                    (proposal(CardCmd::No), "Dismiss"),
                ]
            }
            Self::RecipientDraftApproval => {
                let recipient =
                    |command: CardCmd| keymap::render::rail(Context::RecipientDraft, command);
                vec![
                    (recipient(CardCmd::Yes), "Approve"),
                    (recipient(CardCmd::Edit), "Edit"),
                    (recipient(CardCmd::No), "Reject"),
                    (recipient(CardCmd::WriteOwn), "My own"),
                    (recipient(CardCmd::ViewItem), "View item"),
                ]
            }
            Self::Ticket => {
                let ticket = |command: CardCmd| keymap::render::rail(Context::Ticket, command);
                vec![
                    (ticket(CardCmd::Confirm), "Accept"),
                    (ticket(CardCmd::Delegate), "Decline"),
                    (ticket(CardCmd::CancelPlan), "Cancel task"),
                    (ticket(CardCmd::Edit), "Edit"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
            Self::ReplyArrived => {
                let reply = |command: CardCmd| keymap::render::rail(Context::ReplyArrived, command);
                vec![
                    (reply(CardCmd::Confirm), "Read thread"),
                    (reply(CardCmd::CancelPlan), "Dismiss"),
                    (key(OverlayCmd::PaneEsc), "Later"),
                ]
            }
        }
    }
}

/// The rail an item paints now: its kind's rail, or its open sub-context's (a question's
/// answer field, a skill's file reader, a permission's Review is painted by the frame).
#[must_use]
pub fn rail_of(item: &DecisionItem) -> Vec<(String, &'static str)> {
    // `Question` is the field-open context; the field-closed one is the kind's default.
    match keys::context_of(item) {
        Context::DecisionCompose if item.kind == DecisionKind::Ticket => vec![
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Send),
                "Accept",
            ),
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Back),
                "Back",
            ),
        ],
        Context::DecisionCompose => vec![
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Send),
                "Send",
            ),
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Back),
                "Back",
            ),
        ],
        Context::Question => vec![
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Send),
                "Send",
            ),
            (
                keymap::render::rail(Context::Question, CardCmd::Skip),
                "Skip question",
            ),
            (
                keymap::render::rail(Context::DecisionCompose, CardCmd::Back),
                "Back",
            ),
        ],
        Context::SkillTrustInspect => vec![
            (
                keymap::render::rail(Context::SkillTrustInspect, CardCmd::Back),
                "Back",
            ),
            (
                format!(
                    "[{}]",
                    keymap::render::rail_pair(
                        Context::Decision,
                        OverlayCmd::PaneScrollUp,
                        OverlayCmd::PaneScrollDown
                    )
                ),
                "scroll",
            ),
        ],
        // A disarmed retract (the confirm-time read did not resolve, or the item is already
        // removed) answers `[n]` only: `[y]` is never painted, never a confirm.
        _ => match &item.subject {
            DecisionSubject::RecipientDraft(payload)
                if matches!(
                    payload.stage,
                    super::recipient::RecipientDraftStage::AwaitingOperator
                ) =>
            {
                let recipient =
                    |command: CardCmd| keymap::render::rail(Context::RecipientDraft, command);
                vec![
                    (recipient(CardCmd::WriteOwn), "My own"),
                    (recipient(CardCmd::No), "Decline"),
                    (recipient(CardCmd::ViewItem), "View item"),
                ]
            }
            DecisionSubject::TeamRetract(payload) if !payload.armed => item
                .kind
                .rail()
                .into_iter()
                .filter(|(_, label)| *label != "Retract")
                .collect(),
            DecisionSubject::PeerAdd(pending) if pending.reach_refresh => item
                .kind
                .rail()
                .into_iter()
                .map(|(key, label)| {
                    (
                        key,
                        if label == "Confirm and pin" {
                            "Update the address"
                        } else {
                            label
                        },
                    )
                })
                .collect(),
            _ => item.kind.rail(),
        },
    }
}

/// The existing effect a rail key executes — exactly once (the item is resolved
/// before the effect is returned), unless the effect is marked non-resolving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionEffect {
    RecoveryContinue,
    RecoveryNewSession,
    /// Compact into a summary and carry it into a fresh tab.
    CarryoverAccept,
    /// Open a fresh tab without a summary.
    CarryoverDecline,
    BudgetContinue,
    BudgetSwitchCheaper,
    BudgetPause,
    ContextCompact,
    AttachLargeImage,
    CancelLargeImage,
    ConfirmDelete,
    CancelDelete,
    /// Allow this one tool call.
    PermissionAllowOnce,
    /// Allow this tool for the session.
    PermissionAllowSession,
    /// Always allow this tool (persisted).
    PermissionAllowAlways,
    /// Deny the tool call.
    PermissionDeny,
    /// Deny with the feedback typed into the item's field (the text rides in the payload).
    PermissionDenyFeedback,
    /// Allow the sender's request once (attached).
    ConsentOnce,
    /// Always allow the sender's tool and remember it (attached).
    ConsentAlways,
    /// Decline the sender's request (attached).
    ConsentDecline,
    /// Send the question's answer (the field's text rides in the payload).
    QuestionSend,
    /// Skip the question: the waiting turn gets no answer.
    QuestionSkip,
    /// Trust the skill and activate it.
    SkillTrustAccept,
    /// Decline the skill.
    SkillTrustDecline,
    /// Read the skill file (non-resolving: the item stays queued).
    SkillTrustInspect,
    PlanApproveNormal,
    PlanApproveAutoEdit,
    PlanReject,
    /// Open the plan in the editor (non-resolving: the item stays queued).
    PlanRevise,
    PlanCardApprove,
    PlanCardReject,
    PlanCardEdit,
    PlanDeviationApprove,
    PlanDeviationEdit,
    PlanDeviationReject,
    /// Skip the task and everything that depends on it.
    SkipCascadeSkip,
    /// Skip the task only.
    SkipCascadeContinue,
    /// Keep the task.
    SkipCascadeCancel,
    CancelPlanConfirm,
    CancelPlanDecline,
    DelegateTask,
    RunTaskLocally,
    DelegationCardCancel,
    SpawnGateConfirm,
    /// Cancel the spawn: nothing is spawned.
    SpawnGateCancel,
    AgentKillConfirm,
    AgentKillDecline,
    /// Promote every proposal of the consolidation.
    ConsolidationAccept,
    /// Decline the consolidation: nothing is promoted.
    ConsolidationDecline,
    /// Forget the selected memories.
    ForgetAccept,
    /// Cancel the forget: nothing is purged.
    ForgetDecline,
    /// Apply the patch (or record the report).
    ArtifactAccept,
    /// Cancel the artifact decision.
    ArtifactDecline,
    /// Pin the peer.
    PeerAddConfirm,
    PeerAddDecline,
    /// Mark the item retracted.
    TeamRetractConfirm,
    TeamRetractDecline,
    TicketAccept,
    TicketDecline,
    TicketCancel,
    ReplyRead,
    ReplyDismiss,
    TeamProposalSend,
    TeamProposalDismiss,
    RecipientApprove,
    RecipientReject,
    RecipientSend,
    RecipientView,
    ForkConfirm,
    ForkCancel,
    RewindConfirm,
    RewindForkInstead,
    RewindCancel,
    ExportOverwriteConfirm,
    ExportOverwriteCancel,
}

impl DecisionEffect {
    /// The host action that runs the effect (the delete/image effects, the permission
    /// family and the cards keep their existing arms; consent is an attached frame; the
    /// rest reach the host as `InputAction::Decision` and take their payload with
    /// `take_resolved`).
    #[must_use]
    pub fn into_action(self) -> InputAction {
        match self {
            Self::AttachLargeImage => InputAction::ImageConfirmAttach,
            Self::CancelLargeImage => InputAction::ImageConfirmCancel,
            Self::ConfirmDelete => InputAction::ConfirmDelete,
            Self::CancelDelete => InputAction::CancelDelete,
            Self::PermissionAllowOnce => InputAction::PermissionAllow,
            Self::PermissionAllowSession => InputAction::PermissionSessionAllow,
            Self::PermissionAllowAlways => InputAction::PermissionAlwaysAllow,
            Self::PermissionDeny => InputAction::PermissionDeny,
            Self::PermissionDenyFeedback => InputAction::FeedbackInputSubmit,
            // Consent items exist only on an attached writer, which sends the frame itself.
            Self::ConsentOnce | Self::ConsentAlways | Self::ConsentDecline => InputAction::Consumed,
            Self::SkillTrustAccept => InputAction::SkillTrustAccept,
            Self::SkillTrustDecline => InputAction::SkillTrustDecline,
            Self::SkillTrustInspect => InputAction::SkillTrustInspect,
            Self::PlanApproveNormal => InputAction::PlanApproveNormal,
            Self::PlanApproveAutoEdit => InputAction::PlanApproveAutoEdit,
            Self::PlanReject => InputAction::PlanReject,
            Self::PlanRevise => InputAction::PlanRevise,
            Self::PlanCardApprove => InputAction::PlanCardApprove,
            Self::PlanCardReject => InputAction::PlanCardReject,
            Self::PlanCardEdit => InputAction::PlanCardEdit,
            Self::DelegateTask => InputAction::DelegateTask,
            Self::RunTaskLocally => InputAction::RunTaskLocally,
            Self::DelegationCardCancel => InputAction::DelegationCardCancel,
            Self::SpawnGateConfirm => InputAction::SpawnGateConfirm,
            Self::ConsolidationAccept => InputAction::ConsolidateAcceptAll,
            Self::ConsolidationDecline => InputAction::ConsolidateDeclineAll,
            Self::ForgetAccept => InputAction::ForgetAcceptAll,
            Self::ForgetDecline => InputAction::ForgetDeclineAll,
            Self::ArtifactAccept => InputAction::ApplyCardAccept,
            Self::ArtifactDecline => InputAction::ApplyCardDecline,
            Self::PeerAddConfirm => InputAction::PeerAddConfirm,
            Self::PeerAddDecline => InputAction::PeerAddDecline,
            Self::TeamRetractConfirm => InputAction::TeamRetractConfirm,
            Self::TeamRetractDecline => InputAction::TeamRetractDecline,
            Self::ForkConfirm => InputAction::ForkConfirm,
            Self::ForkCancel => InputAction::ForkCancel,
            Self::RewindConfirm => InputAction::RewindConfirm,
            Self::RewindForkInstead => InputAction::RewindForkInstead,
            Self::RewindCancel => InputAction::RewindCancel,
            Self::ExportOverwriteConfirm => InputAction::ConfirmExportOverwrite,
            Self::ExportOverwriteCancel => InputAction::CancelExportOverwrite,
            other => InputAction::Decision(other),
        }
    }

    /// True when the effect leaves its item queued (a sub-context opens, nothing is decided).
    #[must_use]
    pub fn keeps_item(self) -> bool {
        matches!(
            self,
            Self::SkillTrustInspect | Self::PlanRevise | Self::RecipientView
        )
    }
}

/// The payload a decision carries: what used to live in a singleton slot
/// (`pending_large_image`, `pending_delete`, `pending_permission`, the attached
/// approvals and consent queue, and 6-C's question / skill / plan / delegation slots),
/// moved here, never copied (AD-2110).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionSubject {
    /// The question text is the whole decision.
    Text,
    LargeImage(ImageAttachment),
    Delete(DeleteConfirmTarget),
    Permission(Box<PermissionPayload>),
    Consent(Box<ConsentPayload>),
    Question(Box<QuestionPayload>),
    SkillTrust(Box<SkillTrustPayload>),
    PlanApproval(Box<PlanApprovalPayload>),
    PlanCard(Box<PlanCardPayload>),
    PlanDeviation(Box<PlanDeviationPayload>),
    SkipCascade(Box<SkipCascadePayload>),
    CancelPlan(Box<CancelPlanPayload>),
    Delegation(Box<PendingDelegationCard>),
    SpawnGate(Box<PendingSpawnGate>),
    AgentKill(Box<AgentKillPayload>),
    Consolidation(Box<ConsolidationPayload>),
    Forget(Box<PendingForgetCard>),
    /// An apply or resolve confirm (the item's kind says which).
    Artifact(Box<PendingArtifactCard>),
    PeerAdd(Box<PendingPeerAdd>),
    TeamRetract(Box<TeamRetractPayload>),
    Fork(Box<ForkPayload>),
    Rewind(Box<RewindPayload>),
    ExportOverwrite(Box<ExportOverwritePayload>),
    TeamProposal(Box<super::proposal::TeamProposalPayload>),
    RecipientDraft(Box<super::recipient::RecipientDraftPayload>),
    Ticket(Box<super::ticket::TicketPayload>),
    ReplyArrived(Box<super::reply::ReplyPayload>),
}

/// The copy a decision paints once its source settled elsewhere (A7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleCopy(pub &'static str);

/// Shown when the condition that raised a decision is gone.
pub const NO_LONGER_PENDING: StaleCopy = StaleCopy("This decision is no longer pending.");

/// Shown for a permission or consent request whose approval settled elsewhere:
/// the daemon's `ApprovalRefusal::NoLongerPending` text, verbatim.
pub const APPROVAL_NO_LONGER_PENDING: StaleCopy =
    StaleCopy("approval refused: that request is no longer pending");

/// A stale item answers nothing: its only key is Close (A7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    Live,
    Stale(StaleCopy),
}

/// The original route of a decision; never re-targeted (A2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responder {
    /// Answered through the local runtime.
    Local,
    /// Answered through a `ClientFrame` on an attached writer.
    Attached,
}
