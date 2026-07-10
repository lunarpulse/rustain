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
use crate::domain::models::tool_call::RequestId;
use crate::domain::models::{
    AgentEnvelope, ApprovalOutcome, ChannelKind, ChatMessage, Ed25519Sig, ImageAttachment,
    PeerIdentity, PermissionMode, ToolRisk,
};
use crate::infrastructure::runtime::event_bus::RawEvent;

/// Current wire protocol version. Bump on ANY breaking frame-shape change so an
/// older client/daemon is rejected with [`ProtocolError::VersionMismatch`]
/// rather than mis-parsing (forward-compat for 12.3/12.4).
pub const PROTOCOL_VERSION: u32 = 2;

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
    pub transcript: Vec<ChatMessage>,
    /// The daemon's current permission mode (never `Yolo` headless — AC6).
    pub permission_mode: PermissionMode,
    /// Connected channels (honest — AC4 / `StatusSnapshot.channels`).
    pub channels: Vec<ChannelKind>,
    /// Count of tool actions denied-while-unattended and waiting to be resumed
    /// (AC6 #5 — "N actions waiting on you"). The transcript-render of the
    /// individual skipped actions is 12.2c; 12.2b emits the count.
    pub blocked_actions_waiting: usize,
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
    /// Resolve a pending consolidation card — daemon-authoritative (Story 12.2d AC4).
    /// `accept: true` = promote all retained proposals; `false` = decline all.
    /// The token must match the daemon's retained entry (confused-deputy guard).
    ConsolidationResolve { token: ProposalToken, accept: bool },
    /// Detach cleanly (the turn continues daemon-side — AC4).
    Detach,
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
    /// A page of history in response to [`ClientFrame::HistoryRequest`].
    History {
        messages: Vec<ChatMessage>,
        has_more: bool,
    },
    /// A tool needs approval — the writer client renders the permission card and
    /// replies [`ClientFrame::ApprovalResponse`] (AC6 #1).
    ApprovalRequest {
        request_id: RequestId,
        tool: String,
        input_preview: String,
        risk: ToolRisk,
    },
    /// Consolidation proposals generated by the daemon — sent to the writer only
    /// (Story 12.2d AC2). The client renders the existing `PendingConsolidationCard`
    /// verbatim and sends `ConsolidationResolve` on accept/decline.
    ConsolidationProposed {
        token: ProposalToken,
        proposals: Vec<ProposedFact>,
    },
    /// Peer envelope was verified and accepted by the daemon wire boundary.
    PeerAccepted { sequence: u64 },
    /// Acknowledge a clean [`ClientFrame::Detach`].
    Detached,
    /// A protocol-level error (e.g. version mismatch, read-only write attempt).
    Error(ProtocolError),
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
            DaemonFrame::History {
                messages: vec![],
                has_more: true,
            },
            DaemonFrame::ApprovalRequest {
                request_id: RequestId::new(),
                tool: "Write".into(),
                input_preview: "config.toml".into(),
                risk: ToolRisk::Standard,
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
