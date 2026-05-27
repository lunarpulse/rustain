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
