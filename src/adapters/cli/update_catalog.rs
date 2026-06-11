//! CLI subcommand: `rustain update-catalog` (Story 7.7 AC4).
//!
//! Fetches `/v1/models` from each known OpenAI-compatible provider variant and
//! writes a `CachedCatalog` to `models_variants.json`. Each provider fetch has a
//! 10s timeout, retried once on failure. Exits non-zero if all providers fail.
//!
//! Only compiled when `feature = "openai"` is enabled; the CLI enum variant exists
//! unconditionally but startup.rs returns an error if the feature is missing.

#![allow(unused_imports)] // chrono used in tracing paths

use std::path::PathBuf;

use crate::adapters::model_catalog_cache::{CachedCatalog, CachedModelEntry, CachedProviderEntry};
use crate::domain::models::ModelDescriptor;

/// Known built-in provider metadata for the update-catalog CLI.
struct BuiltinProvider {
    id: &'static str,
    display_name: &'static str,
    default_base_url: &'static str,
    api_key_env: &'static str,
}

const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        id: "openai",
        display_name: "OpenAI",
        default_base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
    },
    BuiltinProvider {
        id: "deepseek",
        display_name: "DeepSeek",
        default_base_url: "https://api.deepseek.com/v1",
        api_key_env: "DEEPSEEK_API_KEY",
    },
    BuiltinProvider {
        id: "openrouter",
        display_name: "OpenRouter",
        default_base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
    },
    BuiltinProvider {
        id: "google",
        display_name: "Google",
        default_base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        api_key_env: "GEMINI_API_KEY",
    },
    BuiltinProvider {
        id: "moonshot",
        display_name: "Moonshot",
        default_base_url: "https://api.moonshot.cn/v1",
        api_key_env: "MOONSHOT_API_KEY",
    },
];

pub async fn run_update_catalog(
    output: Option<PathBuf>,
    filter_providers: Vec<String>,
) -> anyhow::Result<()> {
    let out_path = output.unwrap_or_else(|| PathBuf::from("models_variants.json"));

    let providers: Vec<&BuiltinProvider> = if filter_providers.is_empty() {
        BUILTIN_PROVIDERS.iter().collect()
    } else {
        BUILTIN_PROVIDERS
            .iter()
            .filter(|p| filter_providers.contains(&p.id.to_string()))
            .collect()
    };

    if providers.is_empty() {
        anyhow::bail!(
            "No matching providers found in filter: {:?}",
            filter_providers
        );
    }

    let mut catalog = crate::adapters::model_catalog_cache::load_embedded_seed()
        .cloned()
        .unwrap_or_default();
    let mut updated = 0u32;
    let mut failed = 0u32;

    for provider in &providers {
        let api_key =
            crate::infrastructure::utils::env_var_trimmed(provider.api_key_env).unwrap_or_default();
        let base_url = provider.default_base_url;

        eprintln!(
            "Fetching models from {} ({}):",
            provider.display_name, provider.id
        );

        let result = fetch_models_with_retry(base_url, &api_key, provider.id).await;

        match result {
            Ok(models) => {
                if models.is_empty() {
                    failed += 1;
                    eprintln!("  FAILED: empty model list — skipping to preserve existing catalog");
                    continue;
                }
                let entries: Vec<CachedModelEntry> = models
                    .into_iter()
                    .map(|descriptor| CachedModelEntry { descriptor })
                    .collect();
                let count = entries.len();
                catalog.providers.insert(
                    provider.id.to_string(),
                    CachedProviderEntry {
                        fetched_at_unix: chrono::Utc::now().timestamp(),
                        models: entries,
                    },
                );
                updated += 1;
                eprintln!("  OK: {} models fetched", count);
            }
            Err(e) => {
                failed += 1;
                eprintln!("  FAILED: {}", e);
            }
        }
    }

    if updated == 0 {
        anyhow::bail!(
            "All {} provider(s) failed — no catalog data written",
            providers.len()
        );
    }

    // Sort providers by key and models by model_id for deterministic output
    let mut value = serde_json::to_value(&catalog)?;
    if let Some(providers) = value.get_mut("providers").and_then(|p| p.as_object_mut()) {
        for (_key, provider_val) in providers.iter_mut() {
            if let Some(models) = provider_val
                .get_mut("models")
                .and_then(|m| m.as_array_mut())
            {
                models.sort_by(|a, b| {
                    let a_id = a.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                    let b_id = b.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                    a_id.cmp(b_id)
                });
            }
        }
    }
    let json = serde_json::to_string_pretty(&value)?;
    std::fs::write(&out_path, json)?;
    eprintln!(
        "Wrote catalog to {} ({} updated, {} failed)",
        out_path.display(),
        updated,
        failed
    );

    if failed > 0 {
        eprintln!("Warning: some providers failed — the catalog is partial.");
    }

    Ok(())
}

async fn fetch_models_with_retry(
    base_url: &str,
    api_key: &str,
    provider_id: &str,
) -> Result<Vec<ModelDescriptor>, anyhow::Error> {
    let url = format!(
        "{}/models",
        crate::infrastructure::utils::normalize_base_url(base_url)
    );

    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create HTTP client: {}", e))?;

    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=1 {
        let mut req = client.get(&url).timeout(std::time::Duration::from_secs(10));
        if !api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", api_key));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await?;
                let parsed = crate::adapters::openai::discovery::parse_and_filter_models(
                    &text,
                    &crate::adapters::openai::OpenAiCompatibleVariant::Custom {
                        provider_id: provider_id.to_string(),
                        display_name: String::new(),
                        context_window: None,
                        supports_tools: None,
                    },
                    &["*".to_string()],
                )
                .map_err(|e| anyhow::anyhow!("{}", e))?;
                return Ok(parsed);
            }
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 401 {
                    return Err(anyhow::anyhow!(
                        "Authentication failed (HTTP 401) — check API key env var"
                    ));
                }
                last_err = Some(anyhow::anyhow!("HTTP {}", status));
            }
            Err(e) => {
                last_err = Some(anyhow::anyhow!("Connection failed: {}", e));
            }
        }

        if attempt == 0 {
            eprintln!("  Retrying in 1s...");
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Unknown error")))
}
