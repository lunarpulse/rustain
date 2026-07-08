//! trybuild runner for the `OwnershipKind::Self_` sealing proof (Story 14.6, AC4, DD2).
//!
//! A lone `compile_fail` is decorative — the `pass`-twin proves the ONLY
//! failure cause is the private-field construction attempt (mirrors the
//! Story 14.3 `trybuild_orchestration.rs` pattern). `self_construct_fails.rs`
//! attempts to build `OwnershipKind::Self_(SealedSelf(()))` from outside the
//! crate and hits E0603 (private tuple-struct field); `self_match_pass.rs`
//! compiles fine, naming the same types and matching (not constructing) the
//! `Self_` arm — proving external code retains full pattern-match ability
//! (required for `#[non_exhaustive]` exhaustiveness) while losing all
//! construction power.
#[test]
fn ac4_self_seal_type_wall() {
    let t = trybuild::TestCases::new();
    t.pass("tests/trybuild/ownership/self_match_pass.rs");
    t.compile_fail("tests/trybuild/ownership/self_construct_fails.rs");
}

/// AI-12.3 post-review party-mode condition (2026-07-02): the "`OwnershipKind`
/// does not derive `Deserialize`" invariant previously had NO executable guard
/// anywhere in the suite (`ac4_domain_ownership_kind_is_not_deserializable` in
/// `conformance_multi_agent_security.rs` was vacuous — `let _ = OwnershipKind::Owned;`).
/// This compile_fail proves it for real: re-adding `#[derive(Deserialize)]` to
/// `OwnershipKind` makes `domain_kind_not_deserializable.rs` start compiling,
/// flipping this test RED.
#[test]
fn ac4_domain_kind_not_deserializable_type_wall() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/trybuild/ownership/domain_kind_not_deserializable.rs");
}
