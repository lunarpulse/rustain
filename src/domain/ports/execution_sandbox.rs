//! `ExecutionSandbox` — the WASM execution-isolation seam (Story 17.3a, FR146).
//!
//! This is a **sibling** port of
//! [`IsolationProvider`](crate::domain::ports::IsolationProvider), never a
//! super-trait or a widening of it (ADR-11-3 rule 3; §1A "different kinds of
//! isolation"). The two share only the English principle "capability-scoped
//! backend behind the approval seam" — no super-trait, no method signature, no
//! value type. `IsolationProvider` models *filesystem* CoW scratch copies and
//! produces a serializable `UnifiedDiff`; `ExecutionSandbox` models a
//! *syscall/memory* boundary and produces execution *outcomes* (output bytes,
//! traps). The `tests/trybuild_execution_sandbox.rs` compile-fail + pass-twin
//! pair proves the sibling relationship structurally (DF-14-5-3).
//!
//! The trait is intentionally minimal (party ruling F2, 17-2c "no dead
//! methods"): a single `invoke`. There is no `load`/`compile`/`warm_up` method
//! — compiling a component is the concrete backend's own concern, reached via
//! its inherent API, and a trait method is added only when a code path calls
//! it through the `dyn` boundary. Registration is not (yet) such a path.
//!
//! Scope honesty (party ruling N4): there is no untrusted-tool population in
//! rustain today, so this port ships as a *proven backend behind the seam*
//! whose proving consumer is the adversarial fixture suite — NOT a production
//! tool-dispatch call site. It is bound at the composition root as
//! `Option<Arc<dyn ExecutionSandbox>>`, defaulting to `None`.

use tokio_util::sync::CancellationToken;

use crate::domain::models::{SandboxInvocation, SandboxOutcome};

/// Failure surface of the sandbox *backend itself* — distinct from a guest
/// trap, which is a normal [`SandboxOutcome`] (the sandbox working correctly).
///
/// Kept in `domain/` so the trait carries no infra error type; the wasmtime
/// adapter maps its own errors into these variants at the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecutionSandboxError {
    /// `invoke` named a component the backend has not compiled/registered.
    ComponentNotRegistered(String),
    /// The named entry point does not exist on the component.
    EntryNotFound(String),
    /// The backend engine/runtime failed for a reason unrelated to guest
    /// behaviour or policy (e.g. a malformed component, an engine
    /// misconfiguration). Carries a sanitized message.
    Backend(String),
}

impl std::fmt::Display for ExecutionSandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ComponentNotRegistered(r) => {
                write!(f, "execution-sandbox: component not registered: {r}")
            }
            Self::EntryNotFound(e) => write!(f, "execution-sandbox: entry not found: {e}"),
            Self::Backend(msg) => write!(f, "execution-sandbox backend error: {msg}"),
        }
    }
}

impl std::error::Error for ExecutionSandboxError {}

/// Run untrusted guest code inside a resource-capped, deny-by-default sandbox.
///
/// Every call gets a fresh instance over a fresh `Store`, its capabilities
/// assembled solely from `req.grant`, its runaway behaviour bounded by
/// `req.caps`. A guest that misbehaves is *trapped* — returned as
/// `Ok(SandboxOutcome { trap: Some(..), .. })`, never a hang and never a host
/// crash. `Err(ExecutionSandboxError)` means the backend itself could not run
/// the call.
#[async_trait::async_trait]
pub trait ExecutionSandbox: Send + Sync {
    /// Invoke `req.entry` on `req.component` with the per-call grant and caps.
    /// `cancel` cooperatively aborts a still-running guest (⇒
    /// [`TrapKind::Cancelled`](crate::domain::models::TrapKind::Cancelled)).
    async fn invoke(
        &self,
        req: SandboxInvocation,
        cancel: CancellationToken,
    ) -> Result<SandboxOutcome, ExecutionSandboxError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn assert_object_safe(_: Arc<dyn ExecutionSandbox>) {}

    #[test]
    fn execution_sandbox_is_object_safe() {
        // Compiles iff `dyn ExecutionSandbox` is object-safe (the composition
        // root binds it as `Arc<dyn ExecutionSandbox>`).
        let _ = assert_object_safe as fn(Arc<dyn ExecutionSandbox>);
    }
}
