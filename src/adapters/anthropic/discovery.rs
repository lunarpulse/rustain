//! Anthropic Models API over the adapter's shared request authentication.
use super::AnthropicAdapter;
use crate::adapters::model_catalog_cache::request_scope;
use crate::adapters::model_filter::{compile_filter_patterns, is_noise_model_id, matches_any};
use crate::domain::errors::ProviderError;
use crate::domain::models::provider::{DiscoveredModels, ModelRecord};
use serde::Deserialize;

#[derive(Deserialize)]
struct Page {
    data: Vec<Item>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    display_name: Option<String>,
}

impl AnthropicAdapter {
    pub(super) async fn fetch_remote_models(
        &self,
        model_filter: &[String],
        scope_context: &str,
    ) -> Result<DiscoveredModels, ProviderError> {
        let url = if self.base_url.ends_with("/v1") {
            format!("{}/models", self.base_url)
        } else {
            format!("{}/v1/models", self.base_url)
        };
        let (mut resolved, plan) = self.current_auth().await?;
        let mut auth_retried = false;
        let mut after: Option<String> = None;
        let mut models = Vec::new();
        let filter = compile_filter_patterns(model_filter);
        for _ in 0..5 {
            let page: Page =
                loop {
                    let mut request = self
                        .request_auth(self.client.get(&url), &resolved.auth, &plan)
                        .query(&[("limit", "1000")])
                        .timeout(std::time::Duration::from_secs(5));
                    if let Some(after) = &after {
                        request = request.query(&[("after_id", after)]);
                    }
                    let profile = self.profile(&resolved.auth, &plan)?;
                    if let Some(profile) = &profile {
                        request = profile.headers(request, false);
                    }
                    let response = request.send().await.map_err(|error| {
                        crate::adapters::provider::classify_reqwest_error(&error)
                    })?;
                    let status = response.status().as_u16();
                    if status == 401 && self.recover_auth(&mut resolved, &mut auth_retried).await? {
                        continue;
                    }
                    if matches!(status, 401 | 403) {
                        return Err(ProviderError::AuthenticationFailed);
                    }
                    if matches!(status, 404 | 405) {
                        return Err(ProviderError::EndpointUnsupported(status));
                    }
                    let text = response.text().await.map_err(|error| {
                        crate::adapters::provider::classify_reqwest_error(&error)
                    })?;
                    if !(200..300).contains(&status) {
                        if let Some(profile) = &profile
                            && super::claude_code::adopt_required(&text, &profile.version)
                        {
                            continue;
                        }
                        return Err(ProviderError::Other(format!(
                            "Models fetch failed: HTTP {status}"
                        )));
                    }
                    break serde_json::from_str(&text).map_err(|_| {
                        ProviderError::Other("Could not parse Anthropic models response".into())
                    })?;
                };
            let last_id = page
                .last_id
                .or_else(|| page.data.last().map(|item| item.id.clone()));
            for item in page.data {
                if !is_noise_model_id(&item.id) && matches_any(&filter, &item.id) {
                    models.push(ModelRecord {
                        model_id: item.id,
                        display_name: item.display_name,
                        ..Default::default()
                    });
                }
            }
            if !page.has_more {
                // Stamp the credential this request actually sent, including a
                // fallthrough past a stale row whose refresh failed.
                return Ok(DiscoveredModels {
                    records: models,
                    credential_scope: request_scope(scope_context, &resolved),
                });
            }
            if last_id.is_none() || last_id == after {
                return Err(ProviderError::Other(
                    "Anthropic models pagination did not advance".into(),
                ));
            }
            after = last_id;
        }
        Err(ProviderError::Other(
            "Anthropic models exceeded the five-page discovery limit".into(),
        ))
    }
}
