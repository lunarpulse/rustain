pub fn substitute_command_args(body: &str, args: &str) -> String {
    let trimmed_args = args.trim();

    if body.contains("{{args}}") {
        body.replace("{{args}}", trimmed_args)
    } else if !trimmed_args.is_empty() {
        format!("{body}\n\n{trimmed_args}")
    } else {
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_args_single_replacement() {
        assert_eq!(
            substitute_command_args("Review {{args}} for bugs.", "src/main.rs"),
            "Review src/main.rs for bugs."
        );
    }

    #[test]
    fn test_substitute_args_multiple_occurrences() {
        assert_eq!(substitute_command_args("{{args}}{{args}}", "foo"), "foofoo");
    }

    #[test]
    fn test_substitute_args_empty_args_with_placeholder() {
        assert_eq!(substitute_command_args("Review {{args}}.", ""), "Review .");
    }

    #[test]
    fn test_substitute_args_no_placeholder_no_args() {
        assert_eq!(substitute_command_args("Just a body.", ""), "Just a body.");
    }

    #[test]
    fn test_substitute_args_no_placeholder_with_args() {
        assert_eq!(
            substitute_command_args("Just a body.", "foo bar"),
            "Just a body.\n\nfoo bar"
        );
    }

    #[test]
    fn test_substitute_args_typo_not_recognized() {
        assert_eq!(
            substitute_command_args("Review {{arg}} here.", "foo"),
            "Review {{arg}} here.\n\nfoo"
        );
    }

    #[test]
    fn test_substitute_args_no_separator() {
        assert_eq!(substitute_command_args("{{args}}{{args}}", "foo"), "foofoo");
    }
}
