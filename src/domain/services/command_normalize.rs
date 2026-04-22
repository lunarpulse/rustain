use std::path::Path;

pub const MAX_COMMAND_NAMESPACE_DEPTH: u8 = 5;

pub fn normalize_command_component(raw: &str) -> Option<String> {
    let lower = raw.to_lowercase();
    let replaced: String = lower
        .chars()
        .map(|c| {
            if c == '_' || c.is_ascii_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let stripped: String = replaced
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    let collapsed = collapse_hyphens(&stripped);
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed)
}

fn collapse_hyphens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result
}

pub fn normalize_command_path(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let relative_str = relative.to_str()?;
    let _relative_str = relative_str.trim_end_matches(".md");
    let file_stem = file.file_stem()?.to_str()?;

    let mut normalized_parts: Vec<String> = Vec::new();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            if let std::path::Component::Normal(os_str) = component {
                let part = os_str.to_str()?;
                let normalized = normalize_command_component(part)?;
                normalized_parts.push(normalized);
            }
        }
    }

    let stem_normalized = normalize_command_component(file_stem)?;
    normalized_parts.push(stem_normalized);

    // Depth cap: total parts (directory components + file stem)
    // MAX_COMMAND_NAMESPACE_DEPTH = 5 means 5 directory levels max.
    // Total parts = directory components + file stem = depth + 1.
    // So 5-level deep: 5 dirs + stem = 6 parts (OK).
    // 6-level deep: 6 dirs + stem = 7 parts (exceeds cap).
    if normalized_parts.len() > MAX_COMMAND_NAMESPACE_DEPTH as usize + 1 {
        return None;
    }

    Some(normalized_parts.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_normalize_table() {
        let cases: Vec<(&str, Option<&str>)> = vec![
            ("review", Some("review")),
            ("Code Review", Some("code-review")),
            ("deploy_prod", Some("deploy-prod")),
            ("Run   Tests", Some("run-tests")),
            ("deploy__staging", Some("deploy-staging")),
            ("Review!v2", Some("reviewv2")),
            ("__hidden__", Some("hidden")),
            (".hidden", Some("hidden")),
            ("README", Some("readme")),
            ("___", None),
            ("!", None),
            ("...", None),
        ];
        for (input, expected) in cases {
            assert_eq!(
                normalize_command_component(input),
                expected.map(String::from),
                "normalize_command_component({input:?})"
            );
        }
    }

    #[test]
    fn test_namespace_path_basic() {
        let root = PathBuf::from("/ws/.claude/commands");
        let file = PathBuf::from("/ws/.claude/commands/deploy/staging.md");
        assert_eq!(
            normalize_command_path(&root, &file),
            Some("deploy:staging".to_string())
        );
    }

    #[test]
    fn test_namespace_deep_path() {
        let root = PathBuf::from("/ws/.claude/commands");
        let file = PathBuf::from("/ws/.claude/commands/a/b/c/d/e/cmd.md");
        assert_eq!(
            normalize_command_path(&root, &file),
            Some("a:b:c:d:e:cmd".to_string())
        );
    }

    #[test]
    fn test_namespace_too_deep() {
        let root = PathBuf::from("/ws/.claude/commands");
        let file = PathBuf::from("/ws/.claude/commands/a/b/c/d/e/f/cmd.md");
        assert_eq!(normalize_command_path(&root, &file), None);
    }

    #[test]
    fn test_namespace_flat() {
        let root = PathBuf::from("/ws/.claude/commands");
        let file = PathBuf::from("/ws/.claude/commands/review.md");
        assert_eq!(
            normalize_command_path(&root, &file),
            Some("review".to_string())
        );
    }

    #[test]
    fn test_namespace_normalized_components() {
        let root = PathBuf::from("/ws/.claude/commands");
        let file = PathBuf::from("/ws/.claude/commands/Deploy_Prod/us-east-1/primary.md");
        assert_eq!(
            normalize_command_path(&root, &file),
            Some("deploy-prod:us-east-1:primary".to_string())
        );
    }

    #[test]
    fn test_leading_dot_file() {
        assert_eq!(
            normalize_command_component(".hidden"),
            Some("hidden".to_string())
        );
        assert_eq!(normalize_command_component("___"), None);
    }
}
