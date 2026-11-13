//! Compile-time type walls (trybuild). One `TestCases` for the whole suite: trybuild
//! compiles `rustain` again under `<target>/tests/trybuild` with the calling test
//! binary's features, so each extra `TestCases` costs another full build.
//!
//! Every `compile_fail` case has a `pass` twin over the same types, so the only
//! failure cause is the forbidden construct. The walls:
//!
//! - Story 14.3 AC5 — `Window<SpokeHandle>` carries no body (E0609).
//! - Story 14.3 AC6 — `DrillBody` is opaque outside the crate (E0616).
//! - Story 14.6 AC4 — `OwnershipKind::Self_` cannot be forged (E0423).
//! - Story 14.6 AC4 / AI-12.3 — `OwnershipKind` does not implement `Deserialize`: a
//!   compile-pass assertion whose marker would turn ambiguous if it did.
//! - Story 17.1a AC1 — the `AgentId` field is sealed (E0423).
//! - Story 17.3a AC1 — `ExecutionSandbox` is a sibling of `IsolationProvider` (E0308).
//!
//! Feature independence: none of the named items, and none of the lists the
//! diagnostics print, is behind `cfg(feature)`, so the snapshots do not change with
//! the feature set. The walls therefore run in the default-features lane only;
//! feature lanes report `compile_time_type_walls` as ignored.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// (pass twin, compile_fail case or `None` for a compile-pass assertion)
const WALLS: &[(&str, Option<&str>)] = &[
    (
        "tests/trybuild/orchestration/inline_body_pass.rs",
        Some("tests/trybuild/orchestration/inline_body_fails.rs"),
    ),
    (
        "tests/trybuild/orchestration/drill_body_tui_pass.rs",
        Some("tests/trybuild/orchestration/drill_body_domain_fails.rs"),
    ),
    (
        "tests/trybuild/ownership/self_match_pass.rs",
        Some("tests/trybuild/ownership/self_construct_fails.rs"),
    ),
    (
        "tests/trybuild/ownership/domain_kind_not_deserializable.rs",
        None,
    ),
    (
        "tests/trybuild/identity/agent_id_field_sealed_pass.rs",
        Some("tests/trybuild/identity/agent_id_field_sealed_fails.rs"),
    ),
    (
        "tests/trybuild/execution_sandbox/sibling_pass.rs",
        Some("tests/trybuild/execution_sandbox/sibling_fails.rs"),
    ),
];

#[test]
#[cfg_attr(
    any(feature = "a2a", feature = "p2p", feature = "test-fake-mcp"),
    ignore = "feature-independent type walls run once, in the default-features lane"
)]
fn compile_time_type_walls() {
    let t = trybuild::TestCases::new();
    for (pass, compile_fail) in WALLS {
        t.pass(pass);
        if let Some(case) = compile_fail {
            t.compile_fail(case);
        }
    }
}

fn collect(dir: &Path, root: &Path, rs: &mut BTreeSet<String>, stderr: &mut BTreeSet<String>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
    for entry in entries {
        let path: PathBuf = entry.expect("dir entry").path();
        if path.is_dir() {
            collect(&path, root, rs, stderr);
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("under manifest dir")
            .to_string_lossy()
            .replace('\\', "/");
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => {
                rs.insert(rel);
            }
            Some("stderr") => {
                stderr.insert(rel);
            }
            _ => panic!("unexpected file under tests/trybuild: {rel}"),
        }
    }
}

/// T2: `WALLS` lists every `tests/trybuild/**/*.rs` exactly once, each `compile_fail`
/// has its `.stderr`, and no other `.stderr` exists (an orphan snapshot is dead weight).
#[test]
fn every_trybuild_fixture_is_listed_once() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (mut on_disk, mut snapshots) = (BTreeSet::new(), BTreeSet::new());
    collect(
        &root.join("tests/trybuild"),
        root,
        &mut on_disk,
        &mut snapshots,
    );

    let mut listed = Vec::new();
    let mut expected_snapshots = BTreeSet::new();
    for (pass, compile_fail) in WALLS {
        listed.push((*pass).to_string());
        if let Some(case) = compile_fail {
            listed.push((*case).to_string());
            expected_snapshots.insert(case.replace(".rs", ".stderr"));
        }
    }
    let unique: BTreeSet<String> = listed.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        listed.len(),
        "a fixture is listed twice in WALLS: {listed:?}"
    );
    assert_eq!(unique, on_disk, "WALLS and tests/trybuild/**/*.rs differ");
    assert_eq!(
        snapshots, expected_snapshots,
        "`.stderr` files must exist for exactly the compile_fail cases"
    );
}
