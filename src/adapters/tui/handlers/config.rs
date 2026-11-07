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
use crate::adapters::tui::text_geometry as geom;
use crate::domain::errors::DomainError;
use crate::domain::events::AppEvent;
use crate::domain::models::AppConfig;
use crate::domain::ports::ConfigStorePort;
use crate::domain::ports::ProfileResolver;
use crate::infrastructure::composition::ComposeContext;
use crate::infrastructure::runtime::agent_core::AgentCore;

use super::HandlerOutcome;

/// Story 21.13 (D5): adopt `[markdown]` for the local Stage. Called at startup
/// and after every config reload; the draw hands the value to each tab's
/// `TabRenderState::set_code_cap`, which invalidates heights only on a change.
pub fn apply_markdown_config(
    state: &mut crate::adapters::tui::state::TuiState,
    config: &crate::domain::models::MarkdownConfig,
) {
    state.code_max_rows = config.code_max_rows();
}

pub struct ReloadContext<'a> {
    pub cli: &'a Cli,
    pub config_store: &'a dyn ConfigStorePort,
    pub profile_store: &'a Arc<ArcSwap<Arc<dyn ProfileResolver>>>,
    /// Pre-parsed `-c` overrides from startup so reload does not silently drop them.
    pub cli_config_overrides: Option<&'a serde_json::Value>,
    /// Story 8.3 AC-8 — AgentCore for re-composition on profile change.
    pub agent_core: &'a Arc<crate::infrastructure::runtime::agent_core::AgentCore>,
    /// Story 8.3 AC-8 — ComposeContext snapshot for reload-time re-composition.
    pub compose_snapshot: &'a Arc<crate::infrastructure::composition::ComposeContext>,
    /// Non-fatal workspace MCP parse notices collected for dispatch through the
    /// event bus after this pure handler returns.
    pub mcp_config_notices: &'a mut Vec<String>,
}

pub fn handle_config_reload_with_two_pass(ctx: ReloadContext<'_>) -> HandlerOutcome {
    // Pass 1: bootstrap with NoopProfileResolver to discover active_profile
    let noop = crate::adapters::profile_resolver::noop::NoopProfileResolver;
    let cli_config_overrides = ctx.cli_config_overrides;
    let bootstrap = crate::infrastructure::config::try_load_with_config_overrides(
        ctx.cli,
        &noop,
        cli_config_overrides,
    );
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
            Ok(mut resolver) => {
                ctx.mcp_config_notices
                    .extend(resolver.take_mcp_config_notices());
                Some(Arc::new(resolver) as Arc<dyn ProfileResolver>)
            }
            Err(crate::domain::errors::ProfileError::ProfileNotFound { name, .. }) => {
                tracing::warn!(
                    "Profile '{}' not found on reload; falling back to 'coding'",
                    name
                );
                match crate::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
                    "coding",
                    profiles_dir,
                ) {
                    Ok(mut fallback) => {
                        ctx.mcp_config_notices
                            .extend(fallback.take_mcp_config_notices());
                        Some(Arc::new(fallback) as Arc<dyn ProfileResolver>)
                    }
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
    let result = crate::infrastructure::config::try_load_with_config_overrides(
        ctx.cli,
        resolver_ref.unwrap_or(&noop),
        cli_config_overrides,
    );
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

/// Truncate a `DomainError` to ≤80 bytes for the status-bar flash.
///
/// The bound is a diagnostic byte limit, not a column count: the cut keeps at
/// most 77 bytes (leaving room for the 3-byte `…`) and backs up to the
/// preceding whole grapheme, so a ZWJ sequence or combining mark that
/// straddles the limit is dropped rather than shown as a fragment.
fn short_reason(e: &DomainError) -> String {
    let msg = e.to_string();
    if msg.len() <= 80 {
        msg
    } else {
        let cut = msg.floor_char_boundary(77.min(msg.len()));
        let keep =
            geom::byte_of_scalar(&msg, geom::snap_boundary(&msg, msg[..cut].chars().count()));
        format!("{}…", &msg[..keep])
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
            config_override: Vec::new(),
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
            serve_a2a: None,
        }
    }

    /// Drive the real two-pass reload handler; `overrides` are the startup
    /// `-c` overrides it re-applies.
    fn reload_outcome(overrides: Option<&serde_json::Value>) -> HandlerOutcome {
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
            storage: Arc::new(crate::adapters::noop::NoOpStorage)
                as Arc<dyn crate::domain::ports::StoragePort>,
            skill_activator: Arc::new(crate::adapters::skill_activation::SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
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
            memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
                crate::adapters::noop::NoOpMemory,
            )
                as Arc<dyn crate::domain::ports::MemoryPort>)),
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: crate::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
        });
        let ctx = ReloadContext {
            cli: &test_cli(),
            config_store: &store,
            profile_store: &profile_swap,
            cli_config_overrides: overrides,
            agent_core: &agent_core_arc,
            compose_snapshot: &compose_snapshot_arc,
            mcp_config_notices: &mut Vec::new(),
        };
        handle_config_reload_with_two_pass(ctx)
    }

    #[test]
    fn reload_success_emits_config_reloaded_true() {
        let outcome = reload_outcome(None);
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

    const G: &str = "👨\u{200d}👩\u{200d}👧";
    const A: &str = "e\u{301}";
    /// A regional-indicator pair: one grapheme that `Debug` does not escape.
    const F: &str = "🇯🇵";

    #[test]
    fn display_width_short_reason_cuts_at_a_whole_grapheme_within_80_bytes() {
        use unicode_segmentation::UnicodeSegmentation;
        let x = |n: usize| "x".repeat(n);
        let cases: Vec<(String, String)> = vec![
            // ASCII control: the old 77-byte cut, unchanged.
            (x(200), format!("{}…", x(77))),
            // Exactly 80 bytes is not cut at all.
            (x(80), x(80)),
            // The cluster straddles byte 77: it is dropped whole.
            (format!("{}{G}tail", x(70)), format!("{}…", x(70))),
            (format!("{}{A}yyyy", x(76)), format!("{}…", x(76))),
            // The cluster ends exactly at byte 77: it is kept.
            (format!("{}{G}yyyyyy", x(59)), format!("{}{G}…", x(59))),
            (format!("{}{A}yyyyyyy", x(74)), format!("{}{A}…", x(74))),
        ];
        for (msg, want) in cases {
            let reason = short_reason(&DomainError::Other(msg.clone()));
            assert_eq!(reason, want, "msg {msg:?}");
            assert!(reason.len() <= 80, "{reason:?}");
            for g in reason.graphemes(true) {
                if g.contains('👨') || g.contains('👩') || g.contains('👧') {
                    assert_eq!(g, G);
                }
            }
        }
    }

    /// The real producer: a failing reload emits
    /// `AppEvent::ConfigReloaded { error }`. A startup `-c` override of the
    /// wrong type fails extraction and quotes its value in the diagnostic (as a
    /// Rust `Debug` string, which escapes joiners and combining marks, so the
    /// quoted cluster is a regional-indicator flag: two scalars, one grapheme,
    /// not escaped), steered until it straddles byte 77.
    #[test]
    fn display_width_reload_failure_event_has_no_cluster_fragment() {
        use crate::adapters::profile_resolver::noop::NoopProfileResolver;
        use unicode_segmentation::UnicodeSegmentation;

        let cli = test_cli();
        let overrides_for = |pad: usize| serde_json::json!({ "log_max_size_mb": format!("{}{}", "x".repeat(pad), F.repeat(6)) });
        let (overrides, msg, start) = (0..=40)
            .find_map(|pad| {
                let overrides = overrides_for(pad);
                let err = crate::infrastructure::config::try_load_with_config_overrides(
                    &cli,
                    &NoopProfileResolver,
                    Some(&overrides),
                )
                .err()?;
                let msg = err.to_string();
                let start = msg
                    .grapheme_indices(true)
                    .find(|(at, g)| *g == F && *at < 77 && 77 < *at + g.len())
                    .map(|(at, _)| at)?;
                Some((overrides, msg, start))
            })
            .expect("an override text that puts a flag cluster across byte 77");

        let HandlerOutcome::Notify(AppEvent::ConfigReloaded { success, error, .. }) =
            reload_outcome(Some(&overrides))
        else {
            panic!("expected Notify(ConfigReloaded)");
        };
        assert!(!success);
        let error = error.expect("a failed reload carries its reason");
        let reason = error
            .split_once("Reason: ")
            .map(|(_, reason)| reason)
            .expect("reason marker");
        let body = reason.strip_suffix('…').expect("cut reason ends in …");
        assert!(reason.len() <= 80, "{reason:?}");
        assert_eq!(body.len(), start, "cut must precede the straddling cluster");
        assert!(msg.starts_with(body), "{msg:?} vs {body:?}");
        for g in reason.graphemes(true) {
            if g.contains('🇯') || g.contains('🇵') {
                assert_eq!(g, F, "fragment in {reason:?}");
            }
        }
    }
}
