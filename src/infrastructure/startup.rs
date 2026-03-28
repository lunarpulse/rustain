use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;

use crate::adapters::cli::commands::Cli;
use crate::adapters::tui::terminal;
use crate::domain::events::AppEvent;
use crate::infrastructure::runtime::event_loop;
use crate::infrastructure::{config, logging, signals};

/// Ordered startup sequence.
/// 1. Parse CLI args
/// 2. Load config
/// 3. Initialize logging
/// 4. Install panic hook
/// 5. Setup terminal
/// 6. Enter event loop
pub async fn run() -> Result<()> {
    // 1. Parse CLI args
    let cli = Cli::parse();

    // 2. Load config
    let app_config = config::load();

    // 3. Initialize logging
    logging::init(&cli.log_level)?;
    tracing::info!("Starting rustain...");

    // 4. Install panic hook
    signals::install_panic_hook();

    // 5. Create domain event channel
    let (domain_tx, mut domain_rx) = mpsc::unbounded_channel::<AppEvent>();

    // Store shutdown sender for signal handlers
    signals::set_shutdown_sender(domain_tx.clone());
    signals::install_signal_handlers().await;

    // 6. Setup terminal
    let mut tui = terminal::setup()?;

    // 7. Run event loop
    let result = event_loop::run(&mut tui, &mut domain_rx, &app_config).await;

    // 8. Teardown terminal (always, even on error)
    if let Err(e) = terminal::teardown() {
        tracing::error!("Terminal teardown failed: {}", e);
    }
    tracing::info!("Rustain shutdown complete.");

    result
}
