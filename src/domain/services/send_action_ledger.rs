//! Story 19.21 `C1` — the pure fold of this host's sender-local send-action
//! facts: what the operator sent, to whom, and how each recipient's send
//! settled.
//!
//! One function serves both rails and both live and replayed data (19.19
//! standing rule: live and snapshot data come from one function). It reads
//! journal entries and nothing else — no network, no clock, no roster.
//!
//! # What it refuses to do
//!
//! ⛔ **Membership is read, never reconstructed.** A dispatch row belongs to a
//! recipient only through its own `{action, index}` reference, and only when
//! the row's peer is the peer the action record froze for that index. An
//! alias, a timing window, a body hash or "the newest dispatch" never decides
//! membership (`M08`). A settlement or dispatch whose action was never
//! recorded, or whose index is out of range, is ignored: a fact about nothing
//! is not evidence of anything.
//! ⛔ **No aggregate.** Each recipient keeps its own settlement; nothing here
//! counts, rolls up or summarizes the set.

use std::collections::HashMap;

use crate::domain::models::{
    DispatchAct, JournalEntry, JournalRecord, RoomEvent, SemanticMessageType, SendActionId,
    SendActionRecipient, SendSettlement,
};

/// The longest `--subject` label, in characters (Story 19.21 `AC1(b)`).
pub const MAX_SUBJECT_CHARS: usize = 64;

/// Why a `--subject` label was refused. Decided before any journal write or
/// recipient I/O, on every rail that can carry one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectRefusal {
    /// No label, or one that starts with `--` (a flag, not a label).
    Missing,
    /// The label is not a single whitespace-free token.
    NotOneToken,
    /// More than [`MAX_SUBJECT_CHARS`] characters.
    TooLong,
}

impl std::fmt::Display for SubjectRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("the subject label is missing"),
            Self::NotOneToken => f.write_str("the subject label must be one token without spaces"),
            Self::TooLong => write!(
                f,
                "the subject label is longer than {MAX_SUBJECT_CHARS} characters"
            ),
        }
    }
}

/// The one grammar of the sender-local label: one whitespace-free token, at
/// most [`MAX_SUBJECT_CHARS`] characters, not starting with `--`. ⛔ The label
/// is operator-authored: it is never derived from body bytes.
///
/// # Errors
///
/// The first rule the label breaks.
pub fn validate_subject(label: &str) -> Result<(), SubjectRefusal> {
    if label.is_empty() || label.starts_with("--") {
        return Err(SubjectRefusal::Missing);
    }
    if label.chars().any(char::is_whitespace) {
        return Err(SubjectRefusal::NotOneToken);
    }
    if label.chars().count() > MAX_SUBJECT_CHARS {
        return Err(SubjectRefusal::TooLong);
    }
    Ok(())
}

/// One recorded operator send action as the journal now reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendActionView {
    pub id: SendActionId,
    /// When the action record was appended (unix ms). `0` marks a line with no
    /// timestamp; a renderer must show that as unknown, never as 1970.
    pub recorded_at_ms: i64,
    pub message_type: SemanticMessageType,
    /// The operator-authored label, when `--subject` was given.
    pub subject: Option<String>,
    /// Operator order. The length is the **addressed count**.
    pub recipients: Vec<SendActionRecipientView>,
}

/// One addressed recipient of a recorded action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendActionRecipientView {
    pub recipient: SendActionRecipient,
    /// The recipient's typed send-time outcome, or `None` while the journal
    /// holds none (in flight, interrupted, or its append failed).
    pub settlement: Option<SendSettlement>,
    /// The `messageId` of every dispatch row that carried this recipient's
    /// `{action, index}` reference, in journal order. The correlation the
    /// board joins remote items against — ⛔ never "newest item for the peer".
    pub tasks: Vec<String>,
}

/// Fold `entries` (in `seq` order) into the recorded actions, **oldest
/// first**. A duplicate action id or a second settlement for one recipient
/// never overrides the first durable fact.
#[must_use]
pub fn fold_send_actions(entries: &[JournalEntry]) -> Vec<SendActionView> {
    let mut actions: Vec<SendActionView> = Vec::new();
    let mut by_id: HashMap<SendActionId, usize> = HashMap::new();
    for entry in entries {
        let JournalRecord::Room(event) = &entry.record else {
            continue;
        };
        match event {
            RoomEvent::TeamSendActionRecorded {
                action,
                recipients,
                message_type,
                subject,
                ..
            } => {
                if by_id.contains_key(action) {
                    continue;
                }
                by_id.insert(action.clone(), actions.len());
                actions.push(SendActionView {
                    id: action.clone(),
                    recorded_at_ms: entry.recorded_at_ms,
                    message_type: *message_type,
                    subject: subject.clone(),
                    recipients: recipients
                        .iter()
                        .map(|recipient| SendActionRecipientView {
                            recipient: recipient.clone(),
                            settlement: None,
                            tasks: Vec::new(),
                        })
                        .collect(),
                });
            }
            RoomEvent::TeamSendRecipientSettled {
                action,
                index,
                settlement,
            } => {
                if let Some(view) = recipient_mut(&mut actions, &by_id, action, *index)
                    && view.settlement.is_none()
                {
                    view.settlement = Some(settlement.clone());
                }
            }
            RoomEvent::RemoteEnvelopeDispatched {
                peer,
                task: Some(task),
                act: DispatchAct::Task,
                action: Some(reference),
                ..
            } => {
                if let Some(view) =
                    recipient_mut(&mut actions, &by_id, &reference.action, reference.index)
                    && view.recipient.peer == *peer
                    && !view.tasks.contains(task)
                {
                    view.tasks.push(task.clone());
                }
            }
            _ => {}
        }
    }
    actions
}

fn recipient_mut<'a>(
    actions: &'a mut [SendActionView],
    by_id: &HashMap<SendActionId, usize>,
    action: &SendActionId,
    index: u32,
) -> Option<&'a mut SendActionRecipientView> {
    let position = *by_id.get(action)?;
    actions
        .get_mut(position)?
        .recipients
        .get_mut(usize::try_from(index).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ContentHash, PeerId, SendActionRef};

    fn peer(byte: u8) -> PeerId {
        PeerId::from_public_key(&[byte; 32]).expect("peer id")
    }

    fn recipient(alias: &str, byte: u8) -> SendActionRecipient {
        SendActionRecipient {
            alias: alias.to_owned(),
            peer: peer(byte),
            endpoint: None,
        }
    }

    fn entry(seq: u64, event: RoomEvent) -> JournalEntry {
        JournalEntry::new(
            seq,
            JournalRecord::Room(event),
            1_700_000_000_000 + seq as i64,
        )
    }

    fn recorded(id: &str, recipients: Vec<SendActionRecipient>) -> RoomEvent {
        RoomEvent::TeamSendActionRecorded {
            action: SendActionId::from_replay(id),
            recipients,
            message_type: SemanticMessageType::StoryAssignment,
            subject: Some("sprint-7".to_owned()),
            body_hash: ContentHash::from_bytes([0; 32]),
            body_bytes: 4,
        }
    }

    fn dispatched(id: &str, index: u32, peer: PeerId, task: &str) -> RoomEvent {
        RoomEvent::RemoteEnvelopeDispatched {
            peer,
            task: Some(task.to_owned()),
            bytes: 4,
            act: DispatchAct::Task,
            content_hash: None,
            endpoint: None,
            action: Some(SendActionRef {
                action: SendActionId::from_replay(id),
                index,
            }),
            authorship: crate::domain::models::Authorship::Unknown,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
        }
    }

    /// Membership comes from the row's own `{action, index}` and the frozen
    /// peer — two actions sharing an alias stay independent, a dispatch for
    /// another peer's slot is ignored, and a later settlement never overrides
    /// the first durable one.
    ///
    /// **Mutant → RED:** attach rows by alias or "newest dispatch": the second
    /// action steals the first's task; drop the peer check: the forged row
    /// lands; let a later settlement win.
    #[test]
    fn membership_is_the_rows_own_reference_and_the_first_fact_wins() {
        let (a, b) = (recipient("jun-dev", 1), recipient("tom-dev", 2));
        let entries = vec![
            entry(1, recorded("sa_one", vec![a.clone(), b.clone()])),
            entry(2, dispatched("sa_one", 0, a.peer.clone(), "task-a1")),
            entry(
                3,
                RoomEvent::TeamSendRecipientSettled {
                    action: SendActionId::from_replay("sa_one"),
                    index: 0,
                    settlement: SendSettlement::Declined,
                },
            ),
            entry(
                4,
                RoomEvent::TeamSendRecipientSettled {
                    action: SendActionId::from_replay("sa_one"),
                    index: 0,
                    settlement: SendSettlement::NotSent,
                },
            ),
            entry(5, recorded("sa_two", vec![a.clone()])),
            entry(6, dispatched("sa_two", 0, a.peer.clone(), "task-a2")),
            // A row naming slot 1 of `sa_one` with the WRONG peer: forged.
            entry(7, dispatched("sa_one", 1, a.peer.clone(), "task-forged")),
            // A fact about an action that was never recorded.
            entry(8, dispatched("sa_ghost", 0, a.peer.clone(), "task-ghost")),
        ];
        let folded = fold_send_actions(&entries);
        assert_eq!(folded.len(), 2, "ghost action is not minted from its rows");
        assert_eq!(folded[0].id.as_str(), "sa_one");
        assert_eq!(folded[0].recipients[0].tasks, vec!["task-a1"]);
        assert_eq!(
            folded[0].recipients[0].settlement,
            Some(SendSettlement::Declined),
            "the first durable settlement stands"
        );
        assert!(folded[0].recipients[1].tasks.is_empty(), "forged peer row");
        assert_eq!(folded[1].recipients[0].tasks, vec!["task-a2"]);
        assert_eq!(folded[1].recipients[0].settlement, None);
        // Positive control: the label and type of the record survive.
        assert_eq!(folded[0].subject.as_deref(), Some("sprint-7"));
        assert_eq!(folded[0].message_type, SemanticMessageType::StoryAssignment);
        assert_eq!(folded[0].recipients.len(), 2, "the addressed count");
    }

    /// A settlement written by a newer build decodes as `Unknown`, and the
    /// fold keeps it as that explicit fact rather than reading it as absent.
    ///
    /// **Mutant → RED:** map `Unknown` to `None`.
    #[test]
    fn a_settlement_from_a_newer_build_is_kept_as_unknown_not_absent() {
        let line = r#"{"event":"team_send_recipient_settled","action":"sa_one","index":0,"settlement":{"outcome":"quantum_delivered"}}"#;
        let event: RoomEvent = serde_json::from_str(line).expect("tolerant decode");
        let folded = fold_send_actions(&[
            entry(1, recorded("sa_one", vec![recipient("jun-dev", 1)])),
            entry(2, event),
        ]);
        assert_eq!(
            folded[0].recipients[0].settlement,
            Some(SendSettlement::Unknown)
        );
    }
}
