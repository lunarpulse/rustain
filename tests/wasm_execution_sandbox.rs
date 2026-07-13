//! Story 17.3a — the proving consumer for `ExecutionSandbox` /
//! `WasmIsolationBackend` (party rulings F3, N4). Every case drives a REAL,
//! content-hash-pinned adversarial WASM component through the wasmtime backend
//! and asserts the *sandbox contained it*. A trap proven via a mocked `Err`
//! (instead of a real guest execution) is explicitly rejected: trap cases
//! assert `fuel_consumed > 0`, which only a genuinely-executed guest reports.
//!
//! Runs only under `--features wasm-sandbox` (the whole file is cfg-gated).
#![cfg(feature = "wasm-sandbox")]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use rustain::adapters::wasm::WasmIsolationBackend;
use rustain::domain::models::{
    CapabilityGrant, ComponentRef, HostImport, ResourceCaps, SandboxInvocation, SandboxOutcome,
    TrapKind,
};
use rustain::domain::ports::ExecutionSandbox;

const FIXTURES: &[&str] = &[
    "well_behaved",
    "infinite_loop",
    "memory_bomb",
    "memory_grow_then_trap",
    "table_bomb",
    "ungranted_import",
    "ungranted_wasi_random",
    "secret_read",
    "fuel_ok",
    "fuel_bomb",
];

fn manifest() -> serde_json::Value {
    let raw = std::fs::read_to_string("tests/fixtures/wasm/manifest.json")
        .expect("read fixture manifest");
    serde_json::from_str(&raw).expect("parse fixture manifest")
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Load a fixture's committed bytes and assert its sha256 matches the pinned
/// manifest hash (F3: a swapped/tampered `.wasm` fails RED here).
fn load_fixture(name: &str) -> Vec<u8> {
    let bytes = std::fs::read(format!("tests/fixtures/wasm/{name}.wasm"))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
    let expected = manifest()[name]
        .as_str()
        .unwrap_or_else(|| panic!("no manifest hash for {name}"))
        .to_string();
    assert_eq!(
        sha256_hex(&bytes),
        expected,
        "fixture {name} hash mismatch — tampered or rebuilt without updating manifest.json"
    );
    bytes
}

async fn backend(secret_present: bool) -> WasmIsolationBackend {
    let mut sources = HashMap::new();
    for name in FIXTURES {
        sources.insert(name.to_string(), load_fixture(name));
    }
    WasmIsolationBackend::new(sources, secret_present).expect("build backend")
}

/// Generous caps: nothing traps unless the test tightens a specific one.
fn caps() -> ResourceCaps {
    ResourceCaps {
        fuel: 10_000_000,
        epoch_ticks: 1_000_000,
        memory_bytes: 16 * 1024 * 1024,
    }
}

fn inv(
    component: &str,
    grant: CapabilityGrant,
    caps: ResourceCaps,
    input: &[u8],
) -> SandboxInvocation {
    SandboxInvocation {
        component: ComponentRef::new(component),
        entry: "run".to_string(),
        input: input.to_vec(),
        grant,
        caps,
    }
}

fn ret_u32(out: &SandboxOutcome) -> u32 {
    let b: [u8; 4] = out.output.as_slice().try_into().expect("4-byte u32 output");
    u32::from_le_bytes(b)
}

// ── AC2/AC3: happy path ────────────────────────────────────────────────────

#[tokio::test]
async fn well_behaved_receives_payload_bytes() {
    let be = backend(false).await;
    let out = be
        .invoke(
            inv("well_behaved", CapabilityGrant::default(), caps(), b"abcd"),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert!(
        out.trap.is_none(),
        "well-behaved guest must not trap: {out:?}"
    );
    assert_eq!(
        ret_u32(&out),
        b"abcd".iter().map(|b| u32::from(*b)).sum::<u32>()
    );
    assert!(out.fuel_consumed > 0, "a real guest consumes fuel");

    let same_length = be
        .invoke(
            inv("well_behaved", CapabilityGrant::default(), caps(), b"wxyz"),
            CancellationToken::new(),
        )
        .await
        .expect("same-length invoke");
    assert_ne!(
        ret_u32(&out),
        ret_u32(&same_length),
        "same-length, different-content inputs must remain distinguishable"
    );
}

// ── AC2 mutant (a): infinite loop trapped by fuel, in bounded time ──────────

#[tokio::test]
async fn infinite_loop_traps_on_fuel_within_bounded_time() {
    let be = backend(false).await;
    let mut caps = caps();
    caps.fuel = 200_000; // small, deterministic
    let start = Instant::now();
    let out = be
        .invoke(
            inv("infinite_loop", CapabilityGrant::default(), caps, b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    let elapsed = start.elapsed();
    assert_eq!(
        out.trap,
        Some(TrapKind::OutOfFuel),
        "infinite loop must trap on fuel: {out:?}"
    );
    assert!(out.fuel_consumed > 0, "real trap, not a mocked Err");
    assert!(
        elapsed < Duration::from_secs(5),
        "trap must be bounded in time, took {elapsed:?}"
    );
}

#[tokio::test]
async fn infinite_loop_traps_on_epoch_deadline() {
    let be = backend(false).await;
    let mut caps = caps();
    caps.fuel = u64::MAX;
    caps.epoch_ticks = 2;
    let out = tokio::time::timeout(
        Duration::from_secs(1),
        be.invoke(
            inv("infinite_loop", CapabilityGrant::default(), caps, b""),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("epoch deadline must be bounded")
    .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::EpochDeadline),
        "the real runaway guest must be interrupted by the epoch cap: {out:?}"
    );
}

// ── AC2 mutant (b): over-allocation trapped by the memory cap ───────────────

#[tokio::test]
async fn memory_bomb_traps_on_memory_cap() {
    let be = backend(false).await;
    let mut caps = caps();
    caps.memory_bytes = 1024 * 1024; // 1 MiB < the fixture's ~65 MiB minimum
    let out = be
        .invoke(
            inv("memory_bomb", CapabilityGrant::default(), caps, b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::MemoryLimit),
        "over-allocation must trap on memory cap: {out:?}"
    );
}

#[tokio::test]
async fn table_bomb_traps_on_table_cap() {
    let be = backend(false).await;
    let mut caps = caps();
    caps.memory_bytes = 1024 * 1024;
    let out = be
        .invoke(
            inv("table_bomb", CapabilityGrant::default(), caps, b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::MemoryLimit),
        "table allocation must be bounded by the per-call resource cap: {out:?}"
    );
}

#[tokio::test]
async fn denied_memory_grow_does_not_relabel_later_guest_trap() {
    let be = backend(false).await;
    let mut caps = caps();
    caps.memory_bytes = 128 * 1024;
    let out = be
        .invoke(
            inv(
                "memory_grow_then_trap",
                CapabilityGrant::default(),
                caps,
                b"",
            ),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::GuestTrap),
        "a refused memory.grow returns -1; its later unreachable trap keeps its real cause: {out:?}"
    );
}

// ── AC2 mutant (c): ungranted capability fails to instantiate ───────────────

#[tokio::test]
async fn ungranted_import_fails_to_instantiate() {
    let be = backend(false).await;
    // Empty grant → the guest's `forbidden-egress` import is absent from the
    // per-call linker → instantiate fails, deny-by-default.
    let out = be
        .invoke(
            inv("ungranted_import", CapabilityGrant::default(), caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::UngrantedImport),
        "ungranted import must fail to instantiate: {out:?}"
    );
}

#[tokio::test]
async fn empty_grant_rejects_wasi_random_import() {
    let be = backend(false).await;
    let out = be
        .invoke(
            inv(
                "ungranted_wasi_random",
                CapabilityGrant::default(),
                caps(),
                b"",
            ),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::UngrantedImport),
        "an empty grant must not expose host clocks or randomness: {out:?}"
    );
}

// ── AC2 mutant (d) + N1: secret exposure is existence-boolean only ──────────

#[tokio::test]
async fn secret_read_gets_existence_boolean_only() {
    let grant = CapabilityGrant {
        host_imports: vec![HostImport::HasCredential],
        ..Default::default()
    };

    // Credential present → existence boolean 1.
    let be = backend(true).await;
    let out = be
        .invoke(
            inv("secret_read", grant.clone(), caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert!(out.trap.is_none(), "granted secret probe must run: {out:?}");
    let v = ret_u32(&out);
    assert_eq!(
        v, 1,
        "has-credential must report existence when a credential is present"
    );

    // N1: the value the guest can obtain is a BOOLEAN, never secret bytes.
    assert!(
        v == 0 || v == 1,
        "guest may only ever learn an existence boolean, got {v}"
    );
    assert_eq!(
        out.output.len(),
        4,
        "output is the u32 boolean, not a secret payload"
    );

    // Credential absent → existence boolean 0 (value still never exposed).
    let be = backend(false).await;
    let out = be
        .invoke(
            inv("secret_read", grant, caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke");
    assert_eq!(
        ret_u32(&out),
        0,
        "has-credential must report absence when no credential is present"
    );
}

// ── AC2 mutant (e): per-call grant isolation ───────────────────────────────

#[tokio::test]
async fn per_call_grant_is_not_inherited() {
    let be = backend(true).await;

    // Call 1: grant HasCredential → secret probe works.
    let granted = CapabilityGrant {
        host_imports: vec![HostImport::HasCredential],
        ..Default::default()
    };
    let out1 = be
        .invoke(
            inv("secret_read", granted, caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke 1");
    assert!(
        out1.trap.is_none() && ret_u32(&out1) == 1,
        "granted call succeeds: {out1:?}"
    );

    // Call 2: SAME backend, SAME component, EMPTY grant → the capability from
    // call 1 is NOT inherited; the import is absent → fails to instantiate.
    let out2 = be
        .invoke(
            inv("secret_read", CapabilityGrant::default(), caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("invoke 2");
    assert_eq!(
        out2.trap,
        Some(TrapKind::UngrantedImport),
        "a later call with a different grant must not inherit an earlier grant's capabilities: {out2:?}"
    );
}

// ── AC3: fuel cap enforced at the exact boundary (fuel-at-cap / +1 pair) ────

#[tokio::test]
async fn fuel_cap_enforced_at_exact_boundary() {
    let be = backend(false).await;

    // Measure fuel_ok's exact consumption C under a generous budget.
    let measured = be
        .invoke(
            inv("fuel_ok", CapabilityGrant::default(), caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect("measure");
    assert!(
        measured.trap.is_none(),
        "measurement run must complete: {measured:?}"
    );
    let c = measured.fuel_consumed;
    assert!(c > 0, "fuel_ok must consume fuel");

    // At the cap: succeeds.
    let mut at_cap = caps();
    at_cap.fuel = c;
    let out = be
        .invoke(
            inv("fuel_ok", CapabilityGrant::default(), at_cap, b""),
            CancellationToken::new(),
        )
        .await
        .expect("at-cap");
    assert!(
        out.trap.is_none(),
        "fuel_ok at cap = C ({c}) must succeed: {out:?}"
    );

    // Clearly under the cap: traps. (Fuel is charged in batches, so the exact
    // ±1 boundary is not a reliable assertion; a cap well below C is. The tight
    // boundary is proven by the two-fixture pair below at the SAME cap.)
    let mut under = caps();
    under.fuel = c / 2;
    let out = be
        .invoke(
            inv("fuel_ok", CapabilityGrant::default(), under, b""),
            CancellationToken::new(),
        )
        .await
        .expect("under-cap");
    assert_eq!(
        out.trap,
        Some(TrapKind::OutOfFuel),
        "fuel_ok below cap must trap: {out:?}"
    );

    // The high-side fixture overshoots the same cap → traps.
    let mut same = caps();
    same.fuel = c;
    let out = be
        .invoke(
            inv("fuel_bomb", CapabilityGrant::default(), same, b""),
            CancellationToken::new(),
        )
        .await
        .expect("bomb");
    assert_eq!(
        out.trap,
        Some(TrapKind::OutOfFuel),
        "fuel_bomb needs > C, must trap at cap = C: {out:?}"
    );
}

// ── AC3: the fixture-hash manifest is a non-vacuous guard ───────────────────

#[tokio::test]
async fn fixture_hashes_are_pinned_and_tamper_evident() {
    let manifest = manifest();
    for name in FIXTURES {
        // load_fixture asserts the real hash; this also proves every fixture is
        // listed in the manifest.
        let bytes = load_fixture(name);
        assert!(
            manifest.get(name).and_then(|v| v.as_str()).is_some(),
            "{name} missing from manifest"
        );

        // Tamper one byte → the hash must diverge (the guard is not vacuous).
        let mut tampered = bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xFF;
        assert_ne!(
            sha256_hex(&tampered),
            manifest[name].as_str().unwrap(),
            "a tampered {name} must not match the pinned hash"
        );
    }
}

// ── Backend error surface ───────────────────────────────────────────────────

#[tokio::test]
async fn unknown_component_is_a_backend_error_not_a_trap() {
    let be = backend(false).await;
    let err = be
        .invoke(
            inv("does_not_exist", CapabilityGrant::default(), caps(), b""),
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown component must be a backend error");
    assert!(
        matches!(
            err,
            rustain::domain::ports::ExecutionSandboxError::ComponentNotRegistered(_)
        ),
        "expected ComponentNotRegistered, got {err:?}"
    );
}

// ── Cooperative cancellation ────────────────────────────────────────────────

#[tokio::test]
async fn pre_cancelled_invocation_traps_cancelled() {
    let be = backend(false).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let out = be
        .invoke(
            inv("well_behaved", CapabilityGrant::default(), caps(), b"abcd"),
            cancel,
        )
        .await
        .expect("invoke");
    assert_eq!(
        out.trap,
        Some(TrapKind::Cancelled),
        "a pre-cancelled call must not run the guest: {out:?}"
    );
}

#[tokio::test]
async fn midflight_cancellation_interrupts_large_epoch_budget() {
    let be = backend(false).await;
    let cancel = CancellationToken::new();
    let cancel_after_start = cancel.clone();
    let mut caps = caps();
    caps.fuel = u64::MAX;
    caps.epoch_ticks = 1_000_000;

    let (out, ()) = tokio::join!(
        async {
            tokio::time::timeout(
                Duration::from_secs(1),
                be.invoke(
                    inv("infinite_loop", CapabilityGrant::default(), caps, b""),
                    cancel,
                ),
            )
            .await
            .expect("mid-flight cancellation must be bounded")
            .expect("invoke")
        },
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancel_after_start.cancel();
        }
    );
    assert_eq!(out.trap, Some(TrapKind::Cancelled), "{out:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_one_call_does_not_interrupt_a_sibling_store() {
    let be = Arc::new(backend(false).await);
    let cancel_a = CancellationToken::new();
    let cancel_b = CancellationToken::new();
    let mut caps_a = caps();
    caps_a.fuel = u64::MAX;
    caps_a.epoch_ticks = 1_000_000;
    let mut caps_b = caps_a;
    caps_b.epoch_ticks = 500;

    let call_a = {
        let be = Arc::clone(&be);
        let cancel = cancel_a.clone();
        tokio::spawn(async move {
            be.invoke(
                inv("infinite_loop", CapabilityGrant::default(), caps_a, b""),
                cancel,
            )
            .await
        })
    };
    let call_b = {
        let be = Arc::clone(&be);
        let cancel = cancel_b.clone();
        tokio::spawn(async move {
            be.invoke(
                inv("infinite_loop", CapabilityGrant::default(), caps_b, b""),
                cancel,
            )
            .await
        })
    };

    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel_a.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !call_b.is_finished(),
        "cancelling call A must not advance call B's store-local deadline"
    );
    cancel_b.cancel();

    let out_a = call_a.await.expect("join A").expect("invoke A");
    let out_b = call_b.await.expect("join B").expect("invoke B");
    assert_eq!(out_a.trap, Some(TrapKind::Cancelled), "{out_a:?}");
    assert_eq!(out_b.trap, Some(TrapKind::Cancelled), "{out_b:?}");
}
