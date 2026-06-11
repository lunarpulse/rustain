//! Tool-exposure telemetry (Story 9.5 — ADR-09-01 v2.2 §Revisit Triggers).
//!
//! Emits 3 metrics via `tracing::info!` after each `ToolExposurePort::render`
//! invocation. Privacy-by-construction: ONLY closed-enum labels + scalars
//! cross the metric boundary; tool names, schemas, arguments, descriptions
//! NEVER appear. CI conformance test
//! `tests/conformance_tool_exposure_metrics_privacy.rs` greps this file
//! for forbidden field references.

use crate::adapters::tool_exposure::RenderDiagnostics;
use crate::infrastructure::telemetry::active_ratio_window::ActiveRatioWindow;

/// Closed-enum provider identifier. Phase A: 3 variants; Phase B may extend
/// (gemini, mistral, etc.) — total cardinality bounded ≤ ~10 series per
/// metric per epics.md AC-9-5-7 line 4162.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Anthropic,
    OpenAi,
    Ollama,
}

impl ProviderId {
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Ollama => "ollama",
        }
    }
}

/// Metric kind — 2 categorical + 6 template-specific variants.
/// Phase B adds ToolCacheHitDelta + ToolPrimitiveDeprecation per
/// ADR-09-01 v2.2 §Revisit Triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// Categorical — keys aggregator rings.
    Tool,
    /// Categorical — keys aggregator rings.
    Skill,
    /// Template-specific — catalog-size threshold warning.
    ToolCatalogSize,
    /// Template-specific — definition-tokens threshold warning.
    ToolDefinitionTokens,
    /// Template-specific — active-ratio threshold warning.
    ToolActiveRatio,
    /// Template-specific — skill-catalog-size threshold warning.
    SkillCatalogSize,
    /// Template-specific — skill-definition-tokens threshold warning.
    SkillDefinitionTokens,
    /// Template-specific — active-skill-ratio threshold warning.
    SkillActiveRatio,
}

impl MetricKind {
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Skill => "skill",
            Self::ToolCatalogSize => "tool_catalog_size",
            Self::ToolDefinitionTokens => "tool_definition_tokens",
            Self::ToolActiveRatio => "tool_active_ratio",
            Self::SkillCatalogSize => "skill_catalog_size",
            Self::SkillDefinitionTokens => "skill_definition_tokens",
            Self::SkillActiveRatio => "skill_active_ratio",
        }
    }
}

/// Emit the 3 tool-exposure metrics after a successful render.
///
/// Called from the event-loop after `ToolExposurePort::render` returns.
pub async fn emit_after_render(
    provider_id: ProviderId,
    catalog_len: usize,
    diagnostics: &RenderDiagnostics,
    aggregator: &std::sync::Arc<ActiveRatioWindow>,
) {
    // Metric 1: catalog_size (gauge) — count of ToolDescriptors in active
    // filtered catalog. Source: epics.md:4162.
    tracing::info!(
        target: "rustain::telemetry::tool_exposure",
        metric = "catalog_size",
        provider_id = provider_id.as_label(),
        value = catalog_len,
    );

    // Metric 2: definition_tokens_p50_p95 (histogram) — tokens consumed by
    // tool definitions per turn. Phase A emits the per-turn scalar; the
    // downstream consumer maintains the p50/p95 over the window.
    // Source: epics.md:4163.
    let definition_tokens = diagnostics
        .definition_tokens_estimate
        .unwrap_or_else(|| estimate_tool_definition_tokens(catalog_len));
    tracing::info!(
        target: "rustain::telemetry::tool_exposure",
        metric = "definition_tokens_per_turn",
        provider_id = provider_id.as_label(),
        value = definition_tokens,
    );

    // Metric 3: active_tool_ratio (gauge) — ratio of tools actually invoked
    // to tools exposed in trailing 7-day window.
    // Source: epics.md:4164.
    aggregator
        .record_exposure(provider_id, MetricKind::Tool, catalog_len)
        .await;
    let ratio = aggregator.active_ratio(provider_id, MetricKind::Tool).await;
    tracing::info!(
        target: "rustain::telemetry::tool_exposure",
        metric = "active_tool_ratio",
        provider_id = provider_id.as_label(),
        value = ratio,
    );
}

/// Best-effort tool-definition-tokens estimate when RenderDiagnostics doesn't
/// carry the field. Phase A: ~500 tok/tool baseline (matches Anthropic's
/// "55k tokens for 5 servers, ~110 tools" anchor from ADR-09-01 §Context).
fn estimate_tool_definition_tokens(catalog_len: usize) -> usize {
    catalog_len.saturating_mul(500)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_emit_does_not_panic_on_empty_catalog() {
        let aggregator = ActiveRatioWindow::new_in_memory();
        let diagnostics = RenderDiagnostics::clean();
        emit_after_render(ProviderId::Anthropic, 0, &diagnostics, &aggregator).await;
    }

    #[test]
    fn test_provider_id_labels_are_closed_enum() {
        assert_eq!(ProviderId::Anthropic.as_label(), "anthropic");
        assert_eq!(ProviderId::OpenAi.as_label(), "openai");
        assert_eq!(ProviderId::Ollama.as_label(), "ollama");
    }
}
