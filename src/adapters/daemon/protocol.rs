//! Daemon ↔ client attach wire protocol (Story 12.2b AC2/AC7).
//!
//! Replaces the 12.1a accept-stub (`lifecycle.rs` drop+log) with a **framed,
//! versioned, bidirectional** protocol over the per-workspace Unix socket. This
//! is the *producer* half: the daemon speaks [`DaemonFrame`], the attach client
//! (Story 12.2c) speaks [`ClientFrame`].
//!
//! ## Framing (zero-Cargo-change, AC7)
//!
//! Each frame is a **`u32` big-endian length prefix + a `serde_json` body**. No
//! `tokio-util` `codec` feature is enabled (it is not a dependency today);
//! [`write_frame`]/[`read_frame`] hand-roll the length-delimiting over any
//! `AsyncRead`/`AsyncWrite`, so they unit-test over an in-memory
//! `tokio::io::duplex` / `UnixStream::pair()` with no real socket. A
//! [`MAX_FRAME_BYTES`] cap bounds the allocation a hostile/garbled length prefix
//! can force.
//!
//! ## `ClientEvent` is the existing `RawEvent` projection — NOT a new one (AC2)
//!
//! [`ClientEvent`] is a **type alias for [`RawEvent`]**
//! (`event_bus.rs::from_app_event`), the project's single serializable projection
//! of `AppEvent`. The daemon's turn forwarder maps each `AppEvent` through that
//! one function and ships the result as [`DaemonFrame::Event`]; the client
//! deserializes the same shape. There is exactly **one** `AppEvent → wire`
//! mapping, in one place — Stories 12.3/12.4 extend `from_app_event`'s coverage
//! there, not here. (`RawEvent` gained `Deserialize` for this in 12.2b; raw
//! `AppEvent` is never serialized — it is `Debug`-only and carries `oneshot`
//! handles.)

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::adapters::rap::AgentSigner;
use crate::domain::models::tool_call::{ApprovalSource, RequestId};
use crate::domain::models::{
    AgentEnvelope, ApprovalOutcome, ChannelKind, ChatMessage, Ed25519Sig, ImageAttachment,
    PeerIdentity, PermissionMode, SemanticMessageType, SendActionId, TeamProposalDecision,
    ToolRisk,
};
use crate::infrastructure::runtime::event_bus::RawEvent;

/// Current wire protocol version. Bump on ANY breaking frame-shape change so an
/// older client/daemon is rejected with [`ProtocolError::VersionMismatch`]
/// rather than mis-parsing (forward-compat for 12.3/12.4).
///
/// v10 (2026-10-24, Story 19.21c): attached thread/followup/retract parity and
/// request-keyed shared view streams; reject older clients before proof.
/// v11 (Story 19.21d): TeamSend carries stories; PeerRowControl carries the story summary; an undecodable client frame is answered and skipped
/// v12 (Story 19.24): ShareRecipientStatus; PeerRowControl carries the status preview
pub const PROTOCOL_VERSION: u32 = 12;

/// Hard cap on a single frame's JSON body (8 MiB). A length prefix larger than
/// this is rejected before allocation — a garbled or hostile peer cannot force
/// an unbounded `Vec` allocation.
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// The serializable projection of `AppEvent` forwarded daemon→client.
///
/// **Reuses [`RawEvent`] wholesale** (see module docs) — one projection, one
/// place. Declared as an alias (not a wrapper) so there is no second type to
/// keep in sync.
pub type ClientEvent = RawEvent;

/// Whether an attachment may submit turns / approvals (writer) or only observe
/// (reader). Multi-attach makes later writers read-only (Story 12.2c); the
/// daemon is the authority — it tags the grant in [`DaemonFrame::AttachAck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttachMode {
    /// May send `UserMessage`/`ApprovalResponse`.
    ReadWrite,
    /// Observe only — write frames are rejected with [`ProtocolError::ReadOnly`].
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionTier {
    #[default]
    TrustedLocal,
    Peer,
}
impl ConnectionTier {
    /// The stable tag bound into an attach-proof transcript. The client signs
    /// over this string and the server verifies against it, so a proof minted
    /// for one tier cannot be replayed (or silently swapped) for another.
    pub const fn proof_tag(self) -> &'static str {
        match self {
            ConnectionTier::TrustedLocal => "trusted-local",
            ConnectionTier::Peer => "peer",
        }
    }
}

/// The initial state the daemon hands a freshly-attached client so it can render
/// the conversation immediately (AC2 `AttachAck.snapshot`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachSnapshot {
    /// The daemon's per-process conversation id.
    pub conversation_id: String,
    /// The current transcript (origin-tagged — AC5). The client may page further
    /// back via [`ClientFrame::HistoryRequest`].
    #[serde(serialize_with = "transcript_serde::serialize")]
    pub transcript: Vec<ChatMessage>,
    /// The daemon's current permission mode (never `Yolo` headless — AC6).
    pub permission_mode: PermissionMode,
    /// Connected channels (honest — AC4 / `StatusSnapshot.channels`).
    pub channels: Vec<ChannelKind>,
    /// Count of tool actions denied-while-unattended and waiting to be resumed
    /// (AC6 #5 — "N actions waiting on you"). The transcript-render of the
    /// individual skipped actions is 12.2c; 12.2b emits the count.
    pub blocked_actions_waiting: usize,
    /// Count of sender-consent cards outstanding and awaiting the designated
    /// writer (F5). Read-only mirrors render this as a passive "N consent
    /// decisions pending" notice; only the ReadWrite writer receives the
    /// actionable card frames.
    #[serde(default)]
    pub pending_consent_cards: usize,
    /// Story 19.20c `O2`: tool approvals retained for the next trusted-local
    /// writer. A read-only mirror prints it as a passive one-line notice; the
    /// writer receives the requests themselves, never this count's card.
    #[serde(default)]
    pub retained_approvals: usize,
    /// Trusted-local row targets only; absent for authenticated Peer tier.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub peer_row_controls:
        std::collections::BTreeMap<String, crate::domain::models::PeerRowControl>,
    #[serde(default, skip_serializing_if = "peer_row_revision_is_zero")]
    pub peer_row_revision: u64,
    /// Process-conversation selection shared by trusted-local attachments only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipient_context_ids: Vec<String>,
    /// Story 19.20c `A6`: transcript tool calls whose arguments this
    /// connection did not receive, and why. Their `input` key is absent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld_tool_inputs: Vec<WithheldToolInput>,
}

/// Story 19.20c `A6` — the typed, host-minted reason a projected tool event or
/// transcript tool call carries no arguments. Never a sentinel inside `input`:
/// the projected `input` key is absent on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolInputWithheld {
    /// Peer-tier attachments never receive tool arguments (owner ruling O1).
    PeerTier,
    /// The arguments' serialized size does not fit one attach frame.
    TooLarge { bytes: u64 },
}

impl ToolInputWithheld {
    /// The ratified tool-block copy (UX Approval Pattern clarification
    /// 2026-10-20, Observers / Too large rows).
    #[must_use]
    pub fn notice(self) -> String {
        match self {
            Self::PeerTier => "input withheld — not shared with peer attachments".to_owned(),
            Self::TooLarge { bytes } => {
                format!("input too large to show here ({})", human_bytes(bytes))
            }
        }
    }
}

/// Client copy for [`DaemonFrame::FrameTooLarge`] (Story 19.20c `P6`):
/// `<frame> too large to show here (<size>)`, mirroring the ratified
/// tool-input notice.
#[must_use]
pub fn frame_too_large_notice(frame: &str, bytes: u64) -> String {
    format!("{frame} too large to show here ({})", human_bytes(bytes))
}

/// `<n> B` / `<n.n> KiB` / `<n.n> MiB` — sizes for the too-large notice.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{:.1} MiB", value / (KIB * KIB))
    }
}

/// One transcript tool call projected without its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithheldToolInput {
    pub message_id: String,
    pub tool_call_id: String,
    pub withheld: ToolInputWithheld,
}

/// Why the daemon refused an [`ClientFrame::ApprovalResponse`] (Story 19.20c
/// `A7`). A refusal resolves nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalRefusal {
    /// This connection is not the registered trusted-local writer.
    WriterOnly,
    /// The request is pending but was issued to another connection.
    IssuedElsewhere,
    /// No such pending request: already decided, cancelled, or never issued.
    NoLongerPending,
}

impl std::fmt::Display for ApprovalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::WriterOnly => "approval refused: only the attached writer can decide",
            Self::IssuedElsewhere => "approval refused: that request was issued to another client",
            Self::NoLongerPending => "approval refused: that request is no longer pending",
        })
    }
}

/// Serialize transcript messages with every `null` tool-call `input` key
/// omitted. Lossless: `ToolCallInfo.input` defaults to `null` on decode. A
/// withheld projection therefore carries no placeholder value on the wire.
mod transcript_serde {
    use super::ChatMessage;
    use serde::Serialize as _;

    pub fn serialize<S: serde::Serializer>(
        messages: &[ChatMessage],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(messages).map_err(serde::ser::Error::custom)?;
        if let Some(messages) = value.as_array_mut() {
            for message in messages {
                let calls = message
                    .get_mut("toolCalls")
                    .and_then(serde_json::Value::as_array_mut);
                for call in calls.into_iter().flatten() {
                    super::remove_null_input(call);
                }
            }
        }
        value.serialize(serializer)
    }
}

/// Serialize one projected event with its tool `input` key omitted (the
/// typed [`ToolInputWithheld`] beside it says why).
mod withheld_event_serde {
    use super::RawEvent;
    use serde::Serialize as _;

    pub fn serialize<S: serde::Serializer>(
        event: &RawEvent,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut value = serde_json::to_value(event).map_err(serde::ser::Error::custom)?;
        for pointer in [
            "/kind/Provider/toolUse",
            "/kind/PeerProvider/toolUse",
            "/kind/Tool/call/request",
        ] {
            if let Some(carrier) = value.pointer_mut(pointer) {
                super::remove_null_input(carrier);
            }
        }
        value.serialize(serializer)
    }
}

fn remove_null_input(carrier: &mut serde_json::Value) {
    if let Some(object) = carrier.as_object_mut() {
        if object.get("input").is_some_and(serde_json::Value::is_null) {
            object.remove("input");
        }
    }
}

fn exact_input_chunk_budget() -> usize {
    // Six bytes per source byte covers `\u00XX`; the remainder is envelope.
    const ENVELOPE_RESERVE: u32 = 64 * 1024;
    (((MAX_FRAME_BYTES - ENVELOPE_RESERVE) / 6) as usize).min(1024 * 1024)
}

/// Story 19.20c `P6`: the one-line summary on the wire is bounded by the same
/// frame-derived budget as one exact-input chunk, ending in `…` when cut — the
/// complete input follows in chunks on the same surface, so it is never lost.
#[must_use]
pub fn bounded_wire_summary(summary: String) -> String {
    let budget = exact_input_chunk_budget();
    if summary.len() <= budget {
        return summary;
    }
    let mut end = budget;
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &summary[..end])
}

/// Story 19.20c `A10` — split the serialized exact input into ordered,
/// UTF-8-safe chunks whose frames stay under [`MAX_FRAME_BYTES`] even at
/// worst-case JSON escaping. Ordinary inputs are one chunk; an empty input is
/// one empty chunk, so every transfer has at least one part.
#[must_use]
pub fn exact_input_chunks(serialized: &str) -> Vec<&str> {
    let budget = exact_input_chunk_budget();
    if serialized.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < serialized.len() {
        let mut end = start.saturating_add(budget).min(serialized.len());
        while end > start && !serialized.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = serialized[start..]
                .char_indices()
                .nth(1)
                .map_or(serialized.len(), |(offset, _)| start + offset);
        }
        chunks.push(&serialized[start..end]);
        start = end;
    }
    chunks
}

/// Why a client discarded an exact-input transfer. The request stays
/// decidable; only Review is unavailable (deciding without Review is allowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactInputError {
    OutOfOrder { expected: u32, got: u32 },
    Overflow { expected_bytes: u64 },
    CountMismatch { expected: u32, got: u32 },
    LengthMismatch { expected_bytes: u64, got_bytes: u64 },
    Decode(String),
}

/// Story 19.20c `A10` — client-side reassembly of one request's exact input.
/// Review is published only from [`Self::finish`]'s `Ok`: partial, missing,
/// duplicate or out-of-order chunks never appear complete. The caller keys one
/// assembly per `request_id`, so another request's chunks never mix in.
#[derive(Debug)]
pub struct ExactInputAssembly {
    expected_bytes: u64,
    next_sequence: u32,
    buffer: String,
}

impl ExactInputAssembly {
    #[must_use]
    pub fn new(expected_bytes: u64) -> Self {
        Self {
            expected_bytes,
            next_sequence: 0,
            buffer: String::new(),
        }
    }

    /// Append the next chunk; anything but the next sequence is an error.
    pub fn push(&mut self, sequence: u32, data: &str) -> Result<(), ExactInputError> {
        if sequence != self.next_sequence {
            return Err(ExactInputError::OutOfOrder {
                expected: self.next_sequence,
                got: sequence,
            });
        }
        let total = self.buffer.len() as u64 + data.len() as u64;
        if total > self.expected_bytes {
            return Err(ExactInputError::Overflow {
                expected_bytes: self.expected_bytes,
            });
        }
        self.buffer.push_str(data);
        self.next_sequence += 1;
        Ok(())
    }

    /// Validate the completion boundary and decode the exact value.
    pub fn finish(self, chunks: u32) -> Result<serde_json::Value, ExactInputError> {
        if chunks != self.next_sequence {
            return Err(ExactInputError::CountMismatch {
                expected: chunks,
                got: self.next_sequence,
            });
        }
        let got_bytes = self.buffer.len() as u64;
        if got_bytes != self.expected_bytes {
            return Err(ExactInputError::LengthMismatch {
                expected_bytes: self.expected_bytes,
                got_bytes,
            });
        }
        serde_json::from_str(&self.buffer)
            .map_err(|error| ExactInputError::Decode(error.to_string()))
    }
}

fn peer_row_revision_is_zero(revision: &u64) -> bool {
    *revision == 0
}

/// A protocol-level error reported to the peer (and surfaced to the user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProtocolError {
    /// The client's `protocol_version` does not match the daemon's
    /// [`PROTOCOL_VERSION`]. Carries both so the client can tell the user which
    /// side to upgrade.
    VersionMismatch { daemon: u32, client: u32 },
    /// A write frame (`UserMessage`/`ApprovalResponse`) arrived on a read-only
    /// attachment (AC6 / multi-attach). Daemon-enforced, not client-honor.
    ReadOnly,
    /// The Attach challenge-response proof was absent, malformed, or failed
    /// verification (bad signature, nonce mismatch, identity not bound to its
    /// key, or the transcript's tier/version/read-only did not match the claim).
    /// Reported before any connection registration or mutable side effect.
    AttachProof(String),
    /// The frame could not be decoded (bad JSON / unknown variant).
    Malformed(String),
    /// A peer-mode frame failed signature, identity, TTL, hash, or replay verification.
    PeerVerification(String),
    /// A peer envelope's signed predecessor does not match the sender's
    /// current single-session feed head.
    FeedForkOrGap,
    /// An internal daemon error prevented servicing the request.
    Internal(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::VersionMismatch { daemon, client } => write!(
                f,
                "attach protocol version mismatch: daemon speaks v{daemon}, client speaks v{client}"
            ),
            ProtocolError::ReadOnly => {
                write!(
                    f,
                    "this attachment is read-only; another client holds the writer slot"
                )
            }
            ProtocolError::Malformed(m) => write!(f, "malformed attach frame: {m}"),
            ProtocolError::PeerVerification(m) => write!(f, "peer frame verification failed: {m}"),
            ProtocolError::FeedForkOrGap => {
                write!(
                    f,
                    "peer frame rejected: signed feed predecessor does not match"
                )
            }
            ProtocolError::AttachProof(m) => {
                write!(f, "attach proof verification failed: {m}")
            }
            ProtocolError::Internal(m) => write!(f, "daemon error: {m}"),
        }
    }
}

/// Opaque token identifying a consolidation proposal generation — the confused-deputy
/// guard (Story 12.2d AC2/AC4, DQ1). Minted by the daemon on generation, carried by the
/// client back in `ConsolidationResolve`. Uniqueness is per-daemon-process via a monotonic
/// counter. Comparable by value — no crypto needed (local Unix socket trust boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProposalToken(pub u64);

/// Per-item identity for a consolidation proposal — the forward-compat seam for per-item
/// toggle (AI-12.2d-2). Stable within a generation; meaningless across generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProposalId(pub u32);

/// One consolidation proposal on the wire: a stable per-item [`ProposalId`] paired with
/// its [`MemoryFact`] (Story 12.2d AC2 / Fork-C). 12.2d resolves card-level
/// accept-all/decline-all, but the id is carried NOW so the per-item-toggle fast-follow
/// (AI-12.2d-2) is purely client-side and frame-additive — no wire-frame rework. The id
/// is the addressable handle the toggle will filter on; it never renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposedFact {
    pub id: ProposalId,
    pub fact: crate::domain::models::MemoryFact,
}

/// Trusted-local resolution of a daemon-owned peer response draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerDraftAction {
    Approve,
    Edit { content: String },
    Reject,
    WriteOwn { content: String },
}

/// Client → daemon frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientFrame {
    /// First frame after connect — negotiates version + declares whether the
    /// client accepts a read-only grant (multi-attach). MUST precede all others,
    /// and MUST answer the server's [`DaemonFrame::AttachChallenge`] with an
    /// Ed25519 proof of identity possession (Story 17.1a). Required for BOTH
    /// `TrustedLocal` and `Peer` tiers; TrustedLocal then keeps its existing
    /// unsigned application frames, Peer continues to send only `PeerEnvelope`.
    Attach {
        protocol_version: u32,
        read_only_ok: bool,
        #[serde(default)]
        tier: ConnectionTier,
        /// The server-issued challenge nonce this proof answers. Echoed back so
        /// the daemon can bind the proof to the exact challenge it minted (a
        /// proof captured elsewhere cannot satisfy a different connection's nonce).
        challenge_nonce: Vec<u8>,
        /// The identity whose private key produced `proof`. Verified to be
        /// self-consistent (PeerId matches the public key) inside `verify_attach_proof`.
        identity: PeerIdentity,
        /// Ed25519 signature over the domain-separated attach-proof transcript
        /// (nonce, version, tier tag, read-only flag) — supplied by
        /// [`AgentSigner::attach_proof`].
        proof: Ed25519Sig,
    },
    /// Signed peer-mode envelope. Only valid after an Attach with `tier=Peer`.
    PeerEnvelope(Box<AgentEnvelope<serde_json::Value>>),
    /// Submit a user turn (AC3). `images` are fresh attachments for this turn.
    UserMessage {
        text: String,
        images: Vec<ImageAttachment>,
    },
    /// Page conversation history backwards (AC2). `before_index` = the absolute
    /// transcript index to page before (`None` = from the tail); `count` items.
    HistoryRequest {
        before_index: Option<usize>,
        count: usize,
    },
    /// Resolve a forwarded [`DaemonFrame::ApprovalRequest`] (AC6 #1).
    ApprovalResponse {
        request_id: RequestId,
        outcome: ApprovalOutcome,
    },
    /// Answer a `Waiting` MCP-task node's elicitation ticket (17.5b / AC1, D2).
    /// `node` is the task node id (from the room's `TicketAssigned` / node view);
    /// `responses` is the raw `inputResponses` map (`{key: {action, content?}}`),
    /// carried as JSON so the non-`mcp` daemon protocol never depends on the
    /// `mcp`-gated `InputResponse` type. Gated on `AttachMode::ReadWrite`.
    InputResponse {
        node: String,
        responses: serde_json::Value,
    },
    /// Resolve a pending consolidation card — daemon-authoritative (Story 12.2d AC4).
    /// `accept: true` = promote all retained proposals; `false` = decline all.
    /// The token must match the daemon's retained entry (confused-deputy guard).
    ConsolidationResolve { token: ProposalToken, accept: bool },
    ResolvePeerDraft {
        node: String,
        action: PeerDraftAction,
    },
    /// Same-host, append-only retraction of one persisted automatic send.
    /// Only a trusted-local read-write attachment may dispatch it.
    /// `target_seq` is `None` on every normal dispatch: the daemon derives the
    /// journaled disclosure seq itself and merely validates a supplied value.
    RetractAutoResponse {
        message_id: String,
        #[serde(default)]
        target_seq: Option<u64>,
    },
    /// Share one room artifact's signed handle into a Topic with a pinned peer
    /// (Story 18.4a; code-review D3). TrustedLocal + ReadWrite only.
    ///
    /// Routed through the daemon rather than dialed from the CLI because the
    /// daemon is the one process holding both the Topic store and the bound
    /// transport — a CLI-side endpoint would sign with the same identity key
    /// over a second connection and fork the receiver's feed, and its store
    /// would be dropped at exit, so the sender host would never retain its
    /// own log.
    PeerShare {
        /// The alias to share with, as recorded by `peer add`.
        alias: String,
        /// The room artifact whose handle is shared.
        artifact: String,
        /// The Topic to share into (a correlation id).
        topic: String,
        /// A ≤240-byte human summary; `None` derives one from the artifact.
        summary: Option<String>,
    },
    /// Deliberately acknowledge one recipient-owned durable item.
    /// Trusted-local + read-write; the daemon also enforces the room-content role.
    AcknowledgeRecipientItem { item_id: String },
    /// Deliberately remove one recipient-owned durable item (FR165).
    /// Trusted-local + read-write; the daemon also enforces the room-content role.
    RemoveRecipientItem { item_id: String },
    /// Story 19.18 `AC2(a)` — answer one recipient-owned item with a reply
    /// turn recorded on THIS host (`/team reply`). Trusted-local + read-write
    /// + room-content role, the ack/remove gate. ⛔ Nothing leaves the host:
    /// the asker reads the turn by pulling the thread.
    ReplyToRecipientItem { item_id: String, text: String },
    /// Record a fixed local defer turn without resolving the pending real answer.
    /// Trusted-local + read-write + room-content role; no reverse delivery.
    DeferRecipientItem { item_id: String },
    /// Share the prepared arrival-time status statement, or canonical nothing.
    /// Trusted-local writer only; never accepts client-supplied status fields.
    ShareRecipientStatus { item_id: String, decline: bool },
    /// Read cached selection/last-use diagnostics without assembly or mutation.
    InspectRecipientContext,
    /// Select/off a daemon-owned received item reference, never a client body.
    SetRecipientContext { item_id: String, selected: bool },
    /// Opt in to the proposal decision surface after a successful attach.
    /// Only a capable TrustedLocal + ReadWrite client may enable publication.
    EnableTeamProposals,
    /// Decide one daemon-owned proposal snapshot. TrustedLocal + ReadWrite only.
    /// Send uses the ordinary send coordinator with `request = proposal`.
    TeamProposalDecision {
        proposal: String,
        decision: TeamProposalDecision,
    },
    /// Story 19.21 `AC4(b)` — one operator `/team send`. `request` is a
    /// client-minted key, unique per connection, echoed by every result and
    /// refusal this command produces. TrustedLocal + ReadWrite only; the
    /// daemon's one send coordinator runs it (never a connection's token).
    TeamSend {
        request: String,
        recipients: Vec<String>,
        text: String,
        #[serde(default)]
        message_type: Option<SemanticMessageType>,
        #[serde(default)]
        subject: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stories: Option<crate::domain::models::StoryManifest>,
    },
    /// Story 19.21 `AC4(b)` — one `/team board` read. TrustedLocal; a
    /// read-only attachment may ask. ⛔ Never pushed: it is an explicit read.
    TeamBoard {
        request: String,
        selector: TeamBoardRequest,
    },
    /// Story 19.21c: trusted-local thread read, including read-only attachments.
    TeamThread {
        request: String,
        peer: String,
        item_id: String,
    },
    /// Story 19.21c: a writer's single-recipient linked human followup.
    TeamFollowup {
        request: String,
        peer: String,
        reply_to: String,
        text: String,
    },
    /// Story 19.21c: writer-only preview; the target stays daemon-owned.
    TeamRetractPreview {
        request: String,
        peer: String,
        item_id: String,
    },
    /// Story 19.21c: compare-and-take the origin connection's preview.
    TeamRetractDecision { request: String, confirm: bool },
    /// Story 19.21 `AC4(b)` — `/team status`, computed
    /// daemon-side over the daemon's composed roster and consent projection.
    /// TrustedLocal; a read-only attachment may ask.
    TeamStatus { request: String },
    /// Story 19.21 `AC4(d)` — `/team untrust <alias>`: revoke the standing
    /// sender consent durably through the daemon's consent path.
    /// TrustedLocal + ReadWrite only.
    RevokeSenderConsent { request: String, alias: String },
    /// Detach cleanly (the turn continues daemon-side — AC4).
    Detach,
}

/// A sender-local proposal lifecycle result, never a recipient delivery outcome.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamProposalCloseOutcome {
    Dismissed,
    Withdrawn,
    Refused { reason: String },
}

/// Which board a [`ClientFrame::TeamBoard`] asks for (Story 19.21 `AC3(a)`).
/// The wire twin of the TUI's `TeamBoardSelector`; the two convert in the
/// client and in the bridge, so the daemon never names a TUI type.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamBoardRequest {
    Inventory,
    Peer(String),
    Last,
    Action(String),
    Actions,
}

/// Story 19.21c: feature-independent rendered preview; never carries a task.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamRetractPreviewView {
    Card {
        peer: String,
        item_id: String,
        body: String,
        armed: bool,
    },
    Answer {
        message: String,
    },
}

/// Why a recipient's send did not complete, with its provenance (Story 19.21
/// `AC1(d)`): a ratified local sentence versus text a peer influenced.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TeamSendCauseKind {
    Ratified,
    PeerInfluenced,
}

/// A typed send-failure cause on the wire — never a pre-rendered row.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamSendCause {
    pub kind: TeamSendCauseKind,
    pub text: String,
}

/// The wire serialization shape for one feature-independent domain
/// `RecipientOutcome`; the client re-renders it through the shared row renderer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TeamSendOutcome {
    Delivered,
    Declined,
    AwaitingApproval,
    AcceptedWithoutItem,
    AskedQuestion {
        task_id: String,
    },
    Unreachable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<TeamSendCause>,
    },
    NoUsableAnswer,
    NotSent,
}

impl From<&crate::domain::models::RecipientOutcome> for TeamSendOutcome {
    fn from(outcome: &crate::domain::models::RecipientOutcome) -> Self {
        use crate::domain::models::{RecipientOutcome as Outcome, SendCause};
        match outcome {
            Outcome::Delivered => Self::Delivered,
            Outcome::Declined => Self::Declined,
            Outcome::AwaitingApproval => Self::AwaitingApproval,
            Outcome::AcceptedWithoutItem => Self::AcceptedWithoutItem,
            Outcome::AskedQuestion { task_id } => Self::AskedQuestion {
                task_id: task_id.clone(),
            },
            Outcome::Unreachable { cause } => Self::Unreachable {
                cause: cause.as_ref().map(|cause| TeamSendCause {
                    kind: match cause {
                        SendCause::Ratified(_) => TeamSendCauseKind::Ratified,
                        SendCause::PeerInfluenced(_) => TeamSendCauseKind::PeerInfluenced,
                    },
                    text: cause.as_str().to_owned(),
                }),
            },
            Outcome::NoUsableAnswer => Self::NoUsableAnswer,
            Outcome::NotSent => Self::NotSent,
        }
    }
}

impl From<TeamSendOutcome> for crate::domain::models::RecipientOutcome {
    fn from(outcome: TeamSendOutcome) -> Self {
        use crate::domain::models::SendCause;
        match outcome {
            TeamSendOutcome::Delivered => Self::Delivered,
            TeamSendOutcome::Declined => Self::Declined,
            TeamSendOutcome::AwaitingApproval => Self::AwaitingApproval,
            TeamSendOutcome::AcceptedWithoutItem => Self::AcceptedWithoutItem,
            TeamSendOutcome::AskedQuestion { task_id } => Self::AskedQuestion { task_id },
            TeamSendOutcome::Unreachable { cause } => Self::Unreachable {
                cause: cause.map(|cause| match cause.kind {
                    TeamSendCauseKind::Ratified => SendCause::Ratified(cause.text),
                    TeamSendCauseKind::PeerInfluenced => SendCause::PeerInfluenced(cause.text),
                }),
            },
            TeamSendOutcome::NoUsableAnswer => Self::NoUsableAnswer,
            TeamSendOutcome::NotSent => Self::NotSent,
        }
    }
}

/// The outcome of a [`ClientFrame::PeerShare`] (Story 18.4a).
///
/// ⛔ `Advertised` says the handle was advertised, ⛔ never that the peer took
/// it, agreed with it, or read it: topic gossip is fire-and-forget and nothing
/// answered. `Refused` carries the operator-ready reason; every refusal arm
/// says what was **not** done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PeerShareOutcome {
    /// The signed handle was written to the peer and recorded locally.
    Advertised,
    /// Nothing was shared; `reason` is the operator-ready line.
    Refused { reason: String },
}

/// The operator action whose exact result is correlated to a recipient target.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecipientAction {
    Reply,
    Defer,
    ShareStatus,
    Acknowledge,
    Response,
    ContextSelect,
    ContextRemove,
    /// Story 19.21 `AC4(b)`: the operator `/team` commands whose request-keyed
    /// refusals travel on this envelope ([`RecipientActionTarget::Request`]).
    TeamSend,
    TeamBoard,
    /// Story 19.21c: request-keyed thread and retract refusals.
    TeamThread,
    TeamRetract,
    TeamStatus,
    RevokeSenderConsent,
}

/// The exact item or peer-response node named by an operator action.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RecipientActionTarget {
    Item {
        item_id: String,
    },
    Node {
        node_id: String,
    },
    /// One client-minted `/team` request key (Story 19.21 `AC4(b)`), scoped by
    /// the originating connection. Never an item or node id.
    Request {
        request: String,
    },
}

/// Authoritative result of one deliberate action; context is process-local.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RecipientActionOutcome {
    /// The action was durably applied. An answer turn, when one was recorded,
    /// is identified by its receipt; a valid no-turn action has no receipt.
    Applied {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<RecipientReplyReceipt>,
    },
    /// The requested state already held; no new durable record was appended.
    Idempotent,
    /// A process-conversation reference was selected/removed, or already held.
    ContextSelection { selected: bool, changed: bool },
    /// No action was applied.
    Refused { error: ProtocolError },
}

/// Receipt for a reply turn durably appended by the action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipientReplyReceipt {
    pub item_id: String,
    pub turn_id: String,
    #[serde(
        default,
        skip_serializing_if = "crate::domain::models::ReplyKind::is_answer"
    )]
    pub kind: crate::domain::models::ReplyKind,
}

/// Daemon → client frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DaemonFrame {
    /// Server-first: a fresh, one-use challenge nonce. Sent immediately on
    /// connect; the client MUST echo it back (bound into its proof) in the
    /// answering [`ClientFrame::Attach`]. A proof minted for any other nonce
    /// fails verification, so a captured attach cannot be replayed on a new
    /// connection (which receives its own fresh nonce).
    AttachChallenge { nonce: Vec<u8> },
    /// Accept an attach: the granted mode + the initial render snapshot.
    AttachAck {
        granted_mode: AttachMode,
        snapshot: AttachSnapshot,
    },
    /// A forwarded turn event (the reused [`ClientEvent`]/`RawEvent` projection).
    Event(ClientEvent),
    /// Story 19.20c `A6` — a tool event projected for this connection without
    /// its arguments: the event's tool `input` key is absent and `withheld`
    /// says why (Peer tier, or too large for one frame).
    ToolEventWithheld {
        #[serde(serialize_with = "withheld_event_serde::serialize")]
        event: ClientEvent,
        withheld: ToolInputWithheld,
    },
    /// Story 19.20c `P6` — a frame that would exceed [`MAX_FRAME_BYTES`] even
    /// after projection. Sent instead of that frame; the connection stays up.
    FrameTooLarge { frame: String, bytes: u64 },
    /// Committed local transcript/control mutation, trusted-local observers only.
    PeerRowOperation(
        #[serde(with = "peer_row_operation_serde")]
        std::sync::Arc<crate::domain::models::PeerRowOperation>,
    ),
    /// A page of history in response to [`ClientFrame::HistoryRequest`].
    History {
        #[serde(serialize_with = "transcript_serde::serialize")]
        messages: Vec<ChatMessage>,
        has_more: bool,
        /// Story 19.20c `A6`: tool calls on this page without their arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        withheld_tool_inputs: Vec<WithheldToolInput>,
    },
    /// A decision is needed — sent to the registered writer only. The writer
    /// renders the card and replies [`ClientFrame::ApprovalResponse`].
    ///
    /// Story 19.20c: for a tool approval `summary` is the one-line per-tool
    /// summary (writer-only, so it may contain values) and `input_bytes` is the
    /// serialized size of the exact input that follows as ordered
    /// [`DaemonFrame::ApprovalInputChunk`]s and one
    /// [`DaemonFrame::ApprovalInputComplete`]. For a sender-consent card
    /// (`tool == "a2a/sender-consent"`) `summary` is the unchanged card text
    /// and `input_bytes` is `None`.
    ApprovalRequest {
        request_id: RequestId,
        tool: String,
        source: ApprovalSource,
        risk: ToolRisk,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_bytes: Option<u64>,
    },
    /// One ordered UTF-8-safe part of a tool approval's serialized exact input.
    ApprovalInputChunk {
        request_id: RequestId,
        sequence: u32,
        data: String,
    },
    /// Completion boundary for a request's exact input: `chunks` parts sent.
    ApprovalInputComplete { request_id: RequestId, chunks: u32 },
    /// Story 19.20c `A7` — an `ApprovalResponse` was refused; nothing resolved.
    ApprovalRefused {
        request_id: RequestId,
        reason: ApprovalRefusal,
    },
    /// Story 19.20c `AC6` — the request issued to this writer was resolved or
    /// cancelled; the client retires its card, queue entry, Review and any
    /// partial transfer. Late chunks for it are ignored.
    ApprovalClosed { request_id: RequestId },
    /// Consolidation proposals generated by the daemon — sent to the writer only
    /// (Story 12.2d AC2). The client renders the existing `PendingConsolidationCard`
    /// verbatim and sends `ConsolidationResolve` on accept/decline.
    ConsolidationProposed {
        token: ProposalToken,
        proposals: Vec<ProposedFact>,
    },
    /// Peer envelope was verified and accepted by the daemon wire boundary.
    PeerAccepted { sequence: u64 },
    /// The outcome of a [`ClientFrame::PeerShare`] (Story 18.4a).
    PeerShareResult { outcome: PeerShareOutcome },
    /// Correlated result of a deliberate recipient action. Every request is answered
    /// with its exact action and target, including refusals and idempotent no-ops.
    RecipientActionResult {
        action: RecipientAction,
        target: RecipientActionTarget,
        outcome: RecipientActionOutcome,
    },
    /// Cached context inventory and shared selection state; trusted-local only.
    RecipientContextState {
        selected_item_ids: Vec<String>,
        inventory: String,
        /// Inventory is foreground Info only in response to explicit inspection.
        #[serde(default)]
        show_inventory: bool,
    },
    /// Exact sender-local snapshot, addressed only to the trusted writer.
    TeamProposal {
        proposal: String,
        recipients: Vec<String>,
        message_type: SemanticMessageType,
        body: String,
    },
    /// Dismissal, withdrawal or correlated refusal. Not a send settlement.
    TeamProposalClosed {
        proposal: String,
        outcome: TeamProposalCloseOutcome,
    },
    /// Story 19.21 `AC4(b)` — the send action was durably recorded; `request`
    /// binds the client's stable block to the sender-local action id.
    /// Origin connection only. ⛔ Never sent when the record failed.
    TeamSendStarted {
        request: String,
        action: SendActionId,
    },
    /// One recipient's durable typed settlement, to the originating connection
    /// only. A request settles each index at most once.
    TeamSendSettled {
        request: String,
        index: usize,
        outcome: TeamSendOutcome,
    },
    /// Story 19.21c: bounded ordered fragment of a board, thread or retract view.
    TeamViewChunk {
        request: String,
        sequence: u32,
        view: String,
    },
    /// Explicit boundary for a complete request-correlated rendered view.
    TeamViewDone { request: String, chunks: u32 },
    /// Story 19.21c: private preview for the originating writer only.
    TeamRetractPreviewed {
        request: String,
        preview: TeamRetractPreviewView,
    },
    /// Story 19.21c: terminal retract sentence, after any remembered board.
    TeamRetractAnswered { request: String, message: String },
    /// The rendered `/team status` answer.
    TeamStatusResult { request: String, message: String },
    /// The durable answer to [`ClientFrame::RevokeSenderConsent`].
    SenderConsentRevoked {
        request: String,
        alias: String,
        message: String,
    },
    /// Acknowledge a clean [`ClientFrame::Detach`].
    Detached,
    /// A protocol-level error (e.g. version mismatch, read-only write attempt).
    Error(ProtocolError),
}

mod peer_row_operation_serde {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        operation: &std::sync::Arc<crate::domain::models::PeerRowOperation>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        operation.as_ref().serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<std::sync::Arc<crate::domain::models::PeerRowOperation>, D::Error> {
        crate::domain::models::PeerRowOperation::deserialize(deserializer).map(std::sync::Arc::new)
    }
}

/// Errors from the framing layer.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame too large: {0} bytes exceeds the {1}-byte cap")]
    TooLarge(u32, u32),
    #[error("truncated frame: peer disconnected mid-header")]
    Truncated,
    #[error("malformed frame: {0}")]
    Malformed(String),
}

/// Write one length-delimited JSON frame. `u32` big-endian length prefix + body,
/// then flush. Rejects an over-cap body before writing.
pub async fn write_frame<W, F>(w: &mut W, frame: &F) -> Result<(), FrameError>
where
    W: AsyncWriteExt + Unpin,
    F: Serialize,
{
    let body = serde_json::to_vec(frame)?;
    let len = body.len();
    if len > MAX_FRAME_BYTES as usize {
        return Err(FrameError::TooLarge(len as u32, MAX_FRAME_BYTES));
    }
    w.write_all(&(len as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-delimited JSON frame.
///
/// Returns `Ok(None)` on a **clean** EOF at a frame boundary (the peer closed the
/// connection) so a read loop can `while let Some(frame) = read_frame(..)?`.
/// Rejects an over-cap length prefix before allocating.
pub async fn read_frame<R, F>(r: &mut R) -> Result<Option<F>, FrameError>
where
    R: AsyncReadExt + Unpin,
    F: DeserializeOwned,
{
    // Read length prefix: distinguish clean EOF (zero bytes) from truncated
    // frame (partial header) by probing the first byte.
    let mut len_buf = [0u8; 4];
    match r.read(&mut len_buf[..1]).await {
        Ok(0) => return Ok(None), // clean EOF at frame boundary
        Ok(1) => {
            // At least one byte arrived — commit to reading the rest.
            r.read_exact(&mut len_buf[1..])
                .await
                .map_err(FrameError::Io)?;
        }
        Ok(_) => unreachable!("read(..1) returned > 1"),
        Err(e) => return Err(FrameError::Io(e)),
    }
    let len = u32::from_be_bytes(len_buf);
    if len == 0 {
        return Err(FrameError::Malformed("zero-length frame".into()));
    }
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len, MAX_FRAME_BYTES));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let frame = serde_json::from_slice(&body)?;
    Ok(Some(frame))
}

/// Client-side challenge-response for the attach handshake.
///
/// Reads the daemon's one-use [`DaemonFrame::AttachChallenge`], builds an
/// Ed25519 proof over it with `signer`, and writes the proof-bearing
/// [`ClientFrame::Attach`]. The proof binds the signer's identity to the server
/// nonce, the negotiated [`PROTOCOL_VERSION`], `tier`'s [`proof_tag`](ConnectionTier::proof_tag),
/// and `read_only_ok`, so it cannot be replayed across nonces, tiers, versions,
/// or read-only grants.
///
/// Returns once the `Attach` is written; the caller reads the `AttachAck` (or
/// `Error`) separately. Generic over `AsyncRead`/`AsyncWrite` so it runs over a
/// `UnixStream::pair()` in tests with no disk or wall clock.
pub async fn answer_attach_challenge<R, W>(
    read_half: &mut R,
    write_half: &mut W,
    read_only_ok: bool,
    tier: ConnectionTier,
    signer: &AgentSigner,
) -> Result<(), FrameError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let nonce = match read_frame::<_, DaemonFrame>(read_half).await? {
        Some(DaemonFrame::AttachChallenge { nonce }) => nonce,
        Some(other) => {
            return Err(FrameError::Malformed(format!(
                "expected AttachChallenge from daemon, got {other:?}"
            )));
        }
        None => {
            return Err(FrameError::Malformed(
                "daemon closed the connection before issuing a challenge".into(),
            ));
        }
    };
    let proof = signer.attach_proof(&nonce, PROTOCOL_VERSION, tier.proof_tag(), read_only_ok);
    write_frame(
        write_half,
        &ClientFrame::Attach {
            protocol_version: PROTOCOL_VERSION,
            read_only_ok,
            tier,
            challenge_nonce: nonce,
            identity: signer.identity().clone(),
            proof,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{NoticeLevel, StreamChunk};
    use crate::infrastructure::runtime::event_bus::RawEventKind;

    /// A well-formed PeerIdentity for wire round-trip fixtures (correctness of
    /// the proof itself is exercised by the daemon/server tests, not here).
    fn sample_identity() -> PeerIdentity {
        PeerIdentity::from_public_key(vec![1u8; 32]).expect("32-byte key is a valid identity")
    }

    fn sample_proof() -> Ed25519Sig {
        Ed25519Sig(vec![0u8; 64])
    }

    /// Story 19.20c `A10`/`M26`/`M27`: every chunk frame fits the cap at
    /// worst-case escaping, chunks split only on char boundaries, and the
    /// assembly reproduces the exact value byte for byte.
    #[test]
    fn exact_input_chunks_fit_the_cap_and_reassemble_exactly() {
        let content = format!("{}\"\\\n漢🙂{}", "\u{1}".repeat(3_000_000), "TAIL");
        let input = serde_json::json!({ "file_path": "/w/big", "content": content });
        let serialized = serde_json::to_string(&input).unwrap();
        let chunks = exact_input_chunks(&serialized);
        assert!(chunks.len() > 1, "a large input travels in several parts");
        let id = RequestId("r-big".into());
        let mut assembly = ExactInputAssembly::new(serialized.len() as u64);
        for (sequence, data) in chunks.iter().enumerate() {
            let frame = DaemonFrame::ApprovalInputChunk {
                request_id: id.clone(),
                sequence: sequence as u32,
                data: (*data).to_owned(),
            };
            let bytes = serde_json::to_vec(&frame).unwrap();
            assert!(bytes.len() < MAX_FRAME_BYTES as usize, "{}", bytes.len());
            assembly.push(sequence as u32, data).unwrap();
        }
        assert_eq!(assembly.finish(chunks.len() as u32).unwrap(), input);
        assert_eq!(exact_input_chunks(""), [""]);
    }

    #[test]
    fn huge_single_line_summary_fits_frame_without_losing_exact_input() {
        let input =
            serde_json::json!({ "command": format!("{}漢🙂TAIL", "x".repeat(10 * 1024 * 1024)) });
        let serialized = serde_json::to_string(&input).unwrap();
        let summary = bounded_wire_summary(
            crate::domain::services::approval_presentation::summary("Bash", &input, true),
        );
        assert!(summary.ends_with('…'));
        let frame = DaemonFrame::ApprovalRequest {
            request_id: RequestId("r-huge-summary".into()),
            tool: "Bash".into(),
            source: ApprovalSource::ForegroundTurn {
                conversation_id: "c".into(),
            },
            risk: ToolRisk::Elevated,
            summary,
            input_bytes: Some(serialized.len() as u64),
        };
        assert!(serde_json::to_vec(&frame).unwrap().len() < MAX_FRAME_BYTES as usize);
        let chunks = exact_input_chunks(&serialized);
        let mut assembly = ExactInputAssembly::new(serialized.len() as u64);
        for (sequence, data) in chunks.iter().enumerate() {
            assembly.push(sequence as u32, data).unwrap();
        }
        assert_eq!(assembly.finish(chunks.len() as u32).unwrap(), input);
        assert_eq!(bounded_wire_summary("short".into()), "short");
        let unicode = bounded_wire_summary("漢🙂".repeat(1024 * 1024));
        assert!(unicode.ends_with('…'));
    }

    /// Partial, duplicate, out-of-order or miscounted transfers never complete.
    #[test]
    fn exact_input_assembly_rejects_incomplete_or_misordered_parts() {
        let serialized = r#"{"command":"ls"}"#;
        let (head, tail) = serialized.split_at(5);
        let mut assembly = ExactInputAssembly::new(serialized.len() as u64);
        assert!(matches!(
            assembly.push(1, tail),
            Err(ExactInputError::OutOfOrder {
                expected: 0,
                got: 1
            })
        ));
        assembly.push(0, head).unwrap();
        assert!(assembly.push(0, head).is_err(), "duplicate part");
        let partial = ExactInputAssembly::new(serialized.len() as u64);
        assert!(partial.finish(0).is_err(), "missing parts");
        let mut short = ExactInputAssembly::new(serialized.len() as u64);
        short.push(0, head).unwrap();
        assert!(matches!(
            short.finish(1),
            Err(ExactInputError::LengthMismatch { .. })
        ));
        let mut over = ExactInputAssembly::new(3);
        assert!(matches!(
            over.push(0, serialized),
            Err(ExactInputError::Overflow { .. })
        ));
        assembly.push(1, tail).unwrap();
        assert!(matches!(
            assembly.finish(3),
            Err(ExactInputError::CountMismatch { .. })
        ));
    }

    /// `A6`: a withheld projection carries no `input` key on the wire — not a
    /// placeholder — and the typed reason travels beside it.
    #[test]
    fn withheld_projections_omit_the_input_key() {
        let frame = DaemonFrame::ToolEventWithheld {
            event: RawEvent {
                conversation_id: None,
                timestamp_ms: 1,
                kind: RawEventKind::Tool(crate::domain::models::ToolCallTransition {
                    conversation_id: "c".into(),
                    call: crate::domain::models::ToolCall::Scheduled {
                        id: "t-1".into(),
                        request: crate::domain::models::ToolCallRequest {
                            id: "t-1".into(),
                            tool_name: "Bash".into(),
                            input: serde_json::Value::Null,
                        },
                    },
                }),
            },
            withheld: ToolInputWithheld::PeerTier,
        };
        let wire = serde_json::to_value(&frame).unwrap();
        let request = wire
            .pointer("/toolEventWithheld/event/kind/Tool/call/request")
            .expect("request present");
        assert!(request.get("input").is_none(), "{wire}");
        assert_eq!(
            wire.pointer("/toolEventWithheld/withheld/kind"),
            Some(&serde_json::json!("peerTier"))
        );
        let decoded: DaemonFrame = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            decoded,
            DaemonFrame::ToolEventWithheld {
                withheld: ToolInputWithheld::PeerTier,
                ..
            }
        ));
    }

    fn sample_client_frames() -> Vec<ClientFrame> {
        vec![
            ClientFrame::Attach {
                protocol_version: PROTOCOL_VERSION,
                read_only_ok: true,
                tier: ConnectionTier::TrustedLocal,
                challenge_nonce: b"server-challenge-nonce".to_vec(),
                identity: sample_identity(),
                proof: sample_proof(),
            },
            ClientFrame::UserMessage {
                text: "hello".into(),
                images: vec![],
            },
            ClientFrame::HistoryRequest {
                before_index: Some(10),
                count: 50,
            },
            ClientFrame::ApprovalResponse {
                request_id: RequestId::new(),
                outcome: ApprovalOutcome::Once,
            },
            ClientFrame::DeferRecipientItem {
                item_id: "ri-1".into(),
            },
            ClientFrame::ShareRecipientStatus {
                item_id: "ri-1".into(),
                decline: false,
            },
            ClientFrame::ShareRecipientStatus {
                item_id: "ri-1".into(),
                decline: true,
            },
            ClientFrame::EnableTeamProposals,
            ClientFrame::TeamProposalDecision {
                proposal: "tp_send".into(),
                decision: TeamProposalDecision::Send,
            },
            ClientFrame::TeamProposalDecision {
                proposal: "tp_dismiss".into(),
                decision: TeamProposalDecision::Dismiss,
            },
            ClientFrame::TeamSend {
                request: "send-1".into(),
                recipients: vec!["alice".into()],
                text: "hello from daemon".into(),
                message_type: Some(SemanticMessageType::StoryAssignment),
                subject: Some("sprint-7".into()),
                stories: None,
            },
            ClientFrame::TeamBoard {
                request: "board-inventory".into(),
                selector: TeamBoardRequest::Inventory,
            },
            ClientFrame::TeamBoard {
                request: "board-peer".into(),
                selector: TeamBoardRequest::Peer("alice".into()),
            },
            ClientFrame::TeamBoard {
                request: "board-last".into(),
                selector: TeamBoardRequest::Last,
            },
            ClientFrame::TeamBoard {
                request: "board-action".into(),
                selector: TeamBoardRequest::Action("sa_action".into()),
            },
            ClientFrame::TeamBoard {
                request: "board-actions".into(),
                selector: TeamBoardRequest::Actions,
            },
            ClientFrame::TeamThread {
                request: "thread-1".into(),
                peer: "alice".into(),
                item_id: "ri_a".into(),
            },
            ClientFrame::TeamFollowup {
                request: "followup-1".into(),
                peer: "alice".into(),
                reply_to: "turn-1".into(),
                text: "more".into(),
            },
            ClientFrame::TeamRetractPreview {
                request: "retract-1".into(),
                peer: "alice".into(),
                item_id: "ri_a".into(),
            },
            ClientFrame::TeamRetractDecision {
                request: "retract-1".into(),
                confirm: true,
            },
            ClientFrame::TeamRetractDecision {
                request: "retract-2".into(),
                confirm: false,
            },
            ClientFrame::TeamStatus {
                request: "status-1".into(),
            },
            ClientFrame::RevokeSenderConsent {
                request: "untrust-1".into(),
                alias: "alice".into(),
            },
            ClientFrame::Detach,
        ]
    }

    fn sample_daemon_frames() -> Vec<DaemonFrame> {
        vec![
            DaemonFrame::AttachChallenge {
                nonce: b"server-challenge-nonce".to_vec(),
            },
            DaemonFrame::AttachAck {
                granted_mode: AttachMode::ReadWrite,
                snapshot: AttachSnapshot {
                    conversation_id: "conv-1".into(),
                    transcript: vec![],
                    permission_mode: PermissionMode::Normal,
                    channels: vec![ChannelKind::Terminal],
                    blocked_actions_waiting: 2,
                    pending_consent_cards: 0,
                    retained_approvals: 1,
                    peer_row_controls: Default::default(),
                    peer_row_revision: 0,
                    recipient_context_ids: Vec::new(),
                    withheld_tool_inputs: vec![WithheldToolInput {
                        message_id: "m-1".into(),
                        tool_call_id: "t-1".into(),
                        withheld: ToolInputWithheld::PeerTier,
                    }],
                },
            },
            DaemonFrame::Event(RawEvent {
                conversation_id: Some("conv-1".into()),
                timestamp_ms: 123,
                kind: RawEventKind::Provider(StreamChunk::Text {
                    content: "hi".into(),
                    parent_tool_use_id: None,
                }),
            }),
            DaemonFrame::Event(RawEvent {
                conversation_id: None,
                timestamp_ms: 1,
                kind: RawEventKind::SystemNotice {
                    level: NoticeLevel::Info,
                    message: "note".into(),
                },
            }),
            DaemonFrame::PeerRowOperation(std::sync::Arc::new(
                crate::domain::models::PeerRowOperation::Upsert {
                    revision: 1,
                    message: ChatMessage {
                        id: "row-1".into(),
                        content: "peer request".into(),
                        ..Default::default()
                    },
                    control: Some(crate::domain::models::PeerRowControl {
                        node_id: "node-1".into(),
                        item_address: None,
                        message_type: crate::domain::models::SemanticMessageType::Consultation,
                        stories: None,
                        status: None,
                        sender_label: "peer-1".into(),
                        stage: crate::domain::models::PeerRowStage::AwaitingOperator,
                        node_resolvable: true,
                    }),
                },
            )),
            DaemonFrame::PeerRowOperation(std::sync::Arc::new(
                crate::domain::models::PeerRowOperation::Remove {
                    revision: 2,
                    row_id: "row-1".into(),
                },
            )),
            DaemonFrame::History {
                messages: vec![],
                has_more: true,
                withheld_tool_inputs: Vec::new(),
            },
            DaemonFrame::ToolEventWithheld {
                event: RawEvent {
                    conversation_id: Some("conv-1".into()),
                    timestamp_ms: 7,
                    kind: RawEventKind::Provider(StreamChunk::ToolUse {
                        id: "t-1".into(),
                        name: "Bash".into(),
                        input: serde_json::Value::Null,
                    }),
                },
                withheld: ToolInputWithheld::TooLarge { bytes: 9_000_000 },
            },
            DaemonFrame::FrameTooLarge {
                frame: "event".into(),
                bytes: 9_000_000,
            },
            DaemonFrame::ApprovalRequest {
                request_id: RequestId::new(),
                tool: "Write".into(),
                source: ApprovalSource::ForegroundTurn {
                    conversation_id: "conv-1".into(),
                },
                risk: ToolRisk::Standard,
                summary: "config.toml…".into(),
                input_bytes: Some(42),
            },
            DaemonFrame::ApprovalInputChunk {
                request_id: RequestId("r-1".into()),
                sequence: 0,
                data: "{\"file_path\":\"config.toml\"}".into(),
            },
            DaemonFrame::ApprovalInputComplete {
                request_id: RequestId("r-1".into()),
                chunks: 1,
            },
            DaemonFrame::ApprovalRefused {
                request_id: RequestId("r-1".into()),
                reason: ApprovalRefusal::IssuedElsewhere,
            },
            DaemonFrame::ApprovalClosed {
                request_id: RequestId("r-1".into()),
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Reply,
                target: RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                outcome: RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: "ri-1".into(),
                        turn_id: "rr-answer".into(),
                        kind: crate::domain::models::ReplyKind::Answer,
                    }),
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Defer,
                target: RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                outcome: RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: "ri-1".into(),
                        turn_id: "rr-defer".into(),
                        kind: crate::domain::models::ReplyKind::Defer { minutes: 30 },
                    }),
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::ShareStatus,
                target: RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                outcome: RecipientActionOutcome::Applied {
                    reply: Some(RecipientReplyReceipt {
                        item_id: "ri-1".into(),
                        turn_id: "rr-status".into(),
                        kind: crate::domain::models::ReplyKind::Status,
                    }),
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Acknowledge,
                target: RecipientActionTarget::Item {
                    item_id: "ri-1".into(),
                },
                outcome: RecipientActionOutcome::Idempotent,
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Response,
                target: RecipientActionTarget::Node {
                    node_id: "node-1".into(),
                },
                outcome: RecipientActionOutcome::Applied { reply: None },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::Reply,
                target: RecipientActionTarget::Item {
                    item_id: "missing".into(),
                },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Malformed("recipient item not found".into()),
                },
            },
            DaemonFrame::TeamSendStarted {
                request: "send-1".into(),
                action: SendActionId::from_replay("sa_action"),
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 0,
                outcome: TeamSendOutcome::Delivered,
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 1,
                outcome: TeamSendOutcome::Declined,
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 2,
                outcome: TeamSendOutcome::AwaitingApproval,
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 3,
                outcome: TeamSendOutcome::AcceptedWithoutItem,
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 4,
                outcome: TeamSendOutcome::AskedQuestion {
                    task_id: "task-1".into(),
                },
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 5,
                outcome: TeamSendOutcome::Unreachable {
                    cause: Some(TeamSendCause {
                        kind: TeamSendCauseKind::Ratified,
                        text: "unavailable".into(),
                    }),
                },
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 6,
                outcome: TeamSendOutcome::Unreachable {
                    cause: Some(TeamSendCause {
                        kind: TeamSendCauseKind::PeerInfluenced,
                        text: "peer influenced".into(),
                    }),
                },
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 7,
                outcome: TeamSendOutcome::Unreachable { cause: None },
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 8,
                outcome: TeamSendOutcome::NoUsableAnswer,
            },
            DaemonFrame::TeamSendSettled {
                request: "send-1".into(),
                index: 9,
                outcome: TeamSendOutcome::NotSent,
            },
            DaemonFrame::TeamProposal {
                proposal: "tp_send".into(),
                recipients: vec!["lena-po".into(), "jun-dev".into()],
                message_type: SemanticMessageType::ArchitectureUpdate,
                body: "Exact body\n\ttabs and e\u{301}界".into(),
            },
            DaemonFrame::TeamProposalClosed {
                proposal: "tp_dismiss".into(),
                outcome: TeamProposalCloseOutcome::Dismissed,
            },
            DaemonFrame::TeamProposalClosed {
                proposal: "tp_withdraw".into(),
                outcome: TeamProposalCloseOutcome::Withdrawn,
            },
            DaemonFrame::TeamProposalClosed {
                proposal: "tp_refused".into(),
                outcome: TeamProposalCloseOutcome::Refused {
                    reason: "That proposal is no longer waiting. Nothing was sent.".into(),
                },
            },
            DaemonFrame::TeamViewChunk {
                request: "board-1".into(),
                sequence: 0,
                view: "Distribution ".into(),
            },
            DaemonFrame::TeamViewChunk {
                request: "board-1".into(),
                sequence: 1,
                view: "board".into(),
            },
            DaemonFrame::TeamViewDone {
                request: "board-1".into(),
                chunks: 2,
            },
            DaemonFrame::TeamRetractPreviewed {
                request: "retract-1".into(),
                preview: TeamRetractPreviewView::Card {
                    peer: "alice".into(),
                    item_id: "ri_a".into(),
                    body: "Exact card".into(),
                    armed: true,
                },
            },
            DaemonFrame::TeamRetractPreviewed {
                request: "retract-2".into(),
                preview: TeamRetractPreviewView::Answer {
                    message: "Nothing was marked.".into(),
                },
            },
            DaemonFrame::TeamRetractAnswered {
                request: "retract-1".into(),
                message: "Retracted.".into(),
            },
            DaemonFrame::TeamStatusResult {
                request: "status-1".into(),
                message: "Team status".into(),
            },
            DaemonFrame::SenderConsentRevoked {
                request: "untrust-1".into(),
                alias: "alice".into(),
                message: "Revoked trust".into(),
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::TeamSend,
                target: RecipientActionTarget::Request {
                    request: "send-refused".into(),
                },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::ReadOnly,
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::TeamBoard,
                target: RecipientActionTarget::Request {
                    request: "board-refused".into(),
                },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::PeerVerification("trusted-local only".into()),
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::TeamStatus,
                target: RecipientActionTarget::Request {
                    request: "status-refused".into(),
                },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal("unavailable".into()),
                },
            },
            DaemonFrame::RecipientActionResult {
                action: RecipientAction::RevokeSenderConsent,
                target: RecipientActionTarget::Request {
                    request: "untrust-refused".into(),
                },
                outcome: RecipientActionOutcome::Refused {
                    error: ProtocolError::Internal("append failed".into()),
                },
            },
            DaemonFrame::Detached,
            DaemonFrame::Error(ProtocolError::VersionMismatch {
                daemon: PROTOCOL_VERSION,
                client: PROTOCOL_VERSION + 1,
            }),
            DaemonFrame::Error(ProtocolError::ReadOnly),
            DaemonFrame::Error(ProtocolError::AttachProof("bad signature".into())),
        ]
    }

    #[test]
    fn every_client_frame_round_trips_through_json() {
        for frame in sample_client_frames() {
            let bytes = serde_json::to_vec(&frame).unwrap();
            let back: ClientFrame = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(format!("{frame:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn every_daemon_frame_round_trips_through_json() {
        for frame in sample_daemon_frames() {
            let bytes = serde_json::to_vec(&frame).unwrap();
            let back: DaemonFrame = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(format!("{frame:?}"), format!("{back:?}"));
        }
    }

    #[tokio::test]
    async fn frames_round_trip_over_a_duplex_stream() {
        // Deterministic in-memory pipe — no real socket needed.
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        let sent = sample_client_frames();
        let sent_for_task = sent.clone();
        let writer = tokio::spawn(async move {
            for f in &sent_for_task {
                write_frame(&mut a, f).await.unwrap();
            }
            // Drop `a` → clean EOF for the reader.
            drop(a);
        });

        let mut received: Vec<ClientFrame> = Vec::new();
        while let Some(f) = read_frame::<_, ClientFrame>(&mut b).await.unwrap() {
            received.push(f);
        }
        writer.await.unwrap();
        assert_eq!(received.len(), sent.len());
        for (s, r) in sent.iter().zip(received.iter()) {
            assert_eq!(format!("{s:?}"), format!("{r:?}"));
        }
    }

    #[tokio::test]
    async fn v7_proposal_frames_preserve_exact_snapshot_without_a2a_features() {
        let bytes = br#"{"teamProposal":{"proposal":"tp_exact","recipients":["jun-dev","lena-po"],"message_type":"architecture_update","body":"  exact\n\tbody \u00e9\u754c  "}}"#;
        let frame: DaemonFrame = serde_json::from_slice(bytes).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(4096);
        write_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_frame::<_, DaemonFrame>(&mut reader)
            .await
            .unwrap()
            .unwrap();
        let DaemonFrame::TeamProposal {
            proposal,
            recipients,
            message_type,
            body,
        } = decoded
        else {
            panic!("feature-independent client must decode the v7 proposal snapshot");
        };
        assert_eq!(proposal, "tp_exact");
        assert_eq!(
            recipients,
            ["jun-dev", "lena-po"],
            "proposal recipient order is immutable"
        );
        assert_eq!(message_type, SemanticMessageType::ArchitectureUpdate);
        assert_eq!(
            body.as_bytes(),
            "  exact\n\tbody é界  ".as_bytes(),
            "proposal body bytes must survive the feature-independent frame path"
        );
        write_frame(&mut writer, &ClientFrame::EnableTeamProposals)
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, ClientFrame>(&mut reader).await.unwrap(),
            Some(ClientFrame::EnableTeamProposals)
        ));
        for (token, expected) in [
            ("send", TeamProposalDecision::Send),
            ("dismiss", TeamProposalDecision::Dismiss),
        ] {
            let frame: ClientFrame = serde_json::from_value(serde_json::json!({
                "teamProposalDecision": {"proposal": "tp_exact", "decision": token}
            }))
            .unwrap();
            assert!(
                matches!(frame, ClientFrame::TeamProposalDecision { proposal, decision }
                if proposal == "tp_exact" && decision == expected)
            );
        }
        assert!(
            serde_json::from_value::<ClientFrame>(serde_json::json!({
                "teamProposalDecision": {"proposal": "tp_exact", "decision": "always"}
            }))
            .is_err(),
            "proposal decisions must never gain an always/grant variant"
        );
        for outcome in [
            TeamProposalCloseOutcome::Dismissed,
            TeamProposalCloseOutcome::Withdrawn,
            TeamProposalCloseOutcome::Refused {
                reason: "Nothing was sent.".into(),
            },
        ] {
            let frame = DaemonFrame::TeamProposalClosed {
                proposal: "tp_exact".into(),
                outcome: outcome.clone(),
            };
            write_frame(&mut writer, &frame).await.unwrap();
            assert!(
                matches!(
                    read_frame::<_, DaemonFrame>(&mut reader).await.unwrap().unwrap(),
                    DaemonFrame::TeamProposalClosed { proposal, outcome: decoded }
                        if proposal == "tp_exact" && decoded == outcome
                ),
                "feature-independent clients must decode every proposal close outcome"
            );
        }
    }

    #[tokio::test]
    async fn read_frame_returns_none_on_immediate_eof() {
        let (a, mut b) = tokio::io::duplex(16);
        drop(a);
        let got: Option<ClientFrame> = read_frame(&mut b).await.unwrap();
        assert!(got.is_none(), "clean EOF must read as None, not an error");
    }

    #[tokio::test]
    async fn oversized_length_prefix_is_rejected_before_alloc() {
        let (mut a, mut b) = tokio::io::duplex(16);
        // Write a length prefix above the cap, then drop.
        let huge = (MAX_FRAME_BYTES + 1).to_be_bytes();
        a.write_all(&huge).await.unwrap();
        drop(a);
        let err = read_frame::<_, ClientFrame>(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_, _)));
    }

    #[test]
    fn client_event_is_the_raw_event_projection() {
        // Compile-time proof that ClientEvent IS RawEvent (one projection, AC2).
        fn identity(e: ClientEvent) -> RawEvent {
            e
        }
        let _ = identity;
    }
}
