//! Interaction-policy file and consent-projection adapter.
//!
//! Reads workspace policy files into domain models and folds the durable room
//! journal into the synchronous consent query consumed by daemon policy
//! composition and `doctor`.
//!
//! Both the daemon and the CLI consume this, and both are **adapters** — which is
//! why dual consumption does not argue for `infrastructure/`. File I/O is an
//! adapter concern; the pure decision already lives in
//! `domain::services::team_policy`.

pub mod config;
pub mod consent;
pub mod resolve;

pub use config::{PolicyConfigError, PolicyFiles, load_workspace_policies};
pub use consent::{EmptyConsentProjection, JournalConsentProjection};
pub use resolve::{collect_consent_lines, resolve_workspace_policy};
#[cfg(feature = "test-instrumentation")]
pub use resolve::{reset_workspace_policy_load_count, workspace_policy_load_count};
