use std::fs;
use std::path::Path;

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path.as_ref()).unwrap_or_else(|e| panic!("{}: {e}", path.as_ref().display()))
}

#[test]
fn isolation_provider_surface_is_start_diff_stop_only() {
    let src = read("src/domain/ports/isolation_provider.rs");
    let methods: Vec<_> = src
        .lines()
        .filter_map(|line| line.trim().strip_prefix("async fn "))
        .map(|rest| rest.split('(').next().unwrap().to_string())
        .collect();
    assert_eq!(methods, ["start", "diff", "stop"]);
    assert!(!src.contains("apply("));
    assert!(!src.contains("merge("));
    assert!(!src.contains("commit("));
}

#[test]
fn isolation_modules_do_not_read_wall_clock_directly() {
    for path in [
        "src/domain/models/isolation.rs",
        "src/domain/ports/isolation_provider.rs",
        "src/adapters/isolation/mod.rs",
    ] {
        let src = read(path);
        assert!(
            !src.contains("Instant::now") && !src.contains("SystemTime::now"),
            "{path} must use injected Clock::wall_now_ms"
        );
    }
}

// Story 17.3a FLIPPED this guard. It previously asserted `ExecutionSandbox` was
// ABSENT everywhere (R2-deferred). It now asserts the port EXISTS as a PROVEN
// SIBLING of `IsolationProvider` — never a super-trait, never a widening
// (ADR-11-3 rule 3) — and is defined exactly once. This is the AC1 structural
// conformance assertion (no shared super-trait / no shared method name). It
// does NOT assert the sandbox is wired into tool dispatch: there is no
// production consumer yet (party ruling N4); the proving consumer is
// `tests/wasm_execution_sandbox.rs`.
#[test]
fn execution_sandbox_exists_as_proven_sibling() {
    let port = read("src/domain/ports/execution_sandbox.rs");
    assert!(
        port.contains("pub trait ExecutionSandbox"),
        "ExecutionSandbox port must exist in execution_sandbox.rs (Story 17.3a)"
    );

    // Sibling, not super-trait: the trait declaration must not extend
    // IsolationProvider.
    let decl = port
        .lines()
        .find(|l| l.contains("pub trait ExecutionSandbox"))
        .expect("trait declaration line");
    assert!(
        !decl.contains("IsolationProvider"),
        "ExecutionSandbox must be a sibling, never a super-trait of \
         IsolationProvider (ADR-11-3 rule 3): {decl:?}"
    );

    // Single-method port sharing NO method name with IsolationProvider
    // (start/diff/stop). Its sole async method is `invoke`.
    let methods: Vec<_> = port
        .lines()
        .filter_map(|line| line.trim().strip_prefix("async fn "))
        .map(|rest| rest.split('(').next().unwrap().to_string())
        .collect();
    assert_eq!(
        methods,
        ["invoke"],
        "ExecutionSandbox is a single-method (invoke) port"
    );
    for shared in ["start", "diff", "stop"] {
        assert!(
            !methods.iter().any(|m| m == shared),
            "ExecutionSandbox must not reuse IsolationProvider's method `{shared}`"
        );
    }

    // Defined exactly once across the scanned tree (not duplicated/redefined).
    let mut definitions = 0;
    for path in ["src/domain", "src/adapters", "src/infrastructure"] {
        for entry in walkdir(path) {
            if read(&entry).contains("pub trait ExecutionSandbox") {
                definitions += 1;
            }
        }
    }
    assert_eq!(
        definitions, 1,
        "the ExecutionSandbox trait must be defined exactly once (in execution_sandbox.rs)"
    );
}

// DN-1 guardrail (party-mode A, 2026-06-30 — unanimous Winston/Amelia/Murat):
// the WriteFs-at-entry gate must be VALIDATE-ONLY. `spend_use(&child_token)`
// may appear exactly ONCE in the runner — at the per-tool-batch ReadFs gate
// (the genuine consumption site). A second occurrence (e.g. a re-added entry
// spend) would double-charge the single shared `uses_remaining` counter and
// brick isolated children (`==Some(1)` → no tool batch can run). This test
// fails RED the moment someone re-introduces an entry `spend_use`.
#[test]
fn isolation_entry_gate_does_not_double_charge_child_token() {
    let src = read("src/adapters/subagent/in_process_runner.rs");
    let count = src.matches("spend_use(&child_token").count();
    assert_eq!(
        count, 1,
        "guardrail: `spend_use(&child_token)` must appear exactly once (the \
         per-tool ReadFs gate); found {count} — an entry-spend would brick \
         isolated children (DN-1)"
    );
}

fn walkdir(root: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_string()];
    while let Some(path) = stack.pop() {
        // P17: symlink_metadata (do NOT follow symlinks — avoids cycles /
        // escaping) and skip unreadable entries instead of aborting the whole
        // conformance suite on one bad file.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    stack.push(entry.path().display().to_string());
                }
            }
        } else if path.ends_with(".rs") {
            out.push(path);
        }
    }
    out
}
