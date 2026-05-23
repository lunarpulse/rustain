//! Skill-exposure telemetry (Story 9.5 — ADR-09-02 v1 §Revisit Triggers).
//!
//! Emits 3 metrics via `tracing::info!` after each `SkillExposurePort::render`
//! invocation. Privacy-by-construction: ONLY closed-enum labels + scalars
//! cross the metric boundary. CI conformance test
//! `tests/conformance_skill_exposure_metrics_privacy.rs` greps this file
//! for forbidden field references.

use crate::adapters::skill_exposure::SkillRenderDiagnostics;
use crate::infrastructure::telemetry::active_ratio_window::ActiveRatioWindow;
use crate::infrastructure::telemetry::tool_exposure_metrics::{MetricKind, ProviderId};

/// Emit the 3 skill-exposure metrics after a successful render.
///
/// Symmetric to `tool_exposure_metrics::emit_after_render`.
pub async fn emit_after_render(
    provider_id: ProviderId,
    diagnostics: &SkillRenderDiagnostics,
    aggregator: &std::sync::Arc<ActiveRatioWindow>,
) {
    // Metric 4: skill_catalog_size (gauge). Source: epics.md:4166.
    tracing::info!(
        target: "rustain::telemetry::skill_exposure",
        metric = "catalog_size",
        provider_id = provider_id.as_label(),
        value = diagnostics.catalog_size,
    );

    // Metric 5: skill_definition_tokens_per_turn (histogram source).
    // Source: epics.md:4167.
    tracing::info!(
        target: "rustain::telemetry::skill_exposure",
        metric = "definition_tokens_per_turn",
        provider_id = provider_id.as_label(),
        value = diagnostics.definition_tokens_estimate,
    );

    // Metric 6: active_skill_ratio (gauge over 7d window).
    // Source: epics.md:4168.
    aggregator.record_exposure(provider_id, MetricKind::Skill, diagnostics.catalog_size).await;
    let ratio = aggregator.active_ratio(provider_id, MetricKind::Skill).await;
    tracing::info!(
        target: "rustain::telemetry::skill_exposure",
        metric = "active_skill_ratio",
        provider_id = provider_id.as_label(),
        value = ratio,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_emit_does_not_panic() {
        let aggregator = ActiveRatioWindow::new_in_memory();
        let diagnostics = SkillRenderDiagnostics::clean(3, 300);
        emit_after_render(ProviderId::Anthropic, &diagnostics, &aggregator).await;
    }
}
