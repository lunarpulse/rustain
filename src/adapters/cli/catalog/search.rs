//! `rustain catalog search` — dry-run a query against the index.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::domain::models::doc_key::DocKey;
use crate::infrastructure::search::merged_index::{CachedProjection, MergedIndex};

pub async fn run_catalog_search(
    index: &MergedIndex,
    query: &str,
    kind: &str,
    top_k: usize,
    json: bool,
    want_matched_terms: bool,
) -> Result<i32> {
    if query.trim().is_empty() {
        eprintln!("query must be non-empty");
        return Ok(2);
    }

    if top_k > 20 {
        eprintln!("top_k must be ≤ 20 (got: {})", top_k);
        return Ok(2);
    }

    if top_k < 1 {
        eprintln!("top_k must be ≥ 1 (got: {})", top_k);
        return Ok(2);
    }

    let clamped_k = top_k;

    let kind_filter = match kind {
        "tool" => Some(crate::domain::models::capability_kind::CapabilityKind::Tool),
        "skill" => Some(crate::domain::models::capability_kind::CapabilityKind::Skill),
        "any" => None,
        other => {
            anyhow::bail!(
                "invalid kind filter: {} (expected tool, skill, or any)",
                other
            );
        }
    };

    let mut hits = index.search(query, kind_filter, clamped_k);

    // Enrich matched_terms if requested.
    if want_matched_terms {
        let query_terms = tokenize_to_set(query);
        // Build lookup from iter_items once for O(log n) access.
        let proj_lookup: BTreeMap<&DocKey, &CachedProjection> = index.iter_items().collect();
        for hit in &mut hits {
            let dk = DocKey::new(hit.kind, hit.name.clone());
            if let Some(proj) = proj_lookup.get(&dk) {
                // Use name + terse as an approximation of searchable_text(name, description)
                // since the MergedIndex only stores the terse projection.
                let doc_text = format!("{} {}", hit.name, proj.terse);
                let doc_terms = tokenize_to_set(&doc_text);
                let intersection: Vec<String> =
                    query_terms.intersection(&doc_terms).cloned().collect();
                if !intersection.is_empty() {
                    hit.matched_terms = Some(intersection);
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for hit in &hits {
            println!(
                "{:<6} {:<35} {:.4}  {}",
                hit.kind.as_str(),
                hit.name,
                hit.score,
                hit.terse
            );
            if let Some(ref terms) = hit.matched_terms {
                println!("       matched_terms: [{}]", terms.join(", "));
            }
        }
    }

    Ok(0)
}

fn tokenize_to_set(text: &str) -> BTreeSet<String> {
    use bm25::Tokenizer;
    let tokenizer = bm25::DefaultTokenizer::new(bm25::Language::English);
    tokenizer.tokenize(text).into_iter().collect()
}
