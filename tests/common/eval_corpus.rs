//! Shared production-corpus building helpers (Story 9-7c).
//!
//! Used by both `integration_skill_eval_harness.rs` and `prop_synonym_collisions.rs`
//! to construct `Bm25SearchEngine` instances backed by the full `skill_eval_corpus/`
//! fixture set.

use arc_swap::ArcSwap;
use rustain::domain::models::capability_kind::CapabilityKind;
use rustain::domain::models::doc_key::DocKey;
use rustain::domain::models::search_hit::SearchHit;
use rustain::domain::ports::search::IndexableItem;
use rustain::domain::services::frontmatter;
use rustain::infrastructure::search::MergedIndex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Test-only IndexableItem impl
// ---------------------------------------------------------------------------
pub struct FixtureItem {
    doc_key_val: DocKey,
    desc: String,
    searchable: String,
}

impl IndexableItem for FixtureItem {
    fn doc_key(&self) -> DocKey {
        self.doc_key_val.clone()
    }

    fn searchable_text(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.searchable)
    }

    fn description(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.desc)
    }

    fn to_search_hit(&self, score: f32, matched_terms: Option<Vec<String>>) -> SearchHit {
        let dk = self.doc_key();
        let terse = rustain::domain::services::compute_terse(&self.desc, &dk.name);
        let mut hit = SearchHit::minimal(dk.name, dk.kind, terse, score);
        hit.matched_terms = matched_terms;
        hit
    }
}

// ---------------------------------------------------------------------------
// Corpus path
// ---------------------------------------------------------------------------
fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skill_eval_corpus")
}

// ---------------------------------------------------------------------------
// Skill frontmatter parsing
// ---------------------------------------------------------------------------
fn parse_skill_frontmatter(content: &str) -> Option<(String, String, Vec<String>)> {
    use rustain::domain::services::frontmatter::{self, extract_field, extract_list_field};
    let (fm_str, _body) = frontmatter::parse_frontmatter(content)?;
    let name = extract_field(fm_str, "name")?;
    let desc = extract_field(fm_str, "description")?;
    let tags = extract_list_field(fm_str, "tags").unwrap_or_default();
    Some((name, desc, tags))
}

// ---------------------------------------------------------------------------
// Directory walks
// ---------------------------------------------------------------------------
fn visit_dir_for_skills(
    root: &Path,
    current: &Path,
    items: &mut Vec<FixtureItem>,
    seen: &mut BTreeSet<String>,
) {
    if let Ok(entries) = std::fs::read_dir(current) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let dirname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if dirname == "tools" {
                    continue;
                }
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    if let Ok(content) = std::fs::read_to_string(&skill_md) {
                        if let Some((name, desc, tags)) = parse_skill_frontmatter(&content) {
                            let is_tool_tagged = tags.iter().any(|t| t == "tool");
                            let kind = if is_tool_tagged {
                                CapabilityKind::Tool
                            } else {
                                CapabilityKind::Skill
                            };
                            let key = format!(
                                "{}::{}",
                                serde_json::to_string(&kind)
                                    .unwrap()
                                    .trim_matches('"')
                                    .to_lowercase(),
                                name
                            );
                            if seen.insert(key.clone()) {
                                let doc_key = DocKey {
                                    kind,
                                    name: name.clone(),
                                };
                                let searchable = format!("{} {}", name, desc);
                                items.push(FixtureItem {
                                    doc_key_val: doc_key,
                                    desc,
                                    searchable,
                                });
                            }
                        }
                    }
                }
                visit_dir_for_skills(root, &path, items, seen);
            }
        }
    }
}

fn visit_dir_for_tools(root: &Path, items: &mut Vec<FixtureItem>, seen: &mut BTreeSet<String>) {
    let tools_dir = root.join("tools");
    if !tools_dir.is_dir() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&tools_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let td_path = path.join("tool_descriptor.json");
                if td_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&td_path) {
                        if let Ok(td) = serde_json::from_str::<serde_json::Value>(&content) {
                            let name = td["name"].as_str().unwrap_or("").to_string();
                            let desc = td["description"].as_str().unwrap_or("").to_string();
                            if name.is_empty() {
                                continue;
                            }
                            let key = format!("tool::{}", name);
                            if seen.insert(key) {
                                let doc_key = DocKey {
                                    kind: CapabilityKind::Tool,
                                    name: name.clone(),
                                };
                                let searchable = format!("{} {}", name, desc);
                                items.push(FixtureItem {
                                    doc_key_val: doc_key,
                                    desc,
                                    searchable,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Terse overrides
// ---------------------------------------------------------------------------
fn load_terse_overrides() -> BTreeMap<DocKey, String> {
    let manifest_path = fixtures_root().join("overrides/manifest.json");
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(_) => return BTreeMap::new(),
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };

    let mut overrides = BTreeMap::new();
    if let Some(fixtures) = manifest["fixtures"].as_array() {
        for fixture in fixtures {
            let doc_key_str = fixture["doc_key"].as_str().unwrap_or("");
            let override_terse = fixture["override_terse"].as_str().unwrap_or("");
            let kind_str = fixture
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("skill");

            if !doc_key_str.is_empty() {
                let kind = match kind_str {
                    "tool" => CapabilityKind::Tool,
                    _ => CapabilityKind::Skill,
                };
                let name = doc_key_str
                    .strip_prefix("skill::")
                    .or_else(|| doc_key_str.strip_prefix("tool::"))
                    .unwrap_or(doc_key_str);
                let doc_key = DocKey {
                    kind,
                    name: name.to_string(),
                };
                overrides.insert(doc_key, override_terse.to_string());
            }
        }
    }
    overrides
}

fn build_items_from_corpus() -> Vec<FixtureItem> {
    let root = fixtures_root();
    let mut items = Vec::new();
    let mut seen = BTreeSet::new();

    visit_dir_for_skills(&root, &root, &mut items, &mut seen);
    visit_dir_for_tools(&root, &mut items, &mut seen);

    items
}

/// Build a `MergedIndex` from the full production corpus with terse overrides.
pub fn build_production_index() -> Arc<ArcSwap<MergedIndex>> {
    let items = build_items_from_corpus();
    let overrides = load_terse_overrides();
    let refs: Vec<&dyn IndexableItem> = items.iter().map(|i| i as &dyn IndexableItem).collect();
    let merged = MergedIndex::from_items_with_overrides(&refs, &overrides);
    Arc::new(ArcSwap::from_pointee(merged))
}
