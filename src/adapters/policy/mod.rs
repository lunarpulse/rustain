//! Interaction-policy file adapter (Story 18.3b).
//!
//! Reads `.rustain/a2a-interaction.toml` and `.rustain/team-policy.toml` into the
//! domain policy types, and ships the empty consent projection the NFR66 explainer
//! consumes until 18-3c fills it.
//!
//! Both the daemon and the CLI consume this, and both are **adapters** — which is
//! why dual consumption does not argue for `infrastructure/`. File I/O is an
//! adapter concern; the pure decision already lives in
//! `domain::services::team_policy`.

pub mod config;
pub mod consent;
pub mod resolve;

pub use config::{PolicyConfigError, PolicyFiles, load_workspace_policies};
pub use consent::EmptyConsentProjection;
pub use resolve::{collect_consent_lines, resolve_workspace_policy};
#[cfg(feature = "test-instrumentation")]
pub use resolve::{reset_workspace_policy_load_count, workspace_policy_load_count};
