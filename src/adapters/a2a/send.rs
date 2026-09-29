//! Outbound A2A text-send core used by human-facing command surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use super::client::CardSlot;
use super::driver::{A2aDelegationRuntime, TaskClient, build_message};
use super::endpoint::resolve_jsonrpc_endpoint;
use super::error::{A2aError, anchor_error};

#[derive(Debug)]
#[non_exhaustive]
pub enum SendError {
    UnknownPeer { peer: String, known: Vec<String> },
    CardNotCached { peer: String },
    Endpoint { peer: String, source: A2aError },
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
        }
    }
}

impl std::error::Error for SendError {}

fn outbound_message(text: &str) -> serde_json::Value {
    build_message(&serde_json::json!({ "message": text }))
}

/// Drive N independent sends and report each row as it settles. The callback
/// runs on the caller's task; it never waits for an unrelated recipient.
pub async fn deliver_to_recipients<F>(
    runtime: &A2aDelegationRuntime,
    recipients: &[String],
    text: &str,
    cancel: CancellationToken,
    mut on_settle: F,
) where
    F: FnMut(usize, RecipientOutcome),
{
    use futures::{StreamExt, stream::FuturesUnordered};

    let pending = FuturesUnordered::new();
    for (index, peer) in recipients.iter().enumerate() {
        pending.push(async move { (index, deliver_text(runtime, peer, text).await) });
    }
    tokio::pin!(pending);
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            next = pending.next() => next,
        };
        let Some((index, outcome)) = next else { break };
        if cancel.is_cancelled() {
            break;
        }
        on_settle(index, outcome);
    }
}

/// A ratified local sentence can exceed the peer-text cap (notably the
/// CardNotCached guidance). Only peer-influenced causes use that tighter cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendCause {
    Ratified(String),
    PeerInfluenced(String),
}

impl SendCause {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Ratified(text) | Self::PeerInfluenced(text) => text,
        }
    }
}

/// Story 19.17: an addressed send settles at the first answer, never at the
/// delegation lifecycle's terminal projection.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecipientOutcome {
    Delivered,
    Declined,
    AwaitingApproval,
    AcceptedWithoutItem,
    AskedQuestion { task_id: String },
    Unreachable { cause: Option<SendCause> },
    NoUsableAnswer,
    NotSent,
}

/// Deliver one independently addressed message. Resolve the roster binding
/// before calling this function; only the rail-three action validates the
/// complete set before starting any recipient I/O.
pub async fn deliver_text(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    text: &str,
) -> RecipientOutcome {
    use super::lifecycle::A2aTaskTransport;

    let Some((spec, client)) = runtime.peer_binding(peer_id) else {
        return RecipientOutcome::NotSent;
    };
    let card = match client.card_slot().await {
        CardSlot::Ready(card, _) => card,
        CardSlot::AnchorRefused(cause) => {
            let error = anchor_error(&spec.id, &cause);
            let reason = error.to_string();
            let _ = runtime.journal_send_preflight_refusal(&spec, &reason).await;
            return RecipientOutcome::Unreachable {
                cause: Some(SendCause::Ratified(reason)),
            };
        }
        CardSlot::Pending | CardSlot::Unavailable => {
            let reason = SendError::CardNotCached {
                peer: peer_id.to_owned(),
            }
            .to_string();
            let _ = runtime.journal_send_preflight_refusal(&spec, &reason).await;
            return RecipientOutcome::Unreachable {
                cause: Some(SendCause::Ratified(reason)),
            };
        }
    };
    let endpoint = match resolve_jsonrpc_endpoint(&card) {
        Ok(endpoint) => endpoint,
        Err(source) => {
            let reason = SendError::Endpoint {
                peer: peer_id.to_owned(),
                source,
            }
            .to_string();
            let _ = runtime.journal_send_preflight_refusal(&spec, &reason).await;
            return RecipientOutcome::Unreachable {
                cause: Some(SendCause::PeerInfluenced(reason)),
            };
        }
    };
    let message = outbound_message(text);
    let task_id = message["message"]["messageId"]
        .as_str()
        .expect("build_message creates messageId")
        .to_owned();
    let transport = TaskClient::new(client, endpoint.url().to_owned());
    let dispatched = AtomicBool::new(false);
    let result = transport
        .send_after(message, || async {
            runtime
                .journal_send_dispatch(&spec, &task_id, text.len())
                .await
                .map_err(|_| A2aError::JournalDispatch)?;
            dispatched.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;
    let outcome = match result {
        Ok(answer) => {
            let outcome = classify_first_answer(&answer, &task_id);
            match &outcome {
                RecipientOutcome::Delivered | RecipientOutcome::AcceptedWithoutItem => {
                    let _ = runtime
                        .journal_send_accepted(&spec, &task_id, &answer)
                        .await;
                }
                RecipientOutcome::Declined => {
                    let detail = answer
                        .pointer("/status/message/parts/0/text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("peer refused the addressed item");
                    let _ = runtime.journal_send_rejected(&spec, &task_id, detail).await;
                }
                RecipientOutcome::AskedQuestion { .. } => {
                    let remote_id = answer
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(&task_id);
                    let cancelled = transport.tasks_cancel(remote_id).await.is_ok_and(|reply| {
                        reply
                            .pointer("/status/state")
                            .and_then(serde_json::Value::as_str)
                            == Some("canceled")
                    });
                    if !cancelled {
                        // A failed or unconfirmed cancellation proves neither a
                        // terminal refusal nor that the peer stopped waiting.
                        // Keep only dispatch.
                        return RecipientOutcome::NoUsableAnswer;
                    }
                    let _ = runtime
                        .journal_send_rejected(
                            &spec,
                            &task_id,
                            "peer asked a question this verb cannot answer",
                        )
                        .await;
                }
                _ => {}
            }
            outcome
        }
        Err(A2aError::JournalDispatch) => RecipientOutcome::NotSent,
        Err(error) if !dispatched.load(Ordering::Relaxed) => {
            let cause = send_preflight_cause(peer_id, &error);
            let _ = runtime
                .journal_send_preflight_refusal(&spec, cause.as_str())
                .await;
            RecipientOutcome::Unreachable { cause: Some(cause) }
        }
        Err(error) => match send_error_class(&error) {
            SendErrorClass::Connect => {
                let _ = runtime
                    .journal_send_rejected(
                        &spec,
                        &task_id,
                        &format!("A2A transport failure: {error}"),
                    )
                    .await;
                RecipientOutcome::Unreachable { cause: None }
            }
            SendErrorClass::Trust => {
                let cause = error.to_string();
                let _ = runtime.journal_send_rejected(&spec, &task_id, &cause).await;
                RecipientOutcome::Unreachable {
                    cause: Some(SendCause::Ratified(cause)),
                }
            }
            SendErrorClass::Unproven => RecipientOutcome::NoUsableAnswer,
        },
    };
    outcome
}

/// The first answer alone decides delivery. Item presence outranks the task
/// state except for an explicit rejection; oversized ids prove neither result.
fn classify_first_answer(answer: &serde_json::Value, submitted_id: &str) -> RecipientOutcome {
    if answer
        .get("id")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|id| id.len() > crate::domain::services::transparency::MAX_PEER_ID_BYTES)
    {
        return RecipientOutcome::NoUsableAnswer;
    }
    let item = answer
        .pointer("/metadata/x-rustain-item-id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty());
    let state = answer
        .pointer("/status/state")
        .and_then(serde_json::Value::as_str);
    match (state, item) {
        (Some("rejected"), _) => RecipientOutcome::Declined,
        (_, Some(item))
            if item.len() <= crate::domain::services::transparency::MAX_PEER_ID_BYTES =>
        {
            RecipientOutcome::Delivered
        }
        (_, Some(_)) => RecipientOutcome::NoUsableAnswer,
        (Some("auth-required"), _) => RecipientOutcome::AwaitingApproval,
        (Some("submitted" | "working" | "completed"), _) => RecipientOutcome::AcceptedWithoutItem,
        (Some("input-required"), _) => {
            let remote_id = answer
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(submitted_id);
            RecipientOutcome::AskedQuestion {
                task_id: remote_id.to_owned(),
            }
        }
        // An A2A Message result is an immediate answer with no task to observe
        // (the lifecycle parser reads it as completed); without an item id it
        // is accepted, never a delivered FR165 item.
        (None, None) if is_message_answer(answer) => RecipientOutcome::AcceptedWithoutItem,
        _ => RecipientOutcome::NoUsableAnswer,
    }
}

/// The A2A Message result shape, as `lifecycle::TaskSnapshot` recognises it.
fn is_message_answer(answer: &serde_json::Value) -> bool {
    answer.get("kind").and_then(serde_json::Value::as_str) == Some("message")
        || answer.get("parts").is_some()
}

/// How a send error bears on the recipient outcome. The single variant match
/// behind both the pre-I/O cause line and the post-hook classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendErrorClass {
    /// The connection never opened: proof the request did not land.
    Connect,
    /// A named credential or trust-anchor refusal (`UX-DR-TM-10`).
    Trust,
    /// Proves neither delivery nor refusal.
    Unproven,
}

fn send_error_class(error: &A2aError) -> SendErrorClass {
    match error {
        A2aError::Connect(_) => SendErrorClass::Connect,
        A2aError::CredentialRejected { .. }
        | A2aError::CredentialMissing { .. }
        | A2aError::CredentialOutOfScope { .. }
        | A2aError::AnchorValidationFailed { .. }
        | A2aError::CaCertUnloadable { .. } => SendErrorClass::Trust,
        A2aError::Request(_)
        | A2aError::HttpStatus { .. }
        | A2aError::JsonRpc { .. }
        | A2aError::InvalidRedirect(_)
        | A2aError::TooManyRedirects
        | A2aError::UnexpectedContentType { .. }
        | A2aError::BodyTooLarge { .. }
        | A2aError::InvalidUtf8
        | A2aError::InvalidJson(_)
        | A2aError::UnknownTaskState { .. }
        | A2aError::MalformedResponse { .. }
        | A2aError::CorrelationMismatch { .. }
        | A2aError::MalformedCard { .. }
        | A2aError::MissingSignatures
        | A2aError::InvalidProtectedHeader
        | A2aError::UnsupportedAlgorithm { .. }
        | A2aError::KeyIdMismatch { .. }
        | A2aError::InvalidPinnedKey
        | A2aError::InvalidSignatureEncoding
        | A2aError::Canonicalization(_)
        | A2aError::BadSignature
        | A2aError::ClientBuild(_)
        | A2aError::UnsafeUrl { .. }
        | A2aError::NoJsonRpcEndpoint { .. }
        | A2aError::Config(_)
        | A2aError::JournalDispatch => SendErrorClass::Unproven,
        // A2aError is non-exhaustive: a future post-hook error proves no outcome.
        #[allow(unreachable_patterns)]
        _ => SendErrorClass::Unproven,
    }
}

fn send_preflight_cause(peer_id: &str, error: &A2aError) -> SendCause {
    match send_error_class(error) {
        SendErrorClass::Trust => SendCause::Ratified(error.to_string()),
        SendErrorClass::Connect | SendErrorClass::Unproven => SendCause::PeerInfluenced(format!(
            "peer `{peer_id}` has no usable A2A endpoint: {error}"
        )),
    }
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

/// Resolve ONE roster peer in the addressed-send order: binding → card slot →
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
            Ok(())
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
    use tokio::sync::{Mutex, mpsc};

    use crate::adapters::a2a::client;
    use crate::adapters::a2a::driver::A2aDelegationRuntime;
    use crate::adapters::a2a::test_fixtures::{PeerFixture, RpcAnswer};
    use crate::domain::events::AppEvent;
    use crate::domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl, RoomEvent};
    use crate::domain::ports::{RoomJournal, RoomJournalError};
    use crate::infrastructure::subagent::NodeTree;

    use super::{
        RecipientOutcome, SendCause, classify_first_answer, deliver_text, outbound_message,
    };

    struct AcceptingJournal;

    #[async_trait]
    impl RoomJournal for AcceptingJournal {
        async fn record_event(&self, _event: RoomEvent) -> Result<(), RoomJournalError> {
            Ok(())
        }
    }

    struct RecordingJournal(Mutex<Vec<RoomEvent>>);

    #[async_trait]
    impl RoomJournal for RecordingJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            self.0.lock().await.push(event);
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
    async fn configured_peer_without_cached_card_refuses_without_on_demand_fetch() {
        let known = peer("known");
        let client = Arc::new(client::A2aClientAdapter::new(&known, None).expect("client"));
        let outcome = deliver_text(&runtime(vec![(known, client)]), "known", "hello").await;
        let RecipientOutcome::Unreachable {
            cause: Some(SendCause::Ratified(text)),
        } = outcome
        else {
            panic!("uncached roster peer must be unreachable with local guidance");
        };
        assert!(text.contains("AgentCard is not cached"), "{text}");
        assert!(text.contains("may still be in flight"), "{text}");
        assert!(text.contains("DF-18-9-CARD-REFRESH"), "{text}");
    }

    #[tokio::test]
    async fn failed_question_cancellation_never_claims_the_remote_task_was_cancelled() {
        let fixture = PeerFixture::plaintext().await;
        fixture
            .answer_with(RpcAnswer::InputRequiredCancelFails)
            .await;
        let spec = A2aPeerSpec::new(
            "questions",
            RedactedUrl::from(fixture.origin.clone()),
            A2aPeerSource::Workspace,
        );
        let client = Arc::new(client::A2aClientAdapter::new(&spec, None).expect("client"));
        client.refresh_agent_card(&spec).await.expect("cached card");
        let journal = Arc::new(RecordingJournal(Mutex::new(Vec::new())));
        let (event_tx, _event_rx) = mpsc::unbounded_channel::<AppEvent>();
        let runtime = A2aDelegationRuntime::new(NodeTree::new(), journal.clone(), event_tx)
            .with_peer_bindings(vec![(spec, client)].into());

        assert_eq!(
            deliver_text(&runtime, "questions", "hello").await,
            RecipientOutcome::NoUsableAnswer
        );
        let methods: Vec<_> = fixture
            .posts()
            .await
            .iter()
            .map(|post| {
                serde_json::from_str::<serde_json::Value>(&post.body)
                    .expect("recorded JSON-RPC")
                    ["method"]
                    .as_str()
                    .expect("method")
                    .to_owned()
            })
            .collect();
        assert_eq!(methods, ["message/send", "tasks/cancel"]);
        let rows = journal.0.lock().await;
        assert_eq!(
            rows.len(),
            1,
            "only a dispatch is proven when cleanup fails"
        );
        assert!(matches!(
            rows[0],
            RoomEvent::RemoteEnvelopeDispatched { .. }
        ));
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
    fn itemless_first_answers_never_claim_a_delivered_item() {
        let accepted = serde_json::json!({"status": {"state": "working"}});
        assert_eq!(
            classify_first_answer(&accepted, "local"),
            RecipientOutcome::AcceptedWithoutItem
        );
        let message = serde_json::json!({
            "kind": "message",
            "messageId": "peer-reply",
            "parts": [{"kind": "text", "text": "noted"}]
        });
        assert_eq!(
            classify_first_answer(&message, "local"),
            RecipientOutcome::AcceptedWithoutItem,
            "a Message result is an immediate answer, not an unusable one"
        );
        let question = serde_json::json!({
            "id": "peer-\nquestion",
            "status": {"state": "input-required"}
        });
        assert_eq!(
            classify_first_answer(&question, "local"),
            RecipientOutcome::AskedQuestion {
                task_id: "peer-\nquestion".into()
            }
        );
        let parked = serde_json::json!({
            "status": {"state": "auth-required"},
            "metadata": {"x-rustain-item-id": "ri_123"}
        });
        assert_eq!(
            classify_first_answer(&parked, "local"),
            RecipientOutcome::Delivered
        );
        let oversized = serde_json::json!({
            "status": {"state": "auth-required"},
            "metadata": {"x-rustain-item-id": "x".repeat(crate::domain::services::transparency::MAX_PEER_ID_BYTES + 1)}
        });
        assert_eq!(
            classify_first_answer(&oversized, "local"),
            RecipientOutcome::NoUsableAnswer
        );
        for state in ["working", "input-required"] {
            let oversized_task = serde_json::json!({
                "id": "x".repeat(crate::domain::services::transparency::MAX_PEER_ID_BYTES + 1),
                "status": { "state": state }
            });
            assert_eq!(
                classify_first_answer(&oversized_task, "local"),
                RecipientOutcome::NoUsableAnswer,
                "an oversized remote task id never authorizes acceptance or cancellation"
            );
        }
    }
}
