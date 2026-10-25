//! Pure read-model projection for durable orchestration rooms.
//!
//! `RoomEvent` is the canonical durable event. `OrchestrationRoom::project`
//! performs no I/O and exposes no mutation surface to observers.
use super::provenance::{Authorship, SendAuthorization};

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::domain::models::NodeOrigin;
use crate::domain::models::SemanticMessageType;
use crate::domain::models::agent_id::AgentId;
use crate::domain::models::approval::ApprovalOutcome;
use crate::domain::models::artifact::{
    ArtifactId, ArtifactKind, ArtifactRef, ContentHash, ReviewStatus,
};
use crate::domain::models::invocation_fingerprint::InvocationFingerprint;
use crate::domain::models::node_state::NodeState;
use crate::domain::models::peer_frame::FrameRefusal;
use crate::domain::models::peer_identity::PeerId;
use crate::domain::models::policy_principal::PrincipalKind;
use crate::domain::models::recipient_item::{ItemAddress, ItemId, ReplyKind, TurnAddress};
use crate::domain::models::room_role::RoomRole;
use crate::domain::models::team_policy::{InteractionPolicySnapshot, NotificationUrgency};
use crate::domain::models::ticket_addressee::TicketAddressee;
use crate::domain::models::tool_call::ApprovalSource;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OrchestrationRoomId(String);

impl OrchestrationRoomId {
    pub fn new() -> Self {
        Self(nanoid::nanoid!(12))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, RoomIdError> {
        let value = value.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RoomIdError {
    #[error("room or wave id must not be empty")]
    Empty,
    #[error("room or wave id must not contain '/'")]
    EmbeddedSeparator,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WaveId(String);

impl WaveId {
    pub fn new() -> Self {
        Self(nanoid::nanoid!(12))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, RoomIdError> {
        let value = value.into();
        validate_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HostBinding {
    pub host_id: String,
    pub workspace_id: String,
}

impl HostBinding {
    pub fn new(host_id: impl Into<String>, workspace_id: impl Into<String>) -> Self {
        Self {
            host_id: host_id.into(),
            workspace_id: workspace_id.into(),
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaveOutcome {
    Completed,
    Failed,
    Cancelled,
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequested,
    Rejected,
    /// A verdict string this build does not understand.
    ///
    /// Same forward-compat argument as [`crate::domain::models::ArtifactKind::Unknown`]:
    /// without this arm a single newer `PatchReviewed` line fails serde and
    /// takes the whole journal file with it, because `RoomEvent`'s
    /// `#[serde(other)] Unrecognized` only catches an unknown `event` tag and
    /// this failure is one level below it.
    ///
    /// ⚠ **Deserialize-only** — re-serializes as `"unknown"`. Harmless: the
    /// journal is append-only and never rewritten.
    ///
    /// ⛔ **Fail-closed at the gate.** An unreadable verdict is not an
    /// approval; it resolves to
    /// [`crate::domain::services::patch_review::PatchDisposition::AwaitingReview`].
    #[serde(other)]
    Unknown,
}

/// Durable outcome of an operator ticket. This is projected on the producing
/// node so a terminal task retains the human-visible reason that closed its
/// ticket (AC7), rather than collapsing every outcome into a generic node
/// state.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum TicketResolution {
    Answered,
    Cancelled,
    CancelUnconfirmed { reason: String },
    ExpiredUnanswered { reason: String },
    Failed { reason: String },
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum RejectReason {
    InvalidSignature,
    Expired,
    Replay,
    Policy { detail: String },
    UnknownRecipient,
    Malformed,
}

/// Which way an A2A interaction crossed this instance's boundary.
///
/// **Why this is persisted rather than derived.** For
/// [`RoomEvent::RemoteEnvelopeAccepted`] the node id still carries the
/// direction as a prefix (`a2a-in/` inbound vs `a2a/` outbound), but
/// [`RoomEvent::RemoteEnvelopeRejected`] carries `{ peer, reason }` and **no
/// node** — the direction is destroyed at the emit boundary and there is
/// nothing left to derive from at replay time. The field is therefore load
/// bearing, not stylistic: do not "optimise" it away in favour of the node
/// prefix. Tests may use the prefix as a cross-check, never as the source.
///
/// `Unknown` is the serde default for missing fields in journals written before
/// Story 18.2, and serde's future-value fallback for direction strings this
/// build does not yet recognise. Both cases render as an explicit unknown
/// rather than a fabricated direction.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// A peer asked this instance to do something.
    Inbound,
    /// This instance asked a peer to do something.
    Outbound,
    /// Journaled before direction was recorded, or a future direction this
    /// build does not understand. Rendered explicitly; never silently coerced.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Direction {
    /// Stable, monochrome-safe label. Paired with a glyph at every render
    /// surface (UX monochrome rule) — this is the text half.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
            _ => "unknown",
        }
    }

    /// Glyph half of the monochrome pair.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Inbound => "←",
            Self::Outbound => "→",
            _ => "?",
        }
    }
}

/// Outcome of one `git apply` attempt against the One-Ring workspace
/// (Story 18.3a-d, FR160(c)).
///
/// ⚠ **Crash-recovery marker, not an audit trail.** The room journal is
/// Landlock-enforced but **not** authenticated
/// (`DF-18-2-AUTHENTICATED-JOURNAL`), so whoever can write the file can forge
/// a [`RoomEvent::PatchApplyResolved`] carrying [`ApplyOutcome::Applied`] for
/// an apply that actually died halfway — converting
/// [`ApplyState::Indeterminate`] into a clean bill of health. These records
/// are honest **under a trusted-filesystem assumption**; they are never
/// evidence, authenticated or tamper-evident.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyOutcome {
    /// `git apply` succeeded and the working tree carries the delta.
    Applied,
    /// The patch is well formed but did not apply cleanly. The working tree is
    /// unchanged — `git apply` is atomic across hunks and `--reject` is never
    /// requested.
    Conflict,
    /// The apply mechanism could not start or complete, or the body was
    /// rejected as malformed.
    Failed,
    /// An outcome value written by a newer build.
    ///
    /// ⚑ **Mandatory, and not decoration.** Without it an unknown outcome
    /// string fails the *whole journal line*, and [`RoomEvent`]'s own
    /// `#[serde(other)] Unrecognized` cannot rescue it: the `event` tag
    /// matched a known variant, so the failure is one level below that
    /// fallback. Same finding as Story 18.3a-c's `ReviewStatus::Unknown`.
    ///
    /// ⚠ **Deserialize-only** — re-serializes as `"unknown"`, which is
    /// harmless because the journal is append-only and never rewritten.
    ///
    /// ⛔ **Never read as success.** [`ApplyState::Resolved`] preserves it and
    /// every consumer fails closed.
    #[serde(other)]
    Unknown,
}

/// What an operator reported after inspecting the working tree themselves
/// (Story 18.3a-f, FR160(d)).
///
/// ⛔ **A human's report, never a machine's observation.** Nothing in the
/// product probes the tree to produce this value: AD-12 (*"Live state is never
/// reconstructed from journal data"*, `ARCHITECTURE-SPINE.md:159`) and
/// `ADR-18-3a-d-01` D2 both forbid the probe. It is a separate type from
/// [`ApplyOutcome`] precisely so no consumer can render *"a human looked"* and
/// *"`git apply` returned 0"* through one token.
///
/// ⚠ Carries the same trusted-filesystem assumption as every other journal
/// record (`ADR-18-3a-d-01` D1) — it is not evidence, not authenticated and
/// not tamper-evident (`DF-18-2-AUTHENTICATED-JOURNAL`).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorApplyFinding {
    /// The operator reports the patch's changes are in the working tree.
    Present,
    /// The operator reports they are not.
    Absent,
    /// A finding value written by a newer build.
    ///
    /// ⚑ **Mandatory, and not decoration** (`ADR-18-3a-d-01:91`). Without it a
    /// finding string this build has never heard of fails the *whole journal
    /// line*, and [`RoomEvent::Unrecognized`] cannot rescue it: the `event` tag
    /// matched a known variant, so the failure is one level below that
    /// fallback — and a mid-file parse failure takes the entire journal with
    /// it.
    ///
    /// ⛔ **Never read as resolved.** `apply_is_refused` fails closed on it,
    /// exactly as for [`ApplyOutcome::Unknown`], and the release verb
    /// deliberately does not clear it: the confirmation card cannot name what
    /// it would be asking the operator to supersede
    /// (`DF-18-3a-f-UNREADABLE-VALUE-LATCH`).
    #[serde(other)]
    Unknown,
}

/// Journal-projected apply state of one patch artifact (Story 18.3a-d).
///
/// The lattice **preserves** the outcome instead of collapsing it: cut 2
/// (`18-3a-e`) renders a per-outcome row vocabulary, and a collapsed
/// `Failed { retryable }` would throw that distinction away while carrying a
/// field that is constant. Not serialized: this is a projection of the
/// journal, never a second store (`ADR-17-CC-01`, `ADR-17-CC-02`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApplyState {
    /// ⛔ **"No apply record in THIS journal" — never "never applied".**
    /// Every journal written before Story 18.3a-d carries no apply records at
    /// all, so a patch the `/fanout` merge-back arm already applied projects
    /// as `NeverAttempted`. That is `DF-18-3a-d-PRE-RECORD-ERA`, owned by
    /// `18-3a-e`. ⛔ Do not "fix" it by making this variant refuse — that
    /// refuses every genuinely new patch too.
    #[default]
    NeverAttempted,
    /// A [`RoomEvent::PatchApplyStarted`] with no matching
    /// [`RoomEvent::PatchApplyResolved`]: the process died inside the apply
    /// window, so the working tree may or may not carry the delta. The honest
    /// answer is "unknown" — the room projects what was recorded and never
    /// repairs what was not (`ADR-18-3a-d-01:72`, applying AD-12's *"Live state
    /// is never reconstructed from journal data"*, `ARCHITECTURE-SPINE.md:159`).
    ///
    /// ⚑ **Cut 1 shipped the latch; `18-3a-f` ships the release.** The operator
    /// inspects the tree themselves and records what they found with
    /// `/artifact resolve <id> present|absent`, which appends
    /// [`RoomEvent::PatchApplyInspected`] and folds to
    /// [`ApplyState::OperatorResolved`]
    /// (`DF-18-3a-d-INDETERMINATE-CLEARING`, closed by `18-3a-f`). ⛔ "Make
    /// `Indeterminate` fail open" remains forbidden as the fix — the release is
    /// a **new durable fact**, never a weakened refusal.
    Indeterminate,
    /// The apply completed and recorded its outcome.
    Resolved(ApplyOutcome),
    /// An operator inspected the working tree and reported what they found
    /// (Story 18.3a-f).
    ///
    /// ⛔ Structurally distinct from [`ApplyState::Resolved`] at **every**
    /// consumer, and that is the whole point: a human's report is not a
    /// `git apply` return code, and no row, card or refusal may render the two
    /// through one phrase.
    OperatorResolved(OperatorApplyFinding),
}

/// The three transport-admission facts [`RoomEvent::PeerAdmissionRecorded`]
/// carries (Story 18.4b, AC6).
///
/// ⛔ Not a tier, a plan, an edition or a posture: this is *what the operator
/// did to one alias in `.rustain/p2p.json`*, and nothing about how strictly
/// admission is enforced. No tier mechanism exists in this tree (ruling A1).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerAdmissionOutcome {
    /// `peer add` confirmed a fingerprint and pinned the offered key.
    Pinned,
    /// `peer revoke` removed the entry. The observable consequence is that the
    /// peer's next frame is refused without a restart — ⛔ not a teardown.
    Revoked,
    /// `peer add` refused a key-mismatch import. The prior pin stands and
    /// nothing was written.
    ImportRefused,
    /// A missing outcome or a value written by a newer build.
    ///
    /// `RoomEvent::Unrecognized` cannot catch a failure below a known event
    /// tag. Defaulting to `Pinned` would fabricate a successful operator act;
    /// this compatibility sentinel renders explicitly unknown instead.
    #[default]
    #[serde(other)]
    Unknown,
}

/// What became of one **outbound** peer frame (Story 18.4d, D10).
///
/// # Why this event exists at all
///
/// The shipped `PeerInteractionRecorder` takes a `PeerDeliveryRecord` whose
/// outcome is `Accepted | Refused` and whose sink converts it *unconditionally*
/// into an inbound record — so there was no way to journal a fact about a frame
/// **this host sent**. Reusing the inbound shape would have recorded a send as a
/// receipt.
///
/// # Why the outcome is a value and not a boolean
///
/// Four things can happen and only two of them are a decision by the peer. A
/// boolean would have collapsed "the peer refused" into "we could not send" and
/// both into "not accepted", which is precisely the false claim the transport's
/// old fire-and-forget shape forced. ⛔ `Accepted` may be written **only** when a
/// verdict was received and validated; a successful write alone is
/// [`Self::OutcomeUnknown`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerFrameAttemptOutcome {
    /// The peer answered that it took the frame.
    Accepted,
    /// The peer answered that it refused the frame. The class is in `refusal`.
    Refused,
    /// The frame never left: a dial, write or local journal failure.
    SendFailed,
    /// The frame was written and no readable answer came back. ⛔ Never rounded
    /// up to `Accepted`.
    OutcomeUnknown,
    /// An outcome this build does not understand, or none was recorded.
    ///
    /// `RoomEvent::Unrecognized` catches an unknown `event` tag but nothing
    /// below one, so this nested sentinel is what keeps a newer build's row
    /// readable instead of failing the whole journal line.
    #[default]
    #[serde(other)]
    Unknown,
}

/// What one `RoomEvent::RemoteEnvelopeDispatched` row dispatched (Story
/// 19.16f `AC5`, owner gate item 4 = B).
///
/// An additive discriminator on the shipped sender row rather than a new
/// variant: a retract is still one outbound JSON-RPC write to one configured
/// peer, durable before its POST, so it shares the dispatch/rejection pair.
/// ⛔ Without it a retract would fold as `task dispatched to peer` — a false
/// ledger line recording a second dispatch of the original task.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DispatchAct {
    /// An A2A `message/send` task — every row written before 19.16f.
    #[default]
    Task,
    /// An `x-rustain-items/retract` of one recipient-minted item on the peer's
    /// host. `item` is peer-minted text, stripped and bounded on write.
    ItemRetract { item: String },
}

impl DispatchAct {
    /// `true` for the pre-19.16f act. Skipped on write, so a task dispatch
    /// row stays byte-identical to every line written before the field.
    #[must_use]
    pub fn is_task(&self) -> bool {
        matches!(self, Self::Task)
    }
}

/// Story 19.21 `C1` — the host-minted id of one operator send action.
///
/// Sender-local: it names a row of THIS host's journal and nothing else. ⛔ It
/// never goes on the wire, no peer can address it, and it is not a group
/// identity (`FR164`): the recipients of one action stay N independent sends.
/// Shape: [`SEND_ACTION_ID_PREFIX`] + a short random token, at most
/// [`MAX_SEND_ACTION_ID_LEN`] characters, so an operator can type it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SendActionId(String);

/// The prefix every minted [`SendActionId`] starts with.
pub const SEND_ACTION_ID_PREFIX: &str = "sa_";

/// Longest minted [`SendActionId`], prefix included.
pub const MAX_SEND_ACTION_ID_LEN: usize = 16;

impl SendActionId {
    /// Mint a fresh id. Uniqueness within one workspace journal is the
    /// caller's to enforce (the runtime re-rolls on a collision with a
    /// recorded id); 13 characters over 36 symbols make a collision
    /// negligible before that check.
    #[must_use]
    pub fn mint() -> Self {
        const ALPHABET: [char; 36] = [
            '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g',
            'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x',
            'y', 'z',
        ];
        const TOKEN_LEN: usize = MAX_SEND_ACTION_ID_LEN - SEND_ACTION_ID_PREFIX.len();
        Self(format!(
            "{SEND_ACTION_ID_PREFIX}{}",
            nanoid::nanoid!(TOKEN_LEN, &ALPHABET)
        ))
    }

    /// Rebuild an id read from the journal or typed by the operator. ⛔ No
    /// validation: a replayed or mistyped id simply matches no recorded
    /// action, and every render bounds and sanitizes it.
    #[must_use]
    pub fn from_replay(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Story 19.21 `C1` — which recipient of which operator send action a journal
/// row belongs to. Carried additively by the dispatch row and directly by the
/// typed settlement, so membership is never reconstructed from an alias,
/// timing, a body hash or "the newest dispatch".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendActionRef {
    pub action: SendActionId,
    /// 0-based position in the action's operator-ordered recipient list.
    pub index: u32,
}

/// One addressed recipient as the action record froze it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendActionRecipient {
    /// The roster alias as the operator typed it.
    pub alias: String,
    /// The resolved identity exactly as the dispatch row resolves it (the
    /// pinned identity, else the alias pseudonym).
    pub peer: PeerId,
    /// Story 19.18c `PQ2` — the roster origin's digest for an **unpinned**
    /// peer (never the raw URL); `None` for a pinned peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// Whether a recorded unreachable cause is a ratified local sentence or text a
/// peer influenced. Decides the render bound only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettlementCauseKind {
    Ratified,
    /// Also where a kind written by a newer build lands: the tighter bound is
    /// the safe one.
    #[default]
    #[serde(other)]
    PeerInfluenced,
}

/// The complete cause of an unreachable settlement, stored raw and sanitized
/// once at render (`AD-1824`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementCause {
    pub kind: SettlementCauseKind,
    pub text: String,
}

/// Story 19.21 `C1` — one recipient's typed send-time outcome, journaled after
/// every row the preserved delivery path emits and before the settle is shown.
///
/// ⛔ Never an invented Accepted/Rejected row, never a reply body, credential
/// or raw capability. `Unknown` is what an older build reads for an outcome a
/// newer one wrote: it renders as an explicit unrecognised row, never as any
/// ratified token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
#[non_exhaustive]
pub enum SendSettlement {
    /// The first answer minted an item (`item`, peer text, when it named one).
    Delivered {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<String>,
    },
    Declined,
    Unreachable {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<SettlementCause>,
    },
    AwaitingApproval,
    AcceptedWithoutItem,
    AskedQuestion {
        task_id: String,
    },
    NoUsableAnswer,
    NotSent,
    #[serde(other)]
    Unknown,
}

#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum RoomEvent {
    NodeRegistered {
        node: AgentId,
        origin: NodeOrigin,
        host: HostBinding,
    },
    NodeStateChanged {
        node: AgentId,
        from: NodeState,
        to: NodeState,
    },
    WaveStarted {
        wave: WaveId,
        coordinator: AgentId,
        spokes: Vec<AgentId>,
    },
    WaveCompleted {
        wave: WaveId,
        outcome: WaveOutcome,
    },
    /// Host-local admission refusal/defer. This event is durable for operator
    /// observability but is never carried by a signed peer envelope.
    AdmissionDeferred {
        coordinator: AgentId,
        spoke: String,
        gate: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalKind>,
    },
    ArtifactCreated {
        artifact: ArtifactRef,
    },
    ApprovalRequested {
        node: AgentId,
        fingerprint: InvocationFingerprint,
    },
    ApprovalResolved {
        node: AgentId,
        fingerprint: InvocationFingerprint,
        source: ApprovalSource,
        outcome: ApprovalOutcome,
    },
    PatchCaptured {
        artifact: ArtifactId,
        producer: AgentId,
    },
    PatchReviewed {
        artifact: ArtifactId,
        reviewer: AgentId,
        verdict: ReviewVerdict,
    },
    /// This host submitted one A2A task to a configured peer. Durable before
    /// the POST; carries volume and correlation only, never message content.
    RemoteEnvelopeDispatched {
        peer: PeerId,
        task: Option<String>,
        bytes: usize,
        /// What was dispatched (Story 19.16f `AC5`, owner gate item 4 = B).
        /// ⛔ `#[serde(default)]` is load-bearing: every journal line written
        /// before this field existed replays as [`DispatchAct::Task`].
        #[serde(default, skip_serializing_if = "DispatchAct::is_task")]
        act: DispatchAct,
        /// Story 19.18 `Q2` — the hash of the outbound text, so the asker's
        /// thread render can label a served item turn `you` only when both
        /// its task AND its content match what this host sent. `None` on
        /// every row written before 19.18 (those turns render `another
        /// sender`) and on a retract row, which sends no text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<ContentHash>,
        /// Story 19.18c `PQ2` — for a send to an **unpinned** peer, the
        /// lowercase sha256 hex of the roster-configured origin it went to
        /// (never the raw URL), so a later alias rename at the same URL still
        /// recognises the row as this host's own send. `None` on a pinned
        /// send, a retract row and every row written before 19.18c — each
        /// serializes byte-identically to a 19.18 row.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// Story 19.21 `C1` — which operator send action and recipient index
        /// this dispatch belongs to. `None` on every row written before 19.21,
        /// on a `/team followup`, a retract row and a model delegation, each of
        /// which serializes byte-identically to the previous shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<SendActionRef>,
        /// Who authored this dispatch. Older journals and future tokens
        /// conservatively replay as unknown and keep the legacy JSON shape.
        #[serde(
            default = "Authorship::unknown",
            skip_serializing_if = "Authorship::is_unknown"
        )]
        authorship: Authorship,
        /// AD-1830 fact 2, recorded at the dispatch act; journal-local (Story 19.20a).
        #[serde(
            default = "SendAuthorization::unknown",
            skip_serializing_if = "SendAuthorization::is_unknown"
        )]
        send_authorization: SendAuthorization,
    },
    /// Defined against 17.1a's `PeerId`; production emission is the sole
    /// 17.1b-gated room-event seam.
    RemoteEnvelopeAccepted {
        peer: PeerId,
        node: AgentId,
        content_hash: ContentHash,
        /// Story 18.2 (AC2). `#[serde(default)]` so pre-18.2 journals replay
        /// as [`Direction::Unknown`] instead of a fabricated direction.
        #[serde(default)]
        direction: Direction,
        /// Original remote task id, if the emitting protocol supplied one.
        /// `None` preserves replay of pre-18.2 journal entries.
        #[serde(default)]
        task: Option<String>,
    },
    /// Defined against 17.1a's `PeerId`; production emission is the sole
    /// 17.1b-gated room-event seam.
    RemoteEnvelopeRejected {
        peer: PeerId,
        reason: RejectReason,
        /// Story 18.2 (AC2). This variant carries no node, so direction is
        /// **unrecoverable** at replay time unless it is persisted here.
        #[serde(default)]
        direction: Direction,
        /// Original remote task id, if the emitting protocol supplied one.
        /// `None` preserves replay of pre-18.2 journal entries.
        #[serde(default)]
        task: Option<String>,
    },
    /// One frame this host **sent** to a peer, and what the peer said about it
    /// (Story 18.4d, D10; FR162's sender half).
    ///
    /// The receiver journals its own row independently, which is the point: two
    /// records of one event, on two hosts, is what makes a federation claim
    /// checkable rather than asserted.
    ///
    /// ⛔ Carries no address and no transport identifier (NFR74) — a `PeerId`,
    /// the correlation, the byte count and the outcome, and nothing else.
    PeerFrameAttempted {
        peer: PeerId,
        /// The frame's correlation id, so the two hosts' rows can be lined up.
        correlation: String,
        /// Encoded frame length. Volume, never content.
        bytes: usize,
        outcome: PeerFrameAttemptOutcome,
        /// The class the peer named, present only for
        /// [`PeerFrameAttemptOutcome::Refused`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<FrameRefusal>,
    },
    HostBoundUnavailable {
        node: AgentId,
        host: HostBinding,
    },
    /// An MCP long-running task was bound to a durable node (Story 17.5a).
    /// `task` is the server-chosen task id; identity is additionally
    /// recoverable from the node id itself (reversible mint), so this event
    /// is the room's display/audit record, not the source of truth.
    McpTaskBound {
        node: AgentId,
        server: String,
        task: String,
    },
    /// 17.5b — an MCP task filed a blocking input-request ticket to the
    /// operator (FR152's shape, adopted early per R-14). Carries the artifact
    /// handle produced for the elicitation. Shipped WITHOUT a `to:` field;
    /// **18.3a-b adds `to: Option<TicketAddressee>`** — an `AgentId`-keyed
    /// variant, not an `AgentPath` (ruling A1: `AgentId` is already the
    /// path-capable addressing type, NFR68 hook #1, and this story mints no
    /// second one).
    ///
    /// **Replay contract (NFR70(d)):** a ticket journaled by 17.5b — no `to`
    /// key at all — replays unchanged, and an unaddressed ticket written by
    /// this build re-serializes to the byte-identical 17.5b shape.
    ///
    /// ⚠ **Which attribute does which, measured not assumed (18.3a-b).**
    /// `skip_serializing_if = "Option::is_none"` is the **load-bearing** half:
    /// drop it and an unaddressed ticket emits `"to":null`, which
    /// `ticket_assigned_serializes_without_a_to_field` catches. `#[serde(default)]`
    /// is **defence in depth, not the mechanism** — serde already resolves a
    /// missing `Option<T>` field to `None` via its `missing_field` helper, so
    /// dropping it changes nothing *today*. It is kept because it states the
    /// intent and because it becomes load-bearing the moment `to` stops being
    /// an `Option`. The replay guarantee itself is pinned behaviourally, on a
    /// 17.5b byte literal, in `tests/conformance_18_3a_b_addressing.rs`.
    ///
    /// ⛔ **No `assigned_at`.** Ordering is [`JournalEntry::seq`] and time is
    /// [`JournalEntry::recorded_at_ms`]; the journal envelope owns wall-clock
    /// at **one stamp site** and emitters never pass it in. A per-event
    /// timestamp would be a second stamp site for one fact.
    ///
    /// [`JournalEntry::seq`]: crate::domain::models::JournalEntry::seq
    /// [`JournalEntry::recorded_at_ms`]: crate::domain::models::JournalEntry::recorded_at_ms
    TicketAssigned {
        node: AgentId,
        artifact: ArtifactId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<TicketAddressee>,
    },
    /// Resolve a previously assigned ticket. The artifact id is the stable
    /// idempotency key: duplicate replay must neither reopen the ticket nor
    /// overwrite its first durable outcome.
    TicketResolved {
        node: AgentId,
        artifact: ArtifactId,
        outcome: TicketResolution,
    },
    /// Story 18.3 (AC4) — FR95 "who saw what": content actually crossed to the
    /// peer. Distinct from the 18.2 status-query record, which only proves the
    /// peer ASKED. A `tasks/get` returning `working` discloses nothing and MUST
    /// NOT produce this event.
    PeerDisclosure {
        /// Authenticated remote principal that received the content.
        #[serde(default)]
        peer: Option<PeerId>,
        node: AgentId,
        /// Original remote task id when the protocol supplied one.
        #[serde(default)]
        task: Option<String>,
        /// Byte length of the content handed back. Never the content itself.
        #[serde(default)]
        disclosed_bytes: usize,
        /// Composition and send decision recorded at actual disclosure.
        #[serde(
            default = "Authorship::unknown",
            skip_serializing_if = "Authorship::is_unknown"
        )]
        authorship: Authorship,
        #[serde(
            default = "SendAuthorization::unknown",
            skip_serializing_if = "SendAuthorization::is_unknown"
        )]
        send_authorization: SendAuthorization,
    },
    /// Append-only same-host retraction of a previously disclosed auto response.
    AutoResponseRetracted {
        /// Journal sequence of the original [`RoomEvent::PeerDisclosure`].
        #[serde(default)]
        target_seq: u64,
        /// Wall-clock timestamp captured by the same-host action dispatcher.
        #[serde(default)]
        retracted_at_ms: i64,
    },
    /// Durable operator resolution of a buffered peer-response draft.
    PeerDraftResolved {
        node: AgentId,
        /// Frozen legacy encoding; never a provenance source for any render, serve or retract decision (Story 19.20a).
        agent_composed: bool,
        sent: bool,
    },
    /// Decision-time record for an inbound peer interaction. This row is
    /// journaled before urgency decides when to interrupt the operator.
    PeerInteractionSurfaced {
        #[serde(default)]
        peer: Option<PeerId>,
        node: AgentId,
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        notification: NotificationUrgency,
        #[serde(default)]
        provenance: InteractionPolicySnapshot,
    },
    /// Auditable boundary after which preceding digest-tier interaction rows
    /// have been shown in one batch.
    PeerDigestFlushed {
        #[serde(default)]
        flushed_at: i64,
        #[serde(default)]
        count: usize,
    },
    /// Durable operator grant for one sender and its policy provenance.
    /// Duplicate grants are replay-idempotent; absent provenance is legacy.
    ConsentGranted {
        /// Stable sender label. Missing senders fail closed during projection.
        #[serde(default)]
        sender: Option<PeerId>,
        #[serde(default)]
        granted_at: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<PrincipalKind>,
    },
    /// Durable operator revocation of a previously granted sender.
    ConsentRevoked {
        /// Stable authenticated sender identity. Missing and never-granted
        /// senders are projection no-ops; revocation never manufactures trust.
        #[serde(default)]
        sender: Option<PeerId>,
        #[serde(default)]
        revoked_at: i64,
    },
    /// Durable operator grant of a **room role** to a configured peer
    /// (Story 18.3a, AC4). Produced by `/room role grant <alias-or-peer-id>
    /// <role>` acting on an entry that already exists in `a2a.json`.
    ///
    /// # Four withdrawals, four meanings — never merge them
    ///
    /// The room journal carries four distinct withdrawal facts and a fold
    /// that cannot tell them apart is a fold that lies to `/team log`:
    ///
    /// 1. [`RoomEvent::ConsentRevoked`] (Story 18.3d, `/team untrust`) —
    ///    *one sender's standing A2A consent* is withdrawn, so the next
    ///    inbound message re-prompts. An application-level operator act about
    ///    delivery, not about the room.
    /// 2. [`RoomEvent::RoomRoleRevoked`] (Story 18.3a, `/room role revoke`) —
    ///    a peer's **room role** is withdrawn: they may no longer make room
    ///    edits. Says nothing about delivery or transport.
    /// 3. FR158 trust-set revocation (Story 18.4b, `peer revoke`) — the peer
    ///    leaves the *transport allowlist*, so the next inbound frame on an
    ///    already-open connection is refused, valid signature notwithstanding.
    ///    **Story 18.4b authored [`RoomEvent::PeerAdmissionRecorded`] for
    ///    that**, carrying [`PeerAdmissionOutcome::Revoked`]; it is not a
    ///    second producer of this one. ⚠ The sentence this block carried
    ///    before 2026-08-14 — *"18.4 authors its own variant for that"* — was
    ///    false at HEAD: cut 1 of 18.4 shipped the substrate and authored no
    ///    variant.
    /// 4. ⛔ Not a withdrawal at all, listed because it is the one most easily
    ///    mistaken for one: `peer revoke` does **not** rekey, tear down a
    ///    session, or recall delivered bytes.
    ///
    /// ⛔ No rekey consequence. Nothing here implies cryptographic exclusion,
    /// key rotation, or that already-delivered bytes are recalled
    /// (`DF-18-CRYPTO-CLUSTER` C4). This is a journaled fact — *"X was granted
    /// role R at T"* — never an enforcement claim.
    RoomRoleGranted {
        /// Stable authenticated peer identity. A missing value from a
        /// forward-written or malformed record is a projection no-op: a role
        /// grant never manufactures an identity.
        #[serde(default)]
        peer: Option<PeerId>,
        /// Defaults to [`RoomRole::Viewer`] — least privilege — when the field
        /// is absent or carries a role string this build cannot read.
        #[serde(default)]
        role: RoomRole,
        #[serde(default)]
        granted_at: i64,
    },
    /// Durable operator withdrawal of a peer's room role (Story 18.3a, AC4).
    ///
    /// See [`RoomEvent::RoomRoleGranted`] for the three-way revocation
    /// boundary this variant exists to keep distinct. Revoking a peer that was
    /// never granted a role is a fold no-op that does not synthesize identity.
    RoomRoleRevoked {
        #[serde(default)]
        peer: Option<PeerId>,
        #[serde(default)]
        revoked_at: i64,
    },
    /// Write-ahead marker: an apply of `artifact` is about to mutate the
    /// One-Ring workspace (Story 18.3a-d — FR160(c), NFR70(c)).
    ///
    /// Appended **and flushed before**
    /// [`crate::domain::ports::PatchApplier::apply`] is invoked, so a crash
    /// inside the apply window leaves a durable trace instead of a silent
    /// divergence between the working tree and the room. A `PatchApplyStarted`
    /// with no matching [`RoomEvent::PatchApplyResolved`] folds to
    /// [`ApplyState::Indeterminate`] — never to success.
    ///
    /// The operator is recorded when the confirmed front door invokes this
    /// chokepoint. Policy-driven `/fanout` applies remain unattributed and omit
    /// the field on the wire, preserving pre-18.3a-e journal compatibility.
    PatchApplyStarted {
        artifact: ArtifactId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        applier: Option<AgentId>,
        /// Best-effort preimage witness: the workspace revision observed just
        /// before the mutation, via `PatchApplier::revision`.
        ///
        /// 🔴 `None` is a first-class honest value and **never** fails an
        /// apply. `git rev-parse HEAD` legitimately fails on a repository with
        /// no commits, and `git apply` works outside a repository entirely, so
        /// a non-git or pre-first-commit workspace is a supported case rather
        /// than an error. An audit field must never become load bearing for
        /// control flow.
        #[serde(default)]
        workspace_revision: Option<String>,
    },
    /// The apply of `artifact` returned, carrying its outcome (Story 18.3a-d).
    ///
    /// Appended after [`crate::domain::ports::PatchApplier::apply`] returns,
    /// on **both** the success and the failure path: skipping it on error
    /// would leave a merely-conflicting apply projecting as
    /// [`ApplyState::Indeterminate`] instead of a resolved failure, which is
    /// the strictly worse lie.
    PatchApplyResolved {
        artifact: ArtifactId,
        outcome: ApplyOutcome,
    },
    /// An operator inspected the working tree and reported what they found
    /// (Story 18.3a-f — FR160(d), NFR70(c)/(d)).
    ///
    /// ⛔ **This is NOT an observation the system made.** It records what a
    /// human said, under the same trusted-filesystem assumption as every other
    /// record in this journal (`ADR-18-3a-d-01` D1) — it is not an audit trail,
    /// not evidence, not authenticated and not tamper-evident. ⚠ It is the
    /// **fourth** member of the unsigned-trusted-on-replay class
    /// (`DF-17-2d-AUTH-1`) and the sharpest: a forged line clears a safety
    /// latch (`DF-18-2-AUTHENTICATED-JOURNAL`).
    ///
    /// Folding it moves the artifact from [`ApplyState::Indeterminate`] to
    /// [`ApplyState::OperatorResolved`] — a **distinct** fact class, never a
    /// second [`RoomEvent::PatchApplyResolved`]: `Resolved(outcome)` means the
    /// journal records what `git apply` returned, and a human report is not
    /// that. (Same ruling shape `DF-18-3a-d-APPLY-UNDO` already applies to
    /// reverse-apply: *"a distinct fact, not a second resolution"*.)
    ///
    /// `inspector` is **attribution, never accountability**: `acting_principal`
    /// returns [`AgentId::local_operator`] unconditionally, so the field has one
    /// possible value in this build and two humans on one workstation are one
    /// identity (`DF-18-3a-f-OPERATOR-SINGULARITY`, UX-DR-ROOM-04). It carries
    /// ⛔ **no** `#[serde(default)]`: [`AgentId::default`] is
    /// [`AgentId::new`] — a fresh random nanoid — so a defaulted attribution
    /// field would fabricate an identity out of a truncated line.
    PatchApplyInspected {
        artifact: ArtifactId,
        finding: OperatorApplyFinding,
        inspector: AgentId,
    },
    /// A durable transport-admission fact (Story 18.4b, AC4/AC6 / FR157,
    /// FR158). ⛔ **One** variant, three outcomes — see [`PeerAdmissionOutcome`].
    ///
    /// Produced by `peer add` ([`PeerAdmissionOutcome::Pinned`] and
    /// [`PeerAdmissionOutcome::ImportRefused`]) and by `peer revoke`
    /// ([`PeerAdmissionOutcome::Revoked`]). ⛔ Never by an inbound frame: the
    /// allowlist verdict already runs per frame and refuses a rotated key as an
    /// unlisted stranger, so the frame path cannot tell a rotation from a
    /// stranger and no record here claims it can (ruling A2).
    ///
    /// # Why one variant and not three
    ///
    /// A new `RoomEvent` variant is a durable, forward-only commitment and a
    /// transparency touch-point cost. These three facts share a subject (one
    /// alias's standing in `.rustain/p2p.json`), a producer family (the `peer`
    /// verbs) and a projection, so they are one variant carrying an outcome —
    /// paying that cost once.
    ///
    /// # What it is not
    ///
    /// A journaled fact, never an enforcement claim. `Revoked` records that the
    /// operator removed an entry; the *observable* consequence is that the
    /// peer's next frame is refused without a restart. ⛔ It does not mean a
    /// connection closed, a session ended, a key rotated, or that anything
    /// already delivered was recalled (`DF-18-CRYPTO-CLUSTER`).
    ///
    /// ⛔ It carries **no** transport address or endpoint identity of any kind
    /// (NFR74). That prohibition is pinned by
    /// `conformance_p2p_transport.rs::nfr74_transport_types_never_become_room_authority_or_provenance`,
    /// which scans this entire file for those type names — so they may not
    /// appear even in a comment here, which is why none are named.
    PeerAdmissionRecorded {
        /// The operator's `.rustain/p2p.json` map key. The only stable
        /// identifier a peer has that is not its key material, and therefore
        /// the only thing a key change is detectable against (ruling A2).
        #[serde(default)]
        alias: String,
        /// The identity derived from the pinned key, when there was one.
        /// `None` for an entry that carried no pin: a record never manufactures
        /// an identity.
        #[serde(default)]
        peer: Option<PeerId>,
        #[serde(default)]
        outcome: PeerAdmissionOutcome,
    },
    /// Two irreconcilable heads were seen for one peer's Topic feed at one
    /// sequence (Story 18.4a, FR150-a).
    ///
    /// # What this record is
    ///
    /// An observation this host made, and nothing more. It says: *at sequence
    /// `n` of `issuer`'s feed for this Topic, `peer` advertised a head that
    /// disagrees with the head this host already held.* ⛔ It is **detection and
    /// recording only**. It does not block, punish or exclude the peer; nothing
    /// downstream may describe it as authenticated, as an accusation, or as
    /// settled fact about who was wrong. Excluding an equivocating peer needs
    /// the revocation/rekey path, which defers with `DF-18-CRYPTO-CLUSTER`.
    ///
    /// # Why the local feed check is not enough
    ///
    /// `ReplayWindow` already refuses a frame that does not chain to the head it
    /// holds — but per connection, in memory, producing no durable record, and
    /// only for frames that peer sent **this** host. It structurally cannot see
    /// what a *different* peer says about the same feed. This is that
    /// cross-peer half, which is why `peer` and `issuer` are separate fields.
    ///
    /// # Identity only
    ///
    /// ⛔ Carries no address and no dialing identifier of any kind (NFR74) — a
    /// [`PeerId`], the Topic's correlation, the sequence and the two heads, and
    /// nothing else. That prohibition is pinned by a scan over this whole file,
    /// comments included, which is why none is named here.
    PeerEquivocated {
        /// The peer whose advertisement disagreed with what this host held.
        #[serde(default)]
        peer: Option<PeerId>,
        /// Whose feed the two heads describe. Frequently a **different**
        /// identity from `peer`: an advertisement about someone else's feed is
        /// the whole reason this record exists.
        #[serde(default)]
        issuer: Option<PeerId>,
        /// The Topic, which is a correlation id (`Topic := CorrelationId`).
        #[serde(default)]
        topic: String,
        #[serde(default)]
        sequence: u64,
        /// Lowercase hex of the head this host already held.
        #[serde(default)]
        held: String,
        /// Lowercase hex of the head that was advertised.
        #[serde(default)]
        advertised: String,
    },
    /// This host admitted handles from a peer's topic feed. An observation of
    /// their arrival, never a claim about the artifacts' contents or truth.
    PeerTopicHandlesReceived {
        #[serde(default)]
        peer: Option<PeerId>,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        sequence: u64,
        /// Lowercase artifact ids in frame order; no peer-chosen prose.
        #[serde(default)]
        artifacts: Vec<String>,
    },
    /// A recipient-owned durable work item was accepted from an inbound peer.
    RecipientItemReceived {
        address: ItemAddress,
        /// Sender-selected correlation. Never used as the recipient item id.
        task: String,
        /// Optional display attribution, not identity.
        #[serde(default)]
        alias: Option<String>,
        /// Verified semantic type carried at ingress, independent of body text.
        #[serde(default, skip_serializing_if = "SemanticMessageType::is_unknown")]
        message_type: SemanticMessageType,
        /// Sender-stated story relationships, fixed when this item is created.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stories: Option<super::ReceivedStories>,
        /// The sending host's immutable selection statement, not a story list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_for: Option<super::ReceivedSelection>,
        #[serde(
            default = "Authorship::unknown",
            skip_serializing_if = "Authorship::is_unknown"
        )]
        authorship: Authorship,
        #[serde(
            default = "SendAuthorization::unknown",
            skip_serializing_if = "SendAuthorization::is_unknown"
        )]
        send_authorization: SendAuthorization,
        content: String,
        /// Story 19.18 `A5` — the item or reply-turn id this follow-up
        /// answers, carried by `x-rustain-in-reply-to` and stored raw
        /// (`AD-1824`). Validated inside the caller's principal before the
        /// item was minted. `None` serializes byte-identically to a pre-19.18
        /// line, so the pinned fixtures do not move.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_reply_to: Option<String>,
        /// Story 19.18c `A13`/`A14` — `sha256` (lowercase hex) of the item
        /// capability the sender presented at ingress; ⛔ never the value
        /// itself. `None` is a legacy item (no carrier: an older or
        /// non-rustain sender) and serializes byte-identically to a 19.18
        /// line. A value that does not decode to 32 bytes is capability-
        /// bearing and never matches (fail closed), ⛔ never legacy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capability_hash: Option<String>,
        /// Story 19.18c `A15` — `true` only for a follow-up whose presented
        /// reply capability matched its reference's nearest item. The
        /// thread walk counts a capability-bearing edge only when this is
        /// set. `false` serializes byte-identically to a 19.18 line.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reply_capability_matched: bool,
    },
    /// A local human deliberately acknowledged a recipient-owned item.
    RecipientItemAcknowledged {
        address: ItemAddress,
        /// Optional display attribution, not identity.
        #[serde(default)]
        alias: Option<String>,
    },
    /// The recipient disposed of their own copy of an item (FR165).
    ///
    /// ⛔ A **distinct fact class**, never a second acknowledgement and never a
    /// retract: retract is the sender marking their own content, removal is
    /// the recipient's housekeeping, and FR165 exempts it from the
    /// append-only discipline binding the sender's edit. The journal line
    /// stands forever — this record marks, it never erases — while the
    /// projection and the transparency row stop carrying the content.
    /// AD-1822's tombstone, distinct from not-found.
    RecipientItemRemoved {
        address: ItemAddress,
    },
    /// The **sender** marked their own previously-sent content as retracted
    /// (FR94-b, NFR64's cross-host clause) — the third fact class
    /// [`Self::RecipientItemRemoved`]'s doc pre-blessed. Written on the
    /// **recipient** host by the served `x-rustain-items/retract` verb (Story
    /// 19.16d), which is its only production writer.
    ///
    /// ⛔ Marks, never erases (`AD-1815`, extended cross-host by 19.16d): the
    /// journal line and the item's content both stand; only the mark changes.
    /// ⛔ No `direction` field: nothing is destroyed at the emit boundary
    /// (the rule [`Self::RemoteEnvelopeRejected`]'s field exists under), so
    /// the direction is derived from the variant like its three siblings.
    /// ⛔ Every byte is host-minted: the address from the authenticated
    /// caller's principal and the recipient's own item id, the stamp and the
    /// collapse flag from this host — no peer-chosen value enters the record.
    RecipientItemRetracted {
        address: ItemAddress,
        /// 🔴 **The RECIPIENT HOST mints this from its own clock at append
        /// time.** ⛔ Never sender-supplied: the item id is the verb's only
        /// wire parameter, and a peer-chosen millisecond would be a peer byte
        /// in the recipient's durable record.
        ///
        /// ⛔ **No `0` sentinel**: the `AutoResponseRetracted` `0 ⇒
        /// recorded_at_ms` convention lives in `fold_transparency`, which
        /// iterates `JournalEntry`; the recipient-item fold has no entry in
        /// scope (`from_entries` passes an index, `apply` a bare event), so it
        /// cannot perform that fallback without making cold-fold and live-apply
        /// disagree. The recipient always mints a real stamp, so `0` is
        /// reachable only from a forged or clock-broken record and surfaces
        /// honestly as `Some(0)`. `#[serde(default)]` is for forward
        /// compatibility only.
        #[serde(default)]
        retracted_at_ms: i64,
        /// Whether the caller reached this host over a loopback bind, where
        /// every local caller is one principal (`SubmitterTrust::Loopback`).
        /// The same predicate the verb's `principalCollapsed` response field
        /// carries, persisted so the disclosure is a durable fact on the
        /// recipient's ledger rather than only a response body a caller can
        /// decline to read (owner answer 2, 2026-09-21). ⛔ A legibility
        /// statement about attribution, never a claim about who acted.
        #[serde(default)]
        principal_collapsed: bool,
    },
    /// Story 19.18 `A4` — the recipient answered one of its items: a **reply
    /// turn**, a new record in its own right. ⛔ Never a write on the
    /// answered item (`AD-1827`): the item-state machine stays three states
    /// and 2 acts.
    ///
    /// Written on the ANSWERING host by the daemon reply arm (`/team reply`,
    /// directly or by resolving a pending card) and by a sending draft
    /// resolution. Served to the asker by `x-rustain-items/thread`; ⛔ never
    /// pushed. Every byte except `content` is host-minted.
    RecipientItemReplied {
        /// The turn's address: the answered item's principal plus a host-
        /// minted `rr_` id.
        address: TurnAddress,
        /// The item this turn answers.
        answers: ItemId,
        /// The reply text, stored raw (`AD-1824`); scrubbed at serve time.
        content: String,
        /// Carried from the act that authored the turn (`A14`).
        #[serde(default = "Authorship::unknown")]
        authorship: Authorship,
        /// The send decision carried from the reply act; journal-local.
        #[serde(
            default = "SendAuthorization::unknown",
            skip_serializing_if = "SendAuthorization::is_unknown"
        )]
        send_authorization: SendAuthorization,
        /// Whether the turn is an answer, a reported defer, or an unknown
        /// future kind. Older journal rows default to Answer.
        #[serde(default, skip_serializing_if = "ReplyKind::is_answer")]
        kind: ReplyKind,
        /// Raw tolerant payload: validation belongs to the recipient fold.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "super::status_statement::deserialize_raw_status"
        )]
        status: Option<serde_json::Value>,
        /// The peer-response node whose card this turn resolved (`Q1`): the
        /// journal — not the draft controller — enforces one turn per node.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via_node: Option<AgentId>,
    },
    /// Story 19.18 `A13` — the ASKER's host received one peer-authored reply
    /// turn by reading a thread (FR95: record what was received). Appended
    /// once per `(peer, turn)`, never per read.
    PeerReplyObserved {
        /// The roster peer's resolved identity the thread was read from.
        peer: PeerId,
        /// Origin digest for an unpinned roster peer. Lets an observed reply
        /// survive an alias rename under the same binding as its dispatch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<String>,
        /// The peer-minted reply-turn id (bounded on read).
        turn: String,
        /// The item id the turn answers, as the peer served it.
        #[serde(default)]
        answers: Option<String>,
        /// The raw reply text, or `None` when the peer withheld it or it
        /// exceeded this host's bound (then only the hash is kept).
        #[serde(default)]
        content: Option<String>,
        /// Hash of the text this host received, when it received any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_hash: Option<ContentHash>,
    },
    /// Story 19.21 `C1` — this host's operator submitted one send action to
    /// N addressed recipients. Journaled **after** whole-action validation and
    /// **before** any recipient preflight, dispatch or POST.
    ///
    /// Sender-local and journal-only: ⛔ never on the wire, never addressable by
    /// a peer, no group outcome, no retry, no body bytes and nothing derived
    /// from them but the hash and the byte count. An older build reads this tag
    /// as [`RoomEvent::Unrecognized`]; this build's transparency projection
    /// deliberately omits it (no new transparency kind).
    TeamSendActionRecorded {
        action: SendActionId,
        /// Operator order. The vector's length is the **addressed count**.
        recipients: Vec<SendActionRecipient>,
        /// The semantic type, or [`SemanticMessageType::Unknown`] for an
        /// untyped send (omitted on write).
        #[serde(default, skip_serializing_if = "SemanticMessageType::is_unknown")]
        message_type: SemanticMessageType,
        /// The operator-authored, sender-local label (`--subject`). ⛔ Never
        /// derived from body bytes and never sent to a peer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        body_hash: ContentHash,
        body_bytes: usize,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        stories: Vec<crate::domain::models::RecordedStory>,
    },
    /// Story 19.21 `C1` — the typed outcome of one recipient of a send action,
    /// appended after every row that recipient's delivery path emitted and
    /// before the settle is shown.
    TeamSendRecipientSettled {
        action: SendActionId,
        /// 0-based position in the action's operator-ordered recipient list.
        index: u32,
        settlement: SendSettlement,
    },
    /// An `event` tag this build does not recognise.
    ///
    /// `RoomEvent` is `#[non_exhaustive]` and the journal is a durable
    /// forward-compatible format: a newer build may append a variant this one
    /// has never heard of. Without `#[serde(other)]` that line would fail
    /// `parse_entries` and take the **whole journal** with it. Catching it
    /// here is what lets UX-DR-ROOM-01 hold — an unrecognised record renders
    /// as an explicit unknown row instead of vanishing or failing the load.
    ///
    /// The payload is deliberately discarded: the journal is append-only and
    /// never rewritten, so nothing round-trips this variant back to disk.
    #[serde(other)]
    Unrecognized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub id: AgentId,
    pub origin: NodeOrigin,
    pub state: NodeState,
    pub host: HostBinding,
    pub host_bound_unavailable: bool,
    pub last_remote_content: Option<ContentHash>,
    /// `(server, taskId)` when this node is a bound MCP task (17.5a).
    pub mcp_task: Option<(String, String)>,
    /// 17.5b — open input-request ticket(s) filed by this node to the
    /// operator. A `Waiting` MCP-task node carries at least one.
    pub open_tickets: Vec<ArtifactId>,
    /// Durable terminal outcomes keyed by the ticket artifact. Keeping the
    /// outcome on the node makes expiry and cancellation failures visible
    /// after replay and across restarts.
    pub resolved_tickets: BTreeMap<ArtifactId, TicketResolution>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaveView {
    pub id: WaveId,
    pub coordinator: AgentId,
    pub spokes: Vec<AgentId>,
    pub outcome: Option<WaveOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalView {
    pub node: AgentId,
    pub fingerprint: InvocationFingerprint,
    pub source: Option<ApprovalSource>,
    pub outcome: Option<ApprovalOutcome>,
}

/// A durable record of a refused remote envelope (Story 17.4b, Ruling 6). Kept
/// so a rejection is inspectable in the room, not just durable-but-invisible.
/// `RemoteEnvelopeRejected` carries no node, so this is room-scoped rather than
/// projected onto a `NodeView`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRejectionView {
    pub peer: PeerId,
    pub reason: RejectReason,
    /// Story 18.2 (AC2, P-3). Carried into the read model deliberately: a
    /// projection that drops a field it was just told to record is the exact
    /// drift this story exists to prevent.
    pub direction: Direction,
}

/// Immutable read model reconstructed from the canonical event stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OrchestrationRoom {
    id: OrchestrationRoomId,
    nodes: BTreeMap<AgentId, NodeView>,
    waves: Vec<WaveView>,
    artifacts: BTreeMap<ArtifactId, ArtifactRef>,
    /// Journal-projected apply state, keyed by patch artifact (Story 18.3a-d).
    ///
    /// 🔴 **A separate map, deliberately — not a field on `ArtifactRef`.**
    /// `EvidenceArtifact` is content-addressed **immutable** metadata living
    /// in the `ArtifactStore`; apply state is **journal-projected mutable**
    /// state. Mixing them violates `ADR-17-CC-02` (the store holds handles and
    /// bodies, the journal holds the mutable projection). Keeping it separate
    /// also lets the fold record an apply for an artifact this projection has
    /// never seen, instead of silently dropping it.
    apply_state: BTreeMap<ArtifactId, ApplyState>,
    /// Patches captured before this journal's first apply record. The fold
    /// consumes event order directly; no `JournalEntry::seq` enters this model.
    predates_apply_records: BTreeSet<ArtifactId>,
    seen_apply_record: bool,
    approvals: Vec<ApprovalView>,
    remote_rejections: Vec<RemoteRejectionView>,
}

impl OrchestrationRoom {
    /// Fold an ordered stream into an immutable room projection.
    pub fn project(id: OrchestrationRoomId, events: impl IntoIterator<Item = RoomEvent>) -> Self {
        let mut room = Self {
            id,
            ..Self::default()
        };

        for event in events {
            room.apply(event);
        }
        room
    }

    /// Host-honest projection: fold, then derive each node's availability from
    /// its recorded host binding vs the current host. A node whose binding
    /// matches the current host is available even if a prior foreign-host
    /// replay left a persisted `HostBoundUnavailable` marker (the marker never
    /// sticks after the node returns home); a foreign binding renders
    /// unavailable with no live handle fabricated (ADR-17-CC-03).
    pub fn project_for_host(
        id: OrchestrationRoomId,
        events: impl IntoIterator<Item = RoomEvent>,
        current_host_id: &str,
    ) -> Self {
        let mut room = Self::project(id, events);
        for view in room.nodes.values_mut() {
            view.host_bound_unavailable = view.host.host_id != current_host_id;
        }
        room
    }

    /// Read-only accessors. The room is a projection, not a store: there is no
    /// public field and no `&mut` observer path to node/authority state (AC9).
    pub fn id(&self) -> &OrchestrationRoomId {
        &self.id
    }

    pub fn nodes(&self) -> &BTreeMap<AgentId, NodeView> {
        &self.nodes
    }

    pub fn waves(&self) -> &[WaveView] {
        &self.waves
    }

    pub fn artifacts(&self) -> &BTreeMap<ArtifactId, ArtifactRef> {
        &self.artifacts
    }

    /// Apply state per patch artifact (Story 18.3a-d). An artifact absent from
    /// this map is [`ApplyState::NeverAttempted`] — read it through
    /// `copied().unwrap_or_default()` so the lattice stays total at the call
    /// site.
    ///
    /// 🔴 **Exactly three production consumers, and every one of them owes a
    /// deliberate decision about every variant** (Story 18.3a-f, ruling A1):
    /// `PatchMergeBack::apply_is_refused` (the guard — fails closed on both
    /// unreadable shapes), `artifacts_panel::apply_state_suffix` (the row —
    /// exhaustive, so the compiler forces a wording decision), and
    /// `artifact_bridge::auto_applies_refusal` (the auto-apply door's honest
    /// sentence). ⚠ It has been a **display** surface since cut 2, not only a
    /// guard input; the pre-18.3a-e claim that it was "not a display surface"
    /// was stale from the day the panel started reading it.
    pub fn apply_state(&self) -> &BTreeMap<ArtifactId, ApplyState> {
        &self.apply_state
    }

    /// Artifacts whose capture precedes this journal's first apply record.
    pub fn predates_apply_records(&self) -> &BTreeSet<ArtifactId> {
        &self.predates_apply_records
    }

    pub fn approvals(&self) -> &[ApprovalView] {
        &self.approvals
    }

    /// Refused remote envelopes, in arrival order (Story 17.4b).
    pub fn remote_rejections(&self) -> &[RemoteRejectionView] {
        &self.remote_rejections
    }

    fn apply(&mut self, event: RoomEvent) {
        match event {
            RoomEvent::NodeRegistered { node, origin, host } => {
                self.nodes.insert(
                    node.clone(),
                    NodeView {
                        id: node,
                        origin,
                        state: NodeState::Created,
                        host,
                        host_bound_unavailable: false,
                        last_remote_content: None,
                        mcp_task: None,
                        open_tickets: Vec::new(),
                        resolved_tickets: BTreeMap::new(),
                    },
                );
            }
            RoomEvent::NodeStateChanged { node, from, to } => {
                if let Some(view) = self.nodes.get_mut(&node)
                    && view.state == from
                {
                    view.state = to;
                }
            }
            RoomEvent::WaveStarted {
                wave,
                coordinator,
                spokes,
            } => self.waves.push(WaveView {
                id: wave,
                coordinator,
                spokes,
                outcome: None,
            }),
            RoomEvent::WaveCompleted { wave, outcome } => {
                if let Some(view) = self.waves.iter_mut().rev().find(|view| view.id == wave) {
                    view.outcome = Some(outcome);
                }
            }
            RoomEvent::AdmissionDeferred { .. } => {}
            RoomEvent::ArtifactCreated { artifact } => {
                self.artifacts.insert(artifact.id.clone(), artifact);
            }
            RoomEvent::ApprovalRequested { node, fingerprint } => {
                self.approvals.push(ApprovalView {
                    node,
                    fingerprint,
                    source: None,
                    outcome: None,
                });
            }
            RoomEvent::ApprovalResolved {
                node,
                fingerprint,
                source,
                outcome,
            } => {
                if let Some(view) = self.approvals.iter_mut().rev().find(|view| {
                    view.node == node && view.fingerprint == fingerprint && view.outcome.is_none()
                }) {
                    view.source = Some(source);
                    view.outcome = Some(outcome);
                }
            }
            RoomEvent::PatchCaptured { artifact, producer } => {
                if !self.seen_apply_record {
                    self.predates_apply_records.insert(artifact.clone());
                }
                if let Some(view) = self.artifacts.get_mut(&artifact)
                    && view.producer == producer
                {
                    view.kind = ArtifactKind::Patch;
                    // Fail-closed: never clobber a review state the journal
                    // already recorded. In particular a `ReviewStatus::Unknown`
                    // written by a newer build must not be erased into
                    // `Pending`, where the shipped merge-back policy would read
                    // an unreadable state as auto-appliable (Story 18.3a-c AC1).
                    if view.review.is_none() {
                        view.review = Some(ReviewStatus::Pending);
                    }
                }
            }
            RoomEvent::PatchReviewed {
                artifact,
                reviewer,
                verdict,
            } => {
                if let Some(view) = self.artifacts.get_mut(&artifact) {
                    view.review = Some(ReviewStatus::Reviewed { reviewer, verdict });
                }
            }
            // 🔴 All three apply arms fold **UNCONDITIONALLY**, into a map of
            // their own. ⛔ Do not wrap them in `if let Some(view) =
            // self.artifacts.get_mut(&artifact)` the way the `PatchReviewed`
            // arm directly above does: that guard silently swallows an event
            // for an artifact this projection has not seen, and an apply
            // record for an unknown artifact is a genuine anomaly that must
            // stay visible rather than vanish.
            RoomEvent::PatchApplyStarted { artifact, .. } => {
                self.seen_apply_record = true;
                self.apply_state.insert(artifact, ApplyState::Indeterminate);
            }
            // Last write wins per artifact, exactly as `PatchReviewed` does.
            // That is the non-vacuous half of NFR70(d): a journal carrying a
            // repeated Started/Resolved pair for one artifact folds to a
            // single state. ⛔ Never accumulate, and ⛔ never add an
            // idempotency guard that hides a second record from the projection
            // while it sits in the log.
            RoomEvent::PatchApplyResolved { artifact, outcome } => {
                self.seen_apply_record = true;
                self.apply_state
                    .insert(artifact, ApplyState::Resolved(outcome));
            }
            // 🔴 The operator's report is a THIRD fact class, and it sets
            // `seen_apply_record` for the same reason the two above do: the
            // flag drives the capture-order stamp (`predates_apply_records`),
            // and a resolution folding without it would stamp every patch
            // captured afterwards as pre-record-era — a false *"this journal
            // predates apply records"* warning on a brand-new patch.
            //
            // ⛔ No idempotency guard, for the reason stated on the arm above:
            // a second report must move the projection, not sit invisible in
            // the log. Last write wins, so the latest report governs — the same
            // shape `ADR-18-3a-c-01` D6 blessed for re-review.
            // ⚠ The structural ratchet
            // (`the_apply_fold_arms_are_unnested_and_the_guard_precedes_the_mutation`)
            // slices this arm from `            RoomEvent::…` to the first
            // 12-space `}` on a line of its own, so rustfmt's wrapped
            // destructure is fine — but a nested block closing at that
            // indentation would truncate the slice and fail its positive
            // control.
            RoomEvent::PatchApplyInspected {
                artifact, finding, ..
            } => {
                self.seen_apply_record = true;
                self.apply_state
                    .insert(artifact, ApplyState::OperatorResolved(finding));
            }
            RoomEvent::RemoteEnvelopeAccepted {
                node, content_hash, ..
            } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.last_remote_content = Some(content_hash);
                }
            }
            RoomEvent::RemoteEnvelopeRejected {
                peer,
                reason,
                direction,
                ..
            } => {
                self.remote_rejections.push(RemoteRejectionView {
                    peer,
                    reason,
                    direction,
                });
            }
            RoomEvent::HostBoundUnavailable { node, host } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.host = host;
                    view.host_bound_unavailable = true;
                }
            }
            RoomEvent::McpTaskBound { node, server, task } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.mcp_task = Some((server, task));
                }
            }
            // `to` is bound and ignored: the addressee is **durable-only** in
            // this cut. `NodeView.open_tickets` stays `Vec<ArtifactId>` — the
            // only consumer that wanted an addressee in the read model was a
            // cross-node inbox sort, and that surface deferred whole
            // (`DF-18-3a-b-INBOX-SURFACE`). Widening it here is a mutant the
            // byte-identical-fold assertion catches.
            RoomEvent::TicketAssigned {
                node,
                artifact,
                to: _,
            } => {
                if let Some(view) = self.nodes.get_mut(&node)
                    && !view.open_tickets.contains(&artifact)
                    && !view.resolved_tickets.contains_key(&artifact)
                {
                    view.open_tickets.push(artifact);
                }
            }
            RoomEvent::TicketResolved {
                node,
                artifact,
                outcome,
            } => {
                if let Some(view) = self.nodes.get_mut(&node) {
                    view.open_tickets.retain(|open| open != &artifact);
                    view.resolved_tickets.entry(artifact).or_insert(outcome);
                }
            }
            // Room-scoped operator facts with no node, wave, artifact or
            // approval to attach to. Each has a dedicated adapter fold that
            // keys on its own variant: consent →
            // `adapters::policy::JournalConsentProjection`, room roles →
            // `adapters::policy::JournalRoomRoleProjection`. Folding them
            // here as well would put the same fact in two read models.
            RoomEvent::PeerDisclosure { .. }
            | RoomEvent::AutoResponseRetracted { .. }
            | RoomEvent::PeerDraftResolved { .. }
            | RoomEvent::PeerInteractionSurfaced { .. }
            | RoomEvent::PeerDigestFlushed { .. }
            | RoomEvent::ConsentGranted { .. }
            | RoomEvent::ConsentRevoked { .. }
            | RoomEvent::RoomRoleGranted { .. }
            | RoomEvent::RoomRoleRevoked { .. }
            // Transport admission is a fact about `.rustain/p2p.json`, not
            // about a room node: no node, wave, artifact or approval exists for
            // it to attach to, and the roster read model is the config file
            // itself. It renders through the transparency projection
            // (`TransparencyKind::TransportAdmission`), so this is absence of a
            // fold target rather than a silent loss.
            | RoomEvent::PeerAdmissionRecorded { .. }
            // A send dispatch has no node yet: the peer assigns the task id
            // needed to mint one only after this durable-before-POST record.
            | RoomEvent::RemoteEnvelopeDispatched { .. }
            // An outbound frame attempt is a fact about one peer and one
            // correlation, not about a room node: this cut's `peer ping` sends
            // from a CLI process that owns no node at all. It renders through
            // the transparency projection, which is where a peer-interaction
            // fact belongs.
            | RoomEvent::PeerFrameAttempted { .. }
            // A divergent Topic head is a fact about one peer's *feed*, not
            // about a room node: the Topic is a correlation, no node/wave/
            // artifact/approval exists for it to attach to, and the peer whose
            // feed diverged may own no node on this host at all. Decided as a
            // deliberate no-op rather than defaulted — the precedent for a
            // decided no-op is `role_events_are_room_read_model_no_ops_and_
            // replay_idempotently` (18.3a), and the precedent for rendering is
            // 18.4b's `PeerAdmissionRecorded`. It renders through the
            // transparency projection, which is where a peer-interaction fact
            // belongs.
            | RoomEvent::PeerEquivocated { .. }
            // A handle arrival is a fact about a peer's feed; no node to attach to.
            | RoomEvent::PeerTopicHandlesReceived { .. }
            // Recipient items have their own projection; they are deliberately
            // not folded into the orchestration node read model.
            | RoomEvent::RecipientItemReceived { .. }
            | RoomEvent::RecipientItemAcknowledged { .. }
            | RoomEvent::RecipientItemRemoved { .. }
            | RoomEvent::RecipientItemRetracted { .. }
            | RoomEvent::RecipientItemReplied { .. }
            // An asker-side receipt of a peer's reply turn: a fact about one
            // peer's served thread, not a room node. It renders through the
            // transparency projection.
            | RoomEvent::PeerReplyObserved { .. }
            // Story 19.21 `C1` — sender-local send-action facts. The action
            // board folds them from the journal (`a2a::board`); no node,
            // wave, artifact or approval exists for them to attach to, and
            // this build's transparency projection deliberately omits them
            // (no new transparency kind).
            | RoomEvent::TeamSendActionRecorded { .. }
            | RoomEvent::TeamSendRecipientSettled { .. } => {}
            // The room read model has nothing to fold an unknown tag into.
            // The transparency projection renders it as an explicit unknown
            // row instead (UX-DR-ROOM-01); dropping it here is not a silent
            // loss, it is the absence of a node/wave/artifact to attach to.
            RoomEvent::Unrecognized => {}
        }
    }
}

fn validate_id(value: &str) -> Result<(), RoomIdError> {
    if value.is_empty() {
        return Err(RoomIdError::Empty);
    }
    if value.contains('/') {
        return Err(RoomIdError::EmbeddedSeparator);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ArtifactId, ContentHash};

    fn sample_artifact_id() -> ArtifactId {
        ArtifactId::from(ContentHash::from_bytes([0x11; 32]))
    }

    #[test]
    fn topic_receipt_defaults_and_is_a_room_no_op() {
        let event: RoomEvent =
            serde_json::from_str(r#"{"event":"peer_topic_handles_received"}"#).expect("defaults");
        assert!(matches!(
            &event,
            RoomEvent::PeerTopicHandlesReceived {
                peer: None, topic, sequence: 0, artifacts
            } if topic.is_empty() && artifacts.is_empty()
        ));
        let room =
            OrchestrationRoom::project(OrchestrationRoomId::new(), vec![event.clone(), event]);
        assert!(room.nodes().is_empty());
    }

    #[test]
    fn direction_defaults_for_legacy_fields_and_absorbs_future_values() {
        assert_eq!(Direction::default(), Direction::Unknown);
        let future: Direction =
            serde_json::from_str(r#""sideways""#).expect("future direction parses");
        assert_eq!(future, Direction::Unknown);
    }

    /// NFR70(d) / AC3: replaying the journal twice yields the identical room,
    /// including the ticket. A `TicketAssigned` folded twice must not duplicate
    /// the artifact handle on the node's `open_tickets`.
    #[test]
    fn ticket_assigned_replays_idempotently() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let room_id = OrchestrationRoomId::new();
        let assigned = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
            to: Some(TicketAddressee::Operator {
                id: AgentId::local_operator(),
            }),
        };
        let events = vec![
            RoomEvent::NodeRegistered {
                node: node.clone(),
                origin: NodeOrigin::Remote,
                host: HostBinding::new("local", "h"),
            },
            assigned.clone(),
            assigned,
        ];
        let room = OrchestrationRoom::project(room_id, events);
        let view = room.nodes().get(&node).expect("node projected");
        assert_eq!(view.open_tickets, vec![artifact]);
    }

    #[test]
    fn ticket_resolution_closes_once_and_duplicate_assignment_cannot_reopen_it() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let assigned = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
            to: Some(TicketAddressee::Operator {
                id: AgentId::local_operator(),
            }),
        };
        let resolved = RoomEvent::TicketResolved {
            node: node.clone(),
            artifact: artifact.clone(),
            outcome: TicketResolution::ExpiredUnanswered {
                reason: "remote task TTL expired".into(),
            },
        };
        let room = OrchestrationRoom::project(
            OrchestrationRoomId::new(),
            vec![
                RoomEvent::NodeRegistered {
                    node: node.clone(),
                    origin: NodeOrigin::Remote,
                    host: HostBinding::new("local", "h"),
                },
                assigned.clone(),
                resolved.clone(),
                resolved,
                assigned,
            ],
        );
        let view = room.nodes().get(&node).expect("node projected");
        assert!(view.open_tickets.is_empty());
        assert_eq!(
            view.resolved_tickets.get(&artifact),
            Some(&TicketResolution::ExpiredUnanswered {
                reason: "remote task TTL expired".into(),
            })
        );
    }

    /// NFR70(d): a `TicketAssigned` journaled by 17.5b (no `to:` field) must
    /// round-trip through serde unchanged. 18.3a-b added
    /// `to: Option<TicketAddressee>` as `#[serde(default,
    /// skip_serializing_if = "Option::is_none")]`, so an unaddressed ticket
    /// still serializes to the byte-identical 17.5b shape and this gate stayed
    /// green — its assertions were never amended.
    ///
    /// **This test is AC1's primary mutant-killer.** Drop `skip_serializing_if`
    /// and the `!json.contains("\"to\"")` assertion fires on `"to":null` —
    /// observed RED. (Dropping `#[serde(default)]` alone is **not** caught, and
    /// nothing else catches it either: serde resolves a missing `Option<T>`
    /// field to `None` regardless. See the variant doc.)
    /// ⛔ Do not delete or weaken it. If you find yourself rewriting the
    /// assertions, you have chosen the wrong serde shape.
    #[test]
    fn ticket_assigned_serializes_without_a_to_field() {
        let node = AgentId::from_validated("mcp/s-srv/t-task");
        let artifact = sample_artifact_id();
        let event = RoomEvent::TicketAssigned {
            node: node.clone(),
            artifact: artifact.clone(),
            to: None,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        // The 17.5b wire shape carries `node` + `artifact` + the `event` tag,
        // and NO `to:` field. A future defaulted `to:` must not break this.
        assert!(
            json.contains("\"event\":\"ticket_assigned\""),
            "json: {json}"
        );
        assert!(
            !json.contains("\"to\""),
            "17.5b tickets carry no `to:`: {json}"
        );
        let back: RoomEvent = serde_json::from_str(&json).expect("deserialize round-trip");
        assert_eq!(event, back);
    }
}
