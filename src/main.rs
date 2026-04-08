// Pre-existing clippy suppressions — these predate Story 3-0.
// Sync with lib.rs — both files must carry identical suppressions.
#![allow(clippy::too_many_arguments)] // TODO(epic-4): large render fns — refactor into smaller units
#![allow(clippy::implicit_saturating_sub)] // TODO(epic-4): audit saturating_sub usage
#![allow(clippy::redundant_closure)] // TODO(epic-4): clean up trivial closures
#![allow(clippy::needless_return)] // TODO(epic-4): remove explicit returns
#![allow(clippy::derivable_impls)] // TODO(epic-4): derive Default where possible
#![allow(clippy::collapsible_if)] // TODO(epic-4): flatten nested ifs
#![allow(clippy::collapsible_else_if)] // TODO(epic-4): flatten nested else-ifs
#![allow(clippy::doc_lazy_continuation)] // TODO(epic-4): fix doc comment formatting
#![allow(clippy::wrong_self_convention)] // TODO(epic-4): audit is_*/has_* methods

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
