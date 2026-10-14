//! Outbound A2A text-send core used by human-facing command surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio_util::sync::CancellationToken;

use super::client::CardSlot;
use super::driver::{
    A2aDelegationRuntime, MessageCarriers, OwnTask, SendLiveness, TaskClient, build_message,
};
use super::endpoint::resolve_jsonrpc_endpoint;
use super::error::{A2aError, anchor_error};
use super::item_capability::ItemCapability;
use crate::domain::models::{RecipientOutcome, SendCause};
use crate::domain::services::transparency::MAX_PEER_ID_BYTES;

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
    /// Story 19.21 `AC1(b)` — the sender-local `--subject` label broke its
    /// grammar. Decided with the unknown-peer check, before any journal write
    /// or I/O.
    InvalidSubject(crate::domain::services::send_action_ledger::SubjectRefusal),
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
            Self::InvalidSubject(refusal) => write!(f, "{refusal}"),
        }
    }
}

impl std::error::Error for SendError {}

fn outbound_message(
    text: &str,
    message_type: Option<crate::domain::models::SemanticMessageType>,
    carriers: MessageCarriers<'_>,
) -> (serde_json::Value, Option<OwnTask>) {
    let input = match message_type {
        Some(kind) => serde_json::json!({
            "message": text,
            "message_type": crate::domain::models::semantic_message_type_metadata(kind).token,
        }),
        None => serde_json::json!({ "message": text }),
    };
    build_message(&input, carriers)
}

/// Story 19.18c `A10` — the capability this host presents for `id` on
/// `spec`'s host, or `None`:
///
/// 1. a reply-turn id maps to the item it answers through this host's
///    `PeerReplyObserved` rows; no row ⇒ `None`, and ⛔ no list call;
/// 2. `x-rustain-items/list` names the item's `task`; no row ⇒ `None`;
/// 3. the task must be one this host dispatched to this peer (`A9`, the
///    `PQ2` own-row rule) ⇒ derive; else `None` — ⛔ never a derivation for
///    a task a peer merely served, which would make this host a MAC oracle.
///
/// "No capability" is not an error: the verb goes out without one and the
/// recipient answers per `A7`. Under an unusable or disabled key nothing is
/// read. Callers: the `/team thread` spawn ([`read_thread`]) and a follow-up
/// in [`deliver_text`]; the retract dispatch applies step 3 to its carried
/// task. Cost: one list round trip per call, bounded by the 1 MiB body cap
/// (`DF-19-16B-ACK-READ-REFOLDS-THE-WHOLE-JOURNAL`).
///
/// # Errors
///
/// The list read's transport or JSON-RPC failure; the caller sends nothing.
pub(crate) async fn capability_for(
    runtime: &A2aDelegationRuntime,
    spec: &crate::domain::models::A2aPeerSpec,
    transport: &TaskClient,
    id: &str,
) -> Result<Option<ItemCapability>, A2aError> {
    if !runtime.resolves_capabilities() {
        return Ok(None);
    }
    let answered;
    let item = if id.starts_with(crate::domain::models::REPLY_TURN_ID_PREFIX) {
        match runtime.answered_item(spec, id).await {
            Some(item) => {
                answered = item;
                answered.as_str()
            }
            None => return Ok(None),
        }
    } else {
        id
    };
    let listed = transport.list_items().await?;
    let task = listed
        .get("items")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|row| row.get("itemId").and_then(serde_json::Value::as_str) == Some(item))
        })
        .and_then(|row| row.get("task"))
        .and_then(serde_json::Value::as_str);
    Ok(match task {
        Some(task) => runtime.own_task_capability(spec, task).await,
        None => None,
    })
}

/// Whole-action validation (Story 19.17 `G4`, 19.21 `AC1(b)`): every addressed
/// alias is a configured A2A roster peer and the optional label keeps its
/// grammar. Decided **before** any journal write, recipient preflight or I/O,
/// and before a front door paints a row — so a refused action leaves no trace.
/// [`coordinate_send`] runs it first; a rail that must refuse synchronously
/// (the standalone TUI paints rows only for a valid action) calls it too.
///
/// # Errors
///
/// [`SendError::UnknownPeer`] / [`SendError::InvalidSubject`].
pub fn validate_send_request(
    runtime: &A2aDelegationRuntime,
    request: &SendRequest<'_>,
) -> Result<(), SendError> {
    let known = runtime.known_peer_ids();
    let unknown: Vec<_> = request
        .recipients
        .iter()
        .filter(|peer| !known.contains(peer))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(SendError::UnknownPeer {
            peer: unknown.join(", "),
            known,
        });
    }
    if let Some(subject) = request.subject {
        crate::domain::services::send_action_ledger::validate_subject(subject)
            .map_err(SendError::InvalidSubject)?;
    }
    Ok(())
}

/// What one operator send asks of the coordinator.
///
/// ⛔ `subject` is the sender-local label: it is journaled in the action record
/// and shown on the board, and it never reaches [`outbound_message`], so no
/// recipient ever sees it (`M11`).
#[derive(Debug, Clone, Copy)]
pub struct SendRequest<'a> {
    pub recipients: &'a [String],
    pub text: &'a str,
    pub message_type: Option<crate::domain::models::SemanticMessageType>,
    /// `Some` only for `/team followup` (N = 1): the one addition to the 19.17
    /// delivery. A follow-up reuses the coordinator and **records no action**
    /// (Story 19.21 `AC1(i)`).
    pub reply_to: Option<&'a str>,
    pub subject: Option<&'a str>,
}

/// The one send coordinator (Story 19.21 `C1`): validate the whole action,
/// journal it, drive N independent sends and settle each recipient as it
/// lands. Rail-neutral — the standalone TUI arm calls it today, the daemon's
/// send frame calls the same function.
///
/// Order is the invariant, asserted on journal sequence (`Rule 4`):
///
/// 1. whole-action validation — an unknown alias or a malformed label refuses
///    the **whole** action before any journal write or I/O (19.17 `G4`);
/// 2. the action record — before any recipient preflight, dispatch or POST. If
///    it cannot be appended nothing else happens: no recipient I/O, every row
///    `NotSent` (`AC1(f)`);
/// 3. per recipient, independently: its delivery rows (preflight / dispatch /
///    outcome rows, exactly as the preserved path emits them), then its typed
///    settlement, then — only if the settlement is durable — `on_settle`.
///
/// `on_started` runs **once**, after the action record is durable and before
/// any recipient I/O, with the sender-local action id (never on a follow-up,
/// which records no action, and never when the record failed). A rail that
/// needs to tell its operator which action a block belongs to — the daemon's
/// `TeamSendStarted` — uses it; the standalone arm passes a no-op.
///
/// A settlement append failure after recipient I/O leaves the recipient
/// durably unsettled: the latched journal-failure notice fires, the remote
/// outcome is ⛔ never rewritten as `NotSent`, `on_settle` is ⛔ not called
/// (no undurable settle is shown), and the board reconciles the recipient
/// through its dispatch correlation (`AC1(g)`). A recorded `NotSent` outcome
/// is visible only after its typed settlement is durable; the separate
/// action-record-failed branch still reports `NotSent` because no action or
/// settlement could be recorded.
///
/// ⛔ No automatic retry, no group identity, no aggregate. Dropping the future
/// (the caller's `cancel`) abandons in-flight recipients exactly as before;
/// the action's liveness ends with it. The daemon's coordinator passes its own
/// token, never a connection's.
///
/// # Errors
///
/// [`SendError::UnknownPeer`] / [`SendError::InvalidSubject`] when the whole
/// action is refused; nothing was journaled or sent.
pub async fn coordinate_send<S, F>(
    runtime: &A2aDelegationRuntime,
    request: SendRequest<'_>,
    cancel: CancellationToken,
    on_started: S,
    mut on_settle: F,
) -> Result<(), SendError>
where
    S: FnOnce(&crate::domain::models::SendActionId),
    F: FnMut(usize, RecipientOutcome),
{
    use futures::{StreamExt, stream::FuturesUnordered};

    validate_send_request(runtime, &request)?;

    let slots: Vec<Option<(crate::domain::models::SendActionRef, SendLiveness)>> =
        if request.reply_to.is_none() {
            match runtime
                .record_send_action(
                    request.recipients,
                    request.text,
                    request.message_type,
                    request.subject,
                )
                .await
            {
                Ok(handle) => {
                    on_started(handle.id());
                    handle.into_recipients().into_iter().map(Some).collect()
                }
                Err(_) => {
                    // The journal could not record the attempt: nothing was sent
                    // and nothing will be (`AC1(f)`).
                    for index in 0..request.recipients.len() {
                        on_settle(index, RecipientOutcome::NotSent);
                    }
                    return Ok(());
                }
            }
        } else {
            request.recipients.iter().map(|_| None).collect()
        };

    let pending = FuturesUnordered::new();
    for (index, (peer, slot)) in request.recipients.iter().zip(slots).enumerate() {
        // `slot` is captured by the future when it is CREATED, so a future
        // dropped before its first poll still drops its liveness token.
        pending.push(async move {
            let (reference, liveness) = match slot {
                Some((reference, liveness)) => (Some(reference), Some(liveness)),
                None => (None, None),
            };
            let mut item = None;
            let outcome = deliver_one(
                runtime,
                peer,
                request.text,
                request.message_type,
                request.reply_to,
                reference.as_ref(),
                &mut item,
            )
            .await;
            // The settlement is journaled INSIDE the recipient's own future,
            // after every row its delivery path emitted and before anything
            // is shown. ⚠ Not in the drain loop below: a sibling future that is
            // suspended mid-append advances only while `pending` is polled,
            // so awaiting an append in the loop body would stall behind it.
            let durable = match reference.as_ref() {
                Some(reference) => runtime
                    .journal_send_settlement(reference, settlement_of(&outcome, item))
                    .await
                    .is_ok(),
                None => true,
            };
            // Rule 9: once its settlement was attempted — durable or not —
            // this recipient is no longer sending, however long its siblings
            // take. Dropping the future elsewhere drops this token the same way.
            drop(liveness);
            (index, outcome, durable)
        });
    }
    tokio::pin!(pending);
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            next = pending.next() => next,
        };
        let Some((index, outcome, durable)) = next else {
            break;
        };
        if cancel.is_cancelled() {
            break;
        }
        if durable {
            on_settle(index, outcome);
        }
    }
    Ok(())
}

/// The typed, journaled form of one recipient's outcome (Story 19.21
/// `AC1(d)`). Every variant is matched explicitly: a new outcome must decide
/// how it is recorded.
fn settlement_of(
    outcome: &RecipientOutcome,
    item: Option<String>,
) -> crate::domain::models::SendSettlement {
    use crate::domain::models::{SendSettlement, SettlementCause, SettlementCauseKind};

    match outcome {
        RecipientOutcome::Delivered => SendSettlement::Delivered { item },
        RecipientOutcome::Declined => SendSettlement::Declined,
        RecipientOutcome::AwaitingApproval => SendSettlement::AwaitingApproval,
        RecipientOutcome::AcceptedWithoutItem => SendSettlement::AcceptedWithoutItem,
        RecipientOutcome::AskedQuestion { task_id } => SendSettlement::AskedQuestion {
            task_id: task_id.clone(),
        },
        RecipientOutcome::Unreachable { cause } => SendSettlement::Unreachable {
            cause: cause.as_ref().map(|cause| SettlementCause {
                kind: match cause {
                    SendCause::Ratified(_) => SettlementCauseKind::Ratified,
                    SendCause::PeerInfluenced(_) => SettlementCauseKind::PeerInfluenced,
                },
                // Stored raw and sanitized once at render (`AD-1824`); only
                // the line's size is bounded here.
                text: bounded_cause_text(cause.as_str()),
            }),
        },
        RecipientOutcome::NoUsableAnswer => SendSettlement::NoUsableAnswer,
        RecipientOutcome::NotSent => SendSettlement::NotSent,
    }
}

/// The longest cause text one settlement row carries — well above both render
/// bounds, so the render's own sanitizer is what cuts visibly.
const MAX_SETTLEMENT_CAUSE_BYTES: usize = 1024;

fn bounded_cause_text(text: &str) -> String {
    if text.len() <= MAX_SETTLEMENT_CAUSE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_SETTLEMENT_CAUSE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Deliver one independently addressed message. Resolve the roster binding
/// before calling this function; only the rail-three action validates the
/// complete set before starting any recipient I/O.
///
/// Story 19.18c: the message carries this host's item capability; a
/// follow-up also resolves its reference's capability (`A10`, `P17`) before
/// the message is built. While the key is unusable, a credentialed peer is
/// refused with the key's repair sentence before any dispatch row or POST
/// (`PQ1`).
pub async fn deliver_text(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    text: &str,
    message_type: Option<crate::domain::models::SemanticMessageType>,
    reply_to: Option<&str>,
) -> RecipientOutcome {
    deliver_one(
        runtime,
        peer_id,
        text,
        message_type,
        reply_to,
        None,
        &mut None,
    )
    .await
}

/// [`deliver_text`] inside one operator send action: `action` rides the
/// dispatch row, and a first answer that minted an item hands its id back
/// through `item_out` for the typed settlement. Every row the delivery path
/// emits is the preserved 19.17/19.18 shape (`AC1`, "byte-identical when
/// `action` is absent").
async fn deliver_one(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    text: &str,
    message_type: Option<crate::domain::models::SemanticMessageType>,
    reply_to: Option<&str>,
    action: Option<&crate::domain::models::SendActionRef>,
    item_out: &mut Option<String>,
) -> RecipientOutcome {
    use super::lifecycle::A2aTaskTransport;

    let Some((spec, client)) = runtime.peer_binding(peer_id) else {
        return RecipientOutcome::NotSent;
    };
    let item_key = match runtime.outbound_item_key(&spec) {
        Ok(item_key) => item_key,
        Err(error) => {
            let cause = send_preflight_cause(peer_id, &error);
            let _ = runtime
                .journal_send_preflight_refusal(&spec, cause.as_str())
                .await;
            return RecipientOutcome::Unreachable { cause: Some(cause) };
        }
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
    let transport = TaskClient::new(client, endpoint.url().to_owned());
    // Story 19.18c `P17` — a follow-up's reply capability goes INSIDE the
    // message, so it is resolved first; a failed list read is a preflight
    // refusal (one row, no dispatch, nothing sent).
    let reply_capability = match reply_to {
        Some(reference) => match capability_for(runtime, &spec, &transport, reference).await {
            Ok(capability) => capability,
            Err(error) => {
                let cause = send_preflight_cause(peer_id, &error);
                let _ = runtime
                    .journal_send_preflight_refusal(&spec, cause.as_str())
                    .await;
                return RecipientOutcome::Unreachable { cause: Some(cause) };
            }
        },
        None => None,
    };
    let (message, own_task) = outbound_message(
        text,
        message_type,
        MessageCarriers {
            reply_to,
            item_key: item_key.as_deref(),
            reply_capability: reply_capability.as_ref(),
        },
    );
    let task_id = message["message"]["messageId"]
        .as_str()
        .expect("build_message creates messageId")
        .to_owned();
    // `A9`: the cancel below presents the capability only for this id.
    let transport = match own_task {
        Some(own_task) => transport.with_own_task(own_task),
        None => transport,
    };
    let dispatched = AtomicBool::new(false);
    let result = transport
        .send_after(message, || async {
            runtime
                .journal_send_dispatch(&spec, &task_id, text, action)
                .await
                .map_err(|_| A2aError::JournalDispatch)?;
            dispatched.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;
    let outcome = match result {
        Ok(answer) => {
            let outcome = classify_first_answer(&answer, &task_id);
            if outcome == RecipientOutcome::Delivered {
                *item_out = answer
                    .pointer("/metadata/x-rustain-item-id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
            }
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
        // Story 19.18 `AC12(d)` — the ONE precise change to the post-hook
        // classification: a `-32040` is the recipient's policy refusing the
        // message, which TM-02 names `declined`. Every other JSON-RPC error
        // proves neither delivery nor refusal and stays `no usable answer`.
        Err(A2aError::JsonRpc { code, message })
            if super::jsonrpc::JsonRpcErrorKind::classify(code)
                == super::jsonrpc::JsonRpcErrorKind::RefusedByPolicy =>
        {
            let _ = runtime
                .journal_send_rejected(&spec, &task_id, &message)
                .await;
            RecipientOutcome::Declined
        }
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
        | A2aError::CaCertUnloadable { .. }
        // Story 19.18c `PQ1`: a local, ratified repair sentence — ⛔ never the
        // `_ =>` arm, which would render it as a peer-influenced endpoint cause.
        | A2aError::ItemCapabilityKeyUnusable { .. } => SendErrorClass::Trust,
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
    /// Story 19.21 `A15` — the retract's dispatch row could not be journaled,
    /// so no request was sent (`AD-1803`: durable before the POST). A local
    /// fact: ⛔ it attributes nothing to the peer.
    JournalDispatch,
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
    /// Story 19.18d `A13` — the host answered `principalBound: true`: this
    /// host's key is bound to its own pinned peer there, so no co-holder
    /// caveat applies. Absent or non-bool → `false` (fails toward disclosure).
    pub principal_bound: bool,
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
        principal_bound: principal_bound_flag(&result),
    })
}

/// Story 19.18d `A13` — `true` only for a literal `principalBound: true`;
/// absent, `false`, a string or a number all read `false`, so the co-holder
/// sentence renders (the board's fail-safe direction).
fn principal_bound_flag(result: &serde_json::Value) -> bool {
    result
        .get("principalBound")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Dispatch one accepted retract (Story 19.16f `AC2`, `AC5`, `AC10`).
///
/// Order is the invariant: resolve → journal the dispatch row (durable
/// **before** the POST; `M20`) → POST → classify on the variant → journal a
/// rejection row **only** when the answer proves nothing was marked. A
/// resolution failure sent nothing and journals nothing.
///
/// Story 19.18c `D2`/`A10` — the carried confirm-time `task` is checked
/// against this host's own dispatches (step 3) and, when own, its
/// capability rides the request; the journaled `bytes` count the params
/// actually sent.
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
    let capability = match task.as_deref() {
        Some(task) => runtime.own_task_capability(&spec, task).await,
        None => None,
    };
    let params = super::driver::item_verb_params(item_id, capability.as_ref());
    let bytes = params.to_string().len();
    let request_started = AtomicBool::new(false);
    let outcome = match transport
        .retract_item(params, || async {
            // Story 19.21 `A15` — fail closed: a dispatch whose row is not
            // durable must not POST. The append result decides.
            runtime
                .journal_item_retract_dispatch(&spec, task.as_deref(), item_id, bytes)
                .await
                .map_err(|_| A2aError::JournalDispatch)?;
            request_started.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
    {
        Ok(result) => retract_success(&result),
        Err(A2aError::JournalDispatch) => RetractOutcome::NotSent(RetractNotSent::JournalDispatch),
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

// ── Story 19.18 · the asker's thread read ───────────────────────────────────
//
// The served verb is `x-rustain-items/thread` on the ANSWERING host; this side
// reads it, bounds it (the asker does not trust the answering host, `P12`),
// settles it against its own ledger and journals each peer reply turn once.
// ⛔ Nothing here polls: one operator act, one read.

/// One served turn after the asker's distrust bounds. Fields hold the wire
/// values raw; the render is the single sanitize point (`AD-1824`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedTurn {
    pub turn_id: String,
    /// `true` for a reply turn (the peer's words), `false` for an item turn.
    pub reply: bool,
    /// An item turn's `task` — the one the `you` check matches.
    pub task: Option<String>,
    pub in_reply_to: Option<String>,
    /// An item turn the recipient removed: rendered with no body.
    pub removed: bool,
    /// The body, or `None` when the host withheld it or it exceeded this
    /// host's bound (`MAX_REPLY_BYTES`).
    pub content: Option<String>,
    /// Hash of the body this host received, when it received one.
    pub content_hash: Option<crate::domain::models::ContentHash>,
    pub kind: crate::domain::models::ReplyKind,
    pub authorship: crate::domain::models::ReplyAuthorship,
    pub retracted_at_ms: Option<i64>,
}

/// One served thread, bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedThread {
    pub root: String,
    pub turns: Vec<ServedTurn>,
    /// Non-root turns the host (or this host's own bound) left out.
    pub omitted_before: usize,
    /// Fails SAFE: an absent flag is read as collapsed, like the board.
    pub principal_collapsed: bool,
    /// Story 19.18d `A13` — fails toward disclosure: absent or non-bool reads
    /// `false`, so the co-holder sentence renders.
    pub principal_bound: bool,
}

/// What one `/team thread` read produced.
#[derive(Debug)]
#[non_exhaustive]
pub enum ThreadRead {
    /// The thread, with — per turn — whether an item turn may render `you`.
    Read {
        thread: ServedThread,
        mine: Vec<bool>,
    },
    /// `-32001`: no thread this credential can address — byte-identical
    /// whether it never existed or is another principal's (R21).
    NotFound,
    /// `-32040`: the peer serves no threads to API-key callers (`AC12`).
    AccessOff,
    /// The roster names no such peer (the shipped `UnknownPeer` sentence).
    UnknownPeer { known: Vec<String> },
    /// Any other failure, in 19.17's outcome vocabulary — ⛔ no new cause
    /// words: `unreachable` (with its cause line) or `no usable answer`.
    Failed(RecipientOutcome),
}

/// Read one thread on `peer_id`'s host and settle it against this host's
/// ledger. The sole production caller is the rail-3 `/team thread` spawn.
pub async fn read_thread(
    runtime: &A2aDelegationRuntime,
    peer_id: &str,
    item_id: &str,
) -> ThreadRead {
    use super::jsonrpc::JsonRpcErrorKind;

    let Some((spec, client)) = runtime.peer_binding(peer_id) else {
        return ThreadRead::UnknownPeer {
            known: runtime.known_peer_ids(),
        };
    };
    let card = match client.card_slot().await {
        CardSlot::Ready(card, _) => card,
        CardSlot::AnchorRefused(cause) => {
            return ThreadRead::Failed(RecipientOutcome::Unreachable {
                cause: Some(SendCause::Ratified(
                    anchor_error(&spec.id, &cause).to_string(),
                )),
            });
        }
        CardSlot::Pending | CardSlot::Unavailable => {
            return ThreadRead::Failed(RecipientOutcome::Unreachable {
                cause: Some(SendCause::Ratified(
                    SendError::CardNotCached {
                        peer: peer_id.to_owned(),
                    }
                    .to_string(),
                )),
            });
        }
    };
    let endpoint = match resolve_jsonrpc_endpoint(&card) {
        Ok(endpoint) => endpoint,
        Err(source) => {
            return ThreadRead::Failed(RecipientOutcome::Unreachable {
                cause: Some(SendCause::PeerInfluenced(
                    SendError::Endpoint {
                        peer: peer_id.to_owned(),
                        source,
                    }
                    .to_string(),
                )),
            });
        }
    };
    let transport = TaskClient::new(client, endpoint.url().to_owned());
    // Story 19.18c `A10` — resolve this host's capability for the item first;
    // a failed list read renders like a failed thread read and sends nothing.
    let capability = match capability_for(runtime, &spec, &transport, item_id).await {
        Ok(capability) => capability,
        Err(error) => return ThreadRead::Failed(transport_failure(&error)),
    };
    let result = match transport.read_thread(item_id, capability.as_ref()).await {
        Ok(result) => result,
        Err(A2aError::JsonRpc { code, .. }) => {
            return match JsonRpcErrorKind::classify(code) {
                JsonRpcErrorKind::TaskNotFound => ThreadRead::NotFound,
                JsonRpcErrorKind::RefusedByPolicy => ThreadRead::AccessOff,
                _ => ThreadRead::Failed(RecipientOutcome::NoUsableAnswer),
            };
        }
        Err(error) => return ThreadRead::Failed(transport_failure(&error)),
    };
    let Some(thread) = parse_thread(&result) else {
        return ThreadRead::Failed(RecipientOutcome::NoUsableAnswer);
    };
    let observed: Vec<super::driver::ObservedTurn<'_>> = thread
        .turns
        .iter()
        .map(|turn| {
            if turn.reply {
                super::driver::ObservedTurn::Reply {
                    turn: &turn.turn_id,
                    answers: turn.in_reply_to.as_deref(),
                    content: turn.content.as_deref(),
                    content_hash: turn.content_hash,
                }
            } else {
                super::driver::ObservedTurn::Item {
                    task: turn.task.as_deref(),
                    content_hash: turn.content_hash,
                }
            }
        })
        .collect();
    let mine = runtime.settle_thread(&spec, &observed).await;
    ThreadRead::Read { thread, mine }
}

/// A failed read in 19.17's outcome vocabulary — ⛔ no new cause words:
/// `unreachable` (with its cause line) or `no usable answer`.
fn transport_failure(error: &A2aError) -> RecipientOutcome {
    match send_error_class(error) {
        SendErrorClass::Connect => RecipientOutcome::Unreachable { cause: None },
        SendErrorClass::Trust => RecipientOutcome::Unreachable {
            cause: Some(SendCause::Ratified(error.to_string())),
        },
        SendErrorClass::Unproven => RecipientOutcome::NoUsableAnswer,
    }
}

/// Parse and bound one served thread (`P12`). `None` when the answer is not a
/// thread this build can read: no `turns` array, a turn with no id or an id
/// over `MAX_PEER_ID_BYTES`, or a kind this build cannot name — ⛔ never
/// rendered as if it were a partial thread.
pub(crate) fn parse_thread(result: &serde_json::Value) -> Option<ServedThread> {
    use crate::domain::models::{MAX_REPLY_BYTES, ReplyAuthorship, ReplyKind};

    let bounded = |value: Option<&serde_json::Value>| {
        value
            .and_then(serde_json::Value::as_str)
            .filter(|text| text.len() <= MAX_PEER_ID_BYTES)
            .map(str::to_owned)
    };
    let root = bounded(result.get("root"))?;
    let mut turns = Vec::new();
    for turn in result.get("turns")?.as_array()? {
        let turn_id = turn
            .get("turnId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= MAX_PEER_ID_BYTES)?
            .to_owned();
        let reply = match turn.get("kind").and_then(serde_json::Value::as_str)? {
            "item" => false,
            "reply" => true,
            _ => return None,
        };
        let served = turn.get("content").and_then(serde_json::Value::as_str);
        let content_hash = served.map(super::driver::text_content_hash);
        let content = served
            .filter(|content| content.len() <= MAX_REPLY_BYTES)
            .map(str::to_owned);
        turns.push(ServedTurn {
            turn_id,
            reply,
            task: bounded(turn.get("task")),
            in_reply_to: bounded(turn.get("inReplyTo")),
            removed: turn.get("state").and_then(serde_json::Value::as_str) == Some("removed"),
            content,
            content_hash,
            authorship: turn
                .get("authorship")
                .and_then(serde_json::Value::as_str)
                .map_or(ReplyAuthorship::Unknown, ReplyAuthorship::from_wire),
            kind: match turn.get("replyKind") {
                None => ReplyKind::Answer,
                Some(serde_json::Value::String(kind)) => match kind.as_str() {
                    "answer" => ReplyKind::Answer,
                    "defer" => match turn.get("deferMinutes").and_then(serde_json::Value::as_u64) {
                        Some(minutes) if (1..=u64::from(u16::MAX)).contains(&minutes) => {
                            ReplyKind::Defer {
                                minutes: minutes as u16,
                            }
                        }
                        _ => ReplyKind::Unknown,
                    },
                    _ => ReplyKind::Unknown,
                },
                Some(_) => ReplyKind::Unknown,
            },
            retracted_at_ms: turn
                .get("retractedAtMs")
                .and_then(serde_json::Value::as_i64),
        });
    }
    let mut omitted_before = result
        .get("omittedBefore")
        .and_then(serde_json::Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0);
    // A hostile host can serve more than it promised: keep the root and the
    // newest turns under the shared bound, and count the rest as omitted.
    if turns.len() > crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURNS {
        let root_turn = turns
            .iter()
            .position(|turn| turn.turn_id == root)
            .map(|index| turns.remove(index));
        let keep = crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURNS
            - usize::from(root_turn.is_some());
        let cut = turns.len() - keep;
        omitted_before = omitted_before.saturating_add(cut);
        turns.drain(..cut);
        if let Some(root_turn) = root_turn {
            turns.insert(0, root_turn);
        }
    }
    Some(ServedThread {
        root,
        turns,
        omitted_before,
        principal_collapsed: result
            .get("principalCollapsed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        principal_bound: principal_bound_flag(result),
    })
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
        parse_thread,
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

    /// Story 19.21 rule 9 — the live-send registry's lifecycle, observed on the
    /// SAME runtime (a restart's fresh runtime starts empty and proves nothing
    /// about cleanup).
    ///
    /// A slot is live exactly while its recipient token has an owner; a dropped
    /// (cancelled) send is dead on the very next read; and recording the next
    /// action prunes every dead slot, so repeated cancellations cannot grow the
    /// registry.
    ///
    /// **Mutants → RED:** `M57-no-prune-on-insert` (the insert-time prune is
    /// removed: the dead first slot is still counted), `M57-stale-entry-live`
    /// (a dead entry's mere presence answers `live`), `M57-shared-action-token`
    /// (one token for every slot: the dropped sibling below stays live).
    #[tokio::test]
    async fn dropped_recipient_tokens_are_dead_and_the_next_insert_prunes_them() {
        let known = peer("known");
        let client = Arc::new(client::A2aClientAdapter::new(&known, None).expect("client"));
        let rt = runtime(vec![(known, client)]);
        let two = ["known".to_owned(), "known".to_owned()];

        // Action 1: two recipient slots, each with its own owner.
        let first = rt
            .record_send_action(&two, "one", None, None)
            .await
            .expect("the action is recorded");
        let first_id = first.id().clone();
        let mut owners = first.into_recipients();
        assert_eq!(rt.live_send_slot_entries().await, 2);
        assert!(
            rt.send_recipient_is_live(&first_id, 0).await
                && rt.send_recipient_is_live(&first_id, 1).await,
            "positive control: owned tokens are live"
        );

        // Slot 0's recipient finishes (its token drops); slot 1 is still held.
        drop(owners.remove(0));
        assert!(
            !rt.send_recipient_is_live(&first_id, 0).await,
            "a finished recipient is not sending, however long its sibling takes"
        );
        assert!(
            rt.send_recipient_is_live(&first_id, 1).await,
            "the held sibling is still live"
        );

        // The cancelled send drops every remaining owner. Nothing reads the
        // registry before the next insert, so only the insert-time prune can
        // remove the dead slots.
        drop(owners);
        let second = rt
            .record_send_action(&two, "two", None, None)
            .await
            .expect("the next action is recorded");
        assert_eq!(
            rt.live_send_slot_entries().await,
            2,
            "recording the next action pruned the dead action; only its own slots remain"
        );
        assert!(
            !rt.send_recipient_is_live(&first_id, 1).await,
            "a dropped send is dead on the very next read"
        );
        let second_id = second.id().clone();
        assert!(
            rt.send_recipient_is_live(&second_id, 0).await,
            "the newer action is unaffected"
        );
    }

    #[tokio::test]
    async fn configured_peer_without_cached_card_refuses_without_on_demand_fetch() {
        let known = peer("known");
        let client = Arc::new(client::A2aClientAdapter::new(&known, None).expect("client"));
        let outcome = deliver_text(
            &runtime(vec![(known, client)]),
            "known",
            "hello",
            None,
            None,
        )
        .await;
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
            deliver_text(&runtime, "questions", "hello", None, None).await,
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
        let (message, _) = outbound_message(text, None, super::MessageCarriers::default());
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

    /// Story 19.18 `K19` (in-crate half, recorded as such: no tampering proxy
    /// is constructible in the front-door harness) — the asker's distrust
    /// bounds on a served thread (`P12`). An oversized turn id makes the
    /// answer unusable; an oversized body is withheld with its hash kept;
    /// more than `MAX_THREAD_TURNS` turns keep the root and the newest.
    #[test]
    fn a_served_thread_is_bounded_before_it_is_believed() {
        use crate::domain::models::MAX_REPLY_BYTES;
        use crate::domain::services::recipient_thread_bounds::MAX_THREAD_TURNS;
        use crate::domain::services::transparency::MAX_PEER_ID_BYTES;

        let turn = |id: &str, kind: &str, content: &str| serde_json::json!({ "turnId": id, "kind": kind, "content": content, "ordinal": 0 });
        let oversized_id = serde_json::json!({
            "root": "ri_q",
            "turns": [turn(&"x".repeat(MAX_PEER_ID_BYTES + 1), "item", "hi")],
            "omittedBefore": 0,
        });
        assert!(
            parse_thread(&oversized_id).is_none(),
            "an oversized turn id is refused"
        );
        let huge = "y".repeat(MAX_REPLY_BYTES + 1);
        let oversized_body = serde_json::json!({
            "root": "ri_q",
            "turns": [turn("ri_q", "item", "q"), turn("rr_1", "reply", &huge)],
            "omittedBefore": 0,
            "principalCollapsed": false,
        });
        let parsed = parse_thread(&oversized_body).expect("usable");
        assert_eq!(parsed.turns[1].content, None, "withheld, never believed");
        assert!(parsed.turns[1].content_hash.is_some(), "its hash is kept");
        assert!(!parsed.principal_collapsed);
        let flood: Vec<_> = std::iter::once(turn("ri_q", "item", "q"))
            .chain(
                (0..MAX_THREAD_TURNS + 5).map(|index| turn(&format!("rr_{index}"), "reply", "a")),
            )
            .collect();
        let parsed =
            parse_thread(&serde_json::json!({ "root": "ri_q", "turns": flood })).expect("usable");
        assert_eq!(parsed.turns.len(), MAX_THREAD_TURNS);
        assert_eq!(parsed.turns[0].turn_id, "ri_q");
        assert_eq!(
            parsed.turns.last().map(|turn| turn.turn_id.clone()),
            Some(format!("rr_{}", MAX_THREAD_TURNS + 4))
        );
        assert_eq!(parsed.omitted_before, 6);
        assert!(parsed.principal_collapsed, "an absent flag fails safe");
        let unknown_kind =
            serde_json::json!({ "root": "ri_q", "turns": [turn("ri_q", "verdict", "x")] });
        assert!(parse_thread(&unknown_kind).is_none());
    }

    /// Story 19.18d `K08` (parser half) — only a literal `principalBound: true`
    /// suppresses the co-holder sentence; absent, `false`, a string or a
    /// number read `false`, so the sentence renders (`M17`, `M46`).
    #[test]
    fn principal_bound_is_true_only_for_a_literal_true() {
        let thread = |flag: Option<serde_json::Value>| {
            let mut result = serde_json::json!({
                "root": "ri_q",
                "turns": [{ "turnId": "ri_q", "kind": "item", "content": "q", "ordinal": 0 }],
                "omittedBefore": 0,
                "principalCollapsed": false,
            });
            if let Some(flag) = flag {
                result["principalBound"] = flag;
            }
            parse_thread(&result).expect("usable").principal_bound
        };
        assert!(thread(Some(serde_json::json!(true))), "positive control");
        for flag in [
            None,
            Some(serde_json::json!(false)),
            Some(serde_json::json!("true")),
            Some(serde_json::json!(1)),
        ] {
            assert!(!thread(flag.clone()), "{flag:?}");
        }
    }

    #[test]
    fn additive_reply_kind_is_bounded_without_poisoning_a_thread() {
        use crate::domain::models::ReplyKind;

        let parse = |kind: Option<serde_json::Value>, minutes: Option<serde_json::Value>| {
            let mut turn = serde_json::json!({
                "turnId": "rr_defer",
                "kind": "reply",
                "content": "Busy — defer 30m",
            });
            if let Some(kind) = kind {
                turn["replyKind"] = kind;
            }
            if let Some(minutes) = minutes {
                turn["deferMinutes"] = minutes;
            }
            parse_thread(&serde_json::json!({
                "root": "ri_q",
                "turns": [
                    {"turnId":"ri_q","kind":"item","content":"question"},
                    turn
                ]
            }))
            .expect("bad additive values cannot erase thread")
        };
        assert_eq!(
            parse(None, None).turns[1].kind,
            ReplyKind::Answer,
            "old readers' structural reply remains an answer"
        );
        assert_eq!(
            parse(Some(serde_json::json!("defer")), None).turns[1].kind,
            ReplyKind::Unknown,
            "a missing interval is unknown"
        );
        for kind in [
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!("future"),
            serde_json::json!({"type":"defer","minutes":30}),
        ] {
            assert_eq!(
                parse(Some(kind.clone()), Some(serde_json::json!(30))).turns[1].kind,
                ReplyKind::Unknown,
                "{kind}"
            );
        }
        for minutes in [
            None,
            Some(serde_json::json!(null)),
            Some(serde_json::json!(false)),
            Some(serde_json::json!("30")),
            Some(serde_json::json!([])),
            Some(serde_json::json!({})),
            Some(serde_json::json!(0)),
            Some(serde_json::json!(-1)),
            Some(serde_json::json!(65536)),
            Some(serde_json::json!(1.0)),
        ] {
            let thread = parse(Some(serde_json::json!("defer")), minutes.clone());
            assert_eq!(thread.turns[1].kind, ReplyKind::Unknown, "{minutes:?}");
            assert_eq!(
                thread.turns[1].content.as_deref(),
                Some("Busy — defer 30m"),
                "invalid additive intervals must not withhold a valid reply body"
            );
        }
        for minutes in [1, u16::MAX] {
            assert_eq!(
                parse(
                    Some(serde_json::json!("defer")),
                    Some(serde_json::json!(minutes)),
                )
                .turns[1]
                    .kind,
                ReplyKind::Defer { minutes }
            );
        }
    }
}
