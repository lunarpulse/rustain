//! Decision kinds, payloads and the per-kind policy table (Story 21.6 A2, A3).
//!
//! One row per kind: title, deferrability, the meaning of `Esc` and the rail.
//! Every rail key text is generated from the registry's `Context::Decision`
//! rows, so a key the rail paints is a key the handler honours (Story 21.3).

use crate::adapters::tui::app::InputAction;
use crate::adapters::tui::keymap::table_overlay::OverlayCmd;
use crate::adapters::tui::keymap::{self, Context};
use crate::domain::models::ImageAttachment;
use crate::domain::models::visual::DeleteConfirmTarget;

/// The six interim decision kinds (21-1 A9); later slices add the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecisionKind {
    Recovery,
    Carryover,
    Budget,
    ContextWarning,
    LargeImage,
    DeleteConversations,
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
    /// key text is generated from the `Decision` rows of the registry (Story 21.3 AC15).
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
        }
    }
}

/// The existing effect a rail key executes — exactly once (the item is resolved
/// before the effect is returned).
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
}

impl DecisionEffect {
    /// The host action that runs the effect (the four delete/image effects keep
    /// their existing arms).
    #[must_use]
    pub fn into_action(self) -> InputAction {
        match self {
            Self::AttachLargeImage => InputAction::ImageConfirmAttach,
            Self::CancelLargeImage => InputAction::ImageConfirmCancel,
            Self::ConfirmDelete => InputAction::ConfirmDelete,
            Self::CancelDelete => InputAction::CancelDelete,
            other => InputAction::Decision(other),
        }
    }
}

/// The payload a decision carries: what used to live in a singleton slot
/// (`pending_large_image`, `pending_delete`), moved here, never copied (AD-2110).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionSubject {
    /// The question text is the whole decision.
    Text,
    LargeImage(ImageAttachment),
    Delete(DeleteConfirmTarget),
}

/// The copy a decision paints once its source settled elsewhere (A7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaleCopy(pub &'static str);

/// Shown when the condition that raised a decision is gone.
pub const NO_LONGER_PENDING: StaleCopy = StaleCopy("This decision is no longer pending.");

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
