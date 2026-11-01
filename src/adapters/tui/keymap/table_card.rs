//! Rows of the decision cards (Story 21.3 AC15-AC17, A8, A9, KG-1, KG-2).
//!
//! Each card owns the keys while it is the current context. The cards that 21-6 deletes
//! keep their hand-written arms (FR-21-3-b); these rows are the truth they are checked
//! against (K11) and the source of every rail and cue key (`render::rail`, `render::card_cue`).
//! `Esc` and `Ctrl+C` never decide a card: the shared `Card` parent answers both with a
//! cue (`Cue`); a card that owns a sub-frame back overrides it (`Back`).
//!
//! A row with a label is a key the cue names; a row without one (scroll, edit and cursor
//! keys) is not offered as a decision.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the decision cards. One name per meaning; a context's rows bind it to its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardCmd {
    /// `Esc` / `Ctrl+C`: no decision, one cue naming the explicit keys.
    Cue,
    /// `Esc`: back one frame (a sub-frame of a card, never a decision).
    Back,
    /// `Enter`: send the text (feedback, answer).
    Send,
    /// `Backspace`: delete the last character of the field.
    Erase,
    /// `Ctrl+D`: skip the question (the old `Esc` effect, explicit).
    Skip,
    /// `y`: the card's affirmative answer.
    Yes,
    /// `n`: the card's negative answer.
    No,
    /// `a`: approve and switch to auto-edit (plan approval) or always allow (consent).
    Always,
    /// `e`: edit / revise the plan.
    Edit,
    /// `i`: inspect the skill file.
    Inspect,
    /// `r`: review (team proposal).
    Review,
    /// `d`: delegate the task.
    Delegate,
    /// `l`: run the task locally.
    RunLocal,
    /// `x`: cancel the plan at this task.
    CancelPlan,
    /// `s`: skip the task and its dependants.
    CascadeSkip,
    /// `c`: continue anyway (skip-cascade).
    Continue,
    /// `c`: cap the spawn at the threshold.
    Cap,
    /// `Enter`: confirm the spawn gate.
    Confirm,
    /// `Left`: adjust the spawn count down.
    Left,
    /// `Right`: adjust the spawn count up.
    Right,
    /// `f`: fork instead of rewinding.
    ForkInstead,
    /// `Space`: toggle the focused forget candidate.
    Toggle,
    /// `j` / `Down`: next row.
    Down,
    /// `k` / `Up`: previous row.
    Up,
}

const fn b(ctx: Context, keys: &'static [Key], cmd: CardCmd, label: &'static str) -> Binding {
    bind(ctx, keys, CommandId::Card(cmd), label)
}

const fn k(ctx: Context, keys: &'static [Key], cmd: CardCmd, label: &'static str) -> Binding {
    bind(ctx, keys, CommandId::Card(cmd), label).local()
}

const CUE: &str = "Never decides the card: posts one cue naming the keys that do";

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── shared parent: Esc / Ctrl+C never decide ──
    b(Context::Card, &[Key::Esc], CardCmd::Cue, "").does(CUE),
    b(Context::Card, &[Key::Ctrl('c')], CardCmd::Cue, "").does(CUE),
    // ── permission feedback field (both hosts) ──
    b(Context::Feedback, &[Key::Enter], CardCmd::Send, "Send").does("Send the feedback and deny the tool call"),
    b(Context::Feedback, &[Key::Esc], CardCmd::Back, "Cancel").does("Cancel the feedback and return to the permission card"),
    b(Context::Feedback, &[Key::Backspace], CardCmd::Erase, "").does("Delete the last character of the feedback"),
    // ── AskUserQuestion ──
    k(Context::Question, &[Key::Enter], CardCmd::Send, "Send").does("Send the answer"),
    k(Context::Question, &[Key::Ctrl('d')], CardCmd::Skip, "Skip question").does("Skip the question: the agent receives no answer"),
    k(Context::Question, &[Key::Backspace], CardCmd::Erase, "").does("Delete the last character of the answer"),
    // ── skill trust ──
    k(Context::SkillTrust, &[Key::Char('y')], CardCmd::Yes, "Trust").does("Trust the skill and activate it"),
    k(Context::SkillTrust, &[Key::Char('n')], CardCmd::No, "Decline").does("Decline the skill"),
    k(Context::SkillTrust, &[Key::Char('i')], CardCmd::Inspect, "Inspect").does("Read the skill file before deciding"),
    k(Context::SkillTrust, &[Key::Char('j')], CardCmd::Down, "").does("Scroll the chat down one line"),
    k(Context::SkillTrust, &[Key::Char('k')], CardCmd::Up, "").does("Scroll the chat up one line"),
    k(Context::SkillTrustInspect, &[Key::Esc], CardCmd::Back, "Back").does("Return to the skill trust card"),
    // ── plan approval, plan card, plan deviation ──
    k(Context::PlanApproval, &[Key::Char('y')], CardCmd::Yes, "Approve").does("Approve the plan"),
    k(Context::PlanApproval, &[Key::Char('a')], CardCmd::Always, "Approve + auto-edit").does("Approve the plan and auto-approve its edits"),
    k(Context::PlanApproval, &[Key::Char('n')], CardCmd::No, "Reject").does("Reject the plan"),
    k(Context::PlanApproval, &[Key::Char('e')], CardCmd::Edit, "Edit").does("Ask the agent to revise the plan"),
    k(Context::PlanDecision, &[Key::Char('y')], CardCmd::Yes, "Approve").does("Approve the plan or the deviation"),
    k(Context::PlanDecision, &[Key::Char('e')], CardCmd::Edit, "Edit").does("Ask the agent to revise it"),
    k(Context::PlanDecision, &[Key::Char('n')], CardCmd::No, "Reject").does("Reject the plan or the deviation"),
    // ── skip cascade, delegation, spawn gate ──
    k(Context::SkipCascade, &[Key::Char('s')], CardCmd::CascadeSkip, "Skip cascade").does("Skip the task and everything that depends on it"),
    k(Context::SkipCascade, &[Key::Char('c')], CardCmd::Continue, "Continue anyway").does("Skip the task only and continue its dependants"),
    k(Context::SkipCascade, &[Key::Char('n')], CardCmd::No, "Cancel skip").does("Keep the task"),
    k(Context::Delegation, &[Key::Char('d')], CardCmd::Delegate, "Delegate").does("Delegate the task to the suggested agent"),
    k(Context::Delegation, &[Key::Char('l')], CardCmd::RunLocal, "Run locally").does("Run the task in this session"),
    k(Context::Delegation, &[Key::Char('x')], CardCmd::CancelPlan, "Cancel plan").does("Cancel the plan at this task"),
    k(Context::SpawnGate, &[Key::Enter], CardCmd::Confirm, "Confirm").does("Confirm the spawn count"),
    k(Context::SpawnGate, &[Key::Char('c')], CardCmd::Cap, "Cap").does("Cap the spawn at the threshold"),
    k(Context::SpawnGate, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel the spawn: nothing is spawned"),
    k(Context::SpawnGate, &[Key::Left], CardCmd::Left, "").does("Spawn one fewer"),
    k(Context::SpawnGate, &[Key::Right], CardCmd::Right, "").does("Spawn one more"),
    // ── the y / n confirmations ──
    bind(Context::Confirm, &[Key::Char('y')], CommandId::Card(CardCmd::Yes), "Confirm").does("Confirm the pending action"),
    bind(Context::Confirm, &[Key::Char('n')], CommandId::Card(CardCmd::No), "Decline").does("Decline: nothing is changed"),
    k(Context::Forget, &[Key::Char('y')], CardCmd::Yes, "Forget").does("Forget the selected memories"),
    k(Context::Forget, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel: nothing is forgotten"),
    k(Context::Forget, &[Key::Char(' ')], CardCmd::Toggle, "Toggle").does("Select or unselect the focused memory"),
    k(Context::Forget, &[Key::Char('j')], CardCmd::Down, "").does("Focus the next memory"),
    k(Context::Forget, &[Key::Char('k')], CardCmd::Up, "").does("Focus the previous memory"),
    k(Context::Forget, &[Key::Down], CardCmd::Down, "").does("Focus the next memory"),
    k(Context::Forget, &[Key::Up], CardCmd::Up, "").does("Focus the previous memory"),
    k(Context::Rewind, &[Key::Char('y')], CardCmd::Yes, "Rewind").does("Rewind the conversation to this message"),
    k(Context::Rewind, &[Key::Char('f')], CardCmd::ForkInstead, "Fork instead").does("Fork a new conversation at this message instead"),
    k(Context::Rewind, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel the rewind"),
    k(Context::ExportOverwrite, &[Key::Char('y')], CardCmd::Yes, "Overwrite").does("Overwrite the existing export file (Y too)"),
    k(Context::ExportOverwrite, &[Key::Char('n')], CardCmd::No, "Cancel").does("Keep the existing file (N too)"),
    // ── team proposal (agent proposes a message; empty composer) and attached consent ──
    bind(Context::TeamProposal, &[Key::Char('y')], CommandId::Card(CardCmd::Yes), "Send").does("Send the proposed message"),
    bind(Context::TeamProposal, &[Key::Char('r')], CommandId::Card(CardCmd::Review), "Review").does("Review the proposed message"),
    bind(Context::TeamProposal, &[Key::Char('n')], CommandId::Card(CardCmd::No), "Dismiss").does("Dismiss the proposal"),
    bind(Context::Consent, &[Key::Char('y')], CommandId::Card(CardCmd::Yes), "Allow once").attached().does("Allow the sender's request once"),
    bind(Context::Consent, &[Key::Char('a')], CommandId::Card(CardCmd::Always), "Always allow").attached().does("Always allow this sender's tool and remember it"),
    bind(Context::Consent, &[Key::Char('n')], CommandId::Card(CardCmd::No), "Reject").attached().does("Reject the sender's request"),
];
