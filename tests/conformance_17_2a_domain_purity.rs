//! Structural guards for Story 17.2a domain purity (AC9) and the immutable
//! room read-model. Grep-based, mirroring `test_no_capability_registry_on_app_state`.

fn source(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn code_lines(content: &str) -> impl Iterator<Item = &str> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//") && !line.starts_with("/*") && !line.starts_with('*'))
}

#[test]
fn orchestration_room_exposes_no_public_mutator() {
    let content = source("src/domain/models/orchestration_room.rs");
    let writable = code_lines(&content)
        .filter(|line| line.contains("pub fn") && line.contains("&mut self"))
        .collect::<Vec<_>>();
    assert!(
        writable.is_empty(),
        "OrchestrationRoom must be a read-only projection; found public mutator(s): {writable:?}"
    );
}

#[test]
fn orchestration_room_has_no_public_fields() {
    // A projection is read-only: public fields are a writable store just as
    // surely as a `&mut` setter (any holder of `&mut OrchestrationRoom` could
    // clear/insert nodes/waves/artifacts/approvals). Accessors return `&`.
    let content = source("src/domain/models/orchestration_room.rs");
    let start = content
        .find("pub struct OrchestrationRoom {")
        .expect("OrchestrationRoom struct present");
    let body = &content[start..];
    let open = body.find('{').expect("struct body opens");
    let end = body.find('}').expect("struct body closes");
    let public_fields = body[open + 1..end]
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("pub "))
        .collect::<Vec<_>>();
    assert!(
        public_fields.is_empty(),
        "OrchestrationRoom must expose no public (writable) field; use read-only accessors. Found: {public_fields:?}"
    );
}

#[test]
fn room_and_artifact_domain_modules_import_no_infrastructure() {
    for path in [
        "src/domain/models/orchestration_room.rs",
        "src/domain/models/artifact.rs",
    ] {
        let content = source(path);
        for line in code_lines(&content).filter(|line| line.starts_with("use ")) {
            assert!(
                !line.contains("crate::infrastructure")
                    && !line.contains("crate::adapters")
                    && !line.contains("std::fs")
                    && !line.contains("tokio"),
                "{path} must stay pure domain (serde/thiserror/domain only); offending use: {line}"
            );
        }
    }
}
