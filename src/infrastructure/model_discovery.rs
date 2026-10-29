//! One catalogue fold and refresh path for interactive and headless consumers.
use crate::adapters::model_catalog_cache::{
    CachedDiscoveryEntry, DiscoveryTarget, ModelCatalogCache,
};
use crate::adapters::model_filter::{compile_filter_patterns, matches_any};
use crate::adapters::provider::ProviderRegistry;
use crate::domain::errors::ProviderError;
use crate::domain::events::AppEvent;
use crate::domain::models::provider::{
    DiscoveryStatus, ModelCapability, ModelDescriptor, ModelRecord,
};
use crate::domain::models::{AppConfig, AuthSnapshot, NoticeLevel};
use crate::domain::ports::StreamingProvider;
use crate::infrastructure::runtime::event_bus::EventBus;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub struct CatalogLayers {
    pub provider_id: String,
    pub kind: String,
    pub bundled: Vec<ModelDescriptor>,
    pub models_dev: Vec<ModelRecord>,
    pub cache: Option<CachedDiscoveryEntry>,
    pub model_filter: Vec<String>,
    /// The adapter whose canonical lookup supplies contexts every layer omits.
    pub adapter: Option<Arc<dyn StreamingProvider>>,
}
pub enum LiveOutcome {
    NotAttempted,
    Success(Vec<ModelRecord>),
    Failed(bool),
}
pub struct MergeResult {
    pub served: Vec<ModelDescriptor>,
    pub cache_write: Option<Vec<ModelRecord>>,
}
#[derive(Debug)]
pub struct RefreshReport {
    pub status: DiscoveryStatus,
    pub model_count: usize,
    pub notice: Option<String>,
}

fn fold_record(earlier: &mut ModelRecord, later: &ModelRecord) {
    if let Some(name) = &later.display_name
        && !name.is_empty()
        && (name != &later.model_id || earlier.display_name.is_none())
    {
        earlier.display_name = Some(name.clone());
    }
    if let Some(context) = later.context_window.filter(|context| *context > 0) {
        earlier.context_window = Some(context);
    }
    if let Some(capabilities) = &later.capabilities {
        earlier
            .capabilities
            .get_or_insert_with(HashSet::new)
            .extend(capabilities.iter().cloned());
    }
    if later.pricing_tier.is_some() {
        earlier.pricing_tier.clone_from(&later.pricing_tier);
    }
    if later.wire_model_id.is_some() {
        earlier.wire_model_id.clone_from(&later.wire_model_id);
    }
}

/// D11 bottom: the adapter's canonical lookup, else (adapterless locked and
/// `update-catalog` catalogues) the bundled/built-in variant tables.
fn bottom_context(layers: &CatalogLayers, id: &str) -> u32 {
    if let Some(context) = layers
        .adapter
        .as_ref()
        .and_then(|adapter| adapter.default_context_window(id))
    {
        return context;
    }
    if let Some(model) = layers
        .bundled
        .iter()
        .find(|m| m.model_id == id || m.model_id.starts_with(id) || id.starts_with(&m.model_id))
    {
        return model.context_window;
    }
    #[cfg(feature = "openai")]
    {
        use crate::adapters::openai::{OpenAiCompatibleVariant, allowlists};
        if let Some(variant) = OpenAiCompatibleVariant::builtin(&layers.kind) {
            return allowlists::variant_default_context(&variant);
        }
    }
    if layers.kind == "anthropic" {
        200_000
    } else {
        layers
            .bundled
            .first()
            .map(|m| m.context_window)
            .unwrap_or(8_192)
    }
}

pub fn merge_catalog(layers: CatalogLayers, live: LiveOutcome) -> MergeResult {
    let (live_records, rejected) = match live {
        LiveOutcome::Success(records) if records.is_empty() => {
            return MergeResult {
                served: vec![],
                cache_write: None,
            };
        }
        LiveOutcome::Success(records) => (Some(records), false),
        LiveOutcome::Failed(rejected) => (None, rejected),
        LiveOutcome::NotAttempted => (None, false),
    };
    let mut records: Vec<ModelRecord> = Vec::new();
    let mut indices: HashMap<String, usize> = HashMap::new();
    for model in &layers.bundled {
        let record = ModelRecord::from(model);
        if let Some(&index) = indices.get(&record.model_id) {
            fold_record(&mut records[index], &record);
        } else {
            indices.insert(record.model_id.clone(), records.len());
            records.push(record);
        }
    }
    // A row the live roster lists is owned by bundled ⊕ models.dev ⊕ live:
    // the cache layer is never folded under it.
    let listed: HashSet<&str> = live_records
        .iter()
        .flatten()
        .map(|record| record.model_id.as_str())
        .collect();
    let cached = layers
        .models_dev
        .iter()
        .chain(
            layers
                .cache
                .iter()
                .flat_map(|cache| &cache.models)
                .filter(|record| !listed.contains(record.model_id.as_str())),
        )
        .map(|record| (record, false));
    for (record, live) in cached.chain(live_records.iter().flatten().map(|record| (record, true))) {
        let index = if let Some(&index) = indices.get(&record.model_id) {
            index
        } else {
            let index = records.len();
            indices.insert(record.model_id.clone(), index);
            records.push(ModelRecord {
                model_id: record.model_id.clone(),
                ..Default::default()
            });
            index
        };
        fold_record(&mut records[index], record);
        // A live roster owns the wire id outright, `None` included: an older
        // layer's wire id never outlives the discovery that dropped it.
        if live {
            records[index]
                .wire_model_id
                .clone_from(&record.wire_model_id);
        }
    }
    let authoritative = live_records.as_ref().or_else(|| {
        layers
            .cache
            .as_ref()
            .filter(|cache| cache.authoritative)
            .map(|cache| &cache.models)
    });
    let order: Vec<_> = if let Some(authoritative) = authoritative {
        authoritative
            .iter()
            .filter_map(|record| indices.get(&record.model_id).copied())
            .collect()
    } else {
        (0..records.len()).collect()
    };
    let filter = compile_filter_patterns(&layers.model_filter);
    let mut seen = HashSet::new();
    let served = order
        .into_iter()
        .filter_map(|index| {
            let record = &records[index];
            if !seen.insert(index) || !matches_any(&filter, &record.model_id) {
                return None;
            }
            Some(ModelDescriptor {
                model_id: record.model_id.clone(),
                display_name: record
                    .display_name
                    .clone()
                    .unwrap_or_else(|| record.model_id.clone()),
                provider_id: layers.provider_id.clone(),
                context_window: record
                    .context_window
                    .unwrap_or_else(|| bottom_context(&layers, &record.model_id)),
                capabilities: record
                    .capabilities
                    .clone()
                    .unwrap_or_else(|| HashSet::from([ModelCapability::ToolUse])),
                pricing_tier: record.pricing_tier.clone(),
                stale: rejected,
                wire_model_id: record.wire_model_id.clone(),
            })
        })
        .collect();
    MergeResult {
        served,
        cache_write: live_records,
    }
}

pub fn models_dev_layer(provider: &str) -> Vec<ModelRecord> {
    #[cfg(feature = "models-dev")]
    {
        crate::adapters::models_dev::catalog_for(provider)
    }
    #[cfg(not(feature = "models-dev"))]
    {
        let _ = provider;
        vec![]
    }
}

pub fn public_catalog(id: &str, kind: &str, filter: &[String]) -> Vec<ModelDescriptor> {
    let bundled = crate::adapters::model_catalog_cache::load_embedded_seed()
        .and_then(|seed| seed.providers.get(kind))
        .map(|entry| entry.models.iter().map(|m| m.descriptor.clone()).collect())
        .unwrap_or_default();
    merge_catalog(
        CatalogLayers {
            provider_id: id.into(),
            kind: kind.into(),
            bundled,
            models_dev: models_dev_layer(kind),
            cache: None,
            model_filter: filter.to_vec(),
            adapter: None,
        },
        LiveOutcome::NotAttempted,
    )
    .served
}

fn layers(target: &DiscoveryTarget, cache: Option<CachedDiscoveryEntry>) -> CatalogLayers {
    CatalogLayers {
        provider_id: target.provider_id.clone(),
        kind: target.kind.clone(),
        bundled: target.bundled.clone(),
        models_dev: models_dev_layer(&target.kind),
        cache,
        model_filter: target.model_filter.clone(),
        adapter: Some(target.provider.clone()),
    }
}

/// The cache layer usable for `scope`: the newer of this process's last live
/// roster (kept even when persisting it failed) and the disk entry, each only
/// when stamped with that credential scope.
fn usable_entry(
    target: &DiscoveryTarget,
    disk: Option<CachedDiscoveryEntry>,
    scope: Option<&str>,
) -> Option<CachedDiscoveryEntry> {
    let scope = scope?;
    let memory = target.last_live.load_full().map(|entry| (*entry).clone());
    // On equal timestamps the in-memory roster, later in the chain, wins.
    [disk, memory]
        .into_iter()
        .flatten()
        .filter(|entry| entry.credential_fingerprint == scope)
        .max_by_key(|entry| entry.fetched_at_unix)
}

pub fn seed_target(target: &DiscoveryTarget, cache: &ModelCatalogCache) {
    let disk = cache.load_sync().providers.remove(&target.provider_id);
    let entry = usable_entry(target, disk, Some(target.credential_fingerprint.as_str()));
    target.provider.set_discovered_models(
        merge_catalog(layers(target, entry), LiveOutcome::NotAttempted).served,
    );
}

pub fn rejected_notice(target: &DiscoveryTarget) -> String {
    format!(
        "{} rejected its credential (HTTP 401/403) — models could not be discovered. {}",
        target.display_name, target.sign_in_hint
    )
}

pub async fn refresh_target(
    target: &DiscoveryTarget,
    cache: &ModelCatalogCache,
    registry: Option<&ProviderRegistry>,
    force: bool,
) -> RefreshReport {
    // A refresh started later always wins publication, so a delayed response
    // for a replaced account never overwrites the newer roster or status.
    let ticket = target.refresh_order.ticket();
    // Re-read the scope now through the adapter's own request path, so a
    // credential changed or removed since startup never reuses another roster.
    let scope = target.provider.discovery_scope(&target.scope_context).await;
    let disk = cache.load().await.providers.remove(&target.provider_id);
    let entry = usable_entry(target, disk, scope.as_deref());
    let previous = registry.and_then(|registry| registry.discovery_status(&target.provider_id));
    if !force
        && entry.as_ref().is_some_and(|entry| {
            cache.is_fresh(
                entry,
                target.cache_ttl_seconds,
                crate::infrastructure::clock_util::now_unix(),
            )
        })
    {
        let status = previous
            .filter(|s| *s != DiscoveryStatus::Idle)
            .unwrap_or(DiscoveryStatus::Ok);
        let outcome = match status {
            DiscoveryStatus::Empty => LiveOutcome::Success(vec![]),
            DiscoveryStatus::Unauthenticated => LiveOutcome::Failed(true),
            _ => LiveOutcome::NotAttempted,
        };
        let Some(_slot) = target.refresh_order.publish(ticket).await else {
            return superseded(target, registry);
        };
        let publish = || {
            let merged = merge_catalog(layers(target, entry), outcome);
            let count = merged.served.len();
            target.provider.set_discovered_models(merged.served);
            if let Some(registry) = registry {
                registry.set_discovery_status(&target.provider_id, status);
            }
            count
        };
        let model_count = if let Some(registry) = registry {
            registry.with_catalog_update(publish)
        } else {
            publish()
        };
        return RefreshReport {
            status,
            model_count,
            notice: None,
        };
    }
    // Cache the unfiltered roster: `model_filter` applies to every merged layer,
    // so a later filter change never hides models behind a fresh cache entry.
    let mut stamped = None;
    let result = target
        .provider
        .discover_models(&[], &target.scope_context)
        .await
        .unwrap_or(Err(ProviderError::EndpointUnsupported(405)))
        .map(|found| {
            stamped = Some(found.credential_scope);
            found.records
        });
    // A request that fell through to another credential (a stale row whose
    // refresh failed) is never folded with, or stamped as, the predicted scope.
    let entry = match &stamped {
        Some(stamped) => entry.filter(|entry| &entry.credential_fingerprint == stamped),
        None => entry,
    };
    let (status, outcome, notice) = match result {
        Ok(models) if models.is_empty() => (
            DiscoveryStatus::Empty,
            LiveOutcome::Success(models),
            Some(format!(
                "Model discovery for '{}' returned 0 models.",
                target.provider_id
            )),
        ),
        Ok(models) => (DiscoveryStatus::Ok, LiveOutcome::Success(models), None),
        Err(ProviderError::AuthenticationFailed) => (
            DiscoveryStatus::Unauthenticated,
            LiveOutcome::Failed(true),
            Some(rejected_notice(target)),
        ),
        Err(ProviderError::EndpointUnsupported(code)) => {
            tracing::info!(provider = %target.provider_id, code, "Model discovery endpoint unsupported");
            (
                DiscoveryStatus::Unavailable,
                LiveOutcome::Failed(false),
                None,
            )
        }
        Err(error) => (
            if entry.is_some() {
                DiscoveryStatus::Cached
            } else {
                DiscoveryStatus::Unavailable
            },
            LiveOutcome::Failed(false),
            Some(format!(
                "Model catalog refresh for '{}' failed: {} — showing bundled/cached models",
                target.provider_id, error
            )),
        ),
    };
    let Some(_slot) = target.refresh_order.publish(ticket).await else {
        return superseded(target, registry);
    };
    // Persist before publishing: a concurrent models.dev remerge must never
    // replace a new authoritative roster with an older disk snapshot.
    if let LiveOutcome::Success(models) = &outcome
        && !models.is_empty()
        && let Some(stamped) = stamped
    {
        let cached = CachedDiscoveryEntry {
            fetched_at_unix: crate::infrastructure::clock_util::now_unix(),
            credential_fingerprint: stamped,
            authoritative: true,
            models: models.clone(),
        };
        // Kept in memory first, so a failed write cannot let a later remerge
        // fall back to an older disk roster or the public catalogue.
        target.last_live.store(Some(Arc::new(cached.clone())));
        if let Err(error) = cache.upsert_provider(&target.provider_id, cached).await {
            tracing::warn!(%error, "models_cache.json save failed; live roster kept in memory");
        }
    }
    let publish = || {
        let merged = merge_catalog(layers(target, entry), outcome);
        let count = merged.served.len();
        target.provider.set_discovered_models(merged.served);
        if let Some(registry) = registry {
            registry.set_discovery_status(&target.provider_id, status);
        }
        count
    };
    let model_count = if let Some(registry) = registry {
        registry.with_catalog_update(publish)
    } else {
        publish()
    };
    RefreshReport {
        status,
        model_count,
        notice: notice.filter(|_| previous != Some(status)),
    }
}

/// A later-started refresh of this target already published: its catalogue,
/// cache entries and status stand.
fn superseded(target: &DiscoveryTarget, registry: Option<&ProviderRegistry>) -> RefreshReport {
    RefreshReport {
        status: registry
            .and_then(|registry| registry.discovery_status(&target.provider_id))
            .unwrap_or(DiscoveryStatus::Idle),
        model_count: target.provider.list_models().len(),
        notice: None,
    }
}

pub async fn refresh_and_announce(
    target: &DiscoveryTarget,
    cache: &ModelCatalogCache,
    registry: &ProviderRegistry,
    force: bool,
    events: &Arc<EventBus>,
    tracker: &Arc<crate::adapters::tui::refresh_tracker::RefreshTracker>,
) -> RefreshReport {
    // The in-flight mark ends with the fetch, before the announcement: a
    // listener never sees a finished refresh as running, and needs no
    // explicit removal that could clear a newer refresh of the same provider.
    let report = {
        let _guard = tracker.insert(target.provider_id.clone());
        refresh_target(target, cache, Some(registry), force).await
    };
    if let Some(message) = &report.notice {
        let _ = events.emit_domain(AppEvent::SystemNotice {
            conversation_id: None,
            level: NoticeLevel::Warning,
            message: message.clone(),
        });
    }
    let _ = events.emit_domain(AppEvent::ProviderCatalogRefreshed {
        provider_id: target.provider_id.clone(),
    });
    report
}

pub async fn remerge_all(
    registry: &ProviderRegistry,
    targets: &[DiscoveryTarget],
    events: &Arc<EventBus>,
    config: &AppConfig,
) {
    // Scopes may need the adapter's async request path; read them before the
    // synchronous publication boundary, which never awaits.
    let mut scopes = Vec::with_capacity(targets.len());
    for target in targets {
        scopes.push(target.provider.discovery_scope(&target.scope_context).await);
    }
    registry.with_catalog_update(|| {
        let cached = ModelCatalogCache::new().load_sync();
        for (target, scope) in targets.iter().zip(&scopes) {
            let disk = cached.providers.get(&target.provider_id).cloned();
            let entry = usable_entry(target, disk, scope.as_deref());
            let outcome = match registry.discovery_status(&target.provider_id) {
                Some(DiscoveryStatus::Empty) => LiveOutcome::Success(vec![]),
                Some(DiscoveryStatus::Unauthenticated) => LiveOutcome::Failed(true),
                _ => LiveOutcome::NotAttempted,
            };
            target
                .provider
                .set_discovered_models(merge_catalog(layers(target, entry), outcome).served);
            let _ = events.emit_domain(AppEvent::ProviderCatalogRefreshed {
                provider_id: target.provider_id.clone(),
            });
        }
        let entries = super::provider_availability::effective_providers(config);
        for mut locked in registry.list_locked() {
            if let Some(entry) = entries.iter().find(|entry| entry.id == locked.provider_id) {
                locked.models = public_catalog(&entry.id, entry.kind(), &entry.cfg.model_filter);
                super::provider_factory::restrict_tools(
                    &mut locked.models,
                    entry.cfg.supports_tools == Some(false),
                );
            }
            let provider_id = locked.provider_id.clone();
            registry.register_locked(locked);
            let _ = events.emit_domain(AppEvent::ProviderCatalogRefreshed { provider_id });
        }
    });
}

pub fn spawn_headless(targets: Vec<DiscoveryTarget>, registry: Arc<ProviderRegistry>) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        let cache = ModelCatalogCache::new();
        for target in targets {
            let cache = cache.clone();
            let registry = registry.clone();
            runtime.spawn(async move {
                let report = refresh_target(&target, &cache, Some(&registry), false).await;
                // Headless rails have no notice surface; keep failures observable.
                if let Some(notice) = report.notice {
                    tracing::warn!(provider = %target.provider_id, "{notice}");
                }
            });
        }
    }
}

/// What an auth change of `provider_id` is compared by: the row versions and
/// the active row. A selection-only change counts; a no-op change does not.
pub(crate) fn auth_change_key(
    snapshot: &AuthSnapshot,
    provider_id: &str,
) -> (
    std::collections::BTreeSet<crate::domain::models::CredentialStamp>,
    Option<crate::domain::models::CredentialId>,
) {
    (
        snapshot
            .rows(provider_id)
            .iter()
            .map(crate::domain::models::StoredCredential::stamp)
            .collect(),
        snapshot.selected(provider_id),
    )
}

/// Refreshes the provider's live roster after any credential change: a login, a
/// new active row (`auth use`) or a removed row (see `auth_change_key`).
pub async fn rediscover_after_auth_change(
    provider_id: &str,
    app_config: &AppConfig,
    before: &AuthSnapshot,
) -> Option<RefreshReport> {
    let after = crate::adapters::auth_store::FileAuthStore::try_snapshot_sync().ok()?;
    if auth_change_key(&after, provider_id) == auth_change_key(before, provider_id) {
        return None;
    }
    let entries: Vec<_> = super::provider_availability::effective_providers(app_config)
        .into_iter()
        .filter(|entry| {
            entry.rows_key() == provider_id && entry.cfg.enabled && entry.cfg.discover_models
        })
        .filter(|entry| {
            matches!(
                super::provider_availability::availability(
                    entry,
                    &after,
                    &super::utils::env_var_trimmed
                ),
                super::provider_availability::Availability::Signed(_)
            )
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let layer = super::startup::init_provider_layer(app_config);
    let cache = ModelCatalogCache::new();
    let mut combined: Option<RefreshReport> = None;
    for entry in entries {
        let Some(target) = layer
            .discovery_targets
            .iter()
            .find(|target| target.provider_id == entry.id)
        else {
            continue;
        };
        let report = refresh_target(target, &cache, Some(&layer.registry), true).await;
        if let Some(combined) = &mut combined {
            combined.model_count += report.model_count;
            // Preserve a failure in the summary rather than hiding it behind a
            // later successful alias. Every matching entry is still refreshed.
            if combined.status == DiscoveryStatus::Ok {
                combined.status = report.status;
            }
            if let Some(notice) = report.notice {
                match &mut combined.notice {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(&notice);
                    }
                    None => combined.notice = Some(notice),
                }
            }
        } else {
            combined = Some(report);
        }
    }
    combined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        id: &str,
        name: Option<&str>,
        context: Option<u32>,
        capabilities: Option<Vec<ModelCapability>>,
    ) -> ModelRecord {
        ModelRecord {
            model_id: id.into(),
            display_name: name.map(Into::into),
            context_window: context,
            capabilities: capabilities.map(|caps| caps.into_iter().collect()),
            pricing_tier: None,
            wire_model_id: None,
        }
    }
    fn catalog() -> CatalogLayers {
        CatalogLayers {
            provider_id: "openai".into(),
            kind: "openai".into(),
            bundled: vec![ModelDescriptor {
                model_id: "model".into(),
                display_name: "Bundled".into(),
                provider_id: "openai".into(),
                context_window: 100,
                capabilities: HashSet::from([ModelCapability::ToolUse]),
                pricing_tier: Some("base".into()),
                stale: false,
                wire_model_id: None,
            }],
            models_dev: vec![record(
                "model",
                Some("Metadata"),
                Some(200),
                Some(vec![ModelCapability::Thinking]),
            )],
            cache: None,
            model_filter: vec!["*".into()],
            adapter: None,
        }
    }

    #[test]
    fn partial_fields_preserve_metadata_and_explicit_values_override_additively() {
        let merged = merge_catalog(
            catalog(),
            LiveOutcome::Success(vec![record("model", Some("model"), Some(0), None)]),
        );
        let model = &merged.served[0];
        assert_eq!(
            (&*model.display_name, model.context_window),
            ("Metadata", 200)
        );
        assert!(model.capabilities.contains(&ModelCapability::ToolUse));
        assert!(model.capabilities.contains(&ModelCapability::Thinking));
        assert_eq!(model.pricing_tier.as_deref(), Some("base"));
        let mut live = record(
            "model",
            Some("Live"),
            Some(300),
            Some(vec![ModelCapability::Vision]),
        );
        live.pricing_tier = Some("premium".into());
        let merged = merge_catalog(catalog(), LiveOutcome::Success(vec![live]));
        let model = &merged.served[0];
        assert_eq!((&*model.display_name, model.context_window), ("Live", 300));
        assert!(model.capabilities.contains(&ModelCapability::Vision));
        assert!(model.capabilities.contains(&ModelCapability::Thinking));
        assert_eq!(model.pricing_tier.as_deref(), Some("premium"));
    }

    #[test]
    fn a_live_roster_owns_the_wire_id_and_a_cached_one_survives_only_without_it() {
        let served = |live| {
            let mut cached = record("model", None, None, None);
            cached.wire_model_id = Some("cached-wire".into());
            let mut layers = catalog();
            layers.cache = Some(CachedDiscoveryEntry {
                fetched_at_unix: 0,
                credential_fingerprint: "scope".into(),
                authoritative: true,
                models: vec![cached],
            });
            merge_catalog(layers, live).served[0].wire_model_id.clone()
        };
        // Without a live roster the cached wire id stands.
        assert_eq!(
            served(LiveOutcome::NotAttempted).as_deref(),
            Some("cached-wire")
        );
        // A live record without one clears it; a live wire id replaces it.
        let live = record("model", None, None, None);
        assert_eq!(served(LiveOutcome::Success(vec![live.clone()])), None);
        let mut wired = live;
        wired.wire_model_id = Some("live-wire".into());
        assert_eq!(
            served(LiveOutcome::Success(vec![wired])).as_deref(),
            Some("live-wire")
        );
    }

    #[test]
    fn a_live_roster_owns_its_rows_and_the_cache_serves_only_without_it() {
        let served = |live| {
            let mut layers = catalog();
            layers.cache = Some(CachedDiscoveryEntry {
                fetched_at_unix: 0,
                credential_fingerprint: "scope".into(),
                authoritative: true,
                models: vec![record(
                    "model",
                    Some("Cached"),
                    Some(999),
                    Some(vec![ModelCapability::Vision]),
                )],
            });
            merge_catalog(layers, live).served.remove(0)
        };
        let cached = served(LiveOutcome::NotAttempted);
        assert_eq!(
            (&*cached.display_name, cached.context_window),
            ("Cached", 999)
        );
        assert!(cached.capabilities.contains(&ModelCapability::Vision));
        let live = served(LiveOutcome::Success(vec![record(
            "model", None, None, None,
        )]));
        assert_eq!(
            (&*live.display_name, live.context_window),
            ("Metadata", 200)
        );
        assert!(!live.capabilities.contains(&ModelCapability::Vision));
    }

    #[test]
    fn cache_failure_retains_only_authoritative_ids_and_filters_every_branch() {
        for live in [
            LiveOutcome::NotAttempted,
            LiveOutcome::Failed(false),
            LiveOutcome::Failed(true),
        ] {
            let mut layers = catalog();
            layers.model_filter = vec!["cached".into()];
            layers.cache = Some(CachedDiscoveryEntry {
                fetched_at_unix: 0,
                credential_fingerprint: "scope".into(),
                authoritative: true,
                models: vec![
                    record("cached", None, None, None),
                    record("other", None, None, None),
                ],
            });
            let rejected = matches!(live, LiveOutcome::Failed(true));
            let merged = merge_catalog(layers, live);
            assert_eq!(
                merged
                    .served
                    .iter()
                    .map(|m| m.model_id.as_str())
                    .collect::<Vec<_>>(),
                ["cached"]
            );
            assert_eq!(merged.served[0].stale, rejected);
            assert!(merged.cache_write.is_none());
        }
        let mut layers = catalog();
        layers.model_filter = vec!["no-match".into()];
        assert!(
            merge_catalog(
                layers,
                LiveOutcome::Success(vec![record("model", None, None, None)])
            )
            .served
            .is_empty()
        );
    }

    #[test]
    fn silent_capabilities_get_bottom_default_but_explicit_empty_is_not_invented() {
        let mut layers = catalog();
        layers.bundled.clear();
        layers.models_dev.clear();
        let merged = merge_catalog(
            layers,
            LiveOutcome::Success(vec![
                record("silent", None, None, None),
                record("explicit", None, None, Some(vec![])),
            ]),
        );
        assert!(
            merged.served[0]
                .capabilities
                .contains(&ModelCapability::ToolUse)
        );
        assert!(merged.served[1].capabilities.is_empty());
        assert_eq!(merged.served[0].context_window, 128000);
    }
}
