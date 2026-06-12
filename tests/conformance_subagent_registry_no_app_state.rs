use std::fs;

#[test]
fn test_no_subagent_registry_on_app_state() {
    let files = [
        "src/adapters/tui/state.rs",
        "src/infrastructure/runtime/app_state.rs",
    ];

    for path in &files {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{} must exist for conformance check", path));
        for (line_no, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("// ") || trimmed.starts_with("use ") {
                continue;
            }
            if trimmed.contains("subagent_registry:") {
                panic!(
                    "SubagentRegistry field found on app state at {}:{}\n  {}",
                    path,
                    line_no + 1,
                    line
                );
            }
        }
    }
}

/// Grep-based conformance: `subagent_provider` field must exist exactly
/// once — in `src/adapters/composite_toolset_adapter.rs`. No other file in
/// `src/` may hold a `subagent_provider` struct field.
///
/// This is the symmetric Flag 1 invariant for SubagentProvider (Story 10-2
/// ratifies the full pattern alongside CapabilityRegistry).
#[test]
fn test_subagent_provider_is_internal_to_composite() {
    let composite_content =
        std::fs::read_to_string("src/adapters/composite_toolset_adapter.rs").unwrap_or_default();
    let has_field = composite_content.lines().any(|line| {
        let t = line.trim();
        !t.starts_with("//")
            && !t.starts_with("/*")
            && !t.starts_with('*')
            && t.contains("subagent_provider:")
    });
    assert!(
        has_field,
        "subagent_provider field should exist on CompositeToolsetAdapter"
    );

    // No other src file may hold a `subagent_provider` struct field.
    let src_files = collect_src_rs_files();
    let mut violations = Vec::new();
    for file in &src_files {
        let fname = file.to_string_lossy();
        if fname.contains("composite_toolset_adapter.rs") {
            continue;
        }
        let fc = std::fs::read_to_string(file).unwrap_or_default();
        let has_struct_field = fc.lines().any(|line| {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') {
                return false;
            }
            if t.starts_with("use ") || t.starts_with("pub use ") {
                return false;
            }
            if t.contains("subagent::") || t.contains("subagent_provider::") {
                return false;
            }
            t.contains("subagent_provider:")
        });
        if has_struct_field {
            violations.push(fname.to_string());
        }
    }
    assert!(
        violations.is_empty(),
        "SubagentProvider struct field found in non-composite files: {:?}. \
         Only CompositeToolsetAdapter may hold a subagent_provider field (Flag 1 extends to Subagent).",
        violations
    );
}

fn collect_src_rs_files() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let src = std::path::Path::new("src");
    if !src.is_dir() {
        return files;
    }
    collect_rs_recursive(src, &mut files);
    files
}

fn collect_rs_recursive(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_recursive(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}

/// AC-10-2-4 "no new event stream" guard: greps events.rs for forbidden
/// subagent-specific event variant names.
#[test]
fn test_no_new_event_variants_for_subagent() {
    let events_content = std::fs::read_to_string("src/domain/events.rs").unwrap_or_default();
    let forbidden = [
        "SubagentRegistered",
        "SubagentDeregistered",
        "OwnershipChanged",
        "RegistryUpdated",
    ];
    for token in &forbidden {
        assert!(
            !events_content.contains(token),
            "Forbidden event variant token '{}' found in src/domain/events.rs. \
             Subagent events MUST flow through existing CapabilityEvent variants (AC-10-2-4).",
            token
        );
    }
}

#[test]
fn test_subagent_panel_does_not_hold_registry() {
    let widget_files = [
        "src/adapters/tui/widgets/agent_panel.rs",
        "src/adapters/tui/widgets/agent_inspector.rs",
    ];
    for path in &widget_files {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|_| panic!("{} must exist for conformance check", path));
        assert!(
            !content.contains("SubagentRegistry"),
            "Widget {} must not import SubagentRegistry — use AgentRowView instead (dependency rule)",
            path
        );
    }
}

#[test]
fn test_subagent_provider_access_only_via_composite() {
    let tui_files = collect_tui_rs_files();
    let forbidden_tokens = ["SubagentRegistry", "SubagentProvider"];
    for file in &tui_files {
        let fname = file.to_string_lossy();
        let content = std::fs::read_to_string(file).unwrap_or_default();
        for token in &forbidden_tokens {
            assert!(
                !content.contains(token),
                "TUI file {} contains forbidden token '{}'. \
                 Panel/inspector widgets must consume AgentRowView, never the registry/provider directly.",
                fname,
                token
            );
        }
    }
}

fn collect_tui_rs_files() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new("src/adapters/tui");
    let mut files = Vec::new();
    if dir.is_dir() {
        collect_rs_recursive(dir, &mut files);
    }
    files
}
