pub fn parse_frontmatter(content: &str) -> Option<(&str, &str)> {
    let after_first = content
        .strip_prefix("---\r\n")
        .or_else(|| content.strip_prefix("---\n"))?;

    if after_first.starts_with("---\n") || after_first.starts_with("---\r\n") {
        let body = after_first
            .strip_prefix("---")
            .unwrap()
            .trim_start_matches(['\n', '\r']);
        return Some(("", body));
    }

    let end_idx = after_first.find("\n---")?;
    let frontmatter = &after_first[..end_idx];
    let body_start = end_idx + 4;
    let body = after_first[body_start..].trim_start_matches(['\n', '\r']);
    Some((frontmatter, body))
}

/// Extracts a named field from YAML frontmatter text, returning the unquoted
/// value. Surrounding `"..."` or `'...'` quotes are stripped per standard YAML
/// semantics — callers always receive the inner string.
pub fn extract_field(frontmatter: &str, field: &str) -> Option<String> {
    let field_lower = field.to_lowercase();
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(colon_idx) = trimmed.find(':') {
            let key = trimmed[..colon_idx].trim();
            if key.to_lowercase() == field_lower {
                let value = trimmed[colon_idx + 1..].trim();
                let unquoted = strip_quotes(value);
                if !unquoted.is_empty() {
                    return Some(unquoted.to_string());
                }
                return None;
            }
        }
    }
    None
}

/// Strips one layer of matching surrounding double or single quotes.
/// Normalizes YAML quoting so callers always get the inner string.
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

pub fn extract_list_field(frontmatter: &str, field: &str) -> Option<Vec<String>> {
    let field_lower_hyphen = field.replace('_', "-");
    let field_lower_underscore = field.replace('-', "_");

    let mut in_target_field = false;
    let mut items: Vec<String> = Vec::new();

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(colon_idx) = trimmed.find(':') {
            let key = trimmed[..colon_idx].trim();
            let key_lower = key.to_lowercase();
            if key_lower == field_lower_hyphen || key_lower == field_lower_underscore {
                in_target_field = true;
                let value = trimmed[colon_idx + 1..].trim();
                if value == "[]" || value.is_empty() {
                    if value == "[]" {
                        return Some(vec![]);
                    }
                    continue;
                }
                if value.starts_with('[') && value.ends_with(']') {
                    let inner = &value[1..value.len() - 1];
                    let parsed: Vec<String> = inner
                        .split(',')
                        .map(|s| strip_quotes(s.trim()).to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if parsed.is_empty() {
                        return Some(vec![]);
                    }
                    return Some(parsed);
                }
                continue;
            } else if in_target_field {
                break;
            }
        } else if in_target_field {
            if let Some(stripped) = trimmed.strip_prefix("- ") {
                let value = strip_quotes(stripped.trim()).to_string();
                if !value.is_empty() {
                    items.push(value);
                }
            } else if trimmed == "-" || trimmed.is_empty() {
                // Bare `-` (no value) and blank lines — skip, do not terminate the list.
                continue;
            } else {
                break;
            }
        }
    }

    if !items.is_empty() {
        return Some(items);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_frontmatter() {
        let content = "---\nname: foo\ndescription: bar\n---\nBody text";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert_eq!(fm, "name: foo\ndescription: bar");
        assert_eq!(body, "Body text");
    }

    #[test]
    fn missing_open_delimiter() {
        let content = "name: foo\n---\nBody";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn missing_close_delimiter() {
        let content = "---\nname: foo\nBody";
        assert!(parse_frontmatter(content).is_none());
    }

    #[test]
    fn crlf_line_endings() {
        let content = "---\r\nname: foo\r\n---\r\nBody";
        let (fm, _) = parse_frontmatter(content).unwrap();
        assert!(fm.contains("name: foo"));
    }

    #[test]
    fn empty_frontmatter_body() {
        let content = "---\n---\nBody";
        let (fm, body) = parse_frontmatter(content).unwrap();
        assert_eq!(fm, "");
        assert_eq!(body, "Body");
    }

    #[test]
    fn no_frontmatter() {
        assert!(parse_frontmatter("Just a body").is_none());
    }

    #[test]
    fn extract_field_basic() {
        let fm = "name: my-skill\ndescription: A skill";
        assert_eq!(extract_field(fm, "name"), Some("my-skill".to_string()));
        assert_eq!(
            extract_field(fm, "description"),
            Some("A skill".to_string())
        );
    }

    #[test]
    fn extract_field_quoted() {
        let fm = "description: \"A quoted desc\"";
        assert_eq!(
            extract_field(fm, "description"),
            Some("A quoted desc".to_string())
        );
    }

    #[test]
    fn extract_field_single_quoted() {
        let fm = "description: 'single quotes'";
        assert_eq!(
            extract_field(fm, "description"),
            Some("single quotes".to_string())
        );
    }

    #[test]
    fn extract_field_case_insensitive() {
        let fm = "Description: A desc\nName: foo";
        assert_eq!(extract_field(fm, "description"), Some("A desc".to_string()));
        assert_eq!(extract_field(fm, "name"), Some("foo".to_string()));
    }

    #[test]
    fn extract_field_missing() {
        let fm = "name: foo";
        assert_eq!(extract_field(fm, "description"), None);
    }

    #[test]
    fn extract_list_field_inline() {
        let fm = "allowed-tools: [\"Read\", \"Grep\"]";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    #[test]
    fn extract_list_field_block() {
        let fm = "name: foo\nallowed-tools:\n  - Read\n  - Grep\n  - Glob";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep", "Glob"]);
    }

    #[test]
    fn extract_list_field_empty() {
        let fm = "allowed-tools: []";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn extract_list_field_missing() {
        let fm = "name: foo";
        assert!(extract_list_field(fm, "allowed-tools").is_none());
    }

    #[test]
    fn extract_list_field_underscore_variant() {
        let fm = "allowed_tools:\n  - Read\n  - Bash";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Bash"]);
    }

    #[test]
    fn extract_list_field_hyphen_query_underscore_field() {
        let fm = "allowed_tools: [\"Read\"]";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read"]);
    }

    #[test]
    fn extract_list_field_block_filters_empty_items() {
        // A bare `- ` entry must not push an empty string into the item list.
        let fm = "allowed-tools:\n  - Read\n  - \n  - Grep";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    #[test]
    fn extract_field_strips_double_quotes() {
        let fm = "description: \"hello world\"";
        assert_eq!(
            extract_field(fm, "description"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_field_strips_single_quotes() {
        let fm = "description: 'hello world'";
        assert_eq!(
            extract_field(fm, "description"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn extract_field_empty_quoted_value_returns_none() {
        let fm = "description: \"\"";
        assert_eq!(extract_field(fm, "description"), None);
    }
}
