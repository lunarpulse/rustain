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
