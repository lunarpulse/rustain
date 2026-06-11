use async_trait::async_trait;

use crate::domain::models::sandbox::SandboxPolicy;

/// OS-level sandbox enforcement (ADR-06-04).
///
/// **Orthogonal** to `PermissionMode` and `PermissionChain::check`:
/// - `PermissionMode` is the UX intent surface (`/mode` keybind).
/// - `PermissionChain::check` is the *application-layer* refusal: it asks
///   *"should the model be told no?"* and emits a permission-denied tool result
///   that the model can react to.
/// - `SandboxManager` is the *OS-layer* enforcement: it asks *"if the model
///   ignores the application-layer refusal and the call somehow lands at
///   `tokio::process::Command::spawn`, can the OS prevent the actual write?"*
///
/// Both layers run; both are independent; defense-in-depth.
///
/// # Phase A (Story 9.5)
///
/// Two impls ship:
/// - `NoOpSandbox` (default on macOS/Windows; default on Linux without the
///    `sandbox` cargo feature). All three methods are `Ok(())` no-ops.
/// - `LandlockSandbox` (Linux only, gated on `sandbox` cargo feature). Uses
///    `landlock` crate. `apply()` wraps `Command::pre_exec` with a
///    Landlock ruleset derived from the policy; `restrict_self()` calls
///    `RulesetCreated::restrict_self()` on the current rustain process at
///    startup (belt-and-braces; even if a future code path bypasses the
///    `apply()` wrapper, the parent process is still restricted).
///
/// # Phase B (DEFERRED to a later epic if needed)
///
/// macOS Seatbelt (sandbox-exec) and Windows AppContainer adapters are
/// out-of-scope per ADR-06-04 §Negative ("Platform coverage matrix: Linux P0
/// (Landlock), macOS/Windows deferred — documented as known limitation in
/// NFR33"). Adding either is a new adapter behind a new feature gate; the
/// trait surface stays stable.
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
    /// capability matching. Phase A: `NoOp` or `Landlock`.
    fn kind(&self) -> crate::adapters::sandbox::SandboxAdapterKind;

    /// Apply the policy to a `tokio::process::Command` before `spawn()`.
    ///
    /// For `NoOpSandbox`: returns `Ok(())` no-op. The Command spawns
    /// unrestricted (matching pre-9.5 behavior).
    ///
    /// For `LandlockSandbox`: builds a Landlock ruleset from the policy
    /// (read-only paths, writable roots, network restrictions) and wraps the
    /// Command with `pre_exec` to call `RulesetCreated::restrict_self()` in
    /// the child process between `fork()` and `execve()`. The child inherits
    /// the ruleset; subsequent forks within the child also inherit. This
    /// matches Codex's "applied to the entire process tree spawned by a tool
    /// call" semantics per ADR-06-04 §Context.
    ///
    /// Returns `SandboxError::Unsupported` if the policy variant is not
    /// expressible on the current OS (e.g. requesting network=false on a
    /// platform without Landlock network restriction — Phase A is Linux only).
    /// Returns `SandboxError::RulesetBuildFailed(_)` if the Landlock crate
    /// rejects the ruleset (e.g. malformed path, kernel ABI too old).
    async fn apply(
        &self,
        cmd: &mut tokio::process::Command,
        policy: &SandboxPolicy,
    ) -> Result<(), crate::adapters::sandbox::SandboxError>;

    /// Apply the policy to the *current* rustain process.
    ///
    /// Called once at startup AFTER all initialization that needs filesystem
    /// write access (config load, log file open, skill cache populate) and
    /// BEFORE the first tool call.
    ///
    /// Landlock rulesets are one-way restrictive — once `restrict_self()` is
    /// called, the ruleset can be made stricter but never relaxed. So Phase A's
    /// `restrict_self()` is called with the **least-restrictive plausible
    /// policy** for the current session (workspace-write semantics with
    /// network access) so subsequent per-call `apply()` invocations can
    /// further restrict (Plan mode tightens to read-only; this is allowed).
    ///
    /// Returns `Ok(())` on `NoOpSandbox`. On `LandlockSandbox`, returns
    /// `SandboxError::AbiTooOld(_)` if the kernel ABI is below v3 (Landlock's
    /// minimum). Errors here are NON-FATAL — startup continues with a warning
    /// per ADR-06-04 §Negative ("Platform coverage matrix: Linux P0 ...
    /// documented as known limitation").
    async fn restrict_self(
        &self,
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
