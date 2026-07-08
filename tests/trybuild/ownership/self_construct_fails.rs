// AC4 seal COMPILE_FAIL (Story 14.6, DD2). `OwnershipKind::Self_` is a tuple
// variant whose sole field is `SealedSelf`, itself a tuple struct with a
// private `()` field defined in `subagent_view`. From outside the crate
// (this trybuild test compiles as a separate crate) there is no way to
// produce a `SealedSelf` value, so `OwnershipKind::Self_(SealedSelf(()))`
// cannot be written here — E0603 (private field, module-private struct
// field is inaccessible). The sibling `self_match_pass.rs` compiles,
// proving the ONLY failure cause here is the private-field construction,
// not e.g. the enum itself being inaccessible.
use rustain::domain::models::subagent_view::{OwnershipKind, SealedSelf};

fn main() {
    // Attempting to forge the privileged root tier from outside the crate.
    let _forged = OwnershipKind::Self_(SealedSelf(()));
}
