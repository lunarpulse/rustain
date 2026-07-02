// AC4 seal PASS-twin (Story 14.6, DD2). External code CAN name the
// `OwnershipKind`/`SealedSelf` types and match on the `Self_` variant
// (required so `#[non_exhaustive]` callers can still handle every arm) —
// it simply cannot CONSTRUCT the `SealedSelf` payload. This proves the
// compile_fail in `self_construct_fails.rs` is caused ONLY by the private
// field, not by the types being unreachable from outside the crate.
use rustain::domain::models::subagent_view::OwnershipKind;

fn is_self(kind: &OwnershipKind) -> bool {
    matches!(kind, OwnershipKind::Self_(_))
}

fn main() {
    let owned = OwnershipKind::Owned;
    let peer = OwnershipKind::Peer;
    assert!(!is_self(&owned));
    assert!(!is_self(&peer));
}
