use std::sync::Arc;

use anyhow::Result;

use crate::adapters::approval_persistence_toml::ApprovalPersistenceToml;
use crate::adapters::cli::commands::{
    AuthAction, Cli, Command, ConfigAction, DaemonAction, ProfileAction,
};
use crate::adapters::cli::session::SessionAction;
use crate::adapters::filesystem::FileSystemStorage;
use crate::adapters::ledger::FileUsageLedger;
use crate::adapters::persona_adapter::PersonaAdapter;
use crate::adapters::project_context_loader::ProjectContextLoader;
use crate::adapters::security_adapter::SecurityAdapter;
use crate::adapters::skill_activation::SkillActivator;
use crate::adapters::skill_registry::SkillRegistry;
use crate::adapters::toolset_adapter::ToolSetAdapter;
use crate::adapters::tui::terminal;
use crate::adapters::workspace_registry::FileWorkspaceRegistry;
use crate::domain::errors::ProviderError;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::models::{AutoApprovePolicy, PermissionMode, ProviderConfig, SandboxPolicy};
use crate::domain::ports::{
    ClipboardPort, PersonaPort, ProfileResolver, SecurityPort, StoragePort, StreamingProvider,
    ToolSetPort, WorkspaceRegistryReaderPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::plan_manager::PlanManager;
use crate::domain::services::plan_mode_injector::{DefaultPlanInjector, PlanModeInjector};
use crate::infrastructure::runtime::app_state::AppState;
use crate::infrastructure::runtime::event_loop;
use crate::infrastructure::{config, logging, paths, permission_rules, signals};

/// Error type for subcommand exits where output was already printed.
/// Carries the exit code so destructive/scriptable subcommands can return
/// distinct non-zero codes (Story 13.5b).
#[derive(Debug)]
pub struct SubcommandExit(pub i32);

impl SubcommandExit {
    /// Generic non-zero exit code used by most subcommand failures.
    pub const GENERIC: i32 = 1;
}

impl std::fmt::Display for SubcommandExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "subcommand exited with code {}", self.0)
    }
}

impl std::error::Error for SubcommandExit {}

fn ensure_a2a_feature_enabled(
    peers: &[crate::domain::models::A2aPeerSpec],
    serve_requested: bool,
) -> Result<()> {
    #[cfg(feature = "a2a")]
    {
        let _ = (peers, serve_requested);
        Ok(())
    }
    #[cfg(not(feature = "a2a"))]
    {
        if peers.is_empty() && !serve_requested {
            Ok(())
        } else {
            anyhow::bail!(
                "A2A peers or serving are configured, but this build has the `a2a` feature disabled"
            )
        }
    }
}

/// Decision-Core (Story 18.0 pattern): effect-free, value-returning.
///
/// Command intercepts run in source order, so a combination that both branches
/// would claim silently drops one of the two modes. Story 18.1a refused every
/// combination for exactly that reason. Story 18.1b makes ONE of them real:
/// `daemon` composes the A2A listener as a sibling task inside its own
/// lifecycle, so the pair is genuinely served, not silently halved. Every other
/// subcommand is still refused.
fn evaluate_serve_a2a_combination(
    serve_requested: bool,
    subcommand: Option<&Command>,
) -> Result<()> {
    if !serve_requested {
        return Ok(());
    }
    match subcommand {
        None
        | Some(Command::Daemon {
            action: DaemonAction::Start { .. } | DaemonAction::Run,
        }) => Ok(()),
        Some(_) => anyhow::bail!(
            "--serve-a2a can run standalone or combined with `rustain daemon`, but not with \
             this subcommand: the command intercepts run in source order, so one of the two \
             modes would be silently discarded"
        ),
    }
}

#[cfg(test)]
mod a2a_feature_tests {
    use super::ensure_a2a_feature_enabled;
    use crate::domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl};

    fn configured_peer() -> A2aPeerSpec {
        A2aPeerSpec {
            id: "peer".to_owned(),
            url: RedactedUrl::from("https://peer.example"),
            pinned_key: None,
            source: A2aPeerSource::Workspace,
        }
    }

    #[test]
    fn configured_peer_matches_the_compile_time_feature_policy() {
        let peer = configured_peer();
        let result = ensure_a2a_feature_enabled(std::slice::from_ref(&peer), false);
        #[cfg(feature = "a2a")]
        assert!(result.is_ok());
        #[cfg(not(feature = "a2a"))]
        assert!(
            result
                .expect_err("feature-off A2A config must fail loud")
                .to_string()
                .contains("feature disabled")
        );
        assert!(ensure_a2a_feature_enabled(&[], false).is_ok());
        let serve_result = ensure_a2a_feature_enabled(&[], true);
        #[cfg(feature = "a2a")]
        assert!(serve_result.is_ok());
        #[cfg(not(feature = "a2a"))]
        assert!(serve_result.is_err());
    }

    #[test]
    fn serve_a2a_pairs_with_daemon_and_refuses_to_shadow_anything_else() {
        use super::{Command, evaluate_serve_a2a_combination};
        use crate::adapters::cli::commands::DaemonAction;

        let start = Command::Daemon {
            action: DaemonAction::Start { foreground: true },
        };
        let run = Command::Daemon {
            action: DaemonAction::Run,
        };
        assert!(evaluate_serve_a2a_combination(false, None).is_ok());
        assert!(evaluate_serve_a2a_combination(true, None).is_ok());
        assert!(evaluate_serve_a2a_combination(false, Some(&start)).is_ok());
        // These are the only daemon actions that compose the sibling listener.
        assert!(evaluate_serve_a2a_combination(true, Some(&start)).is_ok());
        assert!(evaluate_serve_a2a_combination(true, Some(&run)).is_ok());
        for action in [
            DaemonAction::Stop,
            DaemonAction::Status { json: false },
            DaemonAction::Attach { plain: false },
            DaemonAction::Install {
                print: false,
                system: false,
            },
            DaemonAction::Uninstall { system: false },
        ] {
            let daemon = Command::Daemon { action };
            assert!(
                evaluate_serve_a2a_combination(true, Some(&daemon))
                    .expect_err("a daemon action that does not start the listener must fail loud")
                    .to_string()
                    .contains("not with this subcommand")
            );
        }
        assert!(
            evaluate_serve_a2a_combination(true, Some(&Command::Init))
                .expect_err("a combination that drops one mode must fail loud")
                .to_string()
                .contains("not with this subcommand")
        );
    }
}

/// Ordered startup sequence.
/// 1. Parse CLI args
/// 2. Initialize logging (so config warnings are captured)
/// 3. Load config
/// 4. Install panic hook
/// 5. Construct provider
/// 6. Setup terminal
/// 7. Enter event loop
pub async fn run() -> Result<()> {
    // Story 13.7 AC1 — install the startup panic hook BEFORE everything else
    // (even CLI parsing) so any panic during arg parsing, logging init, `-c`
    // override parsing, or config loading is captured to ~/.rustain/panic.log
    // with a user-friendly stderr message. The TUI hook (install_panic_hook)
    // installs later (~line 245) and superseds this one via AtomicBool.
    signals::install_startup_panic_hook();
    // 1. Parse CLI args — augment with rich long_version (FR109)
    let cli = {
        use clap::{CommandFactory, FromArgMatches};
        // Leak the version string to get a 'static str required by clap's API.
        // This runs once at startup; the allocation is intentionally permanent.
        let long_ver: &'static str =
            Box::leak(crate::adapters::tui::version_info::version_string().into_boxed_str());
        let cmd = Cli::command().long_version(long_ver);
        let matches = cmd.get_matches();
        Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
    };

    // 2. Initialize logging BEFORE config load so parse warnings are captured
    let _log_guard = logging::init(cli.log_level.as_deref().unwrap_or("info"))?;
    tracing::info!("Starting rustain...");

    let cli_config_overrides = if cli.config_override.is_empty() {
        None
    } else {
        match config::parse_config_overrides(&cli.config_override) {
            Ok(value) => Some(value),
            Err(e) => {
                eprintln!("{e}");
                return Err(anyhow::anyhow!("Invalid -c override: {e}"));
            }
        }
    };

    // Story 8.1 AC-10 — capture CLI snapshot for config reload handler
    let cli_snapshot = Cli {
        log_level: cli.log_level.clone(),
        command: None,
        new: cli.new,
        session: cli.session.clone(),
        snapshot_retention: cli.snapshot_retention,
        config_file: cli.config_file.clone(),
        model: cli.model.clone(),
        config_override: cli.config_override.clone(),
        profile: cli.profile.clone(),
        persona: cli.persona.clone(),
        memory: cli.memory.clone(),
        session_adapter: cli.session_adapter.clone(),
        tools: cli.tools.clone(),
        channels: cli.channels.clone(),
        scheduler: cli.scheduler.clone(),
        context: cli.context.clone(),
        tool_exposure: cli.tool_exposure.clone(),
        skill_exposure: cli.skill_exposure.clone(),
        sandbox_adapter: cli.sandbox_adapter.clone(),
        serve_a2a: cli.serve_a2a.clone(),
    };

    // Story 18.1a — refuse an unserveable combination BEFORE any command
    // intercept runs. Placed here because the intercepts return in source order,
    // so a check sited next to the serve intercept would already have been
    // skipped by the daemon branch above it. Story 18.1b permits `daemon`.
    evaluate_serve_a2a_combination(cli.serve_a2a.is_some(), cli.command.as_ref())?;

    // 3. Load config — two-pass for story 8.2 chicken-and-egg resolution (AC-15).
    //
    // Pass 1: bootstrap config with NoopProfileResolver to discover active_profile name
    let noop = crate::adapters::profile_resolver::noop::NoopProfileResolver;
    let bootstrap_config =
        config::load_with_config_overrides(&cli, &noop, cli_config_overrides.as_ref());

    // Resolve effective active profile name (CLI > env > config > default "coding")
    let config_dir = paths::config_dir().unwrap_or_else(|_| std::path::PathBuf::from(".rustain"));
    let profiles_dir = config_dir.join("profiles");

    let effective_name =
        crate::infrastructure::profile_resolution::effective_profile_name(&cli, &bootstrap_config);

    // Pass 2: construct TomlProfileResolver, load full config with profile overrides at layer 6
    let (toml_resolver, startup_notices): (
        crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver,
        Vec<String>,
    ) = match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
        &effective_name,
        profiles_dir.clone(),
    ) {
        Ok(r) => (r, Vec::new()),
        Err(crate::domain::errors::ProfileError::ProfileNotFound {
            name,
            search_paths: _,
        }) => {
            // AC-6 fallback: profile not found -> fall back to coding + emit warning
            tracing::warn!("Profile '{}' not found; falling back to 'coding'", name);
            let fallback =
                match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
                    "coding",
                    profiles_dir.clone(),
                ) {
                    Ok(r) => r,
                    Err(fallback_err) => {
                        tracing::error!(
                            "Critical: coding profile fallback failed: {}",
                            fallback_err
                        );
                        eprintln!(
                            "Critical: built-in 'coding' profile failed to load: {}",
                            fallback_err
                        );
                        std::process::exit(2);
                    }
                };
            let notices = vec![format!(
                "Profile '{}' not found in any search path; falling back to 'coding'",
                name
            )];
            (fallback, notices)
        }
        Err(e) => {
            // AC-8: ALL OTHER validation errors are FATAL at startup
            eprintln!("Profile load failed: {}", e);
            std::process::exit(2);
        }
    };

    let app_config =
        config::load_with_config_overrides(&cli, &toml_resolver, cli_config_overrides.as_ref());

    // Story 9.4 — validate tools.exposure BEFORE AgentCore composition
    if let Err(e) = validate_tools_exposure(&app_config.tools.exposure) {
        eprintln!("Config validation failed: {}", e);
        std::process::exit(1);
    }

    // Story 9.6 — validate skill_exposure.kind BEFORE AgentCore composition
    if let Err(e) = validate_skill_exposure(&app_config.skill_exposure.kind) {
        eprintln!("Config validation failed: {}", e);
        std::process::exit(1);
    }

    // Story 9.5 — validate sandbox.adapter BEFORE AgentCore composition
    if let Err(e) = validate_sandbox_adapter(&app_config.sandbox.adapter) {
        eprintln!("Config validation failed: {}", e);
        std::process::exit(1);
    }

    // Story 11.6 — validate assembler.strategy BEFORE AgentCore composition
    if let Err(e) = validate_assembler_strategy(&app_config.assembler.strategy) {
        eprintln!("Config validation failed: {}", e);
        std::process::exit(1);
    }

    // Story 9.7 — validate [search] config (on/off per ADR-09-02)
    #[cfg(feature = "meta-search")]
    if let Err(e) = app_config.search.validate() {
        eprintln!("Config validation failed: {}", e);
        std::process::exit(1);
    }

    // Accumulate any profile-related notices for post-EventBus flush
    let mut accumulated_notices: Vec<String> = startup_notices;

    // AC-10: preview warning notice (once per process lifetime)
    if let Some(preview_name) = toml_resolver.take_preview_warning() {
        accumulated_notices.push(format!(
            "Profile '{} (preview)' partially loaded: telegram, cron adapters not yet available (coming in Epic 12)",
            preview_name
        ));
    }

    // AC-10: warn if custom (non-built-in) profile sets preview=true
    {
        use crate::domain::ports::ProfileResolver;
        if let Some(resolved) = toml_resolver.resolve_active() {
            if resolved.preview
                && !crate::adapters::profile_resolver::embedded::embedded_names()
                    .contains(&resolved.name.as_str())
            {
                accumulated_notices.push(format!(
                    "Profile '{}' sets preview=true but is not a built-in profile. The preview flag is intended for built-in profiles only.",
                    resolved.name
                ));
            }
        }
    }

    // Wrap resolver for hot-swap (Story 8.4 — profile switching) per Gate 1.5
    let profile_resolver_arc: Arc<dyn crate::domain::ports::ProfileResolver> =
        Arc::new(toml_resolver);
    let profile_resolver_swap: Arc<
        arc_swap::ArcSwap<Arc<dyn crate::domain::ports::ProfileResolver>>,
    > = Arc::new(arc_swap::ArcSwap::from_pointee(
        profile_resolver_arc.clone(),
    ));

    // Replace line: let app_config = config::load(&cli, &NoopProfileResolver);

    // 4. Install panic hook
    signals::install_panic_hook();

    // 4a. Intercept init/doctor/migrate subcommands BEFORE provider construction and terminal setup
    if let Some(Command::Init) = cli.command {
        return crate::adapters::cli::init::run_init().await;
    }
    // Story 13.3b — Completions subcommand intercept. Sync (no .await), no provider needed.
    if let Some(Command::Completions { shell, bin_name }) = cli.command {
        return crate::adapters::cli::completions::run_completions(shell, bin_name).map_err(|e| {
            // Surface the error to the user on stderr; main.rs suppresses
            // SubcommandExit errors, so logging alone would hide the message.
            eprintln!("{e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    // Story 13.4a/13.4b/13.4c — Auth subcommand intercept. Runs before provider
    // construction and terminal setup. All auth subcommands are read-only/offline-safe
    // except `auth login` which validates via an ad hoc candidate adapter.
    if let Some(Command::Auth { action }) = &cli.command {
        match action {
            AuthAction::Login { provider, json } => {
                let store: Arc<dyn crate::domain::ports::AuthStorePort> =
                    Arc::new(crate::adapters::auth_store::FileAuthStore::new());
                return crate::adapters::cli::auth::login::run_auth_login(
                    provider.clone(),
                    *json,
                    &store,
                    &app_config,
                )
                .await
                .map_err(|e| {
                    tracing::error!("Auth login subcommand failed: {e}");
                    SubcommandExit(SubcommandExit::GENERIC).into()
                });
            }
            AuthAction::Status { json } => {
                let store: Arc<dyn crate::domain::ports::AuthStorePort> =
                    Arc::new(crate::adapters::auth_store::FileAuthStore::new());
                return crate::adapters::cli::auth::status::run_auth_status(*json, &store)
                    .await
                    .map_err(|e| {
                        tracing::error!("Auth status subcommand failed: {e}");
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    });
            }
            AuthAction::List { json } => {
                let store: Arc<dyn crate::domain::ports::AuthStorePort> =
                    Arc::new(crate::adapters::auth_store::FileAuthStore::new());
                return crate::adapters::cli::auth::list::run_auth_list(*json, &app_config, &store)
                    .await
                    .map_err(|e| {
                        tracing::error!("Auth list subcommand failed: {e}");
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    });
            }
        }
    }
    // Story 13.5a / 13.5b — Session subcommand intercept. Runs before provider
    // construction and terminal setup. `session list` is read-only, offline-safe,
    // and non-billable. `session delete` is the first irreversible, scriptable
    // destructive operation; it carries distinct exit codes.
    if let Some(Command::Session { action }) = &cli.command {
        match action {
            SessionAction::List { json, all } => {
                let workspace = paths::workspace_dir()?;
                let sessions_dir = paths::sessions_dir(&workspace);
                let storage: Arc<dyn StoragePort> = Arc::new(
                    FileSystemStorage::with_workspace_root(sessions_dir, workspace.clone()),
                );
                let reader: Arc<dyn WorkspaceRegistryReaderPort> =
                    Arc::new(FileWorkspaceRegistry::new()?);
                return crate::adapters::cli::session::list::run_session_list(
                    *json, *all, &workspace, &storage, &reader,
                )
                .await
                .map_err(|e| {
                    tracing::error!("Session list subcommand failed: {e}");
                    SubcommandExit(SubcommandExit::GENERIC).into()
                });
            }
            SessionAction::Delete {
                id,
                all,
                all_workspaces,
                workspace,
                force,
                dry_run,
                json,
            } => {
                use std::io::IsTerminal;
                let workspace_root = paths::workspace_dir()?;
                let storage_for = |ws: &std::path::Path| -> Arc<dyn StoragePort> {
                    Arc::new(FileSystemStorage::with_workspace_root(
                        paths::sessions_dir(ws),
                        ws.to_path_buf(),
                    ))
                };
                let reader: Arc<dyn WorkspaceRegistryReaderPort> =
                    Arc::new(FileWorkspaceRegistry::new()?);
                let holder = crate::adapters::daemon::session_holder::DaemonSessionHolder;
                let stdin = std::io::stdin();
                let mut stdin_lock = stdin.lock();
                let mut stdout = std::io::stdout();
                let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
                return crate::adapters::cli::session::delete::run_session_delete(
                    id.clone(),
                    *all,
                    *all_workspaces,
                    workspace.clone(),
                    *force,
                    *dry_run,
                    *json,
                    &workspace_root,
                    storage_for,
                    &holder,
                    &*reader,
                    is_tty,
                    &mut stdin_lock,
                    &mut stdout,
                )
                .await
                .map_err(|e| {
                    tracing::error!("Session delete subcommand failed: {e}");
                    if e.downcast_ref::<SubcommandExit>().is_some() {
                        e
                    } else {
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    }
                });
            }
        }
    }
    if let Some(Command::Doctor {
        terminal,
        adapters,
        json,
    }) = cli.command
    {
        // Build a minimal provider layer so `rustain doctor` can run non-billable
        // connectivity probes against configured providers (Story 13.2 AC8).
        let provider_layer = init_provider_layer(&app_config);
        let provider_pairs: Vec<(String, Option<Arc<dyn StreamingProvider>>)> = provider_layer
            .registry
            .iter_provider_arcs()
            .into_iter()
            .map(|(id, provider)| (id, Some(provider)))
            .collect();
        // Story 13.2b: resolve MCP servers from active profile (mirrors `providers` threading).
        let mcp_servers = {
            use crate::domain::ports::ProfileResolver;
            profile_resolver_swap
                .load()
                .resolve_active()
                .map(|p| p.mcp_servers.clone())
                .unwrap_or_default()
        };
        return crate::adapters::cli::doctor::run_doctor(
            terminal,
            adapters,
            json,
            provider_pairs,
            mcp_servers,
        )
        .await
        .map_err(|e| {
            tracing::error!("Doctor subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Migrate {
        from,
        path,
        yes,
        select,
        dry_run,
    }) = cli.command
    {
        return crate::adapters::cli::migrate::run_migrate(from, path, yes, select, dry_run)
            .await
            .map_err(|e| {
                tracing::error!("Migrate subcommand failed: {e}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
    }
    #[cfg(feature = "openai")]
    if let Some(Command::UpdateCatalog { output, provider }) = cli.command {
        return crate::adapters::cli::update_catalog::run_update_catalog(output, provider)
            .await
            .map_err(|e| {
                tracing::error!("UpdateCatalog subcommand failed: {e}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
    }
    #[cfg(not(feature = "openai"))]
    if let Some(Command::UpdateCatalog { .. }) = cli.command {
        anyhow::bail!(
            "update-catalog requires the 'openai' feature — rebuild with --features openai"
        );
    }
    #[cfg(feature = "self-update")]
    if let Some(Command::Update {
        check,
        output_format,
    }) = cli.command
    {
        if check {
            // run_check always returns ExitCode::SUCCESS; we discard it and exit 0.
            let _exit_code =
                crate::adapters::self_update::orchestrator::run_check(&output_format).await;
            std::process::exit(0);
        }
        return crate::adapters::self_update::orchestrator::run_update()
            .await
            .map_err(|e| {
                eprintln!("✗ {e}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
    }
    #[cfg(not(feature = "self-update"))]
    if let Some(Command::Update { .. }) = cli.command {
        anyhow::bail!(
            "self-update requires the 'self-update' feature — rebuild with --features self-update"
        );
    }
    // Story 8.1 AC-9 + Story 13.2a — Config subcommand intercept
    if let Some(Command::Config { action }) = &cli.command {
        match action {
            ConfigAction::Reload => {
                return crate::adapters::cli::config_cmd::run_config_reload()
                    .await
                    .map_err(|e| {
                        tracing::error!("Config reload subcommand failed: {e}");
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    });
            }
            ConfigAction::Show { json } => {
                return crate::adapters::cli::config_cmd::run_config_show_with_overrides(
                    *json,
                    &profile_resolver_arc,
                    &cli,
                    cli_config_overrides.as_ref(),
                )
                .await
                .map_err(|e| {
                    tracing::error!("Config show subcommand failed: {e}");
                    SubcommandExit(SubcommandExit::GENERIC).into()
                });
            }
            ConfigAction::Edit { global } => {
                return crate::adapters::cli::config_cmd::run_config_edit(*global, &cli)
                    .await
                    .map_err(|e| {
                        tracing::error!("Config edit subcommand failed: {e}");
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    });
            }
            ConfigAction::Path { json } => {
                return crate::adapters::cli::config_cmd::run_config_path(*json, &cli)
                    .await
                    .map_err(|e| {
                        tracing::error!("Config path subcommand failed: {e}");
                        SubcommandExit(SubcommandExit::GENERIC).into()
                    });
            }
            ConfigAction::Validate { json } => {
                return crate::adapters::cli::config_cmd::run_config_validate_with_overrides(
                    *json,
                    &profile_resolver_arc,
                    &cli,
                    cli_config_overrides.as_ref(),
                )
                .await
                .map_err(|e| {
                    tracing::error!("Config validate subcommand failed: {e}");
                    SubcommandExit(SubcommandExit::GENERIC).into()
                });
            }
        }
    }

    // Story 8.4 AC-7 — Profile switch subcommand intercept
    if let Some(Command::Profile {
        action: ProfileAction::Switch { name, start },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_switch(name.clone(), *start)
            .await
            .map_err(|e| {
                tracing::error!("Profile switch subcommand failed: {e}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
    }

    // Profile subcommands (Story 8.6a) — terminate before terminal setup
    if let Some(Command::Profile {
        action: ProfileAction::List { json },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_list(
            *json,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile list subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action:
            ProfileAction::Show {
                name,
                json,
                toml_out,
            },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_show(
            name.clone(),
            *json,
            *toml_out,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile show subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action:
            ProfileAction::Create {
                name,
                extends,
                from,
            },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_create(
            name.clone(),
            extends.clone(),
            from.clone(),
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile create subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action: ProfileAction::Edit { name, no_validate },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_edit(
            name.clone(),
            *no_validate,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile edit subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action: ProfileAction::Validate { name, all, json },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_validate(
            name.clone(),
            *all,
            *json,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile validate subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action: ProfileAction::Export { name, output },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_export(
            name.clone(),
            output.clone(),
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile export subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    if let Some(Command::Profile {
        action: ProfileAction::Import { path, name, force },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_import(
            path.clone(),
            name.clone(),
            *force,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile import subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    // Profile install (Story 8.6b) — public-repo HTTPS fetch + validate + community/ dir install.
    // Network call inside; honours --strict-features.
    #[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
    if let Some(Command::Profile {
        action:
            ProfileAction::Install {
                spec,
                name,
                force,
                strict_features,
            },
    }) = &cli.command
    {
        return crate::adapters::cli::profile::run_profile_install(
            spec.clone(),
            name.clone(),
            *force,
            *strict_features,
            &profile_resolver_arc,
            &cli,
            &bootstrap_config,
        )
        .await
        .map_err(|e| {
            tracing::error!("Profile install subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }
    #[cfg(not(any(feature = "anthropic", feature = "openai", feature = "ollama")))]
    if let Some(Command::Profile {
        action: ProfileAction::Install { .. },
    }) = &cli.command
    {
        eprintln!(
            "Error: 'rustain profile install' requires HTTPS support. Rebuild with --features anthropic."
        );
        return Err(SubcommandExit(SubcommandExit::GENERIC).into());
    }

    // Story 9.8 — Catalog dev-tool dispatch (before TUI initialization)
    #[cfg(feature = "meta-search")]
    if let Some(Command::Catalog { action }) = cli.command.clone() {
        let workspace_path = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;
        let exit_code =
            crate::adapters::cli::catalog::run_catalog_action(action, &app_config, workspace_path)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("rustain catalog: {}", e);
                    1
                });
        std::process::exit(exit_code);
    }

    // Story 12.1a — Daemon subcommand intercept. MUST run before provider
    // construction + terminal setup: the daemon is headless (no TUI, no
    // provider layer in 12.1a) and `start` re-execs a detached child. The memory
    // adapter name is resolved from the active profile so the headless daemon
    if let Some(Command::Daemon { action }) = cli.command.clone() {
        // Story 18.1b — `--serve-a2a` is honoured HERE, inside daemon mode: the
        // listener becomes a sibling task sharing the daemon's node tree, core
        // and event bus, which is the only composition that can execute inbound
        // tasks. `evaluate_serve_a2a_combination` already cleared the pair.
        if cli.serve_a2a.is_some() {
            ensure_a2a_feature_enabled(&[], true)?;
        }
        use crate::domain::models::profile::PortDimension;
        let workspace = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;
        let resolved_selection = profile_resolver_arc.resolve_active().map(|r| r.selection);
        let memory_adapter = resolved_selection
            .as_ref()
            .and_then(|sel| {
                sel.dimensions
                    .get(&PortDimension::Memory)
                    .map(|a| a.adapter.clone())
            })
            .unwrap_or_else(|| "noop".to_string());
        // Story 12.2b — the daemon composes its full turn runtime (lazily) from the
        // active profile selection, so thread it through (not just the memory name).
        let selection = resolved_selection.unwrap_or_default();
        return crate::adapters::daemon::run_daemon(
            action,
            workspace,
            app_config,
            memory_adapter,
            selection,
            cli.serve_a2a.clone(),
        )
        .await
        .map_err(|e| {
            tracing::error!("Daemon subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }

    // Story 18.1a — loopback A2A server intercept, standalone. Reached only when
    // there is no subcommand: `daemon` handles the combined form above.
    //
    // Standalone serves DISCOVERY only. It has no `DaemonCore`, so it has no
    // peer-turn path to run an inbound task on, and admission answers a policy
    // verdict naming `rustain daemon start --serve-a2a=<addr>` rather than
    // pretending a capability it does not have.
    if let Some(addr) = cli.serve_a2a.clone() {
        ensure_a2a_feature_enabled(&[], true)?;
        #[cfg(feature = "a2a")]
        {
            let workspace = std::env::current_dir()
                .map_err(|e| anyhow::anyhow!("Failed to get current directory: {e}"))?;
            let node_journal = std::sync::Arc::new(
                crate::infrastructure::subagent::NodeJournal::open_workspace(&workspace)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("opening node journal for standalone A2A: {error}")
                    })?,
            );
            let room: std::sync::Arc<dyn crate::domain::ports::RoomJournal> = std::sync::Arc::new(
                crate::infrastructure::subagent::NodeRoomJournal::new(node_journal, None),
            );
            let transparency = std::sync::Arc::new(
                crate::adapters::a2a::transparency::TransparencySink::new(room),
            );
            return crate::adapters::a2a::server::run(
                addr,
                app_config,
                workspace,
                None,
                transparency,
                None,
            )
            .await
            .map_err(|e| {
                tracing::error!("A2A server failed: {e:#}");
                // AC4a's refusal has to reach the operator, not just the
                // log: `SubcommandExit` carries only an exit code, so a
                // bare map would turn "non-loopback bind refused, see
                // 18-1b" into a silent exit 1.
                eprintln!("rustain: A2A server failed: {e:#}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
        }
        #[cfg(not(feature = "a2a"))]
        {
            let _ = addr;
            unreachable!("ensure_a2a_feature_enabled rejects serving without the feature");
        }
    }
    // Story 14.7 — ACP server intercept. MUST run before provider construction
    // and terminal setup: stdout is the JSON-RPC transport.
    if let Some(Command::Acp { client }) = cli.command.clone() {
        let workspace = std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;
        return crate::adapters::acp::run_acp(app_config, workspace, cli.model.clone(), client)
            .await
            .map_err(|e| {
                tracing::error!("ACP subcommand failed: {e}");
                SubcommandExit(SubcommandExit::GENERIC).into()
            });
    }

    // Story 13.1a — Ask subcommand intercept. MUST run before provider
    // construction + terminal setup: `ask` is headless (no TUI). Like the
    // Daemon block, `run_ask` does its own composition via `build_cli_core`.
    if let Some(Command::Ask {
        query,
        file,
        yolo,
        final_message_only,
        output_format,
        dry_run,
    }) = cli.command.clone()
    {
        return crate::adapters::cli::ask::run_ask(
            query,
            file,
            yolo,
            final_message_only,
            output_format,
            dry_run,
            app_config,
            cli.session.clone(),
            cli.new,
            cli.model.clone(),
        )
        .await
        .map_err(|e| {
            tracing::error!("Ask subcommand failed: {e}");
            SubcommandExit(SubcommandExit::GENERIC).into()
        });
    }

    // 5. Apply model override from env (before provider + event loop, so status bar sees it)
    let mut app_config = app_config;
    // CONFORMANCE_EXCEPTION_ENV_LAYER_BYPASS: legacy env-var, see Story 8.1 Decision Gate item 1.2
    if let Some(model_override) =
        crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_DEFAULT_SONNET_MODEL")
    {
        tracing::info!(
            "Model override from ANTHROPIC_DEFAULT_SONNET_MODEL: {}",
            model_override
        );
        app_config.model = model_override;
    }

    // Story 8.1 AC-7 — wrap config in ArcSwap for atomic reload
    // Clone the config into the ArcSwap; keep app_config for the rest of startup.
    let app_config_swap = Arc::new(arc_swap::ArcSwap::from_pointee(app_config.clone()));

    // 5a. Construct provider layer
    let ProviderLayer {
        router,
        registry: provider_registry,
        deferred_notices,
        active_id: _active_id,
        unsupported_discovery,
        discovery_targets,
    } = init_provider_layer(&app_config);
    #[cfg(not(feature = "openai"))]
    let _ = &discovery_targets; // suppress unused-variable warning on non-openai builds

    // ArcSwap hot-swap holder wraps the router (not a bare adapter)
    let provider_swap = Arc::new(arc_swap::ArcSwap::from_pointee(
        router.clone() as Arc<dyn StreamingProvider>
    ));

    // 5b. Construct security and toolset adapters
    let workspace_path = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("Failed to get current directory: {}", e))?;

    // 6. Create AppState (owns EventBus + CancellationToken + ApprovalRuntime)
    let raw_capacity = app_config.runtime.event_bus.raw_capacity;
    let user_config = paths::config_dir()
        .unwrap_or_else(|_| workspace_path.join(".rustain"))
        .join("config.toml");
    let workspace_rules = workspace_path.join(".rustain").join("permissions.toml");
    let persistence = Arc::new(ApprovalPersistenceToml::new(
        user_config.clone(),
        workspace_rules.clone(),
    ));
    let approval_runtime = ApprovalRuntime::new_with_subagent_policy(
        raw_capacity,
        persistence,
        app_config.subagents.auto_approve,
    );
    if app_config.subagents.auto_approve == AutoApprovePolicy::Allow {
        let msg = "⚠ subagents.auto_approve = 'allow' — subagent tool calls bypass user approval. Use only on trusted workloads.";
        tracing::warn!("{}", msg);
        accumulated_notices.push(msg.to_string());
    }
    approval_runtime.load_session().await;
    if let Ok(ruleset) = permission_rules::load_rules(&user_config, &workspace_rules) {
        let seed = ruleset.seed_session();
        approval_runtime.seed_session(seed).await;
    }
    let plans_dir = workspace_path.join(".rustain").join("plans");
    let plan_manager = Arc::new(PlanManager::new(plans_dir));
    let plan_injector = Arc::new(DefaultPlanInjector::new());

    let initial_mode = if app_config.default_plan_mode {
        PermissionMode::Plan
    } else {
        PermissionMode::Normal
    };
    let sandbox_policy = SandboxPolicy::from_mode(initial_mode, &workspace_path);

    let usage_ledger: Arc<dyn crate::domain::ports::UsageLedgerPort> =
        Arc::new(FileUsageLedger::new());

    // Story 7.5 AC7 — load BudgetState (dismissed-until) once at startup.
    let budget_state_store = Arc::new(crate::adapters::budget::BudgetStateStore::new());

    let refresh_tracker = crate::adapters::tui::refresh_tracker::RefreshTracker::new();

    // Story 8.3 AC-7 — create EventBus early so domain_tx is available
    // for storage/tools/persona construction and AgentCore composition
    // before AppState::new is called later.
    let (event_bus, domain_rx) =
        crate::infrastructure::runtime::event_bus::EventBus::new(raw_capacity);
    let event_bus = Arc::new(event_bus);
    let domain_tx = event_bus.domain_tx.clone();

    // Flush accumulated profile-related notices after EventBus is wired.  Story 8.2
    // AC-6 fallback + AC-10 preview-warning both queue notices during two-pass load
    // (before EventBus exists); emit them now via emit_domain to preserve the
    // EventBus bypass ratchet (MAX_KNOWN_BYPASSES = 48, unchanged).
    for msg in &accumulated_notices {
        let _ = event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: msg.clone(),
        });
    }

    // Story 7.6 AC7 — emit startup toast for providers that don't support discovery
    for (id, kind) in &unsupported_discovery {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!(
                "{} doesn't support model discovery — using config.toml list",
                kind
            ),
        });
        tracing::warn!(
            "Provider '{}' (kind={}) uses a static catalog — dynamic discovery is not yet supported.",
            id,
            kind
        );
    }

    #[cfg(feature = "openai")]
    {
        // Story 7.7 AC1/AC6 — Tier-0 JSON seed from embedded models_variants.json (zero I/O)
        if let Some(seed_catalog) = crate::adapters::model_catalog_cache::load_embedded_seed() {
            for target in &discovery_targets {
                if let Some(entry) = seed_catalog.providers.get(&target.provider_id) {
                    target.adapter.set_discovered_models(entry.models.clone());
                    tracing::info!(
                        "Tier-0 seed: JSON catalog for '{}' ({} models)",
                        target.provider_id,
                        entry.models.len()
                    );
                }
            }
        } else {
            tracing::error!(
                "Failed to parse embedded models_variants.json — catalog seed unavailable"
            );
        }

        // Story 7.6 AC4/AC5 — Tier-1 disk cache seed BEFORE health check (synchronous, ≤10ms)
        let cache = crate::adapters::model_catalog_cache::ModelCatalogCache::new();
        let cached = cache.load().await;

        for target in &discovery_targets {
            if let Some(entry) = cached.providers.get(&target.provider_id) {
                target.adapter.set_discovered_models(entry.models.clone());
                tracing::info!("Tier-1 seed: cached catalog for '{}'", target.provider_id);
            }
        }
    }

    // D2: Health check — emit TUI warning notice on failure and update registry (AC4)
    // Health-check ALL registered providers and emit notices for failures.
    let all_provider_ids: Vec<String> = provider_registry.provider_ids().into_iter().collect();
    for id in &all_provider_ids {
        if let Some(adapter) = router.get_provider(id) {
            match adapter.health_check().await {
                Ok(()) => {
                    tracing::info!("Provider '{}' health check passed", id);
                    provider_registry.update_health(id, true);
                }
                Err(e) => {
                    tracing::warn!("Provider '{}' health check failed: {}", id, e);
                    provider_registry.update_health(id, false);
                    let (level, message) = match e {
                        ProviderError::ConnectionFailed(ref msg) => {
                            (NoticeLevel::Error, msg.clone())
                        }
                        _ => (
                            NoticeLevel::Warning,
                            format!("Provider '{}' unavailable: {}", id, e),
                        ),
                    };
                    let _ = domain_tx.send(AppEvent::SystemNotice {
                        conversation_id: None,
                        level,
                        message,
                    });
                }
            }
        }
    }

    // Flush deferred construction-failure notices
    for (id, e) in &deferred_notices {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!("Failed to construct provider '{}': {}", id, e),
        });
    }

    #[cfg(feature = "openai")]
    {
        // Story 7.6 AC4/AC5 — Tier-2 background refresh AFTER health check (non-blocking)
        let cache = crate::adapters::model_catalog_cache::ModelCatalogCache::new();
        let cached = cache.load().await;

        // Clone before the for-loop consumes it (used by periodic timer below)
        let discovery_targets_periodic = discovery_targets.clone();
        let refresh_tracker_clone = refresh_tracker.clone();
        for target in discovery_targets {
            let cache = cache.clone();
            let tracker = refresh_tracker_clone.clone();
            let domain_tx = domain_tx.clone();
            let provider_id = target.provider_id.clone();
            let adapter = target.adapter.clone();
            let model_filter = target.model_filter.clone();
            let ttl = target.cache_ttl_seconds;

            // Check freshness before spawning
            let is_fresh = cached.providers.get(&provider_id).is_some_and(|entry| {
                cache.is_fresh(entry, ttl, crate::infrastructure::clock_util::now_unix())
            });

            if is_fresh {
                tracing::debug!(
                    "catalog cache fresh for '{}'; skipping refresh",
                    provider_id
                );
                continue;
            }

            tokio::spawn(async move {
                let _guard = tracker.insert(provider_id.clone());
                match adapter.fetch_remote_models(&model_filter).await {
                    Ok(models) => {
                        if models.is_empty() {
                            tracing::warn!("Empty catalog from '{}'; not caching", provider_id);
                            let _ = domain_tx.send(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "Model catalog for '{}' returned empty — showing bundled models",
                                    provider_id
                                ),
                            });
                            return;
                        }
                        // Serialize cache writes so concurrent providers don't overwrite each other.
                        let _lock = cache.lock().await;
                        let mut catalog = cache.load().await;
                        let models_with_stale =
                            crate::adapters::model_catalog_cache::merge_with_live(
                                catalog.providers.get(&provider_id),
                                &models,
                            );
                        adapter.set_discovered_models(models_with_stale.clone());
                        catalog.providers.insert(
                            provider_id.clone(),
                            crate::adapters::model_catalog_cache::CachedProviderEntry {
                                fetched_at_unix: crate::infrastructure::clock_util::now_unix(),
                                models: models_with_stale,
                            },
                        );
                        if let Err(e) = cache.save(&catalog).await {
                            tracing::warn!("models_cache.json save failed: {}", e);
                        }
                        let _ = domain_tx.send(AppEvent::ProviderCatalogRefreshed { provider_id }); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: Story 7.6 AC8 — live refresh redraw signal
                    }
                    Err(e) => {
                        tracing::warn!(
                            "model discovery for '{}' failed: {}; using cached/bundled catalog",
                            provider_id,
                            e
                        );
                        let _ = domain_tx.send(AppEvent::SystemNotice {
                            conversation_id: None,
                            level: NoticeLevel::Warning,
                            message: format!(
                                "Model catalog refresh for '{}' failed: {} — showing bundled/cached models",
                                provider_id, e
                            ),
                        });
                    }
                }
            });
        }

        // Story 7.7 AC3 — periodic auto-refresh timer (4h intervals, UTC-aligned)
        spawn_periodic_catalog_refresh(
            cache.clone(),
            discovery_targets_periodic,
            refresh_tracker.clone(),
            domain_tx.clone(),
        );
    }

    let security_adapter = SecurityAdapter::new(workspace_path.clone());
    security_adapter.set_mode(initial_mode);
    let security: Arc<dyn SecurityPort> = Arc::new(security_adapter);

    if app_config.default_plan_mode {
        let _ = plan_manager.ensure_dir().await;
        plan_injector.as_ref().reset_reentry();
    }
    // Storage must be constructed before ToolSetAdapter (Story 4-3b: tools use storage for snapshots).
    // Story 4-3b P2: pass the real workspace root so `snapshot_file` can enforce
    // path-traversal checks without falling back to the sessions_dir grandparent proxy.
    let tools_sessions_dir = paths::sessions_dir(&workspace_path);
    let tools_storage: Arc<dyn StoragePort> = Arc::new(
        FileSystemStorage::with_workspace_root(tools_sessions_dir.clone(), workspace_path.clone())
            .with_workspace_registrar(Arc::new(FileWorkspaceRegistry::new()?)),
    );
    let shared_skill_registry = Arc::new(tokio::sync::RwLock::new(SkillRegistry::new()));
    let skill_activator = Arc::new(SkillActivator::with_registry(shared_skill_registry));
    skill_activator.set_event_tx(domain_tx.clone()).await;
    // Story 9.6 — two-layer skill cache (L1 LRU + L2 disk snapshot).
    // The cache is constructed BEFORE SkillRegistry::discover (which runs in
    // event_loop.rs). The `SkillCache::warm_up(...)` call that populates the
    // cache from the discovered registry is deferred to event_loop.rs after
    // `SkillRegistry::discover` completes. The cache itself is available
    // immediately (empty L1) for early consumers like `skill_view`.
    let shared_skill_cache = Arc::new(crate::infrastructure::skill_cache::SkillCache::new(
        crate::infrastructure::skill_cache::SkillCacheConfig::default(),
    ));
    // Story 9.5 — shared sandbox slot and policy ref, created EARLY so
    // ToolSetAdapter and ComposeContext/AppState share the SAME Arc instances.
    // The slot starts as NoOpSandbox and is updated after AgentCore::compose().
    let sandbox_slot: Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::SandboxManager>>> = {
        use crate::adapters::sandbox::NoOpSandbox;
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(NoOpSandbox) as Arc<dyn crate::domain::ports::SandboxManager>
        ))
    };
    let sandbox_policy_ref: Arc<tokio::sync::RwLock<SandboxPolicy>> =
        Arc::new(tokio::sync::RwLock::new(sandbox_policy.clone()));

    let agent_activator = Arc::new(crate::adapters::agent_activation::AgentActivator::new(
        Arc::clone(&security),
    ));
    let mut tools_adapter = ToolSetAdapter::new(
        workspace_path.clone(),
        Arc::clone(&tools_storage),
        Arc::clone(&sandbox_slot),
        Arc::clone(&sandbox_policy_ref),
    );
    tools_adapter.set_activator(Arc::clone(&skill_activator));
    tools_adapter.set_skill_cache(Arc::clone(&shared_skill_cache));
    tools_adapter.set_plan_manager(plan_manager.clone());
    tools_adapter.set_event_tx(domain_tx.clone());

    // Story 9.7 Phase B — late-init slot for CapabilityRegistry.
    // The registry is created inside CompositeToolsetAdapter during
    // AgentCore::compose() — AFTER this meta-search block. The rebuild_fn
    // captures this OnceLock; it is set once compose + populate complete.
    #[cfg(feature = "meta-search")]
    let capability_registry_slot: Arc<
        std::sync::OnceLock<Arc<crate::domain::models::capability_registry::CapabilityRegistry>>,
    > = Arc::new(std::sync::OnceLock::new());

    // Story 9.7 Phase B — construct meta-search engine (gated)
    #[cfg(feature = "meta-search")]
    let (meta_search_engine, _catalog_registry): (
        Option<Arc<dyn crate::domain::ports::search::MetaSearchEngine>>,
        Option<Arc<crate::infrastructure::composition::catalog_observer_registry::CatalogObserverRegistry>>,
    ) = {
        let index_arcswap: Arc<arc_swap::ArcSwap<crate::infrastructure::search::MergedIndex>> =
            Arc::new(arc_swap::ArcSwap::from_pointee(crate::infrastructure::search::MergedIndex::empty()));
        let engine: Arc<dyn crate::domain::ports::search::MetaSearchEngine> =
            Arc::new(crate::infrastructure::search::Bm25SearchEngine::new(Arc::clone(&index_arcswap)));
        tools_adapter.set_meta_search_engine(Arc::clone(&engine));

        let rebuild_fn: std::sync::Arc<dyn Fn() -> Arc<crate::infrastructure::search::MergedIndex> + Send + Sync> = {
            let skill_cache = Arc::clone(&shared_skill_cache);
            let registry_slot = Arc::clone(&capability_registry_slot);
            let tools_fallback: Arc<dyn crate::domain::ports::ToolSetPort> = Arc::new(tools_adapter.clone());
            std::sync::Arc::new(move || {
                let tool_descs: Vec<crate::domain::models::tool_descriptor::ToolDescriptor> = if let Some(reg) = registry_slot.get() {
                    let caps = reg.snapshot();
                    caps.iter().map(|c| crate::domain::models::tool_descriptor::ToolDescriptor::from(c)).collect()
                } else {
                    tools_fallback.describe()
                };

                let skill_metas = skill_cache.try_snapshot_metadata();

                let mut overrides: std::collections::BTreeMap<crate::domain::models::doc_key::DocKey, String> = std::collections::BTreeMap::new();
                for s in &skill_metas {
                    if let Some(ref terse) = s.terse {
                        overrides.insert(
                            crate::domain::models::doc_key::DocKey::new(
                                crate::domain::models::capability_kind::CapabilityKind::Skill,
                                s.name.clone(),
                            ),
                            terse.clone(),
                        );
                    }
                }

                let mut refs: Vec<&dyn crate::domain::ports::search::IndexableItem> = Vec::with_capacity(tool_descs.len() + skill_metas.len());
                for t in &tool_descs { refs.push(t); }
                for s in &skill_metas { refs.push(s); }

                Arc::new(crate::infrastructure::search::MergedIndex::from_items_with_overrides(&refs, &overrides))
            })
        };
        let registry = crate::infrastructure::composition::catalog_observer_registry::CatalogObserverRegistry::new(
            Arc::clone(&index_arcswap),
            rebuild_fn,
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let _handle: tokio::task::JoinHandle<()> = Arc::clone(&registry).spawn_reindex_task(cancel).await;
        // Story 9.7 Phase B — build initial merged index from builtin tools so
        // `search_skills` AND `search_tools` return non-empty results before any catalog deltas arrive.
        registry.rebuild_now();
        (Some(engine), Some(registry))
    };
    #[cfg(not(feature = "meta-search"))]
    let (meta_search_engine, _catalog_registry): (
        Option<Arc<dyn crate::domain::ports::search::MetaSearchEngine>>,
        Option<()>,
    ) = (None, None);

    // Story 16.9: construct progress channel when live_tail is enabled
    let (progress_tx, progress_rx) = if app_config.tool_progress.live_tail {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tools_adapter.set_progress_tx(Some(tx.clone())).await;
        tools_adapter
            .set_tool_progress_config(app_config.tool_progress.clone())
            .await;
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let _tools_direct: Arc<dyn ToolSetPort> = Arc::new(tools_adapter); // Story 8.3: kept for dual-construction, superseded by agent_core-sourced tools below

    // 5c. Discover and load project context
    let context_loader = ProjectContextLoader::new(workspace_path.clone());
    let project_context = context_loader.discover().unwrap_or_else(|e| {
        tracing::warn!("Failed to discover project context: {}", e);
        crate::domain::models::project_context::ProjectContext::empty()
    });
    let persona_adapter = PersonaAdapter::new(project_context.clone());

    // Emit context loading notices (Phase D: Task 7)
    if persona_adapter.has_context() {
        let paths: Vec<String> = persona_adapter
            .file_paths()
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let msg = format!(
            "Project context: {} ({} chars)",
            paths.join(", "),
            persona_adapter.total_chars(),
        );
        tracing::info!("{}", msg);
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: msg,
        });

        if persona_adapter.is_truncated() {
            let warn_msg = format!(
                "Project context truncated: some files omitted (budget: {} chars)",
                crate::domain::models::project_context::CONTEXT_BUDGET_CHARS,
            );
            tracing::warn!("{}", warn_msg);
            let _ = domain_tx.send(AppEvent::SystemNotice {
                conversation_id: None,
                level: NoticeLevel::Warning,
                message: warn_msg,
            });
        }
    }

    // Story 8.3 AC-7 — compose AgentCore from active profile selection
    let resolved = profile_resolver_arc
        .resolve_active()
        .expect("post-Pass-2 toml_resolver always has resolve_active populated");
    ensure_a2a_feature_enabled(&resolved.a2a_peers, false)?;
    let compose_ctx = crate::infrastructure::composition::ComposeContext {
        workspace_path: workspace_path.clone(),
        project_context: project_context.clone(),
        storage: Arc::clone(&tools_storage) as Arc<dyn StoragePort>,
        skill_activator: Arc::clone(&skill_activator),
        mcp_servers: resolved.mcp_servers.clone(),
        a2a_peers: resolved.a2a_peers.clone(),
        include_builtin_tools: resolved.include_builtin_tools,
        domain_tx: Some(domain_tx.clone()),
        channel_turn_tx: None,
        tool_exposure: app_config.tools.exposure.clone(),
        assembler: app_config.assembler.strategy.clone(),
        skill_exposure: app_config.skill_exposure.kind.clone(),
        skill_cache: Arc::clone(&shared_skill_cache),
        sandbox_adapter: app_config.sandbox.adapter.clone(),
        sandbox_startup_policy: sandbox_policy.clone(),
        sandbox_slot: Arc::clone(&sandbox_slot),
        sandbox_policy: Arc::clone(&sandbox_policy_ref),
        // Story 11.1 — shared memory slot for the `remember` tool. `build_memory`
        // publishes the composed adapter into it during compose (and on reload).
        memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
            crate::adapters::noop::NoOpMemory,
        )
            as Arc<dyn crate::domain::ports::MemoryPort>)),
        memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
        #[cfg(feature = "meta-search")]
        search_config: app_config.search.clone(),
        #[cfg(feature = "meta-search")]
        meta_search_engine: meta_search_engine.clone(),
    };
    let agent_core_inner = match crate::infrastructure::runtime::agent_core::AgentCore::compose(
        &resolved.name,
        &resolved.selection,
        &compose_ctx,
    ) {
        Ok(core) => Arc::new(core),
        Err(e) => {
            eprintln!("Adapter composition failed: {}", e);
            std::process::exit(2);
        }
    };
    let compose_snapshot = Arc::new(compose_ctx);

    // Story 9.5 — wire the resolved sandbox adapter into the shared slot
    // (used by ToolSetAdapter for Bash-tool enforcement) and into AgentCore.
    {
        let resolved = agent_core_inner.sandbox.load_full();
        sandbox_slot.store(Arc::clone(&resolved));
    }

    // Story 9.5 — restrict the parent rustain process with Landlock
    // (NoOp on non-Linux; non-fatal on ABI-too-old fallback).
    // Must happen AFTER all 10 AgentCore slots are filled AND AFTER
    // SkillCache warm_up has populated frontmatter (which happens on
    // first skill_exposure render), but BEFORE the event loop enters.
    //
    // Uses the LEAST-RESTRICTIVE plausible policy (WorkspaceWrite + network)
    // per sandbox.rs docstring: Landlock is one-way restrictive — once
    // restricted, the process can never regain access. The mode-derived
    // policy (e.g. ReadOnly in Plan mode) would permanently lock rustain.
    // Per-call apply() tightens further for individual Bash invocations.
    {
        let startup_restrict_policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![workspace_path.clone()],
            read_only_paths: vec![workspace_path.join(".git"), workspace_path.join(".rustain")],
            network: true,
        };
        let sandbox = agent_core_inner.sandbox.load_full();
        if let Err(e) = sandbox.restrict_self(&startup_restrict_policy).await {
            tracing::warn!(
                error = %e,
                "Sandbox restrict_self failed at startup (non-fatal); \
                 ADR-06-04 §Negative — documented as known limitation"
            );
        }
    }

    // Story 8.5 AC-8 — apply startup-time adapter overrides from CLI flags
    {
        use crate::domain::models::profile::{AdapterRef, PortDimension};
        let cli_overrides: [(PortDimension, Option<&str>); 7] = [
            (PortDimension::Persona, cli.persona.as_deref()),
            (PortDimension::Memory, cli.memory.as_deref()),
            (PortDimension::Session, cli.session_adapter.as_deref()),
            (PortDimension::Tools, cli.tools.as_deref()),
            (PortDimension::Channels, cli.channels.as_deref()),
            (PortDimension::Scheduler, cli.scheduler.as_deref()),
            (PortDimension::Context, cli.context.as_deref()),
        ];
        for (port, name_opt) in &cli_overrides {
            if let Some(name) = name_opt {
                let adapter_ref = AdapterRef {
                    adapter: name.to_string(),
                    _config: None,
                };
                match crate::infrastructure::composition::build_for_port(
                    *port,
                    &adapter_ref,
                    &compose_snapshot,
                ) {
                    Ok(built) => {
                        agent_core_inner.store_for_port(built);
                        tracing::info!(port = ?port, adapter = %name, source = "cli", "Startup CLI adapter override applied");
                    }
                    Err(e) => {
                        eprintln!(
                            "Adapter override failed: --{}='{}' ({})",
                            crate::domain::services::adapter_overlay::port_label(*port),
                            name,
                            e
                        );
                        std::process::exit(1);
                    }
                }
            }
        }
    }

    // Story 8.3 AC-9 — replace legacy direct port extraction with agent_core-sourced
    let persona: Arc<dyn PersonaPort> = Arc::clone(&*agent_core_inner.persona.load());
    let tools: Arc<dyn ToolSetPort> = Arc::clone(&*agent_core_inner.tools.load());

    // Story 14.3b — hoist the orchestrator out of the composite-tools block so
    // AppState can hold it. The orchestrator is bound to the SAME ports as the
    // runner (AC8); those ports only exist when the toolset is the
    // CompositeToolsetAdapter (the app default). A non-composite toolset opts
    // out of the subagent subsystem entirely, so `/fanout` is unavailable there
    // by construction.
    let mut orchestrator: Option<Arc<dyn crate::domain::ports::Orchestrator>> = None;
    // Story 10.2 — wire subagent provider into CompositeToolsetAdapter
    {
        use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
        if let Some(composite) = tools.as_any().downcast_ref::<CompositeToolsetAdapter>() {
            #[cfg(feature = "a2a")]
            let a2a_provider_concrete: Option<
                Arc<crate::adapters::a2a::provider::A2aProvider>,
            > = {
                let mut bindings = Vec::with_capacity(resolved.a2a_peers.len());
                for spec in resolved.a2a_peers.iter().cloned() {
                    let client = Arc::new(
                        crate::adapters::a2a::client::A2aClientAdapter::new(&spec, None).map_err(
                            |error| {
                                anyhow::anyhow!(
                                    "A2A peer {:?} configuration failed: {error}",
                                    spec.id
                                )
                            },
                        )?,
                    );
                    bindings.push((spec, client));
                }

                let refresh_bindings = bindings.clone();
                let a2a_provider =
                    Arc::new(crate::adapters::a2a::provider::A2aProvider::new(bindings));
                composite.set_a2a_provider(
                    a2a_provider.clone() as Arc<dyn crate::domain::ports::CapabilityProvider>
                );

                for (spec, client) in refresh_bindings {
                    let event_tx = domain_tx.clone();
                    tokio::spawn(async move {
                        match client.refresh_agent_card(&spec).await {
                            Ok(()) => {
                                let skill_count = client
                                    .cached_card()
                                    .await
                                    .map(|(card, _)| card.skills.len())
                                    .unwrap_or(0);
                                let _ = event_tx.send(AppEvent::A2aCatalogChanged {
                                    peer_id: spec.id,
                                    skill_count,
                                });
                            }
                            Err(error) => {
                                tracing::warn!(
                                    peer_id = %spec.id,
                                    %error,
                                    "A2A AgentCard refresh failed"
                                );
                            }
                        }
                    });
                }
                Some(a2a_provider)
            };

            // Eager agent discovery (needed for SubagentProvider::discover)
            let agent_registry = Arc::new(tokio::sync::RwLock::new(
                crate::adapters::agent_registry::AgentRegistry::discover(&workspace_path),
            ));

            let root_authority = crate::domain::models::CapabilityToken::r1_root(
                crate::domain::models::AgentId::root(),
            );
            let node_journal = Arc::new(
                crate::infrastructure::subagent::NodeJournal::open_workspace(&workspace_path)
                    .await
                    .expect("NodeJournal creation failed"),
            );
            let orchestration_clock = Arc::new(crate::domain::clock::SystemClock::default())
                as Arc<dyn crate::domain::clock::Clock>;
            let authority_ledger = Arc::new(
                crate::domain::services::authority_ledger::AuthorityLedger::new(
                    root_authority.clone(),
                    orchestration_clock.clone(),
                )
                .with_journal_sink(
                    node_journal.clone() as Arc<dyn crate::domain::ports::LedgerJournalSink>
                ),
            );
            // Story 17.2c (D4): restore the ledger conservation head from the
            // durable journal so spent budget cannot silently reappear and a
            // grant cannot be double-counted across a restart.
            {
                let records = node_journal
                    .load()
                    .await
                    .expect("NodeJournal load for ledger recovery failed")
                    .into_iter()
                    .filter_map(|entry| match entry.record {
                        crate::domain::models::JournalRecord::LedgerConservation(record) => {
                            Some(record)
                        }
                        _ => None,
                    });
                authority_ledger.recover_conservation(records);
            }
            // Construct subagent infrastructure first so trust-drop revoke can
            // route into cascade_kill (AC5): the provider holds an Arc<NodeTree>.
            let now_fn = {
                use crate::domain::clock::Clock;
                let clock = Arc::new(crate::domain::clock::SystemClock::default());
                Arc::new(move || clock.wall_now_ms())
            };
            let subagent_registry = Arc::new(
                crate::infrastructure::subagent::NodeTree::with_event_tx(domain_tx.clone(), now_fn)
                    .with_journal(node_journal.clone())
                    .with_host_binding(crate::infrastructure::subagent::current_host_binding(
                        &workspace_path,
                    ))
                    .with_on_cascade_kill({
                        let authority_ledger = authority_ledger.clone();
                        Arc::new(move |id| {
                            let _ = authority_ledger.revoke_scope(id);
                        })
                    }),
            );
            // Rehydrate the process-local NodeTree before any fork-join resume.
            // The daemon singleton serializes reconciliation's journal folds;
            // it is released before the shorter per-wave journal claim.
            let recovery_report =
                match crate::infrastructure::subagent::DaemonSingletonLock::try_acquire(
                    &workspace_path,
                )
                .await
                {
                    Ok(singleton) => {
                        let host_id =
                            crate::infrastructure::subagent::current_host_id(&workspace_path);
                        match crate::infrastructure::subagent::NodeRecovery::reconcile(
                            &node_journal,
                            subagent_registry.as_ref(),
                            &singleton,
                            &host_id,
                        )
                        .await
                        {
                            Ok(report) => Some(report),
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    "interactive node recovery failed; durable parks remain for a later restart"
                                );
                                None
                            }
                        }
                    }
                    Err(crate::infrastructure::subagent::RecoveryError::SingletonBusy) => {
                        tracing::info!(
                            "another process owns node reconciliation; this process will not resume its parks"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "node recovery lock failed; durable parks remain for a later restart"
                        );
                        None
                    }
                };
            // Story 17.4b: now that the node tree and durable journal exist,
            // inject the A2A delegation runtime so `A2aProvider::invoke` can
            // materialize peer nodes and journal room events (durable-first).
            #[cfg(feature = "a2a")]
            if let Some(provider) = a2a_provider_concrete.as_ref() {
                provider.set_delegation_runtime(Arc::new(
                    crate::adapters::a2a::driver::A2aDelegationRuntime::new(
                        subagent_registry.as_ref().clone(),
                        Some(node_journal.clone()),
                        domain_tx.clone(),
                    ),
                ));
            }
            // Story 17.5a — inject the MCP Tasks runtime into every MCP
            // client now that the node tree, journal, and clock all exist.
            // One runtime per client: `disconnect` cascades only that
            // client's own task nodes (AC5). Lifecycle flows exclusively
            // through the domain seams (`TaskNodes` / `SupervisedNodes` /
            // `RoomJournal`); the MCP adapter never holds a concrete
            // `NodeTree`/`NodeJournal` (ADR-17-5-01 D2).
            #[cfg(feature = "mcp")]
            let mcp_task_runtimes: Vec<
                Arc<crate::adapters::mcp::task_driver::McpTaskRuntime>,
            > = {
                let task_nodes: Arc<dyn crate::domain::ports::TaskNodes> =
                    subagent_registry.clone();
                let supervised: Arc<dyn crate::domain::ports::SupervisedNodes> =
                    subagent_registry.clone();
                let room: Arc<dyn crate::domain::ports::RoomJournal> =
                    Arc::new(crate::infrastructure::subagent::NodeRoomJournal::new(
                        node_journal.clone(),
                        Some(domain_tx.clone()),
                    ));
                let task_clock: Arc<dyn crate::domain::clock::Clock> =
                    Arc::new(crate::domain::clock::SystemClock::default());
                let mut runtimes = Vec::new();
                for client in composite.mcp_clients() {
                    let runtime = Arc::new(crate::adapters::mcp::task_driver::McpTaskRuntime::new(
                        task_nodes.clone(),
                        supervised.clone(),
                        room.clone(),
                        task_clock.clone(),
                    ));
                    client.set_task_runtime(Arc::clone(&runtime));
                    runtimes.push(runtime);
                }
                runtimes
            };
            let authority_provider: Arc<dyn crate::domain::ports::AuthorityProvider> = Arc::new(
                crate::adapters::authority::InProcessAuthorityProvider::new(
                    authority_ledger.clone(),
                )
                .with_node_tree(subagent_registry.clone()),
            );
            let spool = Arc::new(
                crate::infrastructure::subagent::SubagentSpool::new(
                    paths::data_dir()
                        .unwrap_or_else(|_| workspace_path.join(".rustain"))
                        .join("spool"),
                )
                .await
                .expect("SubagentSpool creation failed"),
            );

            // Minimal ProviderInfoPort wrapper for SubagentProvider
            struct StartupProviderInfo {
                router: Arc<crate::adapters::provider::ProviderRouter>,
                registry: Arc<crate::adapters::provider::ProviderRegistry>,
            }
            impl crate::domain::ports::ProviderInfoPort for StartupProviderInfo {
                fn active_delegate_id(&self) -> String {
                    self.router.active_delegate_id()
                }
                fn get_model(
                    &self,
                    provider_id: &str,
                    model_id: &str,
                ) -> Option<crate::domain::models::provider::ModelDescriptor> {
                    self.registry.get_model(provider_id, model_id)
                }
                fn get_model_provider(
                    &self,
                    model_id: &str,
                    prefer: Option<&str>,
                ) -> Option<String> {
                    self.registry.get_model_provider(model_id, prefer)
                }
                fn list_providers(
                    &self,
                ) -> Vec<crate::domain::models::provider::ProviderDescriptor> {
                    self.registry.list_providers()
                }
                fn list_models_by_provider(
                    &self,
                    provider_id: &str,
                ) -> Vec<crate::domain::models::provider::ModelDescriptor> {
                    self.registry.list_models_by_provider(provider_id)
                }
                fn get_provider(
                    &self,
                    provider_id: &str,
                ) -> Option<Arc<dyn crate::domain::ports::StreamingProvider>> {
                    self.router.get_provider(provider_id)
                }
                fn set_active_provider(
                    &self,
                    provider_id: &str,
                ) -> Result<(), crate::domain::errors::ProviderError> {
                    self.router.set_active(provider_id)
                }
                fn now_unix(&self) -> i64 {
                    crate::infrastructure::clock_util::now_unix()
                }
                fn today_start_unix_ms(&self) -> i64 {
                    crate::infrastructure::clock_util::today_start_unix_ms()
                }
            }

            let model_router: Arc<dyn crate::domain::ports::ProviderInfoPort> =
                Arc::new(StartupProviderInfo {
                    router: router.clone(),
                    registry: provider_registry.clone(),
                });

            // ToolScheduler for the runner (child body is v0 no-op; scheduler unused until 10.7)
            let subagent_scheduler = crate::domain::services::tool_scheduler::ToolScheduler::new(
                security.clone(),
                _tools_direct.clone(),
                approval_runtime.clone(),
                1024,
            );

            let isolation_provider: Arc<dyn crate::domain::ports::IsolationProvider> =
                Arc::new(crate::adapters::isolation::CowIsolationProvider::default());
            let tools_factory_storage = tools_storage.clone();
            let tools_factory_sandbox_slot = sandbox_slot.clone();
            let tools_factory_sandbox_policy = sandbox_policy_ref.clone();
            let tools_factory_skill_activator = skill_activator.clone();
            let tools_factory_skill_cache = shared_skill_cache.clone();
            let tools_factory_plan_manager = plan_manager.clone();
            let tools_factory_domain_tx = domain_tx.clone();
            let tools_factory: Arc<
                dyn Fn(&std::path::Path) -> Arc<dyn crate::domain::ports::ToolSetPort>
                    + Send
                    + Sync,
            > = Arc::new(move |workspace| {
                let mut adapter = crate::adapters::toolset_adapter::ToolSetAdapter::new(
                    workspace.to_path_buf(),
                    tools_factory_storage.clone(),
                    tools_factory_sandbox_slot.clone(),
                    tools_factory_sandbox_policy.clone(),
                );
                adapter.set_activator(tools_factory_skill_activator.clone());
                adapter.set_skill_cache(tools_factory_skill_cache.clone());
                adapter.set_plan_manager(tools_factory_plan_manager.clone());
                adapter.set_event_tx(tools_factory_domain_tx.clone());
                Arc::new(adapter) as Arc<dyn crate::domain::ports::ToolSetPort>
            });

            let runner: Arc<dyn crate::domain::ports::SubagentRunner> = Arc::new(
                crate::adapters::subagent::InProcessSubagentRunner::new(
                    router.clone() as Arc<dyn crate::domain::ports::StreamingProvider>,
                    tools_storage.clone(),
                    security.clone(),
                    _tools_direct.clone(),
                    approval_runtime.clone(),
                    subagent_scheduler,
                    event_bus.clone(),
                    subagent_registry.clone(),
                    sandbox_policy_ref.clone(),
                    spool.clone(),
                    authority_provider.clone(),
                    root_authority.clone(),
                )
                .with_isolation(
                    workspace_path.clone(),
                    isolation_provider,
                    tools_factory,
                ),
            );

            // Story 14.3 — fork-join orchestrator, bound to the SAME ports as
            // the runner (AC8: drives children through the SubagentRunner port,
            // never a concrete impl). The executor is the multi-node
            // generalization of the single-turn driver; the coordinator's turn
            // loop invokes `run_fork_join` when it fans out (the trigger — a
            // model fan-out intent — is the connection point wired with the
            // turn-loop integration).
            let supervisor = Arc::new(
                crate::infrastructure::supervisor::Supervisor::new(
                    crate::domain::models::FORK_JOIN_SPAWN_CAP,
                    crate::domain::models::FORK_JOIN_SPAWN_CAP,
                    authority_ledger.clone(),
                    root_authority.clone(),
                    orchestration_clock.clone(),
                    event_bus.clone(),
                )
                .with_journal(node_journal.clone())
                .with_nodes(
                    subagent_registry.clone() as Arc<dyn crate::domain::ports::SupervisedNodes>
                ),
            );
            let recovered_occupancy = subagent_registry
                .list()
                .await
                .into_iter()
                .filter(|entry| {
                    matches!(
                        entry.current_status,
                        crate::domain::models::NodeState::Running
                            | crate::domain::models::NodeState::Waiting
                    )
                })
                .count();
            supervisor
                .derive_recovered_occupancy(recovered_occupancy)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let artifact_store: Arc<dyn crate::domain::ports::ArtifactStore> = Arc::new(
                crate::adapters::artifact::FileSystemArtifactStore::new(&workspace_path),
            );
            let artifact_host =
                crate::infrastructure::subagent::current_host_binding(&workspace_path);
            // 17.5b (Task 6): wire the input-request artifact sink into every
            // MCP task runtime. The coordinator authority + host are
            // orchestrator-only fields; the sink impl supplies them so the
            // adapter stays free of authority plumbing.
            #[cfg(feature = "mcp")]
            {
                let sink_room: Arc<dyn crate::domain::ports::RoomJournal> =
                    Arc::new(crate::infrastructure::subagent::NodeRoomJournal::new(
                        node_journal.clone(),
                        Some(domain_tx.clone()),
                    ));
                let sink: Arc<dyn crate::domain::ports::ArtifactSink> =
                    Arc::new(crate::infrastructure::subagent::JournalArtifactSink::new(
                        artifact_store.clone(),
                        sink_room,
                        root_authority.id,
                        artifact_host.clone(),
                    ));
                for runtime in &mcp_task_runtimes {
                    runtime.set_artifact_sink(sink.clone());
                }
            }
            let patch_merge_back =
                Arc::new(crate::infrastructure::orchestrator::PatchMergeBack::new(
                    workspace_path.clone(),
                    artifact_store.clone(),
                    node_journal.clone(),
                    event_bus.clone(),
                    Arc::new(crate::adapters::merge_back::GitPatchApplier),
                ));
            let fork_join_executor = Arc::new(
                crate::infrastructure::orchestrator::ForkJoinExecutor::new(
                    runner.clone(),
                    authority_provider.clone(),
                    authority_ledger.clone(),
                    event_bus.clone(),
                    orchestration_clock,
                    root_authority.clone(),
                )
                .with_journal(node_journal.clone())
                .with_supervisor(supervisor)
                .with_artifact_store(artifact_store, artifact_host)
                .with_patch_merge_back(patch_merge_back)
                // Story 17.3c (D1): preserve the pre-isolation direct-write
                // contract — user-originated fanout edits auto-apply through the
                // journal-authoritative gate; self-originated stay review-gated.
                .with_merge_back_policy(crate::domain::services::patch_review::MergeBackPolicy {
                    auto_approve_user_originated: true,
                })
                .with_permission_source(security.clone()),
            );
            let orchestrator_inner: Arc<dyn crate::domain::ports::Orchestrator> =
                fork_join_executor.clone();
            orchestrator = Some(orchestrator_inner);

            let subagent_provider = Arc::new(crate::adapters::subagent::SubagentProvider::new(
                runner,
                subagent_registry.clone(),
                agent_registry,
                model_router,
                spool.clone(),
            ));
            subagent_provider
                .set_authority(authority_provider.clone(), root_authority.clone())
                .await;

            composite.set_subagent_provider(subagent_provider);

            // Resume only after composition is complete. The wave runs under a
            // supervised Tokio task so a long-lived provider cannot block
            // application startup; the durable claim prevents another process
            // from selecting the same park concurrently.
            if let Some(recovery_report) = recovery_report
                && !recovery_report.parked.is_empty()
            {
                let resume_executor = fork_join_executor.clone();
                tokio::spawn(async move {
                    match resume_executor.resume_fork_join_run(&recovery_report).await {
                        Ok(resumed) if !resumed.is_empty() => {
                            tracing::info!(
                                waves = resumed.len(),
                                "resumed durably parked fork-join wave(s) after restart"
                            );
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "fork-join resumption failed; parked spokes remain durable for the next restart"
                            );
                        }
                    }
                });
            }

            // Story 14-4a (AC5, CS-1) — re-store the agent_message_bus slot with
            // a LocalMessageBus wired to the REAL subagent_registry tree (not the
            // phantom Default::default() empty tree composed at AgentCore::compose).
            // This matches the 11-slot re-store pattern (sandbox, tool_exposure, etc.).
            agent_core_inner.agent_message_bus.store(Arc::new(Arc::new(
                crate::infrastructure::agent_message_bus::LocalMessageBus::new(
                    (*subagent_registry).clone(),
                    Arc::new(crate::domain::ports::RelationshipDeliveryPolicy),
                ),
            )
                as Arc<dyn crate::domain::ports::AgentMessageBus>));
        }
    }
    // PATCH-6 (review): the orchestrator stays an `Option`. A non-composite
    // toolset override legitimately has no subagent subsystem (no runner), so
    // `/fanout` surfaces a SystemNotice ("fan-out unavailable with the current
    // toolset") instead of the process panicking at startup. The normal
    // (composite-toolset) path sets it. `orchestrator` (the Option from above)
    // flows to AppState::new unchanged.

    // Deferred AppState::new — now that AgentCore is composed, create the runtime state
    // Story 9.5 — telemetry aggregator (7-day rolling window for active-ratio metrics).
    let telemetry = {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from(".").join(".cache"))
            .join("rustain")
            .join("telemetry");
        crate::infrastructure::telemetry::ActiveRatioWindow::new(Some(
            cache_dir.join("active_ratio_window.json"),
        ))
        .await
    };

    // Story 17.3a (Tasks 6 + 7) — composition-root opt-in point for the WASM
    // execution sandbox (`WasmIsolationBackend`, the sole file that may name the
    // concrete type). Party ruling N4: there is NO untrusted-tool/extension
    // population in rustain today, so this binding is intentionally inert —
    // `None`, not configured — and NO production tool dispatch routes through
    // it. When such a population exists (Epic 18 territory), construct
    // `crate::adapters::wasm::WasmIsolationBackend` here and inject the
    // `Arc<dyn ExecutionSandbox>` BELOW `ToolSetPort::execute`; the approval
    // seam (`ApprovalRuntime` / `PermissionMode`) and `ToolScheduler` stay
    // untouched ("subprocess-today → WASM-later changes no call site"). The
    // proving consumer for 17.3a is the adversarial fixture suite, not this
    // call site (`tests/wasm_execution_sandbox.rs`).
    #[cfg(feature = "wasm-sandbox")]
    let _execution_sandbox: Option<std::sync::Arc<dyn crate::domain::ports::ExecutionSandbox>> =
        None;

    #[cfg(feature = "meta-search")]
    let catalog_registry_for_app_state = _catalog_registry.clone();

    let (app_state, domain_rx) = AppState::new(
        event_bus,
        domain_rx,
        approval_runtime.clone(),
        sandbox_policy_ref,
        plan_manager.clone(),
        plan_injector.clone(),
        provider_swap,
        provider_registry.clone(),
        usage_ledger,
        budget_state_store,
        app_config_swap.clone(),
        agent_core_inner,
        orchestrator,
        compose_snapshot,
        profile_resolver_swap,
        cli_snapshot,
        cli_config_overrides.clone(),
        telemetry,
        #[cfg(feature = "meta-search")]
        catalog_registry_for_app_state,
    );

    // 5d. Use the same storage adapter constructed above for session management.
    // Both tools and the event loop share one FileSystemStorage instance pointing
    // to the same sessions directory (Story 4-3b: snapshots are co-located with conversations).
    let sessions_dir = tools_sessions_dir.clone();
    // Downcast to FileSystemStorage to access ensure_dir (concrete method).
    // Mirror the workspace_root configuration from `tools_storage` above.
    // AC1: CLI --snapshot-retention takes precedence over config file value.
    let retention = cli
        .snapshot_retention
        .or(app_config.snapshot_retention_count);
    let storage = FileSystemStorage::with_workspace_root(sessions_dir, workspace_path.clone())
        .with_workspace_registrar(Arc::new(FileWorkspaceRegistry::new()?))
        .with_snapshot_retention(retention);
    if let Err(e) = storage.ensure_dir().await {
        tracing::warn!("Failed to create sessions directory: {}", e);
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: format!("Session persistence unavailable: {}", e),
        });
    }

    // DF-109 (AC3): Reconcile any rewind transactions that were interrupted by a crash.
    // Must run before session restoration so that recovered conversations are in a
    // consistent state when we attempt to load them.
    use crate::domain::ports::StoragePort as _;
    if let Err(e) = storage.reconcile_pending_txns().await {
        tracing::warn!("Failed to reconcile pending rewind transactions: {}", e);
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

    signals::set_shutdown_sender(app_state.event_bus.domain_tx.clone());
    signals::set_event_bus(app_state.event_bus.clone());
    signals::set_session_cancel(app_state.session_cancel.clone());
    signals::install_signal_handlers().await;

    // 5e. Construct clipboard adapter
    #[cfg(feature = "clipboard")]
    let clipboard: Arc<dyn ClipboardPort> =
        Arc::new(crate::adapters::clipboard_adapter::ArboardClipboard::new());
    #[cfg(not(feature = "clipboard"))]
    let clipboard: Arc<dyn ClipboardPort> =
        Arc::new(crate::adapters::clipboard_adapter::NoOpClipboard::new());

    // 7. Setup terminal (mouse capture gated by config + RUSTAIN_NO_MOUSE env. Story 16.8, AC14)
    let mouse_enabled = app_config_swap.load().mouse.capture
        && crate::infrastructure::utils::env_var_trimmed("RUSTAIN_NO_MOUSE")
            != Some("1".to_string());
    let mut tui = terminal::setup(mouse_enabled)?;

    // P13: AC14 first-launch hint — if mouse capture is active, inform the user.
    if mouse_enabled {
        let _ = domain_tx.send(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Info,
            message: "Mouse scroll enabled. Hold Shift to select text for copy.".to_string(),
        });
    }

    // Story 9.1 — MCP lazy-connect: spawn after terminal setup, before event loop (NFR10).
    #[cfg(feature = "mcp")]
    {
        use crate::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
        if let Some(composite) = tools.as_any().downcast_ref::<CompositeToolsetAdapter>() {
            composite.start_mcp_connections();
            // Story 9.3a — populate the capability registry (best-effort at startup;
            // the registry will also populate via McpCatalogChanged events post-connect).
            if let Err(e) = composite.populate_registry().await {
                tracing::warn!(error = %e, "Capability registry population failed at startup; will retry on MCP catalog changes");
            }
            // Story 9.7 Phase B — wire catalog delta broadcast into the composite adapter
            #[cfg(feature = "meta-search")]
            if let Some(reg) = &_catalog_registry {
                composite.set_catalog_broadcast(reg.tool_sender.clone());
                let _ = capability_registry_slot.set(Arc::clone(composite.capability_registry()));
                reg.rebuild_now();
                tracing::debug!(
                    tools = composite.capability_registry().snapshot().len(),
                    "P-R2-1: initial MergedIndex built from live catalog"
                );
            }
        }
    }

    // models.dev live pricing — best-effort background refresh (60-min spaced).
    // Keeps the disk cache fresh; `config` load merges it into the effective
    // pricing map. Detached + non-blocking: failures log and retry next cycle.
    // Mirrors opencode's spaced-refresh posture.
    #[cfg(feature = "models-dev")]
    {
        tokio::spawn(async {
            use crate::adapters::models_dev::{CACHE_TTL, load_cache, refresh};
            loop {
                // Skip the network fetch when the on-disk cache is still fresh —
                // avoids a blocking HTTP request on every app launch.
                let fresh = load_cache()
                    .map(|c| !c.is_stale(CACHE_TTL))
                    .unwrap_or(false);
                if !fresh {
                    if let Err(e) = refresh().await {
                        tracing::warn!(error = %e, "models.dev pricing refresh failed; will retry");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
            }
        });
    }

    let result = event_loop::run(
        &mut tui,
        domain_rx,
        app_state,
        router.clone(),
        router.clone(),
        security,
        tools,
        persona,
        storage_port,
        storage,
        clipboard,
        workspace_path,
        restored_conversation,
        recovery_prompt,
        skill_activator,
        agent_activator,
        approval_runtime,
        progress_tx,
        progress_rx,
        Some(refresh_tracker),
    )
    .await;

    // 9. Teardown terminal (always, even on error)
    if let Err(e) = terminal::teardown(mouse_enabled) {
        tracing::error!("Terminal teardown failed: {}", e);
    }
    tracing::info!("Rustain shutdown complete.");

    result
}

/// Extract the provider construction logic for testability.
/// Use named fields — do NOT revert to a tuple alias. Story 7.6 amendment.
pub struct ProviderLayer {
    pub router: Arc<crate::adapters::provider::ProviderRouter>,
    pub registry: Arc<crate::adapters::provider::ProviderRegistry>,
    pub deferred_notices: Vec<(String, ProviderError)>,
    pub active_id: String,
    /// Providers where `discover_models = true` but the kind doesn't support it (Story 7.6 AC7).
    pub unsupported_discovery: Vec<(String, String)>, // (provider_id, kind)
    #[cfg(feature = "openai")]
    pub discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget>,
    #[cfg(not(feature = "openai"))]
    pub discovery_targets: Vec<()>,
}

pub fn init_provider_layer(app_config: &crate::domain::models::AppConfig) -> ProviderLayer {
    // Clear the openai adapter cache so stale references from previous sessions
    // (e.g., hot-reload in tests) don't leak into the new ProviderLayer.
    #[cfg(feature = "openai")]
    crate::infrastructure::provider_factory::clear_openai_adapters();

    let provider_registry = Arc::new(crate::adapters::provider::ProviderRegistry::new());
    let router = Arc::new(crate::adapters::provider::ProviderRouter::new(
        "anthropic".to_string(),
    ));
    let mut deferred_notices: Vec<(String, ProviderError)> = Vec::new();
    let mut unsupported_discovery: Vec<(String, String)> = Vec::new();

    #[cfg(feature = "openai")]
    let mut discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget> =
        Vec::new();

    let enabled_configs: Vec<(&String, &ProviderConfig)> = app_config
        .provider
        .iter()
        .filter(|(_id, cfg)| cfg.enabled)
        .collect();

    let use_config_path = !app_config.provider.is_empty() && !enabled_configs.is_empty();

    let active_id = if use_config_path {
        let mut first_enabled_id: Option<String> = None;
        for (id, cfg) in enabled_configs {
            if id != &cfg.provider_id {
                tracing::warn!(
                    "Provider config key '{}' does not match provider_id '{}'; using key",
                    id,
                    cfg.provider_id
                );
            }

            // Build provider FIRST; only add discovery target if construction succeeds (Story 7.6 AC5).
            let _provider_built =
                match crate::infrastructure::provider_factory::build_provider_for_config(id, cfg) {
                    Ok(adapter) => {
                        let adapter_arc = Arc::clone(&adapter);
                        router.register(adapter);
                        provider_registry.register_arc(adapter_arc);
                        if first_enabled_id.is_none() {
                            first_enabled_id = Some(id.clone());
                        }
                        tracing::info!("Provider '{}' registered from config", id);
                        true
                    }
                    Err(e) => {
                        tracing::warn!("Failed to construct provider '{}': {}", id, e);
                        deferred_notices.push((id.clone(), e));
                        false
                    }
                };

            #[cfg(feature = "openai")]
            // Build typed OpenAI adapter for discovery (Story 7.6 AC5)
            if _provider_built && cfg.discover_models {
                match crate::infrastructure::provider_factory::build_openai_for_discovery(id, cfg) {
                    Ok(Some(adapter)) => {
                        discovery_targets.push(
                            crate::adapters::model_catalog_cache::DiscoveryTarget {
                                provider_id: id.clone(),
                                adapter,
                                cache_ttl_seconds: cfg.cache_ttl_seconds,
                                model_filter: cfg.model_filter.clone(),
                            },
                        );
                    }
                    Ok(None) => {
                        // Anthropic or Ollama — warn that discovery is not supported
                        tracing::warn!(
                            "Provider '{}' (kind={}) uses a static catalog — dynamic discovery is not yet supported. \
                             Edit [providers.{}] in config.toml to remove discover_models, or accept the static list.",
                            id,
                            cfg.kind.as_deref().unwrap_or(id),
                            id
                        );
                        unsupported_discovery
                            .push((id.clone(), cfg.kind.as_deref().unwrap_or(id).to_string()));
                    }
                    Err(e) => {
                        tracing::warn!("Failed to build discovery adapter for '{}': {}", id, e);
                    }
                }
            }
        }
        first_enabled_id.unwrap_or_else(|| "anthropic".to_string())
    } else {
        if !app_config.provider.is_empty() {
            tracing::info!(
                "No enabled providers in [provider] config; using legacy ANTHROPIC env-var path"
            );
        }
        match build_anthropic_provider_from_env(app_config) {
            Ok(adapter) => {
                let adapter_arc = Arc::clone(&adapter);
                router.register(adapter);
                provider_registry.register_arc(adapter_arc);
                "anthropic".to_string()
            }
            Err(e) => {
                tracing::warn!("Legacy Anthropic fallback failed: {}", e);
                deferred_notices
                    .push(("anthropic".to_string(), ProviderError::Other(e.to_string())));
                "anthropic".to_string()
            }
        }
    };

    if let Err(e) = router.set_active(&active_id) {
        tracing::warn!("Failed to set active provider '{}': {}", active_id, e);
    }

    if provider_registry.provider_ids().is_empty() {
        tracing::warn!(
            "No providers registered — rustain will launch but all completion requests will fail. \
             Configure providers via `rustain init` or add [provider.*] sections to config."
        );
    }

    #[cfg(feature = "openai")]
    {
        ProviderLayer {
            router,
            registry: provider_registry,
            deferred_notices,
            active_id,
            unsupported_discovery,
            discovery_targets,
        }
    }
    #[cfg(not(feature = "openai"))]
    {
        ProviderLayer {
            router,
            registry: provider_registry,
            deferred_notices,
            active_id,
            unsupported_discovery,
            discovery_targets: Vec::new(),
        }
    }
}

/// Build the Anthropic provider from environment variables (legacy fallback).
///
/// Auth precedence (CC-compatible): `ANTHROPIC_AUTH_TOKEN` > `ANTHROPIC_API_KEY`.
/// - `ANTHROPIC_AUTH_TOKEN` → `Authorization: Bearer {token}` (gateways/proxies)
/// - `ANTHROPIC_API_KEY` → `X-Api-Key: {key}` (direct Anthropic)
fn build_anthropic_provider_from_env(
    config: &crate::domain::models::AppConfig,
) -> Result<Arc<dyn StreamingProvider>> {
    #[cfg(feature = "anthropic")]
    {
        use crate::adapters::anthropic::AuthMode;

        // 1. Resolve auth: ANTHROPIC_AUTH_TOKEN > ANTHROPIC_API_KEY (CC precedence)
        let auth_token = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_AUTH_TOKEN");
        let api_key = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_API_KEY");

        if auth_token.is_some() && api_key.is_some() {
            tracing::warn!(
                "Both ANTHROPIC_AUTH_TOKEN and ANTHROPIC_API_KEY are set; using ANTHROPIC_AUTH_TOKEN (Bearer auth)"
            );
        }

        let auth_mode = if let Some(token) = auth_token {
            tracing::info!("Using ANTHROPIC_AUTH_TOKEN (Bearer auth)");
            AuthMode::BearerToken(token.into())
        } else if let Some(key) = api_key {
            tracing::info!("Using ANTHROPIC_API_KEY (X-Api-Key auth)");
            AuthMode::ApiKey(key.into())
        } else if let Some(stored_key) =
            crate::adapters::auth_store::FileAuthStore::get_sync("anthropic")
        {
            // Story 13.4a AC7: auth.json fallback — strictly below env vars.
            tracing::info!("Using stored credential from auth.json (X-Api-Key auth)");
            AuthMode::ApiKey(stored_key.into())
        } else {
            anyhow::bail!(
                "No API key found.\n\n\
                 Set one of:\n\
                 \n\
                 export ANTHROPIC_API_KEY=sk-ant-...       # Direct Anthropic\n\
                 export ANTHROPIC_AUTH_TOKEN=your-key       # Anthropic-compatible gateway\n\
                 rustain auth login anthropic               # Store via auth.json\n\
                 \n\
                 Get your API key at: https://console.anthropic.com/"
            );
        };

        // 2. Resolve base URL (filter empty to preserve default)
        let base_url = crate::infrastructure::utils::env_var_trimmed("ANTHROPIC_BASE_URL");
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

/// Spawn a background periodic catalog refresh timer (Story 7.7 AC3).
///
/// Fires every 4 hours aligned to UTC hour boundaries (00:00, 04:00, 08:00, ...).
/// On each tick, re-fetches `/v1/models` for every provider with `discover_models = true`.
/// Emits `ProviderCatalogRefreshed` on success, `SystemNotice` on failure.
#[cfg(feature = "openai")]
fn spawn_periodic_catalog_refresh(
    cache: crate::adapters::model_catalog_cache::ModelCatalogCache,
    discovery_targets: Vec<crate::adapters::model_catalog_cache::DiscoveryTarget>,
    refresh_tracker: std::sync::Arc<crate::adapters::tui::refresh_tracker::RefreshTracker>,
    domain_tx: tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
) {
    if discovery_targets.is_empty() {
        return;
    }

    tokio::spawn(async move {
        use chrono::Timelike;

        // Align to next UTC 4h boundary
        let now = chrono::Utc::now();
        let current_hour = now.hour();
        let next_boundary_hour = ((current_hour / 4) + 1) * 4;
        let next_boundary = if next_boundary_hour >= 24 {
            // Roll to next day at 00:00 UTC
            now.date_naive()
                .succ_opt()
                .unwrap_or(now.date_naive())
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(now + chrono::Duration::hours(4))
        } else {
            now.date_naive()
                .and_hms_opt(next_boundary_hour, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(now + chrono::Duration::hours(4))
        };

        let until_first = (next_boundary - now)
            .to_std()
            .unwrap_or(std::time::Duration::from_secs(3600 * 4));
        tracing::info!(
            "Periodic catalog refresh: first tick in {:.1}m (next UTC boundary {:02}:00)",
            until_first.as_secs_f64() / 60.0,
            next_boundary_hour % 24,
        );

        tokio::time::sleep(until_first).await;

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600 * 4));
        // First tick fires immediately after sleep
        interval.tick().await;

        loop {
            interval.tick().await;
            tracing::info!("Periodic catalog refresh tick");

            for target in &discovery_targets {
                let provider_id = target.provider_id.clone();
                let adapter = target.adapter.clone();
                let model_filter = target.model_filter.clone();
                let cache = cache.clone();
                let domain_tx = domain_tx.clone();
                let tracker = refresh_tracker.clone();

                tokio::spawn(async move {
                    let _guard = tracker.insert(provider_id.clone());
                    match adapter.fetch_remote_models(&model_filter).await {
                        Ok(models) => {
                            if models.is_empty() {
                                tracing::warn!(
                                    "Periodic refresh: empty catalog from '{}'",
                                    provider_id
                                );
                                let _ = domain_tx.send(AppEvent::SystemNotice {
                                    conversation_id: None,
                                    level: NoticeLevel::Warning,
                                    message: format!(
                                        "Model catalog for '{}' returned empty — keeping current models",
                                        provider_id
                                    ),
                                });
                                return;
                            }
                            let _lock = cache.lock().await;
                            let mut catalog = cache.load().await;
                            let models_with_stale =
                                crate::adapters::model_catalog_cache::merge_with_live(
                                    catalog.providers.get(&provider_id),
                                    &models,
                                );
                            adapter.set_discovered_models(models_with_stale.clone());
                            catalog.providers.insert(
                                provider_id.clone(),
                                crate::adapters::model_catalog_cache::CachedProviderEntry {
                                    fetched_at_unix: crate::infrastructure::clock_util::now_unix(),
                                    models: models_with_stale,
                                },
                            );
                            if let Err(e) = cache.save(&catalog).await {
                                tracing::warn!("Periodic refresh save failed: {}", e);
                            }
                            let _ =
                                domain_tx.send(AppEvent::ProviderCatalogRefreshed { provider_id }); // CONFORMANCE_EXCEPTION_EVENTBUS_BYPASS: Story 7.7 AC3 — periodic refresh redraw signal
                        }
                        Err(e) => {
                            tracing::warn!("Periodic refresh for '{}' failed: {}", provider_id, e);

                            // AC3: mark existing models as stale on refresh failure
                            let current = adapter.list_models();
                            let stale_entries: Vec<
                                crate::adapters::model_catalog_cache::CachedModelEntry,
                            > = current
                                .into_iter()
                                .map(|mut m| {
                                    m.stale = true;
                                    crate::adapters::model_catalog_cache::CachedModelEntry {
                                        descriptor: m,
                                    }
                                })
                                .collect();
                            if !stale_entries.is_empty() {
                                adapter.set_discovered_models(stale_entries);
                            }

                            let _ = domain_tx.send(AppEvent::SystemNotice {
                                conversation_id: None,
                                level: NoticeLevel::Warning,
                                message: format!(
                                    "Model catalog refresh for '{}' failed: {} — showing current models",
                                    provider_id, e
                                ),
                            });
                        }
                    }
                });
            }
        }
    });
}

/// Story 9.6 — validate the `skill_exposure.kind` config value at startup.
///
/// Phase A accepts ONLY `"l1-metadata"` (the DEFAULT) and `"static-full"`.
/// `"meta-search"` produces an actionable error pointing at Story 9.7.
pub fn validate_skill_exposure(kind: &str) -> Result<(), crate::domain::errors::DomainError> {
    use crate::domain::errors::{ConfigError, DomainError};
    if kind.is_empty() {
        return Err(DomainError::Config(ConfigError::Invalid {
            field: "skill_exposure.kind".into(),
            value: "empty exposure strategy value is invalid. \
                    Phase A accepts `\"l1-metadata\"` (the default) or `\"static-full\"` \
                    (codex-parity opt-in)."
                .to_string(),
        }));
    }
    match kind {
        "l1-metadata" | "static-full" => Ok(()),
        #[cfg(feature = "meta-search")]
        "meta-search" => Ok(()),
        #[cfg(not(feature = "meta-search"))]
        "meta-search" => Err(DomainError::Config(ConfigError::Invalid {
            field: "skill_exposure.kind".into(),
            value: "`meta-search` skill exposure strategy requires the `meta-search` cargo feature; \
                     see ADR-09-02 §Phase B Prerequisites. \
                     Compile with `--features meta-search` or set `[skill_exposure].kind = \"l1-metadata\"` \
                     (the default) or remove the key entirely."
                .to_string(),
        })),
        other => Err(DomainError::Config(ConfigError::Invalid {
            field: "skill_exposure.kind".into(),
            value: format!(
                "unknown skill exposure strategy `{}`. Phase A accepts only `\"l1-metadata\"` \
                 (default) and `\"static-full\"` (codex-parity opt-in fallback). \
                 Reserved values: `\"meta-search\"` (Story 9.7 Phase B, currently deferred).",
                other
            ),
        })),
    }
}

/// Story 11.6 — validate the `assembler.strategy` config value at startup.
///
/// Accepts `"passthrough"` (the default) and `"windowing"`. An unknown or empty
/// name produces an actionable error rather than a silent fallback (mirrors
/// `validate_tools_exposure`). NO `GroupingConfig` threshold is configurable —
/// only the strategy name (FR121 / ADR-11-2 "zero user-visible settings").
pub fn validate_assembler_strategy(
    strategy: &str,
) -> Result<(), crate::domain::errors::DomainError> {
    use crate::domain::errors::{ConfigError, DomainError};
    match strategy.trim() {
        "passthrough" | "windowing" => Ok(()),
        "" => Err(DomainError::Config(ConfigError::Invalid {
            field: "assembler.strategy".into(),
            value: "empty assembler strategy is invalid. Accepts `\"passthrough\"` (the \
                    default) or `\"windowing\"` (Story 11.6 Algorithm A+). Remove the \
                    `[assembler]` block to use the default."
                .to_string(),
        })),
        other => Err(DomainError::Config(ConfigError::Invalid {
            field: "assembler.strategy".into(),
            value: format!(
                "unknown assembler strategy `{other}`. Accepts only `\"passthrough\"` \
                 (default, behaviour-preserving) and `\"windowing\"` (Story 11.6 \
                 within-session grouped windowing, Algorithm A+)."
            ),
        })),
    }
}

/// Story 9.4 — validate the `tools.exposure` config value at startup.
///
/// Phase A accepts ONLY `"static-full"`. `"meta-search"` produces an actionable
/// error pointing at Story 9.7. Unknown values produce a generic error.
pub fn validate_tools_exposure(exposure: &str) -> Result<(), crate::domain::errors::DomainError> {
    use crate::domain::errors::{ConfigError, DomainError};
    if exposure.is_empty() {
        return Err(DomainError::Config(ConfigError::Invalid {
            field: "tools.exposure".into(),
            value: "\
                empty exposure strategy value is invalid. \
                Phase A accepts only `\"static-full\"` (the default). \
                Remove `[tools]` block or set `[tools].exposure = \"static-full\"`."
                .to_string(),
        }));
    }
    match exposure {
        "static-full" => Ok(()),
        #[cfg(feature = "meta-search")]
        "meta-search" => Ok(()),
        #[cfg(not(feature = "meta-search"))]
        "meta-search" => Err(DomainError::Config(ConfigError::Invalid {
            field: "tools.exposure".into(),
            value: "`meta-search` exposure strategy requires the `meta-search` cargo feature; \
                     see ADR-09-01 v2.2 §Phase B Prerequisites. \
                     Compile with `--features meta-search` or set `[tools].exposure = \"static-full\"` \
                     (the default) or remove the key entirely."
                .to_string(),
        })),
        other => Err(DomainError::Config(ConfigError::Invalid {
            field: "tools.exposure".into(),
            value: format!(
                "unknown exposure strategy `{}`. Phase A accepts only `\"static-full\"`. \
                 Reserved values: `\"meta-search\"` (Story 9.7 Phase B, requires `--features meta-search`).",
                other
            ),
        })),
    }
}

/// Story 9.5 — validate the `sandbox.adapter` config value at startup.
///
/// Phase A accepts `"noop"` (default on all platforms) and `"landlock"`
/// (Linux only, gated on `sandbox` cargo feature). On Linux without the
/// feature, `"landlock"` produces an actionable error pointing at the
/// `sandbox` cargo feature.
pub fn validate_sandbox_adapter(adapter: &str) -> Result<(), crate::domain::errors::DomainError> {
    use crate::domain::errors::{ConfigError, DomainError};
    if adapter.is_empty() {
        return Err(DomainError::Config(ConfigError::Invalid {
            field: "sandbox.adapter".into(),
            value: "empty sandbox adapter value is invalid. \
                    Phase A accepts `\"noop\"` (the default on all platforms) \
                    or `\"landlock\"` (Linux only, requires `sandbox` cargo feature)."
                .to_string(),
        }));
    }
    match adapter {
        "noop" => Ok(()),
        "landlock" => {
            #[cfg(not(all(target_os = "linux", feature = "sandbox")))]
            {
                return Err(DomainError::Config(ConfigError::Invalid {
                    field: "sandbox.adapter".into(),
                    value: "`landlock` sandbox adapter requires the `sandbox` cargo feature \
                             AND a Linux build. Rebuild with `--features sandbox` or set \
                             `[sandbox].adapter = \"noop\"` (the default). \
                             See ADR-06-04 §Decision."
                        .to_string(),
                }));
            }
            #[cfg(all(target_os = "linux", feature = "sandbox"))]
            Ok(())
        }
        other => Err(DomainError::Config(ConfigError::Invalid {
            field: "sandbox.adapter".into(),
            value: format!(
                "unknown sandbox adapter `{}`. Phase A accepts only `\"noop\"` \
                 (the default) and `\"landlock\"` (Linux + `sandbox` feature only).",
                other
            ),
        })),
    }
}
