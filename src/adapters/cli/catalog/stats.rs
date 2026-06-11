//! `rustain catalog stats` — print index health metrics.

use std::collections::HashMap;

use anyhow::Result;

use crate::adapters::cli::catalog::OfflineIndex;

pub async fn run_catalog_stats(offline: &OfflineIndex, json: bool) -> Result<i32> {
    let index = &offline.index;
    let total_indexed = index.len();

    let mut count_by_kind = HashMap::new();
    count_by_kind.insert("tool", 0usize);
    count_by_kind.insert("skill", 0usize);

    let mut terse_tokens: Vec<usize> = Vec::new();
    let mut all_searchable_text = String::new();

    for (dk, proj) in index.iter_items() {
        *count_by_kind.entry(dk.kind.as_str()).or_insert(0) += 1;
        let tokens = proj.terse.split_whitespace().count();
        terse_tokens.push(tokens);
        all_searchable_text.push_str(&proj.terse);
        all_searchable_text.push(' ');
    }

    terse_tokens.sort_unstable();

    let p50 = percentile(&terse_tokens, 50);
    let p95 = percentile(&terse_tokens, 95);
    let p99 = percentile(&terse_tokens, 99);

    // Top indexed terms.
    let term_counts = count_terms(&all_searchable_text);
    let mut top_terms: Vec<(&String, &usize)> = term_counts.iter().collect();
    top_terms.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    top_terms.truncate(10);

    let index_serialization_size_bytes = total_indexed * 200;

    if json {
        let obj = serde_json::json!({
            "total_indexed": total_indexed,
            "count_by_kind": count_by_kind,
            "terse_token_percentiles": {
                "p50": p50,
                "p95": p95,
                "p99": p99
            },
            "last_index_rebuild_at": offline.built_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "top_indexed_terms": top_terms.iter().map(|(t, c)| {
                serde_json::json!({"term": t, "count": c})
            }).collect::<Vec<_>>(),
            "index_serialization_size_bytes": index_serialization_size_bytes,
        });
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else {
        println!("total_indexed: {}", total_indexed);
        println!("count_by_kind:");
        let mut kind_entries: Vec<_> = count_by_kind.iter().collect();
        kind_entries.sort_by(|a, b| a.0.cmp(b.0));
        for (kind, count) in &kind_entries {
            println!("  {}: {}", kind, count);
        }
        println!("terse_token_percentiles:");
        println!("  p50: {}", p50);
        println!("  p95: {}", p95);
        println!("  p99: {}", p99);
        println!(
            "last_index_rebuild_at: {}",
            offline
                .built_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        println!("top_indexed_terms:");
        for (term, count) in &top_terms {
            let term_display = if term.chars().count() > 20 {
                let t: String = term.chars().take(20).collect();
                format!("{}\u{2026}", t)
            } else {
                term.to_string()
            };
            println!("  {:<20} {}", term_display, count);
        }
        println!(
            "index_serialization_size_bytes: {}",
            index_serialization_size_bytes
        );
    }

    eprintln!("(token count = whitespace-split words; size_bytes = approximate)");

    Ok(0)
}

fn percentile(sorted: &[usize], p: usize) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let idx_f = (sorted.len() as f64 * p as f64 / 100.0).round() as usize;
    let idx = idx_f.min(sorted.len() - 1);
    sorted[idx]
}

fn count_terms(text: &str) -> HashMap<String, usize> {
    use bm25::Tokenizer;
    let tokenizer = bm25::DefaultTokenizer::new(bm25::Language::English);
    let tokens = tokenizer.tokenize(text);
    let mut counts = HashMap::new();
    for t in tokens {
        *counts.entry(t).or_insert(0) += 1;
    }
    counts
}
