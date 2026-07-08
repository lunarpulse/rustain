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

#[test]
fn execution_sandbox_remains_r2_deferred() {
    for path in ["src/domain", "src/adapters", "src/infrastructure"] {
        for entry in walkdir(path) {
            if entry.ends_with("isolation_provider.rs") {
                continue;
            }
            let src = read(&entry);
            assert!(
                !src.contains("trait ExecutionSandbox") && !src.contains("ExecutionSandbox:"),
                "ExecutionSandbox must remain absent until Story 17.3: {entry}"
            );
        }
    }
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
