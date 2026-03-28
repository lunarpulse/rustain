mod adapters;
mod domain;
mod infrastructure;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    infrastructure::startup::run().await
}
