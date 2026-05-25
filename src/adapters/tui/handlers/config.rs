//! Config reload handler — Story 8.1 AC-10, expanded Story 8.2 AC-15.2.
//!
//! Handles `AppEvent::ConfigReload`: performs the two-pass config+profile reload
//! (Pass 1: NoopProfileResolver bootstrap → discover active_profile; Pass 2:
//! TomlProfileResolver → full config with profile overrides at layer 6).
//! Atomically swaps config and profile resolver via ArcSwap.
//!
//! Returns `AppEvent::ConfigReloaded { success, error }` so telemetry
//! subscribers receive truthful outcome data (AC-15).

#![allow(dead_code)]

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::adapters::cli::commands::Cli; // CONFORMANCE_EXCEPTION: config-reload handler needs CLI struct for re-parsing flags (Story 8-1)
use crate::domain::errors::DomainError;
use crate::domain::events::AppEvent;
use crate::domain::models::AppConfig;
use crate::domain::ports::ConfigStorePort;
use crate::domain::ports::ProfileResolver;
use crate::infrastructure::composition::ComposeContext;
use crate::infrastructure::runtime::agent_core::AgentCore;

use super::HandlerOutcome;

pub struct ReloadContext<'a> {
    pub cli: &'a Cli,
    pub config_store: &'a dyn ConfigStorePort,
    pub profile_store: &'a Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    /// Story 8.3 AC-8 — AgentCore for re-composition on profile change.
    pub agent_core: &'a Arc<crate::infrastructure::runtime::agent_core::AgentCore>,
    /// Story 8.3 AC-8 — ComposeContext snapshot for reload-time re-composition.
    pub compose_snapshot: &'a Arc<crate::infrastructure::composition::ComposeContext>,
}

pub fn handle_config_reload_with_two_pass(ctx: ReloadContext<'_>) -> HandlerOutcome {
    // Pass 1: bootstrap with NoopProfileResolver to discover active_profile
    let noop = crate::adapters::profile_resolver::noop::NoopProfileResolver;
    let bootstrap = crate::infrastructure::config::try_load(ctx.cli, &noop);
    let effective_name = ctx
        .cli
        .profile
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| crate::infrastructure::utils::env_var_trimmed("RUSTAIN_PROFILE"))
        .unwrap_or_else(|| {
            bootstrap
                .as_ref()
                .map(|c| c.active_profile.clone())
                .unwrap_or_else(|_| "coding".to_string())
        });

    // Pass 2: construct new TomlProfileResolver and reload
    let profiles_dir = match crate::infrastructure::paths::config_dir() {
        Ok(dir) => dir.join("profiles"),
        Err(_) => std::path::PathBuf::from(".rustain/profiles"),
    };

    let new_resolver =
        match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
            &effective_name,
            profiles_dir.clone(),
        ) {
            Ok(r) => Some(Arc::new(r) as Arc<dyn ProfileResolver>),
            Err(crate::domain::errors::ProfileError::ProfileNotFound { name, .. }) => {
                tracing::warn!(
                    "Profile '{}' not found on reload; falling back to 'coding'",
                    name
                );
                match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
                    "coding",
                    profiles_dir,
                ) {
                    Ok(fallback) => Some(Arc::new(fallback) as Arc<dyn ProfileResolver>),
                    Err(e) => {
                        tracing::error!("Coding fallback failed on reload: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Profile reload failed, keeping previous: {}", e);
                None
            }
        };

    let resolver_ref = new_resolver.as_deref();
    let result = crate::infrastructure::config::try_load(ctx.cli, resolver_ref.unwrap_or(&noop));

    match result {
        Ok(new_config) => {
            ctx.config_store.store(new_config);
            if let Some(resolver) = &new_resolver {
                ctx.profile_store.store(Arc::new(resolver.clone()));
            }

            // Story 8.3 AC-8 — re-compose AgentCore with new profile selection.
            // Extends the D5 exemption precedent chain (8.1 → 8.2 → 8.3).
            // Config + profile swaps above completed successfully; this block
            // re-builds 7 port adapters. On composition failure, config + profile
            // remain swapped but previous adapters are preserved (partial-reload).
            if let Some(resolver_arc) = new_resolver.as_ref() {
                if let Some(new_resolved) = resolver_arc.resolve_active() {
                    let snapshot: &ComposeContext = ctx.compose_snapshot;
                    match AgentCore::compose(&new_resolved.name, &new_resolved.selection, snapshot)
                    {
                        Ok(new_core) => {
                            // ArcSwap each port individually — per spec AC-8,
                            // no inter-port ordering invariant exists (Story 8.4
                            // introduces hot/warm/cold tier transitions).
                            ctx.agent_core
                                .persona
                                .store(Arc::clone(&*new_core.persona.load()));
                            ctx.agent_core
                                .memory
                                .store(Arc::clone(&*new_core.memory.load()));
                            ctx.agent_core
                                .session
                                .store(Arc::clone(&*new_core.session.load()));
                            ctx.agent_core
                                .tools
                                .store(Arc::clone(&*new_core.tools.load()));
                            ctx.agent_core
                                .channels
                                .store(Arc::clone(&*new_core.channels.load()));
                            ctx.agent_core
                                .scheduler
                                .store(Arc::clone(&*new_core.scheduler.load()));
                            ctx.agent_core
                                .context
                                .store(Arc::clone(&*new_core.context.load()));
                            tracing::info!(
                                profile = %new_resolved.name,
                                "AgentCore re-composed on reload"
                            );
                        }
                        Err(e) => {
                            tracing::error!("AgentCore re-composition failed on reload: {}", e);
                            return HandlerOutcome::Notify(AppEvent::ConfigReloaded {
                                success: false,
                                error: Some(format!(
                                    "Profile reload completed but adapter composition failed: {}. Previous adapters remain active.",
                                    e
                                )),
                            });
                        }
                    }
                }
            }

            HandlerOutcome::Notify(AppEvent::ConfigReloaded {
                success: true,
                error: None,
            })
        }
        Err(e) => {
            tracing::warn!("config reload failed: {:?}", e);
            HandlerOutcome::Notify(AppEvent::ConfigReloaded {
                success: false,
                error: Some(format!(
                    "Configuration reload failed — keeping previous config and profile. Reason: {}",
                    short_reason(&e)
                )),
            })
        }
    }
}

/// Truncate a `DomainError` to ≤80 chars for the status-bar flash.
fn short_reason(e: &DomainError) -> String {
    let msg = e.to_string();
    if msg.len() <= 80 {
        msg
    } else {
        let truncate_at = msg.floor_char_boundary(77.min(msg.len()));
        format!("{}…", &msg[..truncate_at])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::ConfigStorePort;
    use std::sync::Arc;

    struct TestConfigStore {
        inner: std::sync::Mutex<Arc<AppConfig>>,
    }

    impl ConfigStorePort for TestConfigStore {
        fn load(&self) -> Arc<AppConfig> {
            self.inner.lock().unwrap().clone()
        }
        fn store(&self, config: AppConfig) {
            *self.inner.lock().unwrap() = Arc::new(config);
        }
    }

    fn test_config() -> AppConfig {
        AppConfig::default()
    }

    fn test_cli() -> Cli {
        Cli {
            log_level: None,
            command: None,
            new: false,
            session: None,
            snapshot_retention: None,
            model: None,
            config_file: None,
            profile: None,
            persona: None,
            memory: None,
            session_adapter: None,
            tools: None,
            channels: None,
            scheduler: None,
            context: None,
            tool_exposure: None,
            skill_exposure: None,
            sandbox_adapter: None,
        }
    }

    #[test]
    fn reload_success_emits_config_reloaded_true() {
        let store = TestConfigStore {
            inner: std::sync::Mutex::new(Arc::new(test_config())),
        };
        let profile_swap = Arc::new(ArcSwap::from_pointee(Arc::new(
            crate::adapters::profile_resolver::noop::NoopProfileResolver,
        ) as Arc<dyn ProfileResolver>));
        let agent_core_arc = Arc::new(AgentCore::test_noop());
        let compose_snapshot_arc = Arc::new(crate::infrastructure::composition::ComposeContext {
            workspace_path: std::path::PathBuf::from("."),
            project_context: crate::domain::models::project_context::ProjectContext::empty(),
            storage: Arc::new(crate::adapters::noop::NoOpStorage::default())
                as Arc<dyn crate::domain::ports::StoragePort>,
            skill_activator: Arc::new(crate::adapters::skill_activation::SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            tool_exposure: "static-full".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new_in_memory()),
            sandbox_adapter: "noop".into(),
            sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy::Permissive,
            sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
                crate::adapters::sandbox::NoOpSandbox,
            )
                as Arc<dyn crate::domain::ports::SandboxManager>)),
            sandbox_policy: Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
            #[cfg(feature = "meta-search")]
            search_config: crate::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
        });
        let ctx = ReloadContext {
            cli: &test_cli(),
            config_store: &store,
            profile_store: &profile_swap,
            agent_core: &agent_core_arc,
            compose_snapshot: &compose_snapshot_arc,
        };
        let outcome = handle_config_reload_with_two_pass(ctx);
        match outcome {
            HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) => {
                assert!(success);
                assert!(error.is_none());
            }
            _other => panic!("expected Notify(ConfigReloaded {{ success: true }})"),
        }
    }

    #[test]
    fn short_reason_truncates_long_message() {
        let err = DomainError::Other("a".repeat(200));
        let reason = short_reason(&err);
        assert!(reason.len() <= 80);
        assert!(reason.ends_with('…'));
    }

    #[test]
    fn short_reason_preserves_short_message() {
        let err = DomainError::Other("short".to_string());
        let reason = short_reason(&err);
        assert_eq!(reason, "short");
    }
}
