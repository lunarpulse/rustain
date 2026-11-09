//! Rows of the decision cards (Story 21.3 AC15-AC17, A8, A9, KG-1, KG-2; 21.6 6-C..6-E).
//!
//! Every decision card is a child of `Context::Decision`: `Esc` defers, `Ctrl+C` backs,
//! `Ctrl+Q` asks to quit, `c` copies and `PgUp`/`PgDn` scroll are the parent's rows. These
//! rows are the source of every rail key (`render::rail`) and the truth the decision
//! frames are checked against (K11).
//!
//! A row with a label is a key the rail names; a row without one (scroll, edit and cursor
//! keys) is not offered as a decision.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the decision cards. One name per meaning; a context's rows bind it to its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CardCmd {
    /// `Esc`: back one frame (a sub-frame of a card, never a decision).
    Back,
    /// `Enter`: send the text (feedback, answer).
    Send,
    /// `Enter` on a question whose answer field is closed: open the field.
    Answer,
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
    /// Write a response without using the draft.
    WriteOwn,
    /// Open the associated item reader without resolving the decision.
    ViewItem,
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
    /// `a`: continue anyway (skip-cascade).
    Continue,
    /// `t`: cap the spawn at the threshold.
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

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── the frame-owned text field (permission feedback, the question answer; both hosts) ──
    b(Context::DecisionCompose, &[Key::Enter], CardCmd::Send, "Send").te().does("Send the text and decide with it"),
    b(Context::DecisionCompose, &[Key::Esc], CardCmd::Back, "Back").te().does("Leave the field and return to the undecided request"),
    b(Context::DecisionCompose, &[Key::Ctrl('c')], CardCmd::Back, "").te().does("Leave the field without deciding (back one frame)"),
    // ── AskUserQuestion: the answer field is open, then closed (Esc leaves it) ──
    k(Context::Question, &[Key::Ctrl('d')], CardCmd::Skip, "Skip question").does("Skip the question: the agent receives no answer"),
    k(Context::QuestionIdle, &[Key::Enter], CardCmd::Answer, "Answer").does("Open the answer field"),
    k(Context::QuestionIdle, &[Key::Ctrl('d')], CardCmd::Skip, "Skip question").does("Skip the question: the agent receives no answer"),
    // ── skill trust ──
    k(Context::SkillTrust, &[Key::Char('y')], CardCmd::Yes, "Trust").does("Trust the skill and activate it"),
    k(Context::SkillTrust, &[Key::Char('n')], CardCmd::No, "Decline").does("Decline the skill"),
    k(Context::SkillTrust, &[Key::Char('i')], CardCmd::Inspect, "Inspect").does("Read the skill file before deciding"),
    k(Context::SkillTrustInspect, &[Key::Esc], CardCmd::Back, "Back").does("Return to the undecided skill trust request"),
    k(Context::SkillTrustInspect, &[Key::Ctrl('c')], CardCmd::Back, "").does("Return to the undecided skill trust request (back one frame)"),
    k(Context::SkillTrustInspect, &[Key::Up], CardCmd::Up, "").does("Scroll the skill file up one line"),
    k(Context::SkillTrustInspect, &[Key::Char('k')], CardCmd::Up, "").does("Scroll the skill file up one line"),
    k(Context::SkillTrustInspect, &[Key::Down], CardCmd::Down, "").does("Scroll the skill file down one line"),
    k(Context::SkillTrustInspect, &[Key::Char('j')], CardCmd::Down, "").does("Scroll the skill file down one line"),
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
    k(Context::SkipCascade, &[Key::Char('a')], CardCmd::Continue, "Continue anyway").does("Skip the task only and continue its dependants"),
    k(Context::SkipCascade, &[Key::Char('n')], CardCmd::No, "Cancel skip").does("Keep the task"),
    k(Context::Delegation, &[Key::Char('d')], CardCmd::Delegate, "Delegate").does("Delegate the task to the suggested agent"),
    k(Context::Delegation, &[Key::Char('l')], CardCmd::RunLocal, "Run locally").does("Run the task in this session"),
    k(Context::Delegation, &[Key::Char('x')], CardCmd::CancelPlan, "Cancel plan").does("Cancel the plan at this task"),
    k(Context::SpawnGate, &[Key::Enter], CardCmd::Confirm, "Confirm").does("Confirm the spawn count"),
    k(Context::SpawnGate, &[Key::Char('t')], CardCmd::Cap, "Cap").does("Cap the spawn at the threshold"),
    k(Context::SpawnGate, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel the spawn: nothing is spawned"),
    k(Context::SpawnGate, &[Key::Left], CardCmd::Left, "").does("Spawn one fewer"),
    k(Context::SpawnGate, &[Key::Right], CardCmd::Right, "").does("Spawn one more"),
    // ── the y / n confirmations ──
    b(Context::DecisionConfirm, &[Key::Char('y')], CardCmd::Yes, "Confirm").does("Confirm the pending action"),
    b(Context::DecisionConfirm, &[Key::Char('n')], CardCmd::No, "Decline").does("Decline: nothing is changed"),
    k(Context::Forget, &[Key::Char('y')], CardCmd::Yes, "Forget").does("Forget the selected memories"),
    k(Context::Forget, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel: nothing is forgotten"),
    k(Context::Forget, &[Key::Char(' ')], CardCmd::Toggle, "Toggle").does("Select or unselect the focused memory"),
    k(Context::Forget, &[Key::Down], CardCmd::Down, "").does("Focus the next memory"),
    k(Context::Forget, &[Key::Up], CardCmd::Up, "").does("Focus the previous memory"),
    k(Context::Forget, &[Key::Char('j')], CardCmd::Down, "").does("Focus the next memory"),
    k(Context::Forget, &[Key::Char('k')], CardCmd::Up, "").does("Focus the previous memory"),
    k(Context::Rewind, &[Key::Char('y')], CardCmd::Yes, "Rewind").does("Rewind the conversation to this message"),
    k(Context::Rewind, &[Key::Char('f')], CardCmd::ForkInstead, "Fork instead").does("Fork a new conversation at this message instead"),
    k(Context::Rewind, &[Key::Char('n')], CardCmd::No, "Cancel").does("Cancel the rewind"),
    k(Context::ExportOverwrite, &[Key::Char('y')], CardCmd::Yes, "Overwrite").does("Overwrite the existing export file"),
    k(Context::ExportOverwrite, &[Key::Char('n')], CardCmd::No, "Cancel").does("Keep the existing file"),
    b(Context::ReplyArrived, &[Key::Enter], CardCmd::Confirm, "Read thread").does("Read the peer thread without adopting its content"),
    b(Context::ReplyArrived, &[Key::Char('x')], CardCmd::CancelPlan, "Dismiss").does("Dismiss this reply arrival"),
    b(Context::Ticket, &[Key::Enter], CardCmd::Confirm, "Accept").does("Validate and send the ticket answer"),
    b(Context::Ticket, &[Key::Char('d')], CardCmd::Delegate, "Decline").does("Decline the request; the task continues"),
    b(Context::Ticket, &[Key::Char('x')], CardCmd::CancelPlan, "Cancel task").does("Cancel the task"),
    b(Context::Ticket, &[Key::Up], CardCmd::Up, "").does("Select the previous answer field"),
    b(Context::Ticket, &[Key::Down], CardCmd::Down, "").does("Select the next answer field"),
    b(Context::Ticket, &[Key::Left], CardCmd::Left, "").does("Select the previous answer"),
    b(Context::Ticket, &[Key::Right], CardCmd::Right, "").does("Select the next answer"),
    b(Context::Ticket, &[Key::Char('e')], CardCmd::Edit, "Edit").does("Edit the selected answer"),
    // ── team proposal (agent proposes a message; empty composer) and attached consent ──
    bind(Context::TeamProposal, &[Key::Char('y')], CommandId::Card(CardCmd::Yes), "Send").does("Send the proposed message"),
    bind(Context::TeamProposal, &[Key::Char('r')], CommandId::Card(CardCmd::Review), "Review first").does("Review the proposed message"),
    bind(Context::TeamProposal, &[Key::Char('n')], CommandId::Card(CardCmd::No), "Dismiss").does("Dismiss the proposal"),
    b(Context::RecipientDraft, &[Key::Char('y')], CardCmd::Yes, "Approve").attached().does("Approve the peer draft"),
    b(Context::RecipientDraft, &[Key::Char('e')], CardCmd::Edit, "Edit").attached().does("Edit the peer draft"),
    b(Context::RecipientDraft, &[Key::Char('n')], CardCmd::No, "Reject").attached().does("Reject or decline the peer response"),
    b(Context::RecipientDraft, &[Key::Char('w')], CardCmd::WriteOwn, "My own").attached().does("Write your own response"),
    b(Context::RecipientDraft, &[Key::Char('v')], CardCmd::ViewItem, "View item").attached().does("Read the associated item without deciding"),
    bind(Context::Consent, &[Key::Char('y')], CommandId::Card(CardCmd::Yes), "Allow once").attached().does("Allow the sender's request once"),
    bind(Context::Consent, &[Key::Char('a')], CommandId::Card(CardCmd::Always), "Always allow").attached().does("Always allow this sender's tool and remember it"),
    bind(Context::Consent, &[Key::Char('n')], CommandId::Card(CardCmd::No), "Decline").attached().does("Decline the sender's request"),
];
