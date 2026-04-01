use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;

use crate::adapters::cli::commands::Cli;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::domain::events::AppEvent;
use crate::domain::ports::{ProviderPort, SecurityPort, ToolSetPort};
use crate::infrastructure::runtime::event_loop;
use crate::infrastructure::{config, logging, signals};

/// Ordered startup sequence.
/// 1. Parse CLI args
/// 2. Load config
/// 3. Initialize logging
/// 4. Install panic hook
/// 5. Construct provider
/// 6. Setup terminal
/// 7. Enter event loop
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

    // 5. Construct provider adapter
    let provider: Arc<dyn ProviderPort> = build_provider(&app_config)?;

    // 5b. Construct security and toolset adapters
    let workspace_path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;
    let session_id = nanoid::nanoid!();
    let security: Arc<dyn SecurityPort> = Arc::new(SecurityAdapter::new(workspace_path.clone()));
    let tools: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(workspace_path, session_id));

    // 6. Create domain event channel
    let (domain_tx, mut domain_rx) = mpsc::unbounded_channel::<AppEvent>();

    // Store shutdown sender for signal handlers
    signals::set_shutdown_sender(domain_tx.clone());
    signals::install_signal_handlers().await;

    // 7. Setup terminal
    let mut tui = terminal::setup()?;

    // 8. Run event loop
    let result = event_loop::run(
        &mut tui,
        &mut domain_rx,
        domain_tx,
        &app_config,
        provider,
        security,
        tools,
    )
    .await;

    // 9. Teardown terminal (always, even on error)
    if let Err(e) = terminal::teardown() {
        tracing::error!("Terminal teardown failed: {}", e);
    }
    tracing::info!("Rustain shutdown complete.");

    result
}

/// Build the provider adapter based on configuration and environment.
fn build_provider(config: &crate::domain::models::AppConfig) -> Result<Arc<dyn ProviderPort>> {
    #[cfg(feature = "anthropic")]
    {
        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(api_key) => {
                let adapter = crate::adapters::anthropic::AnthropicAdapter::new(
                    api_key,
                    config.model.clone(),
                    None,
                )
                .map_err(|e| anyhow::anyhow!("Failed to create Anthropic adapter: {}", e))?;
                tracing::info!("Anthropic provider initialized (model: {})", config.model);
                Ok(Arc::new(adapter))
            }
            Err(_) => {
                eprintln!(
                    "Error: ANTHROPIC_API_KEY not set.\n\n\
                     To use rustain, set your Anthropic API key:\n\
                     \n\
                     export ANTHROPIC_API_KEY=sk-ant-...\n\
                     \n\
                     Get your API key at: https://console.anthropic.com/"
                );
                std::process::exit(3);
            }
        }
    }

    #[cfg(not(feature = "anthropic"))]
    {
        let _ = config;
        tracing::warn!("No provider feature enabled — using NoOp provider");
        Ok(Arc::new(crate::adapters::noop::NoOpProvider))
    }
}
