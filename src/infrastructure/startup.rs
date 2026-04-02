use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;

use crate::adapters::cli::commands::Cli;
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::project_context_loader::ProjectContextLoader;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::ports::{PersonaPort, ProviderPort, SecurityPort, ToolSetPort};
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

    // 6. Create domain event channel (before security adapter, which needs the sender)
    let (domain_tx, mut domain_rx) = mpsc::unbounded_channel::<AppEvent>();

    // 5b. Construct security and toolset adapters
    let workspace_path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;
    let session_id = nanoid::nanoid!();
    let security_adapter = SecurityAdapter::new(workspace_path.clone(), domain_tx.clone());
    // Load AlwaysAllow rules from .claude/settings.json
    security_adapter.init_allowed_rules().await;
    let security: Arc<dyn SecurityPort> = Arc::new(security_adapter);
    let tools: Arc<dyn ToolSetPort> =
        Arc::new(ToolSetAdapter::new(workspace_path.clone(), session_id));

    // 5c. Discover and load project context
    let context_loader = ProjectContextLoader::new(workspace_path.clone());
    let project_context = context_loader.discover().unwrap_or_else(|e| {
        tracing::warn!("Failed to discover project context: {}", e);
        crate::domain::models::project_context::ProjectContext::empty()
    });
    let persona_adapter = PersonaAdapter::new(project_context);

    // Emit context loading notices (Phase D: Task 7)
    if persona_adapter.has_context() {
        let paths: Vec<String> = persona_adapter
            .file_paths()
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().to_string())
            .collect();
        let msg = format!(
            "Project context: {} ({} chars)",
            paths.join(", "),
            persona_adapter.total_chars(),
        );
        tracing::info!("{}", msg);
        let _ = domain_tx.send(AppEvent::SystemNotice(NoticeLevel::Info, msg));

        if persona_adapter.is_truncated() {
            let warn_msg = format!(
                "Project context truncated: some files omitted (budget: {} chars)",
                crate::domain::models::project_context::CONTEXT_BUDGET_CHARS,
            );
            tracing::warn!("{}", warn_msg);
            let _ = domain_tx.send(AppEvent::SystemNotice(NoticeLevel::Warning, warn_msg));
        }
    }

    let persona: Arc<dyn PersonaPort> = Arc::new(persona_adapter);

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
        persona,
        workspace_path,
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
