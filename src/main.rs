mod app;
mod core;
mod tui;
mod types;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Route tracing to file — stdout is owned by ratatui, writing to it corrupts the TUI
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".rustain");
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::File::create(log_dir.join("rustain.log"))?;

    tracing_subscriber::fmt()
        .with_writer(log_file)
        .with_env_filter(EnvFilter::from_default_env().add_directive("rustain=info".parse()?))
        .init();

    tracing::info!("Starting rustain...");

    // Check for API key early — don't crash inside the TUI
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("Warning: ANTHROPIC_API_KEY not set. Set it to use Claude models.");
        eprintln!("  export ANTHROPIC_API_KEY=sk-ant-...");
        eprintln!();
        // Still launch — the UI will show the error in the status bar
    }

    // Initialize and run the TUI application
    // Panic hook for terminal restoration is set inside App::run()
    let mut app = app::App::new().await?;
    app.run().await?;

    Ok(())
}
