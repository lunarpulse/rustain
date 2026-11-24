//! Verified conservation recovery (Story Q-6 AC6).
//!
//! [`verify_history`] is the pure admission decision: given the conservation
//! heads read from the workspace journal and the independent
//! [`AuthorityTrust`], it either returns the heads to replay (in sequence order)
//! or a [`RecoveryRefusal`]. It trusts nothing the journal says about itself —
//! not a structural checksum, a file mode, or a record's own flags.

use crate::domain::models::LedgerConservationRecord;
use crate::domain::ports::{AuthorityAnchor, AuthorityTrust};

/// Why a pre-existing authority was not admitted. Every variant is a category,
/// never a record value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryRefusal {
    /// Conservation history exists but no independent anchor was ever
    /// established for this authority: legacy unsigned state, or an anchor that
    /// is gone. Missing recovery data is never treated as first initialization,
    /// and unsigned state cannot be verified, so it is refused.
    UnanchoredHistory,
    /// The anchor or the local key is present but unreadable, malformed or
    /// bound to a different workspace.
    UnverifiableAnchor,
    /// A record carries no signature or a sequence of zero (legacy format).
    UnsignedRecord,
    /// A record's signature does not verify for this authority.
    InvalidSignature,
    /// Two records share a sequence, or the sequence has a hole.
    SequenceGap,
    /// The journal is older than the anchor: truncated or rolled back.
    RolledBack,
}

impl RecoveryRefusal {
    /// A safe local diagnostic: a category and the operator's next step.
    #[must_use]
    pub fn diagnostic(self) -> &'static str {
        match self {
            Self::UnanchoredHistory => {
                "authority state exists for this workspace but was never anchored (legacy or lost anchor)"
            }
            Self::UnverifiableAnchor => "the local authority anchor or key could not be verified",
            Self::UnsignedRecord => "a stored authority record is unsigned (legacy format)",
            Self::InvalidSignature => "a stored authority record failed signature verification",
            Self::SequenceGap => "stored authority records are not a contiguous sequence",
            Self::RolledBack => "stored authority records are older than the independent anchor",
        }
    }
}

/// The deterministic bytes a conservation head is signed over: every field that
/// carries authority, in a fixed order, with the sequence included so a record
/// cannot be replayed at another position.
#[must_use]
pub fn signing_bytes(record: &LedgerConservationRecord) -> Vec<u8> {
    serde_json::to_vec(&(
        &record.token,
        record.total,
        record.available,
        record.consumed,
        record.uses_remaining,
        record.settled,
        record.revoked,
        record.authority_time_ms,
        record.ledger_seq,
    ))
    .expect("a tuple of plain fields serializes")
}

/// The admitted history, ready for the inner replay.
pub struct VerifiedHistory {
    /// Records in ascending `ledger_seq` order (latest per token wins).
    pub records: Vec<LedgerConservationRecord>,
    /// The sequence the ledger continues from.
    pub last_seq: u64,
    /// The watermark the anchor must be advanced to if it is behind.
    pub reached: AuthorityAnchor,
}

/// Verify `records` against `trust`. Requires an established authority: the
/// first-initialization and legacy cases are decided before this is called.
pub fn verify_history(
    mut records: Vec<LedgerConservationRecord>,
    trust: &dyn AuthorityTrust,
) -> Result<VerifiedHistory, RecoveryRefusal> {
    let anchor = trust.anchor();
    for record in &records {
        if record.ledger_seq == 0 {
            return Err(RecoveryRefusal::UnsignedRecord);
        }
        match record.authority_sig.as_deref() {
            None | Some([]) => return Err(RecoveryRefusal::UnsignedRecord),
            Some(signature) if !trust.verify(&signing_bytes(record), signature) => {
                return Err(RecoveryRefusal::InvalidSignature);
            }
            Some(_) => {}
        }
    }
    records.sort_by_key(|record| record.ledger_seq);
    let mut expected = 1u64;
    for record in &records {
        if record.ledger_seq != expected {
            return Err(RecoveryRefusal::SequenceGap);
        }
        expected += 1;
    }
    // Authority time is a high-water mark: the maximum any record carries.
    let time = records
        .iter()
        .map(|record| record.authority_time_ms)
        .max()
        .unwrap_or(0);
    let last_seq = expected - 1;
    if last_seq < anchor.ledger_seq || time < anchor.authority_time_ms {
        return Err(RecoveryRefusal::RolledBack);
    }
    Ok(VerifiedHistory {
        records,
        last_seq,
        reached: AuthorityAnchor {
            ledger_seq: last_seq,
            authority_time_ms: time,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Budget, CapabilityTokenId};
    use crate::domain::ports::AuthorityTrustError;

    /// Deterministic stand-in key: a "signature" is the reversed message. It
    /// exercises the verifier's decisions, not cryptography (the real Ed25519
    /// trust is covered at the startup front door).
    struct FixedTrust(AuthorityAnchor);

    impl AuthorityTrust for FixedTrust {
        fn sign(&self, message: &[u8]) -> Vec<u8> {
            message.iter().rev().copied().collect()
        }
        fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
            self.sign(message) == signature
        }
        fn anchor(&self) -> AuthorityAnchor {
            self.0
        }
        fn advance_anchor(&self, _: AuthorityAnchor) -> Result<(), AuthorityTrustError> {
            Ok(())
        }
    }

    fn record(seq: u64, time: u64, trust: &FixedTrust) -> LedgerConservationRecord {
        let mut record = LedgerConservationRecord {
            token: CapabilityTokenId::root(),
            total: Budget::ZERO,
            available: Budget::ZERO,
            consumed: Budget::ZERO,
            uses_remaining: None,
            settled: false,
            revoked: false,
            authority_time_ms: time,
            ledger_seq: seq,
            authority_sig: None,
        };
        record.authority_sig = Some(trust.sign(&signing_bytes(&record)));
        record
    }

    fn anchor(seq: u64, time: u64) -> FixedTrust {
        FixedTrust(AuthorityAnchor {
            ledger_seq: seq,
            authority_time_ms: time,
        })
    }

    #[test]
    fn a_signed_contiguous_fresh_history_is_admitted_in_sequence_order() {
        let trust = anchor(2, 20);
        let Ok(verified) = verify_history(
            vec![
                record(3, 30, &trust),
                record(1, 10, &trust),
                record(2, 20, &trust),
            ],
            &trust,
        ) else {
            panic!("a signed, contiguous, fresh history must be admitted");
        };
        assert_eq!(verified.last_seq, 3);
        assert_eq!(verified.reached.authority_time_ms, 30);
        assert_eq!(
            verified
                .records
                .iter()
                .map(|r| r.ledger_seq)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn legacy_unsigned_and_forged_records_are_refused() {
        let trust = anchor(0, 0);
        let mut legacy = record(1, 1, &trust);
        legacy.authority_sig = None;
        assert_eq!(
            verify_history(vec![legacy], &trust).err(),
            Some(RecoveryRefusal::UnsignedRecord)
        );
        let mut unsequenced = record(1, 1, &trust);
        unsequenced.ledger_seq = 0;
        assert_eq!(
            verify_history(vec![unsequenced], &trust).err(),
            Some(RecoveryRefusal::UnsignedRecord)
        );
        let mut forged = record(1, 1, &trust);
        forged.available = Budget {
            requests: 9,
            cost_micros: 9,
        };
        assert_eq!(
            verify_history(vec![forged], &trust).err(),
            Some(RecoveryRefusal::InvalidSignature)
        );
    }

    #[test]
    fn holes_duplicates_and_rollback_behind_the_anchor_are_refused() {
        let trust = anchor(3, 30);
        assert_eq!(
            verify_history(vec![record(1, 10, &trust), record(3, 30, &trust)], &trust).err(),
            Some(RecoveryRefusal::SequenceGap)
        );
        assert_eq!(
            verify_history(vec![record(1, 10, &trust), record(1, 10, &trust)], &trust).err(),
            Some(RecoveryRefusal::SequenceGap)
        );
        assert_eq!(
            verify_history(vec![record(1, 10, &trust), record(2, 20, &trust)], &trust).err(),
            Some(RecoveryRefusal::RolledBack),
            "sequence behind the anchor"
        );
        let stale_time = anchor(2, 99);
        assert_eq!(
            verify_history(
                vec![record(1, 10, &stale_time), record(2, 20, &stale_time)],
                &stale_time
            )
            .err(),
            Some(RecoveryRefusal::RolledBack),
            "authority time behind the anchor"
        );
    }

    #[test]
    fn an_empty_history_is_admitted_only_when_nothing_was_ever_anchored() {
        assert!(verify_history(Vec::new(), &anchor(0, 0)).is_ok());
        assert_eq!(
            verify_history(Vec::new(), &anchor(1, 5)).err(),
            Some(RecoveryRefusal::RolledBack)
        );
    }
}
