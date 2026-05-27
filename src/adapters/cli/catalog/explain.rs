//! `rustain catalog explain` — print full profile of a single capability.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::adapters::cli::catalog::OfflineIndex;
use crate::domain::models::capability_kind::CapabilityKind;
use crate::domain::models::doc_key::DocKey;
use crate::infrastructure::search::merged_index::CachedProjection;

pub async fn run_catalog_explain(offline: &OfflineIndex, doc_key_str: &str) -> Result<i32> {
    let (kind_filter, name_filter) = match parse_doc_key(doc_key_str) {
        Some((k, n)) => (k, n),
        None => {
            eprintln!(
                "invalid doc_key format — expected \"tool::\u{003c}name\u{003e}\", \"skill::\u{003c}name\u{003e}\", or \"tool::\u{003c}provider\u{003e}:\u{003c}name\u{003e}\""
            );
            return Ok(2);
        }
    };

    // Build lookup from iter_items once for O(log n) access.
    let proj_lookup: BTreeMap<&DocKey, &CachedProjection> = offline.index.iter_items().collect();

    // Collect matches across tools and skills, skipping irrelevant loops early.
    let mut matches: Vec<ExplainEntry> = Vec::new();

    // Only scan tools if kind is not explicitly Skill.
    if kind_filter.is_none() || kind_filter == Some(CapabilityKind::Tool) {
        for t in &offline.tools {
            if t.name == name_filter {
                let dk = DocKey::new(CapabilityKind::Tool, t.name.clone());
                if let Some(proj) = proj_lookup.get(&dk) {
                    matches.push(ExplainEntry {
                        name: t.name.clone(),
                        kind: CapabilityKind::Tool,
                        provider: proj.provider.clone(),
                        description: t.description.clone(),
                        terse: proj.terse.clone(),
                        source_file: SourceFile::BuiltIn,
                    });
                }
            }
        }
    }

    // Only scan skills if kind is not explicitly Tool.
    if kind_filter.is_none() || kind_filter == Some(CapabilityKind::Skill) {
        for s in &offline.skills {
            if s.name == name_filter {
                let dk = DocKey::new(CapabilityKind::Skill, s.name.clone());
                if let Some(proj) = proj_lookup.get(&dk) {
                    // Find the original SkillDef to get file path.
                    let source_file =
                        if let Some(def) = offline.skill_defs.iter().find(|d| d.name == s.name) {
                            SourceFile::Path(def.file.clone())
                        } else {
                            SourceFile::BuiltIn
                        };
                    matches.push(ExplainEntry {
                        name: s.name.clone(),
                        kind: CapabilityKind::Skill,
                        provider: proj.provider.clone(),
                        description: s.description.clone(),
                        terse: proj.terse.clone(),
                        source_file,
                    });
                }
            }
        }
    }

    if matches.is_empty() {
        eprintln!("not found: {}", doc_key_str);
        eprintln!("Use 'rustain catalog list' to see indexed capabilities");
        return Ok(3);
    }

    if matches.len() > 1 {
        eprintln!(
            "ambiguous doc_key — disambiguate with provider prefix: tool::\u{003c}provider\u{003e}:\u{003c}name\u{003e}"
        );
        for m in &matches {
            if let Some(ref pid) = m.provider {
                eprintln!("  {}::{} (provider: {})", m.kind.as_str(), m.name, pid);
            } else {
                eprintln!("  {}::{}", m.kind.as_str(), m.name);
            }
        }
        return Ok(2);
    }

    let entry = &matches[0];

    // Compute indexed terms by re-tokenizing searchable_text.
    let searchable_text = format!("{} {}", entry.name, entry.description);
    let indexed_terms = tokenize_to_sorted_set(&searchable_text);

    // Determine terse_source.
    let computed_terse =
        crate::domain::services::meta_search::compute_terse(&entry.description, &entry.name);
    let terse_source = if entry.terse == computed_terse {
        "computed"
    } else {
        "override"
    };

    // Get frozen time for determinism in tests.
    let now = frozen_now().unwrap_or(offline.built_at);

    // Print deterministic explain block.
    println!("name: {}", entry.name);
    println!("kind: {}", capitalize_first(entry.kind.as_str()));
    if let Some(ref pid) = entry.provider {
        println!("provider: {}", pid);
    }
    println!("description: {}", entry.description);
    println!("terse: {}", entry.terse);
    println!("terse_source: {}", terse_source);
    println!(
        "indexed_terms: [{}]",
        indexed_terms
            .iter()
            .map(|s| format!("\"{}\"", s))
            .collect::<Vec<_>>()
            .join(", ")
    );

    match &entry.source_file {
        SourceFile::Path(p) => {
            println!("source_file: {}", p.display());
            if let Ok(meta) = std::fs::metadata(p) {
                if let Ok(mtime) = meta.modified() {
                    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
                    println!(
                        "source_mtime: {}",
                        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    );
                }
                println!("source_size_bytes: {}", meta.len());
            }
        }
        SourceFile::BuiltIn => {
            println!("source_file: (built-in, no source file)");
        }
        SourceFile::Mcp(server_id) => {
            println!("source_file: (provided by MCP server: {})", server_id);
        }
    }

    println!(
        "last_indexed_at: {}",
        now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );

    Ok(0)
}

struct ExplainEntry {
    name: String,
    kind: CapabilityKind,
    provider: Option<String>,
    description: String,
    terse: String,
    source_file: SourceFile,
}

enum SourceFile {
    Path(std::path::PathBuf),
    BuiltIn,
    Mcp(String),
}

/// Parse doc_key format: `tool::name`, `skill::name`, or `tool::provider:name`.
/// Returns (kind_filter, name).
fn parse_doc_key(input: &str) -> Option<(Option<CapabilityKind>, String)> {
    // Split on the first `::` only.
    if let Some(pos) = input.find("::") {
        let prefix = &input[..pos];
        let rest = &input[pos + 2..];

        if rest.is_empty() {
            return None;
        }

        match prefix {
            "tool" => Some((Some(CapabilityKind::Tool), rest.to_string())),
            "skill" => Some((Some(CapabilityKind::Skill), rest.to_string())),
            _ => None,
        }
    } else {
        None
    }
}

fn capitalize_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
    }
}

fn tokenize_to_sorted_set(text: &str) -> Vec<String> {
    use bm25::Tokenizer;
    let tokenizer = bm25::DefaultTokenizer::new(bm25::Language::English);
    let tokens = tokenizer.tokenize(text);
    let set: BTreeSet<String> = tokens.into_iter().collect();
    set.into_iter().collect()
}

/// Return a frozen timestamp from RUSTAIN_FROZEN_NOW env var for test determinism.
fn frozen_now() -> Option<chrono::DateTime<chrono::Utc>> {
    let val = crate::infrastructure::utils::env_var_trimmed("RUSTAIN_FROZEN_NOW")?;
    chrono::DateTime::parse_from_rfc3339(&val)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}
