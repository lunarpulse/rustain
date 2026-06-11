use std::fs;

#[test]
fn test_tab_state_has_read_only_field() {
    let content = fs::read_to_string("src/domain/models/tab.rs")
        .expect("src/domain/models/tab.rs must exist");
    assert!(
        content.contains("pub read_only: bool"),
        "TabState must have a `pub read_only: bool` field (AC-10-4-10)"
    );
}

#[test]
fn test_tab_state_read_only_default_false() {
    let content = fs::read_to_string("src/domain/models/tab.rs")
        .expect("src/domain/models/tab.rs must exist");
    let constructors = ["read_only: false"];
    for expected in &constructors {
        let count = content.matches(expected).count();
        assert!(
            count >= 2,
            "Expected at least 2 occurrences of '{}' in tab.rs (one per constructor), found {}",
            expected,
            count
        );
    }
}
