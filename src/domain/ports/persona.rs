#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
use std::path::Path;

/// System prompt and behavioral specialization for the agent.
///
/// Claudian equivalent: `src/core/prompts/mainAgent.ts`
pub trait PersonaPort: Send + Sync {
    fn system_prompt(&self, workspace_path: &Path) -> String;

    // v0.75+: fn profile_identity(&self) -> ProfileIdentity { ... }
    // v0.75+: fn behavioral_rules(&self) -> Vec<String> { vec![] }
}
