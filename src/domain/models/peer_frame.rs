//! Domain values for the acknowledged first frame (Story 18.4d, D9).
//!
//! # Why a send has an outcome at all
//!
//! Before this story `PeerTransport::send_to` opened a unidirectional stream and
//! returned `()`, so the sender could honestly claim only *"written"*. That is
//! not merely imprecise — it is unusable, because the RAP feed chains each
//! sender's frames by `prev_hash` and the receiver's `ReplayWindow` lives in
//! memory. A sender that *remembers* a position the receiver forgot forks the
//! feed permanently; a sender that forgets one the receiver kept is refused as a
//! replay. Neither is recoverable without knowing what the receiver expects.
//!
//! So the receiver **tells** the sender. [`FrameVerdict`] is that answer:
//! outcome plus, when the refusal was a feed-position refusal, the
//! [`FeedPosition`] the receiver expects next. The sender therefore keeps its
//! position only in memory for the life of one process, starts optimistically at
//! [`FeedPosition::start`], and self-corrects once from the verdict.
//!
//! # Boundary (NFR74)
//!
//! Nothing here is an iroh type and nothing here is a network address. The
//! verdict carries an outcome, a refusal class and a feed position — all
//! host-independent domain facts. The wire encoding of a verdict belongs to the
//! transport adapter, which is why no `Serialize` appears on [`FrameVerdict`]:
//! one adapter owns one codec, and a domain-level derive would quietly become a
//! second wire contract.

use serde::{Deserialize, Serialize};

/// Length of a RAP feed entry hash, which is what a `prev_hash` must be.
pub const FEED_ENTRY_HASH_BYTES: usize = 32;

/// How far a peer-supplied feed position may move the sender's own sequence.
///
/// The guidance is **untrusted input to the local signing path**: `sign_envelope`
/// writes `sequence` and `prev_hash` into the signed header without validating
/// them, so a hostile receiver that answered `u64::MAX - 1` would burn the
/// sender's whole sequence space in one frame. A ping in a fresh process starts
/// at 1 and no real feed reaches a million frames through this verb, so the
/// ceiling is generous for every legitimate case and closes the attack.
pub const MAX_GUIDED_SEQUENCE_JUMP: u64 = 1_000_000;

/// The position a receiver expects next in one sender's feed.
///
/// `next_sequence` is the lowest sequence the receiver would accept and
/// `prev_hash` is the entry hash that frame must chain to — empty when the
/// receiver holds no feed for this sender at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedPosition {
    pub next_sequence: u64,
    pub prev_hash: Vec<u8>,
}

impl FeedPosition {
    /// The optimistic start: a receiver with no feed for this sender requires
    /// sequence 1 and an empty `prev_hash`.
    #[must_use]
    pub fn start() -> Self {
        Self {
            next_sequence: 1,
            prev_hash: Vec::new(),
        }
    }

    /// Whether this position could have come from a correct receiver.
    ///
    /// Shape only: a `prev_hash` is either absent or exactly one entry hash, a
    /// sequence is at least 1, and `u64::MAX` is refused because the position
    /// after it does not exist.
    #[must_use]
    pub fn is_wellformed(&self) -> bool {
        self.next_sequence >= 1
            && self.next_sequence < u64::MAX
            && (self.prev_hash.is_empty() || self.prev_hash.len() == FEED_ENTRY_HASH_BYTES)
    }

    /// The position after a frame at `sequence` whose header hashed to
    /// `entry_hash` was accepted.
    #[must_use]
    pub fn advanced(sequence: u64, entry_hash: Vec<u8>) -> Self {
        Self {
            next_sequence: sequence.saturating_add(1),
            prev_hash: entry_hash,
        }
    }

    /// Accept a receiver's guidance, or refuse it.
    ///
    /// Returns `None` when the offered position is malformed or jumps further
    /// than [`MAX_GUIDED_SEQUENCE_JUMP`] past what this sender was about to use.
    /// A refused guidance is not retried: the sender reports the refusal it
    /// received rather than signing a header a peer dictated.
    #[must_use]
    pub fn accept_guidance(&self, offered: &Self) -> Option<Self> {
        if !offered.is_wellformed() {
            return None;
        }
        if offered.next_sequence > self.next_sequence.saturating_add(MAX_GUIDED_SEQUENCE_JUMP) {
            return None;
        }
        Some(offered.clone())
    }
}

/// Which class of refusal the receiver named.
///
/// Classes, never prose. The sender renders its own sentence from the class, so
/// no remote-authored string ever reaches an operator's terminal and the wording
/// ceiling stays structurally true rather than audited once.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameRefusal {
    /// The transport allowlist did not admit this sender.
    NotAdmitted,
    /// The envelope signature or its identity binding did not hold.
    SignatureInvalid,
    /// The frame's place in the sender's feed did not hold: a replay, a
    /// duplicate nonce, a fork or a gap. This is the class that carries a
    /// [`FeedPosition`].
    FeedPositionMismatch,
    /// The envelope's own validity window had closed.
    Expired,
    /// The frame was not a well-formed peer message.
    Malformed,
    /// The recipient declined it.
    Declined,
    /// The receiver could not take it right now.
    Unavailable,
    /// A class this build does not understand, or none was given.
    ///
    /// `#[serde(other)]` so a newer receiver's class does not fail the whole
    /// decode and silently become "no verdict arrived": an unreadable class is
    /// still a refusal, and saying so is more honest than dropping it.
    #[default]
    #[serde(other)]
    Unclassified,
}

/// What became of one outbound frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// The receiver said it took the frame.
    Accepted,
    /// The receiver said it refused the frame, with this class.
    Refused(FrameRefusal),
    /// The frame was written but no readable answer came back.
    ///
    /// ⛔ Never treat this as acceptance. It is produced locally — by a timeout,
    /// a closed stream or an undecodable reply — and never decoded from a peer.
    Unanswered,
}

impl FrameOutcome {
    /// Whether the receiver said it took the frame.
    #[must_use]
    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// One receiver's answer to one frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameVerdict {
    pub outcome: FrameOutcome,
    /// The position the receiver expects next, when it said one.
    pub expected: Option<FeedPosition>,
}

impl FrameVerdict {
    #[must_use]
    pub fn accepted() -> Self {
        Self {
            outcome: FrameOutcome::Accepted,
            expected: None,
        }
    }

    #[must_use]
    pub fn refused(refusal: FrameRefusal) -> Self {
        Self {
            outcome: FrameOutcome::Refused(refusal),
            expected: None,
        }
    }

    #[must_use]
    pub fn unanswered() -> Self {
        Self {
            outcome: FrameOutcome::Unanswered,
            expected: None,
        }
    }

    #[must_use]
    pub fn with_expected(mut self, expected: FeedPosition) -> Self {
        self.expected = Some(expected);
        self
    }

    /// The guided retry position, if this verdict both refused on feed position
    /// and offered one this sender may use.
    #[must_use]
    pub fn guided_retry(&self, current: &FeedPosition) -> Option<FeedPosition> {
        if self.outcome != FrameOutcome::Refused(FrameRefusal::FeedPositionMismatch) {
            return None;
        }
        current.accept_guidance(self.expected.as_ref()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wellformed_position_is_shape_checked_on_both_fields() {
        assert!(FeedPosition::start().is_wellformed());
        assert!(
            FeedPosition {
                next_sequence: 9,
                prev_hash: vec![7u8; FEED_ENTRY_HASH_BYTES],
            }
            .is_wellformed()
        );
        // Mutant: accept a short prev_hash.
        assert!(
            !FeedPosition {
                next_sequence: 9,
                prev_hash: vec![7u8; 31],
            }
            .is_wellformed()
        );
        // Mutant: accept sequence 0, which no accepted frame can have used.
        assert!(
            !FeedPosition {
                next_sequence: 0,
                prev_hash: Vec::new(),
            }
            .is_wellformed()
        );
        // Mutant: accept u64::MAX and leave no next position.
        assert!(
            !FeedPosition {
                next_sequence: u64::MAX,
                prev_hash: Vec::new(),
            }
            .is_wellformed()
        );
    }

    #[test]
    fn guidance_past_the_jump_ceiling_is_refused() {
        let current = FeedPosition::start();
        let hostile = FeedPosition {
            next_sequence: u64::MAX - 1,
            prev_hash: vec![0u8; FEED_ENTRY_HASH_BYTES],
        };
        assert!(current.accept_guidance(&hostile).is_none());

        // Positive control: a plausible correction is taken.
        let plausible = FeedPosition {
            next_sequence: 4,
            prev_hash: vec![1u8; FEED_ENTRY_HASH_BYTES],
        };
        assert_eq!(current.accept_guidance(&plausible), Some(plausible));
    }

    #[test]
    fn only_a_feed_position_refusal_yields_a_guided_retry() {
        let current = FeedPosition::start();
        let expected = FeedPosition {
            next_sequence: 3,
            prev_hash: vec![2u8; FEED_ENTRY_HASH_BYTES],
        };
        assert_eq!(
            FrameVerdict::refused(FrameRefusal::FeedPositionMismatch)
                .with_expected(expected.clone())
                .guided_retry(&current),
            Some(expected.clone())
        );
        // Mutant: retry on any refusal that happens to carry a position.
        assert!(
            FrameVerdict::refused(FrameRefusal::NotAdmitted)
                .with_expected(expected)
                .guided_retry(&current)
                .is_none()
        );
        // Mutant: infer a retry from an accepted verdict.
        assert!(FrameVerdict::accepted().guided_retry(&current).is_none());
    }

    #[test]
    fn an_unreadable_refusal_class_stays_a_refusal() {
        let decoded: FrameRefusal =
            serde_json::from_str("\"a_class_from_a_newer_build\"").expect("forward compatible");
        assert_eq!(decoded, FrameRefusal::Unclassified);
    }
}
