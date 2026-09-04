//! Outbound A2A text-send core used by human-facing command surfaces.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::driver::{
    A2aDelegationRuntime, DelegationError, TaskClient, build_message, disclosable_task_id,
};
use super::endpoint::resolve_jsonrpc_endpoint;
use super::error::A2aError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendOutcome {
    pub peer: String,
    pub task_id: String,
    pub state: String,
    pub reply_text: Option<String>,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum SendError {
    UnknownPeer {
        peer: String,
        known: Vec<String>,
    },
    CardNotCached {
        peer: String,
    },
    Endpoint {
        peer: String,
        source: A2aError,
    },
    InputRequired {
        peer: String,
        task_id: String,
    },
    Delegation {
        peer: String,
        source: DelegationError,
    },
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPeer { peer, known } => {
                let known = if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                };
                write!(
                    f,
                    "no A2A peer `{peer}` in the configured A2A roster \
                     (`.rustain/a2a.json` or the active profile) (known: {known})"
                )
            }
            Self::CardNotCached { peer } => write!(
                f,
                "peer `{peer}` is configured but its AgentCard is not cached — discovery \
                 runs once at startup and may still be in flight, or the peer was \
                 unreachable when this session started. If it stays refused, restart \
                 the daemon with the peer up; on-demand discovery is \
                 `DF-18-9-CARD-REFRESH`."
            ),
            Self::Endpoint { peer, source } => {
                write!(f, "peer `{peer}` has no usable A2A endpoint: {source}")
            }
            Self::InputRequired { peer, task_id } => write!(
                f,
                "peer `{peer}` asked a question this verb cannot answer (task `{task_id}` \
                 cancelled) — multi-turn arrives with 19.18"
            ),
            Self::Delegation { peer, source } => {
                write!(f, "A2A send to peer `{peer}` failed: {source}")
            }
        }
    }
}

impl std::error::Error for SendError {}

pub async fn send_text(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    text: &str,
    cancel: CancellationToken,
) -> Result<SendOutcome, SendError> {
    let (spec, client) = runtime
        .peer_binding(peer_id)
        .ok_or_else(|| SendError::UnknownPeer {
            peer: peer_id.to_owned(),
            known: runtime.known_peer_ids(),
        })?;
    let Some((card, trust)) = client.cached_card().await else {
        return Err(SendError::CardNotCached {
            peer: peer_id.to_owned(),
        });
    };
    let endpoint = resolve_jsonrpc_endpoint(&card).map_err(|source| SendError::Endpoint {
        peer: peer_id.to_owned(),
        source,
    })?;
    let message = outbound_message(text);
    let submitted_id = message["message"]["messageId"]
        .as_str()
        .expect("build_message always creates a string messageId")
        .to_owned();
    let transport = Arc::new(TaskClient::new(client, endpoint.url().to_owned()));
    let result = runtime
        .delegate(&spec, trust, &submitted_id, transport, message, cancel)
        .await
        .map_err(|source| match source {
            DelegationError::InputRequired { task_id, .. } => SendError::InputRequired {
                peer: peer_id.to_owned(),
                // The remote agent chose this id; it reaches the TUI, so it
                // gets the same bounded, control-stripped form the journal
                // uses (AC8) rather than the raw wire value.
                task_id: disclosable_task_id(&task_id),
            },
            source => SendError::Delegation {
                peer: peer_id.to_owned(),
                source,
            },
        })?;

    Ok(outcome_from_result(peer_id, &submitted_id, &result))
}

fn outcome_from_result(
    peer_id: &str,
    submitted_id: &str,
    result: &serde_json::Value,
) -> SendOutcome {
    // Mirror `TaskSnapshot::from_result`'s correlation precedence (`id` →
    // `taskId` → `messageId`) so the TUI shows the same id the journal's
    // terminal row records; `submitted_id` is the last resort only.
    let task_id = ["id", "taskId", "messageId"]
        .iter()
        .find_map(|key| {
            result
                .get(*key)
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
        })
        .unwrap_or(submitted_id)
        .to_owned();
    let task_id = disclosable_task_id(&task_id);
    let state = result
        .pointer("/status/state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("completed")
        .to_owned();
    let reply_text = first_text_part(result.pointer("/status/message/parts"))
        .or_else(|| first_text_part(result.pointer("/parts")))
        .or_else(|| {
            result
                .pointer("/artifacts")
                .and_then(serde_json::Value::as_array)
                .and_then(|artifacts| {
                    artifacts
                        .iter()
                        .find_map(|a| first_text_part(a.get("parts")))
                })
        });

    SendOutcome {
        peer: peer_id.to_owned(),
        task_id,
        state,
        reply_text,
    }
}

/// First `text` part in an A2A parts array, wherever it sits — a conforming
/// peer may lead with a non-text part or answer entirely via artifacts.
fn first_text_part(parts: Option<&serde_json::Value>) -> Option<String> {
    parts?.as_array()?.iter().find_map(|part| {
        part.get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn outbound_message(text: &str) -> serde_json::Value {
    build_message(&serde_json::json!({ "message": text }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::adapters::a2a::client;
    use crate::adapters::a2a::driver::A2aDelegationRuntime;
    use crate::domain::events::AppEvent;
    use crate::domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl, RoomEvent};
    use crate::domain::ports::{RoomJournal, RoomJournalError};
    use crate::infrastructure::subagent::NodeTree;

    use super::{SendError, outbound_message, outcome_from_result, send_text};

    struct AcceptingJournal;

    #[async_trait]
    impl RoomJournal for AcceptingJournal {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            Ok(())
        }
    }

    fn peer(id: &str) -> A2aPeerSpec {
        A2aPeerSpec {
            id: id.to_owned(),
            url: RedactedUrl::from("http://127.0.0.1:9"),
            pinned_key: None,
            source: A2aPeerSource::Workspace,
        }
    }

    fn runtime(peers: Vec<(A2aPeerSpec, Arc<client::A2aClientAdapter>)>) -> A2aDelegationRuntime {
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<AppEvent>();
        A2aDelegationRuntime::new(NodeTree::new(), Arc::new(AcceptingJournal), event_tx)
            .with_peer_bindings(peers.into())
    }

    #[tokio::test]
    async fn unknown_peer_lists_the_configured_ids_without_attempting_io() {
        let alpha = peer("alpha");
        let client = Arc::new(client::A2aClientAdapter::new(&alpha, None).expect("client"));
        let error = send_text(
            &runtime(vec![(alpha, client)]),
            "missing",
            "hello",
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown peer must be refused before transport");

        assert!(matches!(
            error,
            SendError::UnknownPeer { ref peer, ref known }
                if peer == "missing" && known == &["alpha"]
        ));
        assert_eq!(
            error.to_string(),
            "no A2A peer `missing` in the configured A2A roster \
             (`.rustain/a2a.json` or the active profile) (known: alpha)"
        );
    }

    #[tokio::test]
    async fn configured_peer_without_cached_card_refuses_without_on_demand_fetch() {
        let known = peer("known");
        let client = Arc::new(client::A2aClientAdapter::new(&known, None).expect("client"));
        let error = send_text(
            &runtime(vec![(known, client)]),
            "known",
            "hello",
            CancellationToken::new(),
        )
        .await
        .expect_err("send must not refresh an uncached card");

        assert!(matches!(
            error,
            SendError::CardNotCached { ref peer } if peer == "known"
        ));
        let text = error.to_string();
        assert!(
            text.starts_with("peer `known` is configured but its AgentCard is not cached"),
            "{text}"
        );
        // The refusal must not assert a definite boot-time failure while the
        // startup fetch can still be in flight.
        assert!(text.contains("may still be in flight"), "{text}");
        assert!(text.contains("DF-18-9-CARD-REFRESH"), "{text}");
    }

    #[test]
    fn outbound_message_preserves_exact_unicode_text_in_one_text_part() {
        let text = "  Καλημέρα 🌕\nsecond line  ";
        let message = outbound_message(text);
        let parts = message["message"]["parts"]
            .as_array()
            .expect("message parts");

        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["kind"], "text");
        assert_eq!(parts[0]["text"], text);
    }

    #[test]
    fn outcome_uses_peer_assigned_task_id_and_completed_reply() {
        let result = serde_json::json!({
            "kind": "task",
            "id": "peer-task-42",
            "status": {
                "state": "completed",
                "message": {
                    "role": "agent",
                    "parts": [{ "kind": "text", "text": "peer answer" }]
                }
            }
        });

        let outcome = outcome_from_result("moon", "submitted-local-id", &result);

        assert_eq!(outcome.peer, "moon");
        assert_eq!(outcome.task_id, "peer-task-42");
        assert_eq!(outcome.state, "completed");
        assert_eq!(outcome.reply_text.as_deref(), Some("peer answer"));
    }

    #[test]
    fn outcome_correlates_message_shaped_replies_by_their_own_id() {
        // `TaskSnapshot::from_result` correlates message-shaped responses by
        // `taskId`/`messageId`; the TUI must show the same id the journal's
        // terminal row records, not the locally submitted one.
        let result = serde_json::json!({
            "kind": "message",
            "messageId": "peer-message-1",
            "parts": [{ "kind": "text", "text": "fast reply" }]
        });

        let outcome = outcome_from_result("moon", "submitted-local-id", &result);

        assert_eq!(outcome.task_id, "peer-message-1");
        assert_eq!(outcome.state, "completed");
        assert_eq!(outcome.reply_text.as_deref(), Some("fast reply"));
    }

    #[test]
    fn outcome_extracts_text_from_later_parts_and_artifacts() {
        // A conforming peer may lead with a non-text part, or answer through
        // task artifacts instead of status.message.
        let leading_data_part = serde_json::json!({
            "kind": "task",
            "id": "t-1",
            "status": {
                "state": "completed",
                "message": {
                    "parts": [
                        { "kind": "data", "data": { "a": 1 } },
                        { "kind": "text", "text": "after a data part" }
                    ]
                }
            }
        });
        assert_eq!(
            outcome_from_result("moon", "s", &leading_data_part)
                .reply_text
                .as_deref(),
            Some("after a data part")
        );

        let artifact_answer = serde_json::json!({
            "kind": "task",
            "id": "t-2",
            "status": { "state": "completed" },
            "artifacts": [
                { "parts": [{ "kind": "text", "text": "answer in an artifact" }] }
            ]
        });
        assert_eq!(
            outcome_from_result("moon", "s", &artifact_answer)
                .reply_text
                .as_deref(),
            Some("answer in an artifact")
        );
    }

    #[test]
    fn outcome_task_id_is_sanitized_before_it_reaches_the_tui() {
        // The remote agent controls this id; control characters must not
        // reach terminal-facing sinks (AC8 — same bound as the journal).
        let result = serde_json::json!({
            "kind": "task",
            "id": "evil\u{0007}\u{000a}forged-header",
            "status": { "state": "completed" }
        });

        let task_id = outcome_from_result("moon", "s", &result).task_id;

        assert!(!task_id.contains('\u{0007}'), "{task_id:?}");
        assert!(!task_id.contains('\n'), "{task_id:?}");
    }
}
