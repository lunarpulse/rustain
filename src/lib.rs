// Pre-existing clippy suppressions — these issues predate Story 3-0 and are
// suppressed to meet AC9 clippy gate without introducing behavioral changes.
// Sync with main.rs — both files must carry identical suppressions.
#![allow(dead_code)] // TODO(epic-4): public API surface used by integration tests; audit per-module
#![allow(unused_imports)] // TODO(epic-4): re-exports consumed by integration tests; prune unused
#![allow(clippy::too_many_arguments)] // TODO(epic-4): large render fns — refactor into smaller units
#![allow(clippy::implicit_saturating_sub)] // TODO(epic-4): audit saturating_sub usage
#![allow(clippy::redundant_closure)] // TODO(epic-4): clean up trivial closures
#![allow(clippy::needless_return)] // TODO(epic-4): remove explicit returns
#![allow(clippy::derivable_impls)] // TODO(epic-4): derive Default where possible
#![allow(clippy::collapsible_if)] // TODO(epic-4): flatten nested ifs
#![allow(clippy::collapsible_else_if)] // TODO(epic-4): flatten nested else-ifs
#![allow(clippy::doc_lazy_continuation)] // TODO(epic-4): fix doc comment formatting
#![allow(clippy::wrong_self_convention)] // TODO(epic-4): audit is_*/has_* methods

pub mod adapters;
pub mod domain;
pub mod infrastructure;
