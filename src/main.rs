mod adapters;
mod domain;
mod infrastructure;

#[tokio::main]
async fn main() {
    if let Err(e) = infrastructure::startup::run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
