//! The TUI's live provider layer. Startup and in-process sign-in share one gate
//! per configured entry (`gate_entry`) and one registration (`apply_gated`), so a
//! provider unlocked by a TUI login is indistinguishable from one that started
//! signed in: routed, discoverable, refreshed by the periodic timer and kept
//! unlocked by models.dev remerges.

use crate::adapters::model_catalog_cache::{DiscoveryTarget, ModelCatalogCache};
use crate::adapters::provider::{ProviderRegistry, ProviderRouter};
use crate::adapters::tui::refresh_tracker::RefreshTracker;
use crate::domain::errors::ProviderError;
use crate::domain::events::AppEvent;
use crate::domain::models::provider::{DiscoveryStatus, LockedProvider};
use crate::domain::models::{AppConfig, AuthSnapshot, NoticeLevel};
use crate::domain::ports::StreamingProvider;
use crate::infrastructure::model_discovery::{
    RefreshReport, auth_change_key, public_catalog, refresh_and_announce, remerge_all, seed_target,
};
use crate::infrastructure::provider_availability::{
    Availability, EffectiveProvider, ProviderOrigin, availability, effective_providers,
};
use crate::infrastructure::provider_factory;
use crate::infrastructure::runtime::event_bus::EventBus;
use arc_swap::ArcSwap;
use std::sync::Arc;

/// The outcome of gating one enabled effective entry against a store snapshot.
pub(crate) enum Gated {
    Locked {
        locked: LockedProvider,
        router_message: String,
    },
    Built {
        provider: Arc<dyn StreamingProvider>,
        target: DiscoveryTarget,
    },
    Failed(ProviderError),
}

/// The one availability gate for an entry: locked (no credential), built, or a
/// construction failure.
pub(crate) fn gate_entry(
    entry: &EffectiveProvider,
    snapshot: &AuthSnapshot,
    app_config: &AppConfig,
) -> Gated {
    let (id, cfg) = (&entry.id, &entry.cfg);
    if let Availability::Locked { hint } = availability(
        entry,
        snapshot,
        &crate::infrastructure::utils::env_var_trimmed,
    ) {
        let display_name = entry.display_name();
        let router_message = format!("{display_name} has no credentials configured. {hint}");
        let mut models = public_catalog(id, entry.kind(), &cfg.model_filter);
        provider_factory::restrict_tools(&mut models, cfg.supports_tools == Some(false));
        return Gated::Locked {
            locked: LockedProvider {
                provider_id: id.clone(),
                display_name,
                sign_in_hint: hint,
                models,
            },
            router_message,
        };
    }
    let built = if entry.origin == ProviderOrigin::Bundled && entry.kind() == "anthropic" {
        crate::infrastructure::startup::build_anthropic_provider_from_env(app_config)
            .map_err(|error| ProviderError::Other(error.to_string()))
    } else {
        provider_factory::build_provider_for_config(id, cfg)
    };
    match built {
        Ok(provider) => Gated::Built {
            target: DiscoveryTarget {
                provider_id: provider.provider_id(),
                kind: entry.kind().into(),
                bundled: provider.list_models(),
                credential_fingerprint: provider_factory::discovery_fingerprint(
                    entry.rows_key(),
                    &entry.env_vars(),
                    cfg,
                    provider.as_ref(),
                ),
                scope_context: provider_factory::discovery_scope_context(
                    entry.rows_key(),
                    cfg,
                    provider.as_ref(),
                ),
                last_live: Default::default(),
                refresh_order: Default::default(),
                provider: provider.clone(),
                cache_ttl_seconds: cfg.cache_ttl_seconds,
                model_filter: cfg.model_filter.clone(),
                display_name: entry.display_name(),
                sign_in_hint: entry.sign_in_hint(),
            },
            provider,
        },
        Err(error) => Gated::Failed(error),
    }
}

/// Whether a built entry's live roster is discovered (`ollama` serves its
/// configured list only).
pub(crate) fn discovers(entry: &EffectiveProvider) -> bool {
    entry.cfg.discover_models && entry.kind() != "ollama"
}

/// Registers a gated entry. A built provider leaves the locked lists and starts
/// with a fresh discovery status (a re-login after a 401 clears
/// `Unauthenticated`); a locked one leaves the available maps and keeps its hint.
pub(crate) fn apply_gated(
    registry: &ProviderRegistry,
    router: &ProviderRouter,
    id: &str,
    gated: &Gated,
) {
    match gated {
        Gated::Locked {
            locked,
            router_message,
        } => {
            router.unregister(id);
            registry.unregister(id);
            router.register_locked(id.to_owned(), router_message.clone());
            registry.register_locked(locked.clone());
        }
        Gated::Built { provider, target } => {
            seed_target(target, &ModelCatalogCache::new());
            registry.unlock(id);
            router.unlock(id);
            registry.set_discovery_status(&target.provider_id, DiscoveryStatus::Idle);
            // The rejected-credential pane offers the same actionable hint.
            registry.set_sign_in_hint(&target.provider_id, target.sign_in_hint.clone());
            router.register(provider.clone());
            registry.register_arc(provider.clone());
        }
        Gated::Failed(_) => {}
    }
}

pub struct LiveProviders {
    router: Arc<ProviderRouter>,
    registry: Arc<ProviderRegistry>,
    /// Targets the startup refresh and the periodic timer discover.
    discovery: ArcSwap<Vec<DiscoveryTarget>>,
    /// Every built provider's seeded catalogue, republished by models.dev remerges.
    catalogue: ArcSwap<Vec<DiscoveryTarget>>,
    cache: ModelCatalogCache,
    events: Arc<EventBus>,
    tracker: Arc<RefreshTracker>,
    config: AppConfig,
    /// Serializes `register_change`; never held across discovery.
    apply: tokio::sync::Mutex<()>,
}

impl LiveProviders {
    pub fn new(
        router: Arc<ProviderRouter>,
        registry: Arc<ProviderRegistry>,
        discovery_targets: Vec<DiscoveryTarget>,
        catalogue_targets: Vec<DiscoveryTarget>,
        events: Arc<EventBus>,
        tracker: Arc<RefreshTracker>,
        config: AppConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            router,
            registry,
            discovery: ArcSwap::from_pointee(discovery_targets),
            catalogue: ArcSwap::from_pointee(catalogue_targets),
            cache: ModelCatalogCache::new(),
            events,
            tracker,
            config,
            apply: tokio::sync::Mutex::new(()),
        })
    }

    pub fn discovery_targets(&self) -> Arc<Vec<DiscoveryTarget>> {
        self.discovery.load_full()
    }

    pub fn catalogue_targets(&self) -> Arc<Vec<DiscoveryTarget>> {
        self.catalogue.load_full()
    }

    pub fn cache(&self) -> &ModelCatalogCache {
        &self.cache
    }

    pub fn router(&self) -> &Arc<ProviderRouter> {
        &self.router
    }

    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    /// Phase 1 of an auth change of `rows_key`: re-gate every enabled entry that
    /// reads it and apply the result to the live registry, router and target
    /// sets, then announce each changed provider. Returns the targets phase 2
    /// should discover. Nothing changes when the store's change key for
    /// `rows_key` equals `before`'s.
    pub async fn register_change(
        &self,
        rows_key: &str,
        app_config: &AppConfig,
        before: &AuthSnapshot,
    ) -> Vec<DiscoveryTarget> {
        let _apply = self.apply.lock().await;
        let Ok(after) = crate::adapters::auth_store::FileAuthStore::try_snapshot_sync() else {
            return Vec::new();
        };
        if auth_change_key(&after, rows_key) == auth_change_key(before, rows_key) {
            return Vec::new();
        }
        let mut gated: Vec<_> = effective_providers(app_config)
            .into_iter()
            .filter(|entry| entry.cfg.enabled && entry.rows_key() == rows_key)
            .map(|entry| {
                let gated = gate_entry(&entry, &after, app_config);
                (entry, gated)
            })
            .collect();
        // Lock order: apply -> each outgoing publication gate -> catalogue.
        // Retirement waits only for admitted publication (including detached
        // cache writers), never for credential resolution or a held request.
        let old_catalogue = self.catalogue.load_full();
        let old_discovery = self.discovery.load_full();
        let mut retired = Vec::new();
        for (entry, replacement) in &mut gated {
            if matches!(replacement, Gated::Failed(_)) {
                continue; // Construction failure leaves the serving target alive.
            }
            for old in old_catalogue
                .iter()
                .chain(old_discovery.iter())
                .filter(|old| old.provider_id == entry.id)
            {
                if let Gated::Built { target, .. } = replacement {
                    // Scope filtering remains the responsibility of usable_entry.
                    // Ordering identity, unlike fallback memory, is never reused.
                    target.last_live = old.last_live.clone();
                }
                if !retired
                    .iter()
                    .any(|order| Arc::ptr_eq(order, &old.refresh_order))
                {
                    old.refresh_order.retire().await;
                    retired.push(old.refresh_order.clone());
                }
            }
        }
        let active = self.router.active_delegate_id();
        let mut refresh = Vec::new();
        let mut notices = Vec::new();
        // Inside the catalogue-update section a concurrent remerge can neither
        // re-lock a provider unlocked here nor republish a removed target.
        self.registry.with_catalog_update(|| {
            let mut discovery = Vec::clone(&self.discovery.load());
            let mut catalogue = Vec::clone(&self.catalogue.load());
            for (entry, gated) in &mut gated {
                apply_gated(&self.registry, &self.router, &entry.id, gated);
                let (entry, gated) = (&*entry, &*gated);
                match gated {
                    Gated::Built { target, .. } => {
                        discovery.retain(|t| t.provider_id != target.provider_id);
                        catalogue.retain(|t| t.provider_id != target.provider_id);
                        catalogue.push(target.clone());
                        if discovers(entry) {
                            discovery.push(target.clone());
                            refresh.push(target.clone());
                        }
                    }
                    Gated::Locked { locked, .. } => {
                        discovery.retain(|t| t.provider_id != entry.id);
                        catalogue.retain(|t| t.provider_id != entry.id);
                        if entry.id == active {
                            notices.push(format!(
                                "{} is now locked — choose another model (Ctrl+X, M).",
                                locked.display_name
                            ));
                        }
                    }
                    Gated::Failed(error) => notices.push(format!(
                        "Failed to construct provider '{}': {error}",
                        entry.id
                    )),
                }
            }
            self.discovery.store(Arc::new(discovery));
            self.catalogue.store(Arc::new(catalogue));
        });
        for (entry, _) in &gated {
            let _ = self.events.emit_domain(AppEvent::ProviderCatalogRefreshed {
                provider_id: entry.id.clone(),
            });
        }
        for message in notices {
            let _ = self.events.emit_domain(AppEvent::SystemNotice {
                conversation_id: None,
                level: NoticeLevel::Warning,
                message,
            });
        }
        refresh
    }

    /// Phase 2: forced live discovery of the targets phase 1 returned, outside
    /// the apply lock so a held discovery never blocks another auth change.
    pub async fn refresh_changed(
        &self,
        targets: Vec<DiscoveryTarget>,
    ) -> Vec<(DiscoveryTarget, RefreshReport)> {
        futures::future::join_all(targets.into_iter().map(|target| async move {
            let report = refresh_and_announce(
                &target,
                &self.cache,
                &self.registry,
                true,
                &self.events,
                &self.tracker,
            )
            .await;
            (target, report)
        }))
        .await
    }

    /// One periodic refresh of every current discovery target.
    pub async fn periodic_tick(&self) {
        let targets = self.discovery.load_full();
        futures::future::join_all(targets.iter().map(|target| {
            refresh_and_announce(
                target,
                &self.cache,
                &self.registry,
                true,
                &self.events,
                &self.tracker,
            )
        }))
        .await;
    }

    /// Republishes every current catalogue after a models.dev update.
    pub async fn remerge(&self) {
        let targets = self.catalogue.load_full();
        remerge_all(&self.registry, &targets, &self.events, &self.config).await;
    }
}

/// The live layer is itself a provider-info view (the model picker's source).
impl crate::domain::ports::ProviderInfoPort for LiveProviders {
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
    fn get_model_provider(&self, model_id: &str, prefer: Option<&str>) -> Option<String> {
        self.registry.get_model_provider(model_id, prefer)
    }
    fn list_providers(&self) -> Vec<crate::domain::models::provider::ProviderDescriptor> {
        self.registry.list_providers()
    }
    fn list_models_by_provider(
        &self,
        provider_id: &str,
    ) -> Vec<crate::domain::models::provider::ModelDescriptor> {
        self.registry.list_models_by_provider(provider_id)
    }
    fn list_locked_providers(&self) -> Vec<LockedProvider> {
        self.registry.list_locked()
    }
    fn discovery_status(&self, provider_id: &str) -> Option<DiscoveryStatus> {
        self.registry.discovery_status(provider_id)
    }
    fn sign_in_hint(&self, provider_id: &str) -> Option<String> {
        self.registry.sign_in_hint(provider_id)
    }
    fn get_provider(&self, provider_id: &str) -> Option<Arc<dyn StreamingProvider>> {
        self.router.get_provider(provider_id)
    }
    fn set_active_provider(&self, provider_id: &str) -> Result<(), ProviderError> {
        self.router.set_active(provider_id)
    }
    fn now_unix(&self) -> i64 {
        crate::infrastructure::clock_util::now_unix()
    }
    fn today_start_unix_ms(&self) -> i64 {
        crate::infrastructure::clock_util::today_start_unix_ms()
    }
}
