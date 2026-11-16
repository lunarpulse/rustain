//! Tier-1 skill catalog composer (FR41, Grand Plan row C-1).
//!
//! One place that turns "which skills can this turn activate" into the
//! `<available_skills>` prompt block: source (the caller's [`SkillActivator`]),
//! trust filter, dedupe against Tier 2, the bound [`SkillExposurePort`], the
//! token cap and the diagnostics. String formatting and the cap arithmetic are
//! pure functions in `domain::services::skill_context`.
//!
//! The block is only produced when the turn's final tool list offers
//! `activate_skill`; every other path returns an empty result and the prompt
//! stays byte-identical to the pre-catalog composition. A render failure never
//! fails the turn (D5).

use std::collections::{HashMap, HashSet};

use crate::adapters::provider::{ProviderRegistry, ProviderRouter};
use crate::adapters::skill_activation::{CatalogSkill, SkillActivator};
use crate::adapters::skill_exposure::{
    SkillExposurePayload, SkillFullEntry, SkillRenderDiagnostics,
};
use crate::domain::models::filtered_skill_catalog::FilteredSkillCatalog;
use crate::domain::models::provider_capabilities::{ProviderCapabilities, TransportKind};
use crate::domain::models::{SkillActivationSet, ToolDefinition};
use crate::domain::ports::SkillExposurePort;
use crate::domain::services::skill_context::{
    SkillCatalogEntry, render_skill_catalog, skill_catalog_token_cap,
};

/// Everything the composer needs from one surface (TUI or ACP).
pub struct SkillCatalogSource<'a> {
    pub exposure: &'a dyn SkillExposurePort,
    pub activator: &'a SkillActivator,
    pub conversation_id: &'a str,
    /// Context window of the model this turn budgets against; `0` = unknown.
    pub context_window: u32,
}

/// The composer's result: the block (when one is due) and merged diagnostics.
#[derive(Debug, Default)]
pub struct TurnSkillCatalog {
    pub block: Option<String>,
    pub diagnostics: Option<SkillRenderDiagnostics>,
}

/// Capabilities handed to the exposure port: skills are an in-process
/// provider that streams; nothing else is claimed.
fn in_process_caps() -> ProviderCapabilities {
    ProviderCapabilities {
        supports_streaming: true,
        supports_list_changed: false,
        supports_native_retrieval: None,
        max_tool_count: None,
        transport_kind: TransportKind::InProcess,
    }
}

/// Context window of `model` on the router's active delegate; `0` when that
/// provider does not list it. Deliberately not `get_model_provider`: the
/// router sends the request to the active delegate only, and the registry's
/// cross-provider fallback could report another provider's window.
pub fn active_delegate_context_window(
    registry: &ProviderRegistry,
    router: &ProviderRouter,
    model: &str,
) -> u32 {
    registry
        .get_model(&router.active_delegate_id(), model)
        .map_or(0, |m| m.context_window)
}

/// Compose this turn's Tier-1 catalog.
///
/// `active` is the submission's activation set (those skills are in Tier 2 and
/// are dropped here); `offered_tools` is the turn's final tool list.
pub async fn render_turn_skill_catalog(
    source: Option<SkillCatalogSource<'_>>,
    active: &SkillActivationSet,
    offered_tools: &[ToolDefinition],
) -> TurnSkillCatalog {
    let Some(source) = source else {
        return TurnSkillCatalog::default();
    };
    if !offered_tools.iter().any(|t| t.name == "activate_skill") {
        return TurnSkillCatalog::default();
    }

    let active_names: HashSet<&str> = active
        .active_skills()
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let skills: Vec<CatalogSkill> = source
        .activator
        .catalog_skills(source.conversation_id)
        .await
        .into_iter()
        .filter(|s| !active_names.contains(s.metadata.name.as_str()))
        .collect();
    if skills.is_empty() {
        return TurnSkillCatalog::default();
    }

    let catalog =
        FilteredSkillCatalog::from_metadata(skills.iter().map(|s| s.metadata.clone()).collect());
    let outcome = match source.exposure.render(&catalog, &in_process_caps()).await {
        Ok(outcome) => outcome,
        Err(_) => {
            // The error text may carry skill names; log only the strategy.
            tracing::warn!(
                exposure = ?source.exposure.kind(),
                "skill catalog render failed; the turn continues without a catalog"
            );
            return TurnSkillCatalog::default();
        }
    };

    let cap = skill_catalog_token_cap(source.context_window);
    let catalog_size = catalog.len();
    let rendered = match &outcome.payload {
        SkillExposurePayload::Metadata(metas) => {
            let entries: Vec<SkillCatalogEntry<'_>> = metas
                .iter()
                .map(|metadata| SkillCatalogEntry {
                    metadata,
                    body: None,
                })
                .collect();
            render_skill_catalog(&entries, cap, catalog_size.saturating_sub(metas.len()))
        }
        SkillExposurePayload::Bodies(full) => {
            // A body is shown only for a skill that is trusted in this
            // conversation; an entry the activator did not hand over is
            // treated as untrusted (fail closed).
            let trusted: HashMap<&str, bool> = skills
                .iter()
                .map(|s| (s.metadata.name.as_str(), s.trusted))
                .collect();
            let entries: Vec<SkillCatalogEntry<'_>> = full
                .iter()
                .map(|SkillFullEntry { metadata, body }| SkillCatalogEntry {
                    metadata,
                    body: trusted
                        .get(metadata.name.as_str())
                        .copied()
                        .unwrap_or(false)
                        .then_some(body.as_str()),
                })
                .collect();
            render_skill_catalog(&entries, cap, catalog_size.saturating_sub(full.len()))
        }
        // `search_skills` is the way in under meta-search; any future payload
        // kind is not a catalog either.
        _ => return TurnSkillCatalog::default(),
    };
    let Some(rendered) = rendered else {
        return TurnSkillCatalog::default();
    };

    let dropped = catalog_size.saturating_sub(rendered.listed);
    let unrendered = dropped.saturating_sub(rendered.omitted);
    let reason = (dropped > 0).then(|| {
        format!(
            "{dropped} of {catalog_size} skills not listed ({} over the {cap}-token catalog cap, {unrendered} not rendered)",
            rendered.omitted
        )
    });
    TurnSkillCatalog {
        block: Some(rendered.block),
        diagnostics: Some(SkillRenderDiagnostics {
            truncated: rendered.omitted >= 1,
            dropped_count: dropped,
            reason,
            catalog_size,
            definition_tokens_estimate: rendered.tokens,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::skill_exposure::{
        SkillExposureError, SkillExposureKind, SkillRenderOutcome,
    };
    use crate::adapters::skill_registry::SkillRegistry;
    use crate::domain::models::skill_catalog_delta::SkillCatalogDelta;
    use crate::domain::models::{ActiveSkill, SkillDef, SkillSource};
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::RwLock;

    const CONV: &str = "conv-s";

    /// What the fake port answers with.
    enum Script {
        Metadata,
        /// Bodies for every catalog entry except the named one (a cache miss).
        BodiesDropping(&'static str),
        Search,
        Fail,
    }

    struct FakePort {
        script: Script,
        renders: AtomicUsize,
        seen: parking_lot::Mutex<Vec<ProviderCapabilities>>,
    }

    impl FakePort {
        fn new(script: Script) -> Self {
            Self {
                script,
                renders: AtomicUsize::new(0),
                seen: parking_lot::Mutex::new(Vec::new()),
            }
        }
        fn renders(&self) -> usize {
            self.renders.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SkillExposurePort for FakePort {
        fn kind(&self) -> SkillExposureKind {
            SkillExposureKind::L1Metadata
        }
        async fn render(
            &self,
            catalog: &FilteredSkillCatalog,
            provider: &ProviderCapabilities,
        ) -> Result<SkillRenderOutcome, SkillExposureError> {
            self.renders.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().push(provider.clone());
            let metas = catalog.metadata().to_vec();
            let tokens: usize = metas.iter().map(|m| m.estimated_tokens()).sum();
            let payload = match &self.script {
                Script::Metadata => SkillExposurePayload::Metadata(metas.clone()),
                Script::BodiesDropping(skip) => SkillExposurePayload::Bodies(
                    metas
                        .iter()
                        .filter(|m| m.name != *skip)
                        .map(|m| SkillFullEntry {
                            metadata: m.clone(),
                            body: format!("BODY-{}", m.name),
                        })
                        .collect(),
                ),
                Script::Search => SkillExposurePayload::SearchStub(
                    crate::domain::models::tool_descriptor::ToolDescriptor {
                        id: crate::domain::models::tool_descriptor::ToolId("search_skills".into()),
                        name: "search_skills".into(),
                        description: "search".into(),
                        input_schema: serde_json::json!({}),
                        provider_id: "test".into(),
                        annotations: Default::default(),
                    },
                ),
                Script::Fail => {
                    return Err(SkillExposureError::RenderFailed("secret-name".into()));
                }
            };
            Ok(SkillRenderOutcome {
                payload,
                diagnostics: SkillRenderDiagnostics::clean(metas.len(), tokens),
            })
        }
        async fn on_catalog_changed(
            &self,
            _delta: &SkillCatalogDelta,
        ) -> Result<(), SkillExposureError> {
            Ok(())
        }
    }

    fn def(name: &str, desc: &str, source: SkillSource) -> SkillDef {
        SkillDef {
            name: name.to_string(),
            description: desc.to_string(),
            file: PathBuf::from(format!("/ws/{name}/SKILL.md")),
            directory: PathBuf::from(format!("/ws/{name}")),
            source,
            allowed_tools: None,
            terse: None,
        }
    }

    fn activator(defs: Vec<SkillDef>) -> SkillActivator {
        SkillActivator::with_registry(Arc::new(RwLock::new(SkillRegistry::from_skills(defs))))
    }

    fn offered(names: &[&str]) -> Vec<ToolDefinition> {
        names
            .iter()
            .map(|n| ToolDefinition {
                name: n.to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                parallel_safe: true,
            })
            .collect()
    }

    fn source<'a>(
        port: &'a FakePort,
        activator: &'a SkillActivator,
        context_window: u32,
    ) -> Option<SkillCatalogSource<'a>> {
        Some(SkillCatalogSource {
            exposure: port,
            activator,
            conversation_id: CONV,
            context_window,
        })
    }

    const HEADER: &str = "<available_skills>\nInstalled skills you can load with the activate_skill tool. When a request matches a skill's description, call activate_skill with its exact name.";

    fn two_globals() -> SkillActivator {
        activator(vec![
            def("beta", "Beta desc", SkillSource::GlobalAgents),
            def("alpha", "Alpha desc", SkillSource::GlobalAgents),
        ])
    }

    #[tokio::test]
    async fn s1_without_activate_skill_the_port_is_never_called() {
        let port = FakePort::new(Script::Metadata);
        let act = two_globals();
        let out = render_turn_skill_catalog(
            source(&port, &act, 0),
            &SkillActivationSet::new(),
            &offered(&["read_file"]),
        )
        .await;
        assert!(out.block.is_none() && out.diagnostics.is_none());
        assert_eq!(port.renders(), 0);
    }

    #[tokio::test]
    async fn s2_no_source_renders_nothing() {
        let out = render_turn_skill_catalog(
            None,
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        assert!(out.block.is_none() && out.diagnostics.is_none());
    }

    fn active_skill(name: &str) -> ActiveSkill {
        ActiveSkill {
            name: name.to_string(),
            directory: PathBuf::from(format!("/ws/{name}")),
            allowed_tools: None,
            body: "b".to_string(),
            arguments: String::new(),
            activation_depth: 1,
            source: SkillSource::GlobalAgents,
        }
    }

    #[tokio::test]
    async fn s3_active_skills_are_dropped_and_all_active_skips_the_port() {
        let port = FakePort::new(Script::Metadata);
        let act = two_globals();
        let mut one = SkillActivationSet::new();
        one.push(active_skill("alpha"));
        let out =
            render_turn_skill_catalog(source(&port, &act, 0), &one, &offered(&["activate_skill"]))
                .await;
        assert_eq!(
            out.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"beta\">Beta desc</skill_entry>\n</available_skills>"
            )
        );
        let mut both = one.clone();
        both.push(active_skill("beta"));
        let renders_before = port.renders();
        let out =
            render_turn_skill_catalog(source(&port, &act, 0), &both, &offered(&["activate_skill"]))
                .await;
        assert!(out.block.is_none());
        assert_eq!(port.renders(), renders_before);
    }

    #[tokio::test]
    async fn s4_trust_filter_runs_through_the_activator() {
        let port = FakePort::new(Script::Metadata);
        let act = activator(vec![
            def("g-skill", "G desc", SkillSource::GlobalAgents),
            def("w-skill", "W desc", SkillSource::WorkspaceAgents),
        ]);
        let tools = offered(&["activate_skill"]);
        let none = SkillActivationSet::new();
        let before = render_turn_skill_catalog(source(&port, &act, 0), &none, &tools).await;
        assert_eq!(
            before.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"g-skill\">G desc</skill_entry>\n</available_skills>"
            )
        );
        act.mark_trusted(CONV, PathBuf::from("/ws/w-skill/SKILL.md"))
            .await;
        let after = render_turn_skill_catalog(source(&port, &act, 0), &none, &tools).await;
        assert_eq!(
            after.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"g-skill\">G desc</skill_entry>\n<skill_entry name=\"w-skill\" trust=\"workspace\">W desc</skill_entry>\n</available_skills>"
            )
        );
    }

    #[tokio::test]
    async fn s5_search_stub_payload_is_not_a_catalog() {
        let port = FakePort::new(Script::Search);
        let act = two_globals();
        let out = render_turn_skill_catalog(
            source(&port, &act, 0),
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        assert!(out.block.is_none() && out.diagnostics.is_none());
        assert_eq!(port.renders(), 1);
    }

    #[tokio::test]
    async fn s6_port_error_yields_an_empty_result() {
        let port = FakePort::new(Script::Fail);
        let act = two_globals();
        let out = render_turn_skill_catalog(
            source(&port, &act, 0),
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        assert!(out.block.is_none() && out.diagnostics.is_none());
        assert_eq!(port.renders(), 1);
    }

    #[tokio::test]
    async fn s7_diagnostics_merge_cap_omission_and_port_drop_without_names() {
        // Bodies script drops "b-dropped"; the cap (window 3_000 -> 60) fits
        // only the first listed entry.
        let port = FakePort::new(Script::BodiesDropping("b-dropped"));
        let act = activator(vec![
            def("a-first", "x", SkillSource::GlobalAgents),
            def("b-dropped", "x", SkillSource::GlobalAgents),
            def("c-big", &"d".repeat(200), SkillSource::GlobalAgents),
        ]);
        let out = render_turn_skill_catalog(
            source(&port, &act, 3_000),
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        // a-first: (7+1)/4 + 15 = 17, body "BODY-a-first" 12/4 = 3 -> 20 <= 60.
        // c-big: (5+200)/4 + 15 = 66, body 10/4 = 2 -> 68; 20 + 68 > 60 -> omitted.
        let d = out.diagnostics.unwrap();
        assert_eq!(
            d.reason.as_deref(),
            Some("2 of 3 skills not listed (1 over the 60-token catalog cap, 1 not rendered)")
        );
        assert!(d.truncated);
        assert_eq!(
            (
                d.dropped_count,
                d.catalog_size,
                d.definition_tokens_estimate
            ),
            (2, 3, 20)
        );
        let reason = d.reason.unwrap();
        for name in ["a-first", "b-dropped", "c-big"] {
            assert!(!reason.contains(name), "{reason}");
        }
    }

    #[tokio::test]
    async fn s8_context_window_reaches_the_cap() {
        let port = FakePort::new(Script::Metadata);
        let act = two_globals();
        // window 1_000 -> cap 20: alpha (18) fits, beta (18) does not.
        let out = render_turn_skill_catalog(
            source(&port, &act, 1_000),
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        assert_eq!(
            out.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"alpha\">Alpha desc</skill_entry>\n<omitted count=\"1\">1 more installed skills are not listed.</omitted>\n</available_skills>"
            )
        );
        let d = out.diagnostics.unwrap();
        assert!(d.truncated);
        assert_eq!(
            d.reason.as_deref(),
            Some("1 of 2 skills not listed (1 over the 20-token catalog cap, 0 not rendered)")
        );
    }

    #[tokio::test]
    async fn s9_the_port_sees_in_process_capabilities() {
        let port = FakePort::new(Script::Metadata);
        let act = two_globals();
        let _ = render_turn_skill_catalog(
            source(&port, &act, 0),
            &SkillActivationSet::new(),
            &offered(&["activate_skill"]),
        )
        .await;
        let seen = port.seen.lock();
        assert_eq!(
            *seen,
            vec![ProviderCapabilities {
                supports_streaming: true,
                supports_list_changed: false,
                supports_native_retrieval: None,
                max_tool_count: None,
                transport_kind: TransportKind::InProcess,
            }]
        );
    }

    #[tokio::test]
    async fn s10_bodies_only_for_trusted_skills() {
        let port = FakePort::new(Script::BodiesDropping("none"));
        let act = activator(vec![
            def("g-skill", "G", SkillSource::GlobalAgents),
            def("w-skill", "W", SkillSource::WorkspaceAgents),
        ]);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        act.set_event_tx(tx).await;
        let tools = offered(&["activate_skill"]);
        let none = SkillActivationSet::new();
        let untrusted = render_turn_skill_catalog(source(&port, &act, 0), &none, &tools).await;
        assert_eq!(
            untrusted.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"g-skill\">G\n<instructions>\nBODY-g-skill\n</instructions>\n</skill_entry>\n<skill_entry name=\"w-skill\" trust=\"workspace\">W</skill_entry>\n</available_skills>"
            )
        );
        act.mark_trusted(CONV, PathBuf::from("/ws/w-skill/SKILL.md"))
            .await;
        let trusted = render_turn_skill_catalog(source(&port, &act, 0), &none, &tools).await;
        assert_eq!(
            trusted.block.unwrap(),
            format!(
                "{HEADER}\n<skill_entry name=\"g-skill\">G\n<instructions>\nBODY-g-skill\n</instructions>\n</skill_entry>\n<skill_entry name=\"w-skill\" trust=\"workspace\">W\n<instructions>\nBODY-w-skill\n</instructions>\n</skill_entry>\n</available_skills>"
            )
        );
    }

    struct ModelProvider {
        id: &'static str,
        models: Vec<(&'static str, u32)>,
    }

    #[async_trait]
    impl crate::domain::ports::StreamingProvider for ModelProvider {
        async fn stream_completion(
            &self,
            _messages: Vec<crate::domain::models::Message>,
            _options: crate::domain::models::CompletionOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, crate::domain::models::StreamChunk>,
            crate::domain::errors::ProviderError,
        > {
            Err(crate::domain::errors::ProviderError::Other(
                "not used".into(),
            ))
        }
        async fn abort(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        fn provider_id(&self) -> String {
            self.id.to_string()
        }
        fn list_models(&self) -> Vec<crate::domain::models::ModelDescriptor> {
            self.models
                .iter()
                .map(|(id, window)| crate::domain::models::ModelDescriptor {
                    model_id: id.to_string(),
                    display_name: id.to_string(),
                    provider_id: self.id.to_string(),
                    context_window: *window,
                    capabilities: Default::default(),
                    pricing_tier: None,
                    stale: false,
                    wire_model_id: None,
                })
                .collect()
        }
        async fn health_check(&self) -> Result<(), crate::domain::errors::ProviderError> {
            Ok(())
        }
        async fn connectivity_probe(
            &self,
        ) -> Result<crate::domain::ports::ProbeOutcome, crate::domain::errors::ProviderError>
        {
            Ok(crate::domain::ports::ProbeOutcome {
                latency: std::time::Duration::ZERO,
            })
        }
    }

    #[test]
    fn s11_window_comes_from_the_active_delegate_only() {
        let registry = ProviderRegistry::new();
        let active: Arc<dyn crate::domain::ports::StreamingProvider> = Arc::new(ModelProvider {
            id: "active-p",
            models: vec![("shared-model", 32_000)],
        });
        registry.register_arc(active.clone());
        registry.register_arc(Arc::new(ModelProvider {
            id: "other-p",
            models: vec![("only-other", 200_000), ("shared-model", 8_000)],
        }));
        let router = ProviderRouter::new("active-p".to_string());
        router.register(active);
        // Listed by the active delegate: its own window.
        assert_eq!(
            active_delegate_context_window(&registry, &router, "shared-model"),
            32_000
        );
        // Listed only by the non-active provider: unknown, not the other window.
        assert_eq!(
            active_delegate_context_window(&registry, &router, "only-other"),
            0
        );
    }
}
