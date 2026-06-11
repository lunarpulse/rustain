use rustain::domain::services::command_interpolation::substitute_command_args;

#[test]
fn test_substitute_single_replacement() {
    assert_eq!(
        substitute_command_args("Review {{args}} for bugs.", "src/main.rs"),
        "Review src/main.rs for bugs."
    );
}

#[test]
fn test_substitute_multiple_occurrences() {
    assert_eq!(substitute_command_args("{{args}}{{args}}", "x"), "xx");
}

#[test]
fn test_substitute_empty_args_with_placeholder() {
    assert_eq!(substitute_command_args("Review {{args}}.", ""), "Review .");
}

#[test]
fn test_substitute_no_placeholder_no_args() {
    assert_eq!(substitute_command_args("Just a body.", ""), "Just a body.");
}

#[test]
fn test_substitute_no_placeholder_with_args() {
    assert_eq!(
        substitute_command_args("Just a body.", "foo bar"),
        "Just a body.\n\nfoo bar"
    );
}

#[test]
fn test_substitute_typo_not_recognized() {
    assert_eq!(
        substitute_command_args("Review {{arg}} here.", "foo"),
        "Review {{arg}} here.\n\nfoo"
    );
}

#[test]
fn test_substitute_whitespace_trimmed() {
    assert_eq!(
        substitute_command_args("Target: {{args}}", "  src/main.rs  "),
        "Target: src/main.rs"
    );
}
