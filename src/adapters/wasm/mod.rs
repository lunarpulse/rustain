//! `WasmIsolationBackend` — the wasmtime component-model implementation of the
//! [`ExecutionSandbox`](crate::domain::ports::ExecutionSandbox) port (Story
//! 17.3a, FR146, ADR-17-3a-01). Feature `wasm-sandbox`.
//!
//! # Invariants this backend enforces
//!
//! - **Fresh instance per call.** The expensive Cranelift compile
//!   ([`Component::new`]) happens once, at construction, and is cached. Each
//!   [`invoke`](ExecutionSandbox::invoke) builds a fresh `Store` and a fresh
//!   `Linker`, then instantiates — no state survives between calls.
//! - **Per-call capability grant is the ONLY surface.** The `Linker` is built
//!   from `req.grant` every call; a host import that is not granted is simply
//!   absent, so a guest that imports it **fails to instantiate**
//!   ([`TrapKind::UngrantedImport`]). This is deny-by-default made *structural*
//!   (stronger than gating behaviour inside always-linked closures): it is why
//!   the linker is rebuilt per call rather than the `InstancePre` being cached.
//!   The Cranelift-compiled `Component` — the actual expense — IS cached; only
//!   the cheap linker/type-check is per-call. (Conscious deviation from the
//!   literal "cache the `InstancePre`" wording of the story, recorded in
//!   ADR-17-3a-01, chosen because deny-by-default at an untrusted boundary
//!   outranks a micro-optimization.)
//! - **Caps trap, never hang or crash.** Fuel (deterministic instruction
//!   quota), an epoch deadline (wall-clock, via a background epoch ticker), and
//!   a memory ceiling (a custom [`wasmtime::ResourceLimiter`]) each convert a
//!   runaway guest into a trap outcome.
//! - **Zero secret-VALUE exposure (N1).** The only secret-adjacent host import
//!   is `has-credential() -> bool` — an existence boolean. There is no
//!   byte-returning secret accessor in the vocabulary at all, so a guest cannot
//!   obtain a value regardless of its behaviour. Side-channel oracles
//!   (fuel/timing/memory) are out of scope — DF-17-3a-1.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use wasmtime::component::{Component, HasData, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store, StoreContextMut, UpdateDeadline};
use wasmtime_wasi::filesystem::{WasiFilesystem, WasiFilesystemView};
use wasmtime_wasi::p2::bindings::filesystem;
use wasmtime_wasi::{DirPerms, FilePerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::domain::models::{
    CapabilityGrant, HostImport, SandboxInvocation, SandboxOutcome, TrapKind,
};
use crate::domain::ports::{ExecutionSandbox, ExecutionSandboxError};

/// How often the background ticker advances the engine epoch. A per-call
/// `epoch_ticks` cap therefore corresponds to roughly `epoch_ticks * 2ms` of
/// wall-clock time before an [`TrapKind::EpochDeadline`] trap.
const EPOCH_TICK: std::time::Duration = std::time::Duration::from_millis(2);

/// Engine-wide guest native-stack ceiling (see `ResourceCaps` note: stack is an
/// engine-level cap in wasmtime, not per-`Store`). 512 KiB is generous for the
/// small guests this backend runs while still trapping deep recursion.
const MAX_WASM_STACK: usize = 512 * 1024;

/// Store projection used by the selectively linked WASI I/O interfaces.
struct HasIo;

impl HasData for HasIo {
    type Data<'a> = &'a mut ResourceTable;
}

/// Per-`Store` host state. Carries the WASI context (deny-by-default), the
/// resource-limiter bookkeeping, and the host-held secret existence oracle.
struct HostState {
    wasi: WasiCtx,
    table: ResourceTable,
    /// Existence answer for `has-credential` — host-held. The guest learns only
    /// this boolean, never a secret value (N1).
    secret_present: bool,
    /// Per-call linear-memory ceiling in bytes.
    memory_cap: usize,
    /// Per-call table-element ceiling derived from `memory_cap`.
    table_element_cap: usize,
    /// Set when instantiation is refused because an initial memory or table
    /// allocation exceeds its cap.
    resource_denied: bool,
    /// Cancellation and epoch state are store-local. The engine ticker is
    /// global, but each callback decides independently whether this store ends.
    cancel: CancellationToken,
    epoch_ticks_remaining: u64,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_cap {
            self.resource_denied = true;
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.table_element_cap {
            self.resource_denied = true;
            Ok(false)
        } else {
            Ok(true)
        }
    }
}

/// A dedicated thread advances the engine epoch on a fixed cadence so a
/// CPU-bound guest cannot starve its own timeout on a single-threaded async
/// runtime. The thread is stopped and joined on drop.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// wasmtime component-model implementation of [`ExecutionSandbox`].
pub struct WasmIsolationBackend {
    engine: Engine,
    /// Compiled components, keyed by [`ComponentRef`](crate::domain::models::ComponentRef)
    /// string. Immutable after construction (compile once, reuse).
    components: HashMap<String, Component>,
    /// Host-held: whether a credential is configured. Surfaced to guests only
    /// as an existence boolean via `has-credential`.
    secret_present: bool,
    _ticker: EpochTicker,
}

impl WasmIsolationBackend {
    /// Build the backend, compiling every `(name, component-bytes)` source once
    /// and spawning the epoch ticker. `secret_present` seeds the host-held
    /// existence oracle for `has-credential`.
    pub fn new(
        sources: HashMap<String, Vec<u8>>,
        secret_present: bool,
    ) -> Result<Self, ExecutionSandboxError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(MAX_WASM_STACK);
        let engine =
            Engine::new(&config).map_err(|e| ExecutionSandboxError::Backend(e.to_string()))?;

        let mut components = HashMap::with_capacity(sources.len());
        for (name, bytes) in sources {
            let component = Component::new(&engine, &bytes)
                .map_err(|e| ExecutionSandboxError::Backend(format!("compile {name}: {e}")))?;
            components.insert(name, component);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let engine = engine.clone();
            let thread_stop = Arc::clone(&stop);
            let thread = std::thread::Builder::new()
                .name("rustain-wasm-epoch".to_string())
                .spawn(move || {
                    while !thread_stop.load(Ordering::Acquire) {
                        std::thread::sleep(EPOCH_TICK);
                        engine.increment_epoch();
                    }
                })
                .map_err(|e| ExecutionSandboxError::Backend(format!("spawn epoch ticker: {e}")))?;
            EpochTicker {
                stop,
                thread: Some(thread),
            }
        };

        Ok(Self {
            engine,
            components,
            secret_present,
            _ticker: ticker,
        })
    }

    /// Assemble the deny-by-default WASI context from the grant. An empty grant
    /// ⇒ no filesystem, no network, no env, no args (the guest sees nothing).
    fn build_wasi(grant: &CapabilityGrant) -> Result<WasiCtx, ExecutionSandboxError> {
        let mut builder = WasiCtxBuilder::new();
        for pre in &grant.preopens {
            let (dir_perms, file_perms) = if pre.writable {
                (
                    DirPerms::READ | DirPerms::MUTATE,
                    FilePerms::READ | FilePerms::WRITE,
                )
            } else {
                (DirPerms::READ, FilePerms::READ)
            };
            builder
                .preopened_dir(&pre.host_path, &pre.guest_path, dir_perms, file_perms)
                .map_err(|e| ExecutionSandboxError::Backend(format!("preopen: {e}")))?;
        }
        Ok(builder.build())
    }

    /// Build the per-call linker: WASI (deny-by-default) plus exactly the
    /// granted host imports. A host import the guest needs but the grant omits
    /// is absent here ⇒ instantiate fails ⇒ [`TrapKind::UngrantedImport`].
    fn build_linker(
        &self,
        grant: &CapabilityGrant,
    ) -> Result<Linker<HostState>, ExecutionSandboxError> {
        let mut linker = Linker::<HostState>::new(&self.engine);

        // Do not install the catch-all WASI CLI world: it includes host clocks
        // and RNGs even for an otherwise empty WasiCtx. A filesystem grant gets
        // only filesystem and its required stream/poll/error interfaces.
        if !grant.preopens.is_empty() {
            wasmtime_wasi_io::bindings::wasi::io::error::add_to_linker::<HostState, HasIo>(
                &mut linker,
                |state| &mut state.table,
            )
            .map_err(|e| ExecutionSandboxError::Backend(format!("wasi io-error link: {e}")))?;
            wasmtime_wasi_io::bindings::wasi::io::poll::add_to_linker::<HostState, HasIo>(
                &mut linker,
                |state| &mut state.table,
            )
            .map_err(|e| ExecutionSandboxError::Backend(format!("wasi io-poll link: {e}")))?;
            wasmtime_wasi_io::bindings::wasi::io::streams::add_to_linker::<HostState, HasIo>(
                &mut linker,
                |state| &mut state.table,
            )
            .map_err(|e| ExecutionSandboxError::Backend(format!("wasi io-streams link: {e}")))?;
            filesystem::preopens::add_to_linker::<HostState, WasiFilesystem>(
                &mut linker,
                <HostState as WasiFilesystemView>::filesystem,
            )
            .map_err(|e| ExecutionSandboxError::Backend(format!("wasi preopens link: {e}")))?;
            filesystem::types::add_to_linker::<HostState, WasiFilesystem>(
                &mut linker,
                <HostState as WasiFilesystemView>::filesystem,
            )
            .map_err(|e| ExecutionSandboxError::Backend(format!("wasi filesystem link: {e}")))?;
        }

        for imp in &grant.host_imports {
            match imp {
                HostImport::HasCredential => {
                    linker
                        .root()
                        .func_wrap(
                            "has-credential",
                            |store: StoreContextMut<'_, HostState>, (): ()| {
                                Ok((store.data().secret_present,))
                            },
                        )
                        .map_err(|e| {
                            ExecutionSandboxError::Backend(format!("link has-credential: {e}"))
                        })?;
                }
            }
        }
        Ok(linker)
    }
}

/// Classify a terminal wasmtime trap. Resource-limiter denials during
/// instantiation are handled at that boundary; a denied `memory.grow` during
/// execution returns `-1` and must not relabel a later unrelated trap.
fn classify_trap(err: &wasmtime::Error, cancelled: bool) -> TrapKind {
    if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        return match trap {
            wasmtime::Trap::OutOfFuel => TrapKind::OutOfFuel,
            wasmtime::Trap::Interrupt => {
                if cancelled {
                    TrapKind::Cancelled
                } else {
                    TrapKind::EpochDeadline
                }
            }
            wasmtime::Trap::StackOverflow => TrapKind::StackOverflow,
            wasmtime::Trap::MemoryOutOfBounds => TrapKind::MemoryLimit,
            _ => TrapKind::GuestTrap,
        };
    }
    TrapKind::GuestTrap
}

#[async_trait]
impl ExecutionSandbox for WasmIsolationBackend {
    async fn invoke(
        &self,
        req: SandboxInvocation,
        cancel: CancellationToken,
    ) -> Result<SandboxOutcome, ExecutionSandboxError> {
        let component = self
            .components
            .get(req.component.as_str())
            .ok_or_else(|| {
                ExecutionSandboxError::ComponentNotRegistered(req.component.as_str().to_string())
            })?
            .clone();

        // Fast-path: already cancelled before we start.
        if cancel.is_cancelled() {
            return Ok(SandboxOutcome {
                output: Vec::new(),
                fuel_consumed: 0,
                trap: Some(TrapKind::Cancelled),
            });
        }

        // Per-call linker (deny-by-default) + fresh store.
        let linker = self.build_linker(&req.grant)?;
        let epoch_ticks = req.caps.epoch_ticks.max(1);
        let state = HostState {
            wasi: Self::build_wasi(&req.grant)?,
            table: ResourceTable::new(),
            secret_present: self.secret_present,
            memory_cap: req.caps.memory_bytes,
            table_element_cap: req.caps.memory_bytes / std::mem::size_of::<usize>(),
            resource_denied: false,
            cancel: cancel.clone(),
            epoch_ticks_remaining: epoch_ticks,
        };
        let mut store = Store::new(&self.engine, state);
        store
            .set_fuel(req.caps.fuel)
            .map_err(|e| ExecutionSandboxError::Backend(format!("set_fuel: {e}")))?;
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(|mut cx| {
            let state = cx.data_mut();
            if state.cancel.is_cancelled() || state.epoch_ticks_remaining <= 1 {
                Ok(UpdateDeadline::Interrupt)
            } else {
                state.epoch_ticks_remaining -= 1;
                Ok(UpdateDeadline::Yield(1))
            }
        });
        store.limiter(|s| s as &mut dyn ResourceLimiter);

        // Deny-by-default at the door: an un-granted / unknown import fails the
        // type-check here. For an already-compiled component the ONLY cause is
        // import resolution, so classify any failure as UngrantedImport.
        let instance_pre = match linker.instantiate_pre(&component) {
            Ok(p) => p,
            Err(_) => {
                return Ok(SandboxOutcome {
                    output: Vec::new(),
                    fuel_consumed: 0,
                    trap: Some(TrapKind::UngrantedImport),
                });
            }
        };

        let instance = match instance_pre.instantiate_async(&mut store).await {
            Ok(i) => i,
            Err(e) => {
                let fuel_consumed = req.caps.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
                let trap = if store.data().resource_denied {
                    TrapKind::MemoryLimit
                } else {
                    classify_trap(&e, cancel.is_cancelled())
                };
                return Ok(SandboxOutcome {
                    output: Vec::new(),
                    fuel_consumed,
                    trap: Some(trap),
                });
            }
        };
        // ResourceLimiter denials during execution return failure sentinels to
        // the guest; they must not relabel a later, unrelated terminal trap.
        store.data_mut().resource_denied = false;

        let func = match instance.get_typed_func::<(Vec<u8>,), (u32,)>(&mut store, &req.entry) {
            Ok(f) => f,
            Err(_) => {
                return Err(ExecutionSandboxError::EntryNotFound(req.entry.clone()));
            }
        };

        let result = func.call_async(&mut store, (req.input,)).await;

        let fuel_consumed = req.caps.fuel.saturating_sub(store.get_fuel().unwrap_or(0));
        match result {
            Ok((ret,)) => Ok(SandboxOutcome {
                output: ret.to_le_bytes().to_vec(),
                fuel_consumed,
                trap: None,
            }),
            Err(e) => Ok(SandboxOutcome {
                output: Vec::new(),
                fuel_consumed,
                trap: Some(classify_trap(&e, cancel.is_cancelled())),
            }),
        }
    }
}
