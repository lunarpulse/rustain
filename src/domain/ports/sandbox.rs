use async_trait::async_trait;

use crate::domain::models::sandbox::SandboxPolicy;

/// OS-level sandbox enforcement (ADR-06-04, amended by ADR-23-1-01).
///
/// **Orthogonal** to `PermissionMode` and `PermissionChain::check`:
/// - `PermissionMode` is the UX intent surface (`/mode` keybind).
/// - `PermissionChain::check` is the *application-layer* refusal: it asks
///   *"should the model be told no?"* and emits a permission-denied tool result
///   that the model can react to.
/// - `SandboxManager` is the *OS-layer* enforcement: it asks *"once a command
///   is spawned, what can it and everything it starts touch?"*
///
/// Both layers run; both are independent; defense-in-depth.
///
/// Two impls ship:
/// - `NoOpSandbox`: the sandbox is off, unavailable or not supported on this
///   OS. `apply` is a no-op.
/// - `LandlockSandbox` (Linux, `sandbox` cargo feature, a default feature):
///   `apply` registers a `pre_exec` closure that restricts the child with a
///   Landlock ruleset derived from the policy.
///
/// Confinement is per spawn. The rustain process itself is never confined
/// (ADR-23-1-01 §3): Landlock binds the calling thread only, and a confined
/// rustain could not write its own state.
///
/// # Conformance note
///
/// This trait MUST NOT contain `use` imports from `crate::adapters` or
/// `crate::infrastructure`. Return/adapter types are referenced via full
/// inline paths (e.g., `crate::adapters::sandbox::SandboxError`) per the
/// hexagonal dependency rule enforced by
/// `tests/conformance.rs::test_domain_no_adapter_or_infra_imports`.
#[async_trait]
pub trait SandboxManager: Send + Sync {
    /// Stable identifier for logs, the adapter-status panel (Story 8.5), and
    /// capability matching: `NoOp` or `Landlock`.
    fn kind(&self) -> crate::adapters::sandbox::SandboxAdapterKind;

    /// Apply the policy to a `tokio::process::Command` before `spawn()`.
    ///
    /// For `NoOpSandbox`: returns `Ok(())`; the command spawns unconfined.
    ///
    /// For `LandlockSandbox`: builds the ruleset in the parent and registers a
    /// `pre_exec` closure that restricts the child between `fork()` and
    /// `execve()`; descendants inherit the domain. A restriction that is not
    /// fully enforced fails the spawn: nothing runs unconfined.
    ///
    /// Returns `SandboxError::Unsupported` for a policy the adapter cannot
    /// express (a read-only path under a writable root, or `Permissive`), and
    /// `SandboxError::RulesetBuildFailed(_)` when the ruleset cannot be built.
    async fn apply(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &SandboxPolicy,
    ) -> Result<(), crate::adapters::sandbox::SandboxError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trait_object_safe() {
        let _: Box<dyn SandboxManager> = Box::new(crate::adapters::sandbox::NoOpSandbox);
    }
}
