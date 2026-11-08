//! AskUserQuestion as a queue item (Story 21.6 6-C, A13, RU-33).
//!
//! The waiting turn's reply handle lives in the item, so a deferred question keeps its
//! turn waiting (never cancelled, never answered), and the answer lands in its own turn
//! from any tab. The answer field is frame-owned (`DecisionCompose`): it never reads or
//! writes the composer.

use tokio::sync::oneshot;

use super::handle::ReplyHandle;
use super::{Arrival, DecisionDraft, DecisionKey, DecisionKind, DecisionSubject, Raised, raise};
use crate::adapters::tui::presentation::text_field::TextField;
use crate::adapters::tui::state::TuiState;

/// Characters of the question the breadcrumb and the Needs-you list name it by.
const SUMMARY_CHARS: usize = 40;
/// The longest answer the field takes (the permission feedback's bound is 2 KiB; a
/// pasted answer may be a paragraph or two of context).
pub const ANSWER_MAX_BYTES: usize = 64 * 1024;

/// One question the agent asked the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionPayload {
    pub tool_use_id: String,
    pub question: String,
    /// The frame-owned answer field.
    pub field: TextField,
    /// The field is open and takes the keys; `Esc` closes it, `Enter` reopens it.
    pub editing: bool,
    /// The waiting turn's reply route.
    pub reply: ReplyHandle<String>,
    /// The conversation the question was asked in (breadcrumb only: it is decidable from any tab).
    pub origin_conversation: String,
}

fn summary_of(question: &str) -> String {
    let first = question.lines().next().unwrap_or_default();
    let mut summary: String = first.chars().take(SUMMARY_CHARS).collect();
    if first.chars().count() > SUMMARY_CHARS {
        summary.push('…');
    }
    summary
}

/// Raise a question (Automatic arrival: RU-6 decides whether it takes focus). Its source
/// identity is the tool use id inside its origin conversation (two tabs may use the same
/// id); the item stays decidable from any tab. The answer field starts open.
pub fn raise_question(
    state: &mut TuiState,
    origin_conversation: &str,
    tool_use_id: String,
    question: String,
    reply: oneshot::Sender<String>,
) -> Raised {
    let draft = DecisionDraft::new(DecisionKind::Question, "")
        .source(format!("{origin_conversation}\u{1f}{tool_use_id}"))
        .summary(summary_of(&question))
        .subject(DecisionSubject::Question(Box::new(QuestionPayload {
            tool_use_id,
            question,
            field: TextField::default(),
            editing: true,
            reply: ReplyHandle::new(reply),
            origin_conversation: origin_conversation.to_owned(),
        })));
    raise(state, draft, Arrival::Automatic)
}

/// `conversation_id`'s turn was cancelled or its conversation rewound: its questions can
/// not be answered any more. Each of those waiting turns gets a `RecvError` and each item
/// leaves the queue (an item whose frame is open turns stale instead and waits for
/// Close). Other conversations' questions keep waiting.
pub fn drain_questions(state: &mut TuiState, conversation_id: &str) {
    let keys: Vec<DecisionKey> = state
        .decisions
        .items()
        .filter_map(|item| match &item.subject {
            DecisionSubject::Question(payload)
                if payload.origin_conversation == conversation_id =>
            {
                payload.reply.abandon();
                Some(item.key())
            }
            _ => None,
        })
        .collect();
    for key in keys {
        super::withdraw_key(state, &key, super::NO_LONGER_PENDING);
    }
}

/// True when `item` is a question nobody waits for any more (its turn or tab is gone).
pub(super) fn is_orphan(subject: &DecisionSubject) -> bool {
    matches!(subject, DecisionSubject::Question(payload) if payload.reply.is_closed())
}

/// The answer an effect delivers: `QuestionSend` sends the field's text through the
/// reply route, anything else abandons it (the turn gets no answer).
pub(super) fn deliver(subject: DecisionSubject, send: bool) {
    if let DecisionSubject::Question(payload) = subject {
        if send {
            payload.reply.send(payload.field.as_str().to_owned());
        } else {
            payload.reply.abandon();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_6_the_summary_is_the_first_line_bounded() {
        assert_eq!(summary_of("Name?\nsecond"), "Name?");
        let long = "x".repeat(SUMMARY_CHARS + 5);
        let summary = summary_of(&long);
        assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1);
        assert!(summary.ends_with('…'));
    }
}
