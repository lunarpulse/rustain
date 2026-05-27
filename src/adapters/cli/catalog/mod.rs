//! `rustain catalog` dev-tool CLI surface.
//! Per ADR-09-02 v2 §Audience Split — developer/CI tool only, NOT a user feature.
//! Story 9.8.

pub mod explain;
pub mod list;
pub mod search;
pub mod stats;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::adapters::cli::commands::CatalogAction;
use crate::domain::models::AppConfig;
use crate::domain::models::SkillDef;
use crate::domain::models::SkillMetadata;
use crate::domain::models::doc_key::DocKey;
use crate::domain::ports::ToolSetPort;
use crate::infrastructure::search::MergedIndex;

/// Wrapper around the built offline index + timestamp for determinism.
pub struct OfflineIndex {
    pub index: Arc<MergedIndex>,
    pub built_at: chrono::DateTime<chrono::Utc>,
    /// Source items retained for `explain` full-profile lookup.
    pub tools: Vec<crate::domain::models::tool_descriptor::ToolDescriptor>,
    pub skills: Vec<SkillMetadata>,
    /// Skill definitions retained for source_file / mtime lookup in explain.
    pub skill_defs: Vec<SkillDef>,
}

pub async fn run_catalog_action(
    action: CatalogAction,
    config: &AppConfig,
    workspace: PathBuf,
) -> Result<i32> {
    let with_mcp = matches!(&action, CatalogAction::List { with_mcp: true, .. });
    let offline = build_offline_merged_index(config, &workspace, with_mcp).await?;
    match action {
        CatalogAction::List {
            kind,
            json,
            with_mcp,
        } => list::run_catalog_list(&offline.index, &kind, json, with_mcp).await,
        CatalogAction::Explain { doc_key } => {
            explain::run_catalog_explain(&offline, &doc_key).await
        }
        CatalogAction::Stats { json } => stats::run_catalog_stats(&offline, json).await,
        CatalogAction::Search {
            query,
            kind,
            top_k,
            json,
            no_matched_terms,
        } => {
            search::run_catalog_search(
                &offline.index,
                &query,
                &kind,
                top_k,
                json,
                !no_matched_terms,
            )
            .await
        }
    }
}

/// Build an offline MergedIndex from disk-resident skills + builtin tools, optionally MCP.
/// Mirrors the runtime `rebuild_fn` at startup.rs:812-844 but does NOT require AgentCore composition.
pub(crate) async fn build_offline_merged_index(
    _config: &AppConfig,
    workspace: &std::path::Path,
    with_mcp: bool,
) -> Result<OfflineIndex> {
    let built_at = chrono::Utc::now();

    // 1. Builtin tools via a minimal ToolSetAdapter (no AgentCore needed).
    let storage = Arc::new(crate::adapters::filesystem::FileSystemStorage::new(
        workspace.to_path_buf(),
    ));
    let sandbox_slot: Arc<arc_swap::ArcSwap<Arc<dyn crate::domain::ports::SandboxManager>>> =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(crate::adapters::sandbox::NoOpSandbox)
                as Arc<dyn crate::domain::ports::SandboxManager>,
        ));
    let sandbox_policy = Arc::new(tokio::sync::RwLock::new(
        crate::domain::models::sandbox::SandboxPolicy::Permissive,
    ));
    let tools_adapter = crate::adapters::toolset_adapter::ToolSetAdapter::new(
        workspace.to_path_buf(),
        storage,
        sandbox_slot,
        sandbox_policy,
    );
    let tool_descs: Vec<crate::domain::models::tool_descriptor::ToolDescriptor> =
        tools_adapter.describe();

    // 2. MCP tools (optional, default off for 200ms runtime target).
    #[cfg(feature = "mcp")]
    if with_mcp {
        // TODO: Connect MCP servers per config, call tools/list, collect descriptors.
        // For now, MCP enumeration is a no-op placeholder to satisfy the interface.
        tracing::info!(
            "catalog offline builder: --with-mcp requested but MCP enumeration not yet implemented"
        );
    }

    // 3. Skills from disk.
    let home = dirs::home_dir();
    if home.is_none() {
        tracing::warn!("catalog offline builder: $HOME not set — home-directory skill discovery skipped");
    }
    let registry = tokio::task::spawn_blocking({
        let workspace = workspace.to_path_buf();
        let home = home.clone();
        move || {
            crate::adapters::skill_registry::SkillRegistry::discover(
                &workspace,
                home.as_deref(),
                &[], // no disabled skills in offline builder
            )
        }
    })
    .await?;

    let mut skills: Vec<SkillMetadata> = Vec::new();
    let mut skill_defs: Vec<SkillDef> = Vec::new();
    let mut overrides: BTreeMap<DocKey, String> = BTreeMap::new();
    for def in registry.skills() {
        let meta = SkillMetadata::from_def(def);
        if let Some(ref terse) = def.terse {
            overrides.insert(
                DocKey::new(
                    crate::domain::models::capability_kind::CapabilityKind::Skill,
                    def.name.clone(),
                ),
                terse.clone(),
            );
        }
        skills.push(meta);
        skill_defs.push(def.clone());
    }

    // 4. Build MergedIndex.
    let mut refs: Vec<&dyn crate::domain::ports::search::IndexableItem> =
        Vec::with_capacity(tool_descs.len() + skills.len());
    for t in &tool_descs {
        refs.push(t);
    }
    for s in &skills {
        refs.push(s);
    }

    let index = Arc::new(MergedIndex::from_items_with_overrides(&refs, &overrides));

    Ok(OfflineIndex {
        index,
        built_at,
        tools: tool_descs,
        skills,
        skill_defs,
    })
}
