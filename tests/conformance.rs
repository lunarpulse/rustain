//! Architecture conformance tests — verify hexagonal invariants.
//!
//! These tests scan source files to enforce dependency rules:
//! - domain/ imports NOTHING from adapters/ or infrastructure/
//! - domain/ imports NOTHING from crossterm, ratatui, tokio, futures, reqwest, arc-swap
//! - adapters/ imports from domain/ only (no adapter-to-adapter cross-imports)
//! - The directory structure matches the hexagonal layout spec

use std::fs;
use std::path::Path;

/// Recursively collect all .rs files under a directory.
fn collect_rs_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(collect_rs_files(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Read all use/extern statements from a file.
fn get_imports(path: &Path) -> Vec<(usize, String)> {
    let content = fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            trimmed.starts_with("use ") || trimmed.starts_with("extern crate")
        })
        .map(|(n, line)| (n + 1, line.trim().to_string()))
        .collect()
}

/// AC1 / Task 3.6: Domain layer must not import I/O crates.
#[test]
fn test_domain_no_forbidden_crate_imports() {
    let forbidden = [
        "crossterm",
        "ratatui",
        "tokio",
        "futures",
        "reqwest",
        "arc_swap",
    ];
    let domain_dir = Path::new("src/domain");
    let files = collect_rs_files(domain_dir);
    assert!(!files.is_empty(), "No .rs files found in src/domain/");

    let mut violations = Vec::new();
    for file in &files {
        for (line_num, import) in get_imports(file) {
            for crate_name in &forbidden {
                if import.contains(crate_name) {
                    violations.push(format!(
                        "{}:{} — forbidden import `{}` in: {}",
                        file.display(),
                        line_num,
                        crate_name,
                        import
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Domain purity violated:\n{}",
        violations.join("\n")
    );
}

/// AC1 / Task 3.6: Domain layer must not import from adapters/ or infrastructure/.
#[test]
fn test_domain_no_adapter_or_infra_imports() {
    let forbidden_paths = ["crate::adapters", "crate::infrastructure"];
    let domain_dir = Path::new("src/domain");
    let files = collect_rs_files(domain_dir);

    let mut violations = Vec::new();
    for file in &files {
        for (line_num, import) in get_imports(file) {
            for forbidden in &forbidden_paths {
                if import.contains(forbidden) {
                    violations.push(format!(
                        "{}:{} — imports from {} in: {}",
                        file.display(),
                        line_num,
                        forbidden,
                        import
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Domain imports adapters/infrastructure:\n{}",
        violations.join("\n")
    );
}

/// AC1: Hexagonal directory structure exists with all required modules.
#[test]
fn test_hexagonal_directory_structure() {
    let required_files = [
        // domain
        "src/domain/mod.rs",
        "src/domain/events.rs",
        "src/domain/errors.rs",
        "src/domain/models/mod.rs",
        "src/domain/ports/mod.rs",
        "src/domain/services/mod.rs",
        // adapters
        "src/adapters/mod.rs",
        "src/adapters/noop.rs",
        "src/adapters/tui/mod.rs",
        "src/adapters/tui/terminal.rs",
        "src/adapters/tui/app.rs",
        "src/adapters/tui/layout.rs",
        "src/adapters/tui/state.rs",
        "src/adapters/tui/widgets/mod.rs",
        "src/adapters/cli/mod.rs",
        "src/adapters/cli/commands.rs",
        // infrastructure
        "src/infrastructure/mod.rs",
        "src/infrastructure/startup.rs",
        "src/infrastructure/config.rs",
        "src/infrastructure/logging.rs",
        "src/infrastructure/signals.rs",
        "src/infrastructure/paths.rs",
        "src/infrastructure/runtime/mod.rs",
        "src/infrastructure/runtime/event_loop.rs",
        // entry points
        "src/main.rs",
        "src/lib.rs",
    ];

    let mut missing = Vec::new();
    for path in &required_files {
        if !Path::new(path).exists() {
            missing.push(*path);
        }
    }

    assert!(
        missing.is_empty(),
        "Missing required files:\n{}",
        missing.join("\n")
    );
}

/// Adapters must not import from other adapters (no adapter-to-adapter).
/// Exception: adapters/tui/ submodules can import from each other.
#[test]
fn test_no_cross_adapter_imports() {
    let adapter_groups = [("src/adapters/tui", "tui"), ("src/adapters/cli", "cli")];

    let mut violations = Vec::new();

    for (dir, group_name) in &adapter_groups {
        let files = collect_rs_files(Path::new(dir));
        for file in &files {
            for (line_num, import) in get_imports(file) {
                // Check for imports from other adapter groups
                for (_, other_group) in &adapter_groups {
                    if other_group != group_name
                        && import.contains(&format!("crate::adapters::{}", other_group))
                    {
                        violations.push(format!(
                            "{}:{} — adapter `{}` imports from adapter `{}`: {}",
                            file.display(),
                            line_num,
                            group_name,
                            other_group,
                            import
                        ));
                    }
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Cross-adapter imports found:\n{}",
        violations.join("\n")
    );
}
