// Pre-existing clippy suppressions — these predate Story 3-0.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::implicit_saturating_sub)]
#![allow(clippy::redundant_closure)]
#![allow(clippy::needless_return)]
#![allow(clippy::derivable_impls)]

#![allow(clippy::collapsible_if)]
#![allow(clippy::collapsible_else_if)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::wrong_self_convention)]

mod adapters;
mod domain;
mod infrastructure;

#[tokio::main]
async fn main() {
    if let Err(e) = infrastructure::startup::run().await {
        // Subcommand errors (init, doctor) already printed their own output.
        // Only print the error for non-subcommand failures to avoid duplicate output.
        if e.downcast_ref::<infrastructure::startup::SubcommandExit>().is_none() {
            eprintln!("{e}");
        }
        std::process::exit(1);
    }
}
