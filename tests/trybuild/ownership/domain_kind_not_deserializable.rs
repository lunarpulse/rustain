// AC4 COMPILE_FAIL (Story 14.6, DD2, AI-12.3 post-review closure). `OwnershipKind`
// deliberately does NOT derive `Deserialize` — the domain type must never ride a
// wire/deserialization boundary that could forge `Self_`. This is a real compile-time
// guard for that property: if a future refactor re-adds `#[derive(Deserialize)]` to
// `OwnershipKind`, this file starts compiling and the trybuild suite goes RED.
use rustain::domain::models::subagent_view::OwnershipKind;

fn main() {
    // `OwnershipKind` has no `Deserialize` impl — this call must fail to compile
    // with "the trait bound `OwnershipKind: serde::Deserialize<'_>` is not satisfied".
    let _forged: OwnershipKind = serde_json::from_str(r#""self_""#).unwrap();
}
