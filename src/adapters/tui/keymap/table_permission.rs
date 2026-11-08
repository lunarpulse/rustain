//! Permission rows (Story 21.3 AC5, KG-2; moved onto the decision frame by 21.6 6-B):
//! the keys of the `Decision: Permission` frame in both hosts.
//!
//! The context is a child of `Decision`: `Esc` (defer; with Review open, go back),
//! `Ctrl+C` (back one frame, never a decision), `Ctrl+Q` (quit request) and `PgUp` /
//! `PgDn` (scroll the body, which is the Review reader while it is open) are the
//! parent's rows. Every other key is swallowed by the frame and has no row.

use super::model::{Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the Permission context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionCmd {
    /// `y`: allow this one tool call.
    AllowOnce,
    /// `s`: auto-approve this tool for the rest of the session.
    AllowSession,
    /// `a`: always allow this tool and persist the approval.
    AllowAlways,
    /// `n`: deny the tool call.
    Deny,
    /// `f`: deny and open the feedback field.
    DenyWithFeedback,
    /// `r`: open the exact-input Review reader.
    Review,
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
];

pub const ROWS: &[Binding] = PERMISSION;
