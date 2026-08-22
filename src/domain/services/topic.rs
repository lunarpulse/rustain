//! Topic decision cores (Story 18.4a, FR150 / FR150-a / NFR71).
//!
//! Pure, synchronous, effect-free (Decision-Core Pattern, Story 18.0). The
//! effects — journaling a divergent head, gossiping to admitted members,
//! assembling a bundle — belong to the shells in `adapters::rap::topic` and
//! `adapters::peer_context`.
//!
//! # This is git, not a CRDT
//!
//! `epics.md` Epic-18 shared-context invariant, verbatim. Nothing here merges,
//! reconciles, votes or totally orders across senders. Each sender's feed is
//! already totally ordered by `sequence`; assembly across senders is
//! **order-insensitive** instead, which is a weaker and cheaper property than
//! convergence and the only one FR150 needs.

use std::collections::BTreeSet;

use crate::domain::models::{ContextRef, HeadVerdict, TopicHead};

/// Handles one frame may carry.
///
/// A frame body is already capped by `MAX_PEER_MESSAGE_BYTES`, but a byte cap
/// bounds one frame's *size*, not the number of entries a receiver must sort,
/// dedup and hold. Sixty-four is far past any legitimate turn's evidence set and
/// finite for the other case.
pub const MAX_HANDLES_PER_FRAME: usize = 64;

/// Deduplicate and totally order a handle set for assembly (AC1, NFR71).
///
/// # The property
///
/// `assemble_handles(perm(xs)) == assemble_handles(xs)` for **every** permutation
/// `perm`. That holds because the sort key
/// ([`ContextRef::order_key`]) is *total* over distinct handles and the dedup
/// runs **after** the sort, so which member of a colliding pair survives is a
/// property of the values, never of arrival.
///
/// # Why dedup keeps the lowest key rather than the newest
///
/// Two peers may assert the same `content_hash`. Keeping "whichever arrived
/// last" is arrival-dependent by construction; keeping "whichever the total
/// order names first" is not. ⛔ The survivor is deliberately **not** chosen by
/// `not_after` or by issuer preference: a peer must not be able to evict another
/// peer's identical assertion by re-issuing it with a later expiry.
#[must_use]
pub fn assemble_handles(mut handles: Vec<ContextRef>) -> Vec<ContextRef> {
    handles.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    handles.retain(|handle| seen.insert(handle.content_hash.as_bytes().to_vec()));
    handles
}

/// Drop handles that are no longer live at `now_unix`.
///
/// ⛔ Separate from [`assemble_handles`] on purpose: expiry is time-dependent and
/// ordering is not, so folding them together would make the permutation keystone
/// depend on a clock.
#[must_use]
pub fn live_handles(handles: Vec<ContextRef>, now_unix: i64) -> Vec<ContextRef> {
    handles
        .into_iter()
        .filter(|handle| handle.is_live(now_unix))
        .collect()
}

/// Compare an advertised head against the head this host already holds (FR150-a).
///
/// ⛔ **Detection and recording only.** This returns a verdict; it never blocks,
/// punishes or excludes, and no caller may describe [`HeadVerdict::Equivocated`]
/// as authenticated or as a proof. This host observed two claims and can say
/// only that they disagree.
#[must_use]
pub fn compare_head(held: Option<&[u8]>, advertised: &TopicHead) -> HeadVerdict {
    if !advertised.is_wellformed() {
        return HeadVerdict::Malformed;
    }
    match held {
        None => HeadVerdict::First,
        Some(held) if held == advertised.head.as_slice() => HeadVerdict::Agrees,
        Some(held) => HeadVerdict::Equivocated {
            held: held.to_vec(),
            advertised: advertised.head.clone(),
        },
    }
}

/// Whether `peer` may be advertised the hashes for this Topic (AC7).
///
/// COLLAB `:352`: *"membership gates advertisement."* Membership is a
/// **capability grant** (D2), which here is the set of identities the local
/// authority resolved for the Topic — ⛔ never a new allowlist file.
/// `architecture-review-18-4c-b-2026-08-17.md:90` names why: `ADR-18-4-01` D4
/// exists because that hazard already occurred once, and a third admission model
/// would be the third member of that family.
#[must_use]
pub fn advertises_to(
    members: &BTreeSet<crate::domain::models::PeerId>,
    peer: &crate::domain::models::PeerId,
) -> bool {
    members.contains(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AgentId, ArtifactId, ContentHash, ContextRefProvenance, ContextSummary, CorrelationId,
        PeerId,
    };

    fn peer(seed: u8) -> PeerId {
        PeerId::from_public_key(&[seed; 32]).expect("valid key length")
    }

    fn handle(content: u8, issuer: u8) -> ContextRef {
        let hash = ContentHash::from_bytes([content; 32]);
        ContextRef {
            artifact: ArtifactId::from(hash),
            content_hash: hash,
            producer: AgentId::parse("producer").expect("valid agent id"),
            issuer: peer(issuer),
            summary: ContextSummary::new(format!("summary {content}")).expect("bounded"),
            provenance: ContextRefProvenance::Authored,
            not_after: 10_000,
        }
    }

    #[test]
    fn assembly_is_identical_under_every_permutation() {
        let base = vec![handle(1, 7), handle(2, 8), handle(1, 9)];
        let expected = assemble_handles(base.clone());
        // ⚑ Positive control: the set is non-trivial — two distinct sources and
        // one dedup collision — so this is not comparing two empty vectors.
        assert_eq!(expected.len(), 2, "the collision must actually collide");
        for permutation in [
            vec![base[2].clone(), base[0].clone(), base[1].clone()],
            vec![base[1].clone(), base[2].clone(), base[0].clone()],
            vec![base[2].clone(), base[1].clone(), base[0].clone()],
        ] {
            assert_eq!(assemble_handles(permutation), expected);
        }
    }

    #[test]
    fn a_dedup_collision_keeps_the_total_order_minimum() {
        // Mutant: keep last-seen instead of first-seen → the survivor's issuer
        // flips to peer(9) and this fails.
        let survivors = assemble_handles(vec![handle(1, 9), handle(1, 7)]);
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].issuer, peer(7));
    }

    #[test]
    fn a_second_head_at_one_sequence_is_equivocation_and_a_matching_one_is_not() {
        let advert = TopicHead {
            topic: CorrelationId::new("t"),
            issuer: peer(7),
            sequence: 3,
            head: vec![9u8; 32],
        };
        assert_eq!(compare_head(None, &advert), HeadVerdict::First);
        assert_eq!(compare_head(Some(&[9u8; 32]), &advert), HeadVerdict::Agrees);
        assert_eq!(
            compare_head(Some(&[1u8; 32]), &advert),
            HeadVerdict::Equivocated {
                held: vec![1u8; 32],
                advertised: vec![9u8; 32],
            }
        );
        let malformed = TopicHead {
            head: vec![9u8; 31],
            ..advert
        };
        assert_eq!(
            compare_head(Some(&[1u8; 32]), &malformed),
            HeadVerdict::Malformed
        );
    }
}
