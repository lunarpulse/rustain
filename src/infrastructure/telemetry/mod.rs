//! Telemetry subsystem (Story 9.5 — ADR-09-01 v2.2 + ADR-09-02 v1).
//!
//! Emits 6 metrics via `tracing::info!` structured events:
//! - 3 tool-side: catalog_size, definition_tokens_per_turn, active_tool_ratio
//! - 3 skill-side: catalog_size, definition_tokens_per_turn, active_skill_ratio
//!
//! # Privacy-by-construction
//!
//! Tool/skill names, arguments, schemas, and descriptions NEVER appear in
//! metric labels — only closed-enum `provider_id` labels and scalar values.
//! CI conformance tests at `tests/conformance_tool_exposure_metrics_privacy.rs`
//! and `tests/conformance_skill_exposure_metrics_privacy.rs` enforce this
//! statically.

pub mod active_ratio_window;
pub mod skill_exposure_metrics;
pub mod tool_exposure_metrics;

pub use active_ratio_window::ActiveRatioWindow;
pub use skill_exposure_metrics::emit_after_render as emit_skill_after_render;
pub use tool_exposure_metrics::emit_after_render as emit_tool_after_render;
pub use tool_exposure_metrics::{MetricKind, ProviderId};
