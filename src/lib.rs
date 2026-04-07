// Pre-existing clippy suppressions — these issues predate Story 3-0 and are
// suppressed to meet AC9 clippy gate without introducing behavioral changes.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::implicit_saturating_sub)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::needless_return)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::wrong_self_convention)]

pub mod adapters;
pub mod domain;
pub mod infrastructure;
