// Pre-existing clippy suppressions — these predate Story 3-0.
// Sync with lib.rs — both files must carry identical suppressions.
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
#![allow(clippy::doc_overindented_list_items)] // AI-12.1: pure doc-list formatting, same class as doc_lazy_continuation
#![allow(clippy::field_reassign_with_default)] // AI-12.1: readable `let mut x = T::default(); x.f = …` in test setup
#![allow(clippy::large_enum_variant)] // AI-12.1: daemon protocol/handler enums — boxing changes wire/layout, deferred design call
#![allow(clippy::items_after_test_module)] // AI-12.1: harmless item ordering after `#[cfg(test)] mod tests`
#![allow(clippy::manual_async_fn)] // AI-12.1: explicit `impl Future` kept where lifetime/Send bounds are clearer
#![allow(clippy::let_and_return)] // AI-12.1: named-then-return kept for readability in a few sites
#![allow(clippy::let_unit_value)] // AI-12.1: `let _x = …;` binding unit results, intentional
#![allow(clippy::collapsible_match)] // AI-12.1: same family as collapsible_if/else_if (already allowed)
#![allow(clippy::empty_line_after_doc_comments)] // AI-12.1: doc formatting, same class as doc_lazy_continuation
#![allow(clippy::type_complexity)] // AI-12.1: complex callback/closure tuple types (cron scheduler) — same class as too_many_arguments

mod adapters;
mod domain;
mod infrastructure;

#[tokio::main]
async fn main() {
    if let Err(e) = infrastructure::startup::run().await {
        // Subcommand errors (init, doctor) already printed their own output.
        // Only print the error for non-subcommand failures to avoid duplicate output.
        if e.downcast_ref::<infrastructure::startup::SubcommandExit>()
            .is_none()
        {
            eprintln!("{e}");
        }
        std::process::exit(1);
    }
}
