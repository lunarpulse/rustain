use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;

use crate::adapters::cli::commands::{Cli, Command};
use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::project_context_loader::ProjectContextLoader;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::ports::{PersonaPort, ProviderPort, SecurityPort, StoragePort, ToolSetPort};
use crate::infrastructure::runtime::event_loop;
use crate::infrastructure::{config, logging, paths, signals};

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

    // 3. Initialize logging (hold guard to flush on drop)
    let _log_guard = logging::init(&cli.log_level)?;
    tracing::info!("Starting rustain...");

    // 4. Install panic hook
    signals::install_panic_hook();

    // 4a. Intercept init subcommand BEFORE provider construction and terminal setup
    if let Some(Command::Init) = cli.command {
        return crate::adapters::cli::init::run_init().await;
    }

    // 5. Apply model override from env (before provider + event loop, so status bar sees it)
    let mut app_config = app_config;
    if let Some(model_override) = std::env::var("ANTHROPIC_DEFAULT_SONNET_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        tracing::info!("Model override from ANTHROPIC_DEFAULT_SONNET_MODEL: {}", model_override);
        app_config.model = model_override;
    }

    // 5a. Construct provider adapter
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

    // 5d. Construct storage adapter and load last session
    let sessions_dir = paths::sessions_dir(&workspace_path);
    let storage = FileSystemStorage::new(sessions_dir);
    if let Err(e) = storage.ensure_dir().await {
        tracing::warn!("Failed to create sessions directory: {}", e);
        let _ = domain_tx.send(AppEvent::SystemNotice(
            NoticeLevel::Warning,
            format!("Session persistence unavailable: {}", e),
        ));
    }

    // Session restoration: --new skips restore, --session <id> loads specific session
    // recovery_prompt: Some((title, token_count)) if crash detected
    let (restored_conversation, recovery_prompt) = if cli.new {
        tracing::info!("Starting new session (--new flag)");
        (None, None)
    } else if let Some(ref session_id) = cli.session {
        // Validate --session <id> exists BEFORE terminal setup
        match storage.load_conversation_with_exit(session_id).await {
            Ok(Some((conv, _clean_exit))) => {
                tracing::info!(
                    "Restored specific session: {} ({} messages)",
                    conv.title.as_str(),
                    conv.messages.len()
                );
                // Don't show recovery prompt for explicit --session restore
                (Some(conv), None)
            }
            Ok(None) => {
                eprintln!("Error: session '{}' not found", session_id);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("Error: failed to load session '{}': {}", session_id, e);
                std::process::exit(1);
            }
        }
    } else {
        // Default: load most recent session with crash detection
        match storage.list_conversations().await {
            Ok(summaries) if !summaries.is_empty() => {
                let most_recent = &summaries[0];
                match storage.load_conversation_with_exit(&most_recent.id).await {
                    Ok(Some((conv, clean_exit))) => {
                        tracing::info!(
                            "Restored session: {} ({} messages, clean_exit={})",
                            conv.title.as_str(),
                            conv.messages.len(),
                            clean_exit,
                        );
                        let recovery = if !clean_exit && !conv.messages.is_empty() {
                            // Crash detected: prepare recovery prompt info
                            let title = if conv.title.is_empty() {
                                "Untitled".to_string()
                            } else {
                                conv.title.clone()
                            };
                            let token_count = conv
                                .messages
                                .last()
                                .and_then(|m| m.token_count)
                                .unwrap_or(0);
                            Some((title, token_count))
                        } else {
                            None
                        };
                        (Some(conv), recovery)
                    }
                    Ok(None) => {
                        tracing::warn!("Session file listed but not loadable");
                        (None, None)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to load last session: {}", e);
                        (None, None)
                    }
                }
            }
            _ => (None, None),
        }
    };

    let storage = Arc::new(storage);
    let storage_port: Arc<dyn StoragePort> = storage.clone();

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
        storage_port,
        storage,
        workspace_path,
        restored_conversation,
        recovery_prompt,
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
///
/// Auth precedence (CC-compatible): `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_API_KEY`.
/// - `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer {token}` (gateways/proxies)
/// - `ANTHROPIC_API_KEY` → `X-Api-Key: {key}` (direct Anthropic)
fn build_provider(config: &crate::domain::models::AppConfig) -> Result<Arc<dyn ProviderPort>> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::AuthMode;

        // 1. Resolve auth: ANTHROPIC_AUTH_TOKEN > ANTHROPIC_API_KEY (CC precedence)
        let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok().filter(|s| !s.is_empty());
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty());

        if auth_token.is_some() && api_key.is_some() {
            tracing::warn!("Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY are set; using ANTHROPIC_AUTH_TOKEN (Bearer auth)");
        }

        let auth_mode = if let Some(token) = auth_token {
            tracing::info!("Using ANTHROPIC_AUTH_TOKEN (Bearer auth)");
            AuthMode::BearerToken(token)
        } else if let Some(key) = api_key {
            tracing::info!("Using ANTHROPIC_API_KEY (X-Api-Key auth)");
            AuthMode::ApiKey(key)
        } else {
            anyhow::bail!(
                "No API key found.\n\n\
                 Set one of:\n\
                 \n\
                 export ANTHROPIC_API_KEY=sk-ant-...       # Direct Anthropic\n\
                 export ANTHROPIC_AUTH_TOKEN=your-key       # Anthropic-compatible gateway\n\
                 \n\
                 Get your API key at: https://console.anthropic.com/"
            );
        };

        // 2. Resolve base URL (filter empty to preserve default)
        let base_url = std::env::var("ANTHROPIC_BASE_URL").ok().filter(|s| !s.is_empty());
        if let Some(ref url) = base_url {
            tracing::info!("Custom base URL: {}", url);
        }

        let adapter = crate::adapters::anthropic::AnthropicAdapter::new(
            auth_mode,
            config.model.clone(),
            base_url,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create Anthropic adapter: {}", e))?;
        tracing::info!("Anthropic provider initialized (model: {})", config.model);
        Ok(Arc::new(adapter))
    }

    #[cfg(not(feature = "anthropic"))]
    {
        let _ = config;
        tracing::warn!("No provider feature enabled — using NoOp provider");
        Ok(Arc::new(crate::adapters::noop::NoOpProvider))
    }
}
