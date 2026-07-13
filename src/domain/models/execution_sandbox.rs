//! Domain value types for the [`ExecutionSandbox`](crate::domain::ports::ExecutionSandbox)
//! seam (Story 17.3a, FR146).
//!
//! These are the pure request/outcome shapes crossing the sandbox boundary.
//! They name NO `wasmtime` type: the wasmtime backend
//! (`crate::adapters::wasm::WasmIsolationBackend`, feature `wasm-sandbox`)
//! maps them onto its runtime once, at the adapter edge. Keeping them here lets
//! the port compile unconditionally (pure domain) while the heavy runtime stays
//! behind an off-by-default feature.
//!
//! Design (party rulings F2, N1):
//! - **Per-call capability grant.** A [`CapabilityGrant`] rides on every
//!   [`SandboxInvocation`]; there is no ambient/global capability surface. Two
//!   invocations with different grants share nothing (per-call isolation).
//! - **Zero secret-*value* exposure (N1).** The only secret-adjacent host
//!   surface is existence-boolean ([`HostImport::HasCredential`]) or
//!   host-mediated ([`HostImport::Sign`] — the host holds the secret and
//!   returns only the derived result). A raw secret value never enters the
//!   grant, the invocation, or guest linear memory. Side channels
//!   (fuel/timing/memory oracles) are explicitly out of scope — see DF-17-3a-1.
//! - **A trap is a normal outcome, not a backend error.** When a guest exceeds
//!   a cap or misbehaves, [`ExecutionSandbox::invoke`](crate::domain::ports::ExecutionSandbox::invoke)
//!   returns `Ok(SandboxOutcome { trap: Some(..), .. })` — the sandbox did its
//!   job (contained the guest). `ExecutionSandboxError` is reserved for the
//!   backend itself failing (e.g. an unregistered component).

use std::path::PathBuf;

/// Opaque handle to a guest component the backend has already compiled and
/// type-checked into a cached `InstancePre`.
///
/// This is deliberately **not** raw component bytes: compiling a component is
/// expensive (Cranelift) and happens once, out of band, via the concrete
/// backend's own registration path. `invoke` only ever carries this cheap
/// handle. The wrapped string is an opaque backend key (e.g. a content hash);
/// callers must treat it as such.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ComponentRef(String);

impl ComponentRef {
    /// Wrap a backend-assigned component key.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The opaque key, for cache lookup at the adapter edge.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Per-call resource ceilings. Exceeding any of these **traps** the guest —
/// never hangs the host, never crashes the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceCaps {
    /// Deterministic instruction quota (wasmtime fuel). Exhaustion ⇒
    /// [`TrapKind::OutOfFuel`].
    pub fuel: u64,
    /// Wall-clock timeout expressed in epoch ticks (the backend increments the
    /// engine epoch on a fixed cadence). Exceeded ⇒ [`TrapKind::EpochDeadline`].
    pub epoch_ticks: u64,
    /// Linear-memory ceiling in bytes. A growth request past this ⇒
    /// [`TrapKind::MemoryLimit`].
    pub memory_bytes: usize,
    // NOTE: guest native-stack depth is an *engine-level* cap in wasmtime
    // (`Config::max_wasm_stack`), shared by every `Store` over that engine, so
    // it is configured once on `WasmIsolationBackend` rather than per call. A
    // deep-recursion guest still traps ([`TrapKind::StackOverflow`]); the limit
    // just is not a per-invocation knob.
}

/// A preopened directory offered to the guest. Deny-by-default: a guest with an
/// empty `preopens` list sees no filesystem at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreopenGrant {
    /// Host-side directory to expose.
    pub host_path: PathBuf,
    /// Path the guest sees it mounted at.
    pub guest_path: String,
    /// Whether the guest may write (false ⇒ read-only).
    pub writable: bool,
}

/// A single named host import the guest is permitted to call. Deny-by-default:
/// only imports in [`CapabilityGrant::host_imports`] are linked into the
/// per-call `Linker`; a guest that imports anything else **fails to
/// instantiate** ([`TrapKind::UngrantedImport`]).
///
/// Crucially, this never hands a secret *value* to the guest (N1): the only
/// secret-adjacent import is an existence boolean.
///
/// The vocabulary is intentionally the minimal *enforced + proven* set (17-2c
/// "no dead methods" applied to variants). It is `#[non_exhaustive]`: a
/// host-mediated signer, a wall clock, or a network egress probe are added
/// alongside their first consumer + proving fixture, not shipped inert. See
/// ADR-17-3a-01.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostImport {
    /// `has-credential() -> bool`: existence-only probe. Returns whether a
    /// credential is configured on the host, never its bytes.
    HasCredential,
}

/// The capability surface granted for exactly one invocation. Assembled fresh
/// into a fresh `Store`/`Linker` each call — never cached on the backend, never
/// global. Deny-by-default: an empty grant is a pure-computation sandbox with
/// no filesystem and no host imports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityGrant {
    /// Preopened directories offered to the guest (empty ⇒ no filesystem).
    /// Wired into the deny-by-default `WasiCtxBuilder` at the adapter edge.
    pub preopens: Vec<PreopenGrant>,
    /// Named host imports linked into the guest (empty ⇒ no host surface).
    pub host_imports: Vec<HostImport>,
}

/// One sandboxed call: which component, which export, the input bytes, the
/// per-call capability grant, and the resource ceilings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxInvocation {
    /// The already-registered component to instantiate freshly.
    pub component: ComponentRef,
    /// The component export (entry point) to run.
    pub entry: String,
    /// Opaque input bytes handed to the guest.
    pub input: Vec<u8>,
    /// The only capability surface for this call.
    pub grant: CapabilityGrant,
    /// The resource ceilings for this call.
    pub caps: ResourceCaps,
}

/// Why a guest was trapped. A trap is the sandbox working correctly — it is a
/// normal [`SandboxOutcome`], not an [`ExecutionSandboxError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrapKind {
    /// Deterministic instruction quota exhausted (fuel).
    OutOfFuel,
    /// Wall-clock epoch deadline exceeded.
    EpochDeadline,
    /// Linear-memory ceiling hit.
    MemoryLimit,
    /// Native stack ceiling overrun.
    StackOverflow,
    /// The guest imported a capability it was not granted; refused at
    /// instantiate (deny-by-default).
    UngrantedImport,
    /// The invocation's [`CancellationToken`](tokio_util::sync::CancellationToken)
    /// fired before the guest finished.
    Cancelled,
    /// The guest executed a `wasm` `unreachable`/explicit trap of its own.
    GuestTrap,
}

/// The result of a sandboxed call. `trap == None` ⇒ the guest ran to completion
/// and `output` holds its bytes; `trap == Some(..)` ⇒ the guest was contained
/// and `output` is whatever it produced before the trap (usually empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutcome {
    /// Bytes the guest produced (empty on most traps).
    pub output: Vec<u8>,
    /// Fuel actually consumed (for accounting / reproducibility assertions).
    pub fuel_consumed: u64,
    /// `Some` iff the guest was trapped; carries the reason.
    pub trap: Option<TrapKind>,
}

impl SandboxOutcome {
    /// Whether the guest completed without being trapped.
    pub fn is_ok(&self) -> bool {
        self.trap.is_none()
    }
}
