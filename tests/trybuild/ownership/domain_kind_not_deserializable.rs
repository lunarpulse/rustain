// AC4 COMPILE-PASS NEGATIVE ASSERTION (Story 14.6, DD2, AI-12.3 post-review
// closure). `OwnershipKind` deliberately does NOT derive `Deserialize` — the
// domain type must never ride a deserialization boundary that could forge
// `Self_`.
use rustain::domain::models::subagent_view::OwnershipKind;
use serde::de::DeserializeOwned;

trait AmbiguousIfDeserialize<Marker> {
    fn assert_not_deserializable() {}
}

impl<T: ?Sized> AmbiguousIfDeserialize<()> for T {}
impl<T: ?Sized + DeserializeOwned> AmbiguousIfDeserialize<u8> for T {}

fn main() {
    // With no `Deserialize` impl, `Marker` resolves uniquely to `()`. Re-adding
    // one makes both impls applicable and this call fails with an ambiguity.
    <OwnershipKind as AmbiguousIfDeserialize<_>>::assert_not_deserializable();
}
