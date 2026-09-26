//! Outbound A2A text-send core used by human-facing command surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use super::client::CardSlot;
use super::driver::{
    A2aDelegationRuntime, DelegationError, TaskClient, build_message, disclosable_task_id,
};
use super::endpoint::resolve_jsonrpc_endpoint;
use super::error::{A2aError, AnchorCause, anchor_error, anchor_refusal};

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
    /// The boot card GET — this peer's first TLS handshake — refused its
    /// certificate, or its anchor could not be loaded (`A22`).
    ///
    /// ⛔ Deliberately NOT routed through `DelegationError`: nothing was
    /// delegated, and `Delegation`'s `Display` would prefix the operator's
    /// ratified sentence with `A2A send to peer …: A2A transport failure:`.
    AnchorRefused {
        peer: String,
        cause: AnchorCause,
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
            // The one formatter for forms 5–9, shared with `A2aError`.
            Self::AnchorRefused { peer, cause } => f.write_str(&anchor_refusal(peer, cause)),
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
    // `AnchorRefused` is checked BEFORE `CardNotCached` (`A22` item 2): a
    // retained anchor cause is the actionable one, and `CardNotCached`'s text
    // ("discovery may still be in flight") would be false beside it.
    let (card, trust) = match client.card_slot().await {
        CardSlot::Ready(card, trust) => (card, trust),
        CardSlot::AnchorRefused(cause) => {
            // Durable-first, exactly one row, and ⛔ no `Dispatched` row: nothing
            // was dispatched. A journal failure is latched the way the driver
            // latches it; the send is refused either way, so the operator still
            // sees the anchor form.
            let _ = runtime
                .journal_anchor_refusal(&spec, &anchor_error(&spec.id, &cause))
                .await;
            return Err(SendError::AnchorRefused {
                peer: peer_id.to_owned(),
                cause,
            });
        }
        CardSlot::Pending | CardSlot::Unavailable => {
            return Err(SendError::CardNotCached {
                peer: peer_id.to_owned(),
            });
        }
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

// ── Story 19.16f · the cross-host retract, the sender's half ────────────────
//
// The served verb is Story 19.16d's; this side SPEAKS its contract and defines
// none of it. ⛔ One peer, resolved by name — never a roster fan-out (`M09`):
// the item id is only meaningful on the host that minted it.

/// Why a retract — or its confirm-time read — sent nothing: the single-peer
/// resolution failed before any request existed. Typed end to end, ⛔ never
/// `board::read_peer`'s `Err(())` collapse.
#[derive(Debug)]
#[non_exhaustive]
pub enum RetractNotSent {
    /// `peer` names no configured A2A roster peer.
    UnknownPeer { known: Vec<String> },
    /// The peer's card is not cached (boot discovery pending or failed).
    CardNotCached,
    /// The boot card GET refused the peer's certificate or its anchor could
    /// not load (`A22`) — rendered as its ratified sentence.
    AnchorRefused(A2aError),
    /// The cached card names no usable JSON-RPC endpoint.
    Endpoint(A2aError),
}

/// A retract answer that is not a success, classified on the **variant** —
/// ⛔ never a `Display` string match (`F2`, `F5`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetractRefusal {
    /// `-32041`: the recipient already removed the item (a tombstone).
    Tombstone,
    /// `-32040`: the peer's admission policy refused the write.
    RefusedByPolicy,
    /// `-32001`: no item this credential can address — byte-identical
    /// whether it never existed or is another principal's (R21).
    NotFound,
    /// `-32601`: a build without the verb.
    OldBuild,
    /// `-32602`/`-32600`: the peer refused the request's shape.
    Malformed,
    /// [`A2aError::Connect`]: no request byte reached the peer.
    CouldNotReach,
    /// A credential or anchor refusal, carried as its ratified sentence
    /// (`A2aError`'s Display, `AD-1823`).
    Credential(String),
    /// No usable answer — `-32603`, a transport error after send, a non-2xx
    /// other than 401/403, a shapeless success, or anything unnamed. ⚠ The
    /// recipient journals the mark BEFORE it can still answer `-32603`, so
    /// whether the item was marked is genuinely unknown.
    Unknown,
}

impl RetractRefusal {
    /// Classify one failed answer. The fallback is [`Self::Unknown`]: an error
    /// this build cannot name must never be rendered as "nothing was marked".
    #[must_use]
    pub fn classify(error: &A2aError) -> Self {
        use super::jsonrpc::{CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, JsonRpcErrorKind};

        match error {
            A2aError::JsonRpc { code, .. } => match JsonRpcErrorKind::classify(*code) {
                JsonRpcErrorKind::ItemRemoved => Self::Tombstone,
                JsonRpcErrorKind::RefusedByPolicy => Self::RefusedByPolicy,
                JsonRpcErrorKind::TaskNotFound => Self::NotFound,
                JsonRpcErrorKind::MethodNotFound => Self::OldBuild,
                JsonRpcErrorKind::Other(CODE_INVALID_PARAMS | CODE_INVALID_REQUEST) => {
                    Self::Malformed
                }
                _ => Self::Unknown,
            },
            A2aError::Connect(_) => Self::CouldNotReach,
            // Pre-POST (origin, env, anchor) or the auth layer's 401/403
            // (`status_error`), which answers before `dispatch` runs.
            A2aError::CredentialMissing { .. }
            | A2aError::CredentialRejected { .. }
            | A2aError::CredentialOutOfScope { .. }
            | A2aError::AnchorValidationFailed { .. }
            | A2aError::CaCertUnloadable { .. } => Self::Credential(error.to_string()),
            _ => Self::Unknown,
        }
    }

    /// `true` only when the answer PROVES nothing was marked (`AC5(a)`).
    #[must_use]
    pub fn proves_unmarked(&self) -> bool {
        !matches!(self, Self::Unknown)
    }

    /// The cause half of the sender's rejection row. Locally minted except
    /// the credential sentence, which is ratified and already alias-only.
    fn ledger_cause(&self) -> String {
        match self {
            Self::Tombstone => "already removed by its recipient".to_owned(),
            Self::RefusedByPolicy => "refused by the peer's policy".to_owned(),
            Self::NotFound => "the peer has no item this host can address".to_owned(),
            Self::OldBuild => "the peer runs a build without the retract verb".to_owned(),
            Self::Malformed => "the peer refused the request as malformed".to_owned(),
            Self::CouldNotReach => "could not reach the peer".to_owned(),
            Self::Credential(sentence) => sentence.clone(),
            Self::Unknown => "no usable answer".to_owned(),
        }
    }
}

/// One item as the peer listed it at confirm time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewedItem {
    pub item_id: String,
    pub task: Option<String>,
    pub state: super::board::BoardItemState,
    pub retracted_at_ms: Option<i64>,
}

/// The confirm-time read (Story 19.16f `AC4(c)`).
#[derive(Debug)]
#[non_exhaustive]
pub enum RetractPreview {
    Found(PreviewedItem),
    /// The peer lists no item with this id for this credential.
    NotFound,
    /// The read did not resolve: the card renders DISARMED, naming the cause.
    Unverified(RetractRefusal),
    NotSent(RetractNotSent),
}

/// An accepted retract's answer (Story 19.16f `AC10(c)`).
#[derive(Debug)]
#[non_exhaustive]
pub enum RetractOutcome {
    Landed {
        retracted_at_ms: Option<i64>,
    },
    /// The recipient's mark already existed; the fold is idempotent, the
    /// operator's answer is not (`F7`).
    AlreadyRetracted {
        retracted_at_ms: Option<i64>,
    },
    Refused(RetractRefusal),
    NotSent(RetractNotSent),
}

/// Resolve ONE roster peer in `send_text`'s order: binding → card slot →
/// endpoint → transport.
async fn resolve_retract_peer(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
) -> Result<(crate::domain::models::A2aPeerSpec, TaskClient), RetractNotSent> {
    let (spec, client) =
        runtime
            .peer_binding(peer_id)
            .ok_or_else(|| RetractNotSent::UnknownPeer {
                known: runtime.known_peer_ids(),
            })?;
    let card = match client.card_slot().await {
        CardSlot::Ready(card, _trust) => card,
        CardSlot::AnchorRefused(cause) => {
            return Err(RetractNotSent::AnchorRefused(anchor_error(
                &spec.id, &cause,
            )));
        }
        CardSlot::Pending | CardSlot::Unavailable => return Err(RetractNotSent::CardNotCached),
    };
    let endpoint = resolve_jsonrpc_endpoint(&card).map_err(RetractNotSent::Endpoint)?;
    Ok((spec, TaskClient::new(client, endpoint.url().to_owned())))
}

/// Read the one addressed item from the peer's own list, at confirm time.
///
/// ⛔ **Uncorrelated**: the verb's scope is the credential, not this host's
/// dispatch ledger, so the card previews whatever the retract would address.
/// ⛔ Never a cached board render. Does not touch the board's refresh floor
/// or its in-flight slot — it is one single-peer read, not a board.
pub async fn preview_item_retract(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    item_id: &str,
) -> RetractPreview {
    let (_spec, transport) = match resolve_retract_peer(runtime, peer_id).await {
        Ok(resolved) => resolved,
        Err(not_sent) => return RetractPreview::NotSent(not_sent),
    };
    let result = match transport.list_items().await {
        Ok(result) => result,
        Err(error) => return RetractPreview::Unverified(RetractRefusal::classify(&error)),
    };
    let Some(items) = result.get("items").and_then(serde_json::Value::as_array) else {
        return RetractPreview::Unverified(RetractRefusal::Unknown);
    };
    let Some(item) = items
        .iter()
        .find(|item| item.get("itemId").and_then(serde_json::Value::as_str) == Some(item_id))
    else {
        return RetractPreview::NotFound;
    };
    // A state this build cannot name is an unverified state: the card must
    // not arm on a read it cannot interpret.
    let Some(state) = item
        .get("state")
        .and_then(serde_json::Value::as_str)
        .and_then(super::board::BoardItemState::from_wire)
    else {
        return RetractPreview::Unverified(RetractRefusal::Unknown);
    };
    RetractPreview::Found(PreviewedItem {
        item_id: item_id.to_owned(),
        task: item
            .get("task")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        state,
        retracted_at_ms: item
            .get("retractedAtMs")
            .and_then(serde_json::Value::as_i64),
    })
}

/// Dispatch one accepted retract (Story 19.16f `AC2`, `AC5`, `AC10`).
///
/// Order is the invariant: resolve → journal the dispatch row (durable
/// **before** the POST; `M20`) → POST → classify on the variant → journal a
/// rejection row **only** when the answer proves nothing was marked. A
/// resolution failure sent nothing and journals nothing.
pub async fn retract_item_on_peer(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    item_id: &str,
    task: Option<String>,
) -> RetractOutcome {
    let (spec, transport) = match resolve_retract_peer(runtime, peer_id).await {
        Ok(resolved) => resolved,
        Err(not_sent) => return RetractOutcome::NotSent(not_sent),
    };
    let bytes = serde_json::json!({ "itemId": item_id }).to_string().len();
    let request_started = AtomicBool::new(false);
    let outcome = match transport
        .retract_item(item_id, || async {
            runtime
                .journal_item_retract_dispatch(&spec, task.as_deref(), item_id, bytes)
                .await;
            request_started.store(true, Ordering::Relaxed);
        })
        .await
    {
        Ok(result) => retract_success(&result),
        Err(error) => RetractOutcome::Refused(RetractRefusal::classify(&error)),
    };
    if request_started.load(Ordering::Relaxed)
        && let RetractOutcome::Refused(refusal) = &outcome
        && refusal.proves_unmarked()
    {
        runtime
            .journal_item_retract_refusal(
                &spec,
                task.as_deref(),
                &format!("item retract refused: {}", refusal.ledger_cause()),
            )
            .await;
    }
    outcome
}

/// Read the served success shape. `retractedAtMs` is `null`-when-`None` on
/// this response (absent-not-null on the list): both parse to `None`. A
/// success without `alreadyRetracted` is a shape this client cannot read —
/// **unknown**, never landed.
fn retract_success(result: &serde_json::Value) -> RetractOutcome {
    let Some(already) = result
        .get("alreadyRetracted")
        .and_then(serde_json::Value::as_bool)
    else {
        return RetractOutcome::Refused(RetractRefusal::Unknown);
    };
    let retracted_at_ms = result
        .get("retractedAtMs")
        .and_then(serde_json::Value::as_i64);
    if already {
        RetractOutcome::AlreadyRetracted { retracted_at_ms }
    } else {
        RetractOutcome::Landed { retracted_at_ms }
    }
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
        A2aPeerSpec::new(
            id,
            RedactedUrl::from("http://127.0.0.1:9"),
            A2aPeerSource::Workspace,
        )
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
