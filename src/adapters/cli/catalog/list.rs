//! `rustain catalog list` — enumerate all indexed capabilities.

use anyhow::Result;

use crate::infrastructure::search::merged_index::MergedIndex;

pub async fn run_catalog_list(
    index: &MergedIndex,
    kind: &str,
    json: bool,
    with_mcp: bool,
) -> Result<i32> {
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

    let items: Vec<(
        &crate::domain::models::doc_key::DocKey,
        &crate::infrastructure::search::merged_index::CachedProjection,
    )> = index
        .iter_items()
        .filter(|(_, proj)| {
            if let Some(kf) = kind_filter {
                proj.kind == kf
            } else {
                true
            }
        })
        .collect();

    if json {
        let arr: Vec<serde_json::Value> = items
            .iter()
            .map(|(dk, proj)| {
                let mut obj = serde_json::json!({
                    "name": dk.name,
                    "kind": proj.kind.as_str(),
                    "terse": proj.terse,
                    "indexed": true,
                });
                if let Some(ref pid) = proj.provider {
                    obj["provider"] = serde_json::json!(pid);
                }
                obj
            })
            .collect();
        let json_str = serde_json::to_string_pretty(&arr)?;
        println!("{}", json_str);
    } else {
        println!("{:<6} {:<35} {:<60}", "KIND", "NAME", "TERSE");
        for (dk, proj) in &items {
            let name_trunc = truncate_str(&dk.name, 35);
            let terse_trunc = truncate_str(&proj.terse, 60);
            println!(
                "{:<6} {:<35} {:<60}",
                proj.kind.as_str(),
                name_trunc,
                terse_trunc,
            );
        }
    }

    // Per AC-9-8-1: emit stderr hint when MCP tools are omitted.
    if !with_mcp {
        eprintln!("(MCP tools omitted — pass --with-mcp to include)");
    }

    tracing::info!(
        subcommand = "catalog-list",
        capability_count = items.len(),
        kind_filter = kind
    );
    Ok(0)
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_len - 1).collect();
        format!("{}\u{2026}", truncated)
    }
}
