//! Permission card rows (Story 21.3 AC5, KG-2, both hosts): focus
//! `Confirmation(Permission)` including the Review-open state, and the attached
//! decision cards that share its `y/s/a/n/f/r` grammar (`AttachedApprovals::key`,
//! consent / consolidation / retract arms of `attach_keys::handle_attached_key`).
//!
//! Slice 3-A describes TODAY's arms exactly, the destructive ones included:
//! `Esc` DENIES (closes Review first when it is open) and attached `Ctrl+C` on a
//! consent / consolidation / retract card is remapped to that decline. Slice 3-C
//! changes both (A8: no decision, an info cue) and rewrites these two rows. Every
//! other key is swallowed by the card and has no row.

use super::model::{Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the Permission context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionCmd {
    /// `y`: allow this one tool call (attached consent / consolidation / retract: confirm).
    AllowOnce,
    /// `s`: auto-approve this tool for the rest of the session.
    AllowSession,
    /// `a`: always allow this tool and persist the approval.
    AllowAlways,
    /// `n`: deny the tool call (decline / keep on the attached cards).
    Deny,
    /// `f`: deny and open the feedback field.
    DenyWithFeedback,
    /// `r`: open the exact-input Review reader.
    Review,
    /// `Esc` (3-A, as shipped): close Review when it is open, otherwise DENY.
    BackOrDeny,
    /// `PageUp`: page the Review reader / card up.
    ReviewPageUp,
    /// `PageDown`: page the Review reader / card down.
    ReviewPageDown,
    /// `j`: scroll the chat one line down while the card is up. Local only.
    ScrollLineDown,
    /// `k`: scroll the chat one line up while the card is up. Local only.
    ScrollLineUp,
    /// `G`: scroll the chat to the bottom while the card is up. Local only.
    ScrollToBottom,
    /// `g`: scroll the chat to the top while the card is up. Local only.
    ScrollToTop,
    /// `Ctrl+Q`: ask to quit over the card. Attached only (the local card swallows it).
    QuitRequest,
}

const fn p(keys: &'static [Key], cmd: PermissionCmd, label: &'static str) -> Binding {
    bind(Context::Permission, keys, CommandId::Permission(cmd), label)
}

const P: HelpCat = HelpCat::Permissions;

#[rustfmt::skip]
const PERMISSION: &[Binding] = &[
    p(&[Key::Char('y')], PermissionCmd::AllowOnce, "Allow").help(P, "y", "Allow tool call (this time only)"),
    p(&[Key::Char('s')], PermissionCmd::AllowSession, "Session").help(P, "s", "Session allow — auto-approve this tool for the session"),
    p(&[Key::Char('a')], PermissionCmd::AllowAlways, "Always").help(P, "a", "Always allow — persist approval to settings"),
    p(&[Key::Char('n')], PermissionCmd::Deny, "Deny").help(P, "n", "Deny tool call"),
    p(&[Key::Char('f')], PermissionCmd::DenyWithFeedback, "Deny + feedback").help(P, "f", "Deny + feedback — provide text guidance to the LLM"),
    p(&[Key::Char('r')], PermissionCmd::Review, "Review").help(P, "r", "Review").does("Open the exact tool input for review"),
    p(&[Key::Esc], PermissionCmd::BackOrDeny, "").does("Close the review when it is open; otherwise deny the tool call"),
    p(&[Key::PageUp], PermissionCmd::ReviewPageUp, "").does("Page the review reader up"),
    p(&[Key::PageDown], PermissionCmd::ReviewPageDown, "").does("Page the review reader down"),
    p(&[Key::Char('j')], PermissionCmd::ScrollLineDown, "").local().does("Scroll the chat down one line"),
    p(&[Key::Char('k')], PermissionCmd::ScrollLineUp, "").local().does("Scroll the chat up one line"),
    p(&[Key::Char('G')], PermissionCmd::ScrollToBottom, "").local().does("Scroll the chat to the bottom"),
    p(&[Key::Char('g')], PermissionCmd::ScrollToTop, "").local().does("Scroll the chat to the top"),
    p(&[Key::Ctrl('q')], PermissionCmd::QuitRequest, "").attached().does("Ask to quit"),
    p(&[Key::Ctrl('c')], PermissionCmd::BackOrDeny, "").attached().does("Decline the card (close the review first when it is open)"),
];

pub const ROWS: &[Binding] = PERMISSION;
