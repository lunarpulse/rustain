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

/// If `value` opens with a quote that closes later on the line, returns the
/// quoted scalar's INNER text with any trailing YAML comment discarded.
/// Returns `None` when the value is not a quoted scalar, leaving the caller on
/// the unquoted path.
///
/// Story 19.2 code review: `"Read # Grep"` must keep its `#` — a comment marker
/// inside quotes is content — while `"Read Grep"  # note` must still drop the
/// note. A naive `find(" #")` did neither.
fn split_scalar_comment(value: &str) -> Option<&str> {
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let rest = &value[quote.len_utf8()..];
    let close = rest.find(quote)?;
    let inner = &rest[..close];
    let after = rest[close + quote.len_utf8()..].trim();
    // Anything after the closing quote may only be a comment.
    if after.is_empty() || after.starts_with('#') {
        Some(inner)
    } else {
        None
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
                // Scalar form (Agent Skills spec, story 19.2 A2): the value is a
                // whitespace-separated list, e.g. `allowed-tools: Bash(kubectl:*) Bash(helm:*) Read`.
                // Parentheses stay intact within an item; commas are NOT separators.
                // Like the bracket branch, a scalar returns immediately — block
                // items after it are never consumed.
                //
                // ⚑ Code review (19.2): a value that is ONLY a YAML comment is an
                // empty value, not a scalar. `allowed-tools: # deployment tools`
                // followed by `- Read` block items parsed correctly before the
                // scalar branch existed; treating the comment as content both
                // invented a junk allowlist and swallowed the block list.
                if value.starts_with('#') {
                    continue;
                }
                // ⚑ Code review (19.2): the comment scan is quote-aware. A ` #`
                // INSIDE a quoted scalar is content, not a comment, and cutting
                // there used to leave an unbalanced quote in the parsed name.
                let (scalar, already_unquoted) = match split_scalar_comment(value) {
                    Some(quoted) => (quoted, true),
                    None => (
                        match value.find(" #") {
                            Some(idx) => value[..idx].trim(),
                            None => value,
                        },
                        false,
                    ),
                };
                if scalar.is_empty() {
                    continue;
                }
                // A genuine quoted scalar is unquoted exactly once, as a whole:
                // its inner spaces separate items and its inner quotes are part
                // of the tool name. An unquoted scalar is split first, then each
                // item is unquoted (so `Read "Grep"` still works).
                let parsed: Vec<String> = if already_unquoted {
                    scalar.split_whitespace().map(str::to_string).collect()
                } else {
                    scalar
                        .split_whitespace()
                        .map(|s| strip_quotes(s).to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                };
                if parsed.is_empty() {
                    return Some(vec![]);
                }
                return Some(parsed);
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

    // Story 19.2 AC1 — the Agent Skills spec's scalar form. All six are RED
    // until the scalar branch lands (A2 parsing, A8 comment truncation).
    #[test]
    fn extract_list_field_scalar_prd_journey3_form() {
        // The exact `allowed-tools` line from prd.md § Journey 3.
        let fm = "name: safe-deploy\ndescription: Deploy services following team safety protocols. Use when deploying any service to staging or production.\nallowed-tools: Bash(kubectl:*) Bash(helm:*) Read";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Bash(kubectl:*)", "Bash(helm:*)", "Read"]);
    }

    #[test]
    fn extract_list_field_scalar_quoted() {
        let fm = "allowed-tools: \"Read Grep\"";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    #[test]
    fn extract_list_field_scalar_truncates_yaml_comment() {
        // A8: ` #` opens a comment in a scalar; truncate before splitting.
        let fm = "allowed-tools: Read Grep  # only these two";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    #[test]
    fn extract_list_field_scalar_hash_without_leading_space_is_a_token() {
        // A8: a `#` with no preceding space is a legal token position, not a comment.
        let fm = "allowed-tools: Bash(grep:#tag) Read";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Bash(grep:#tag)", "Read"]);
    }

    #[test]
    fn extract_list_field_scalar_returns_immediately_and_ignores_block_items() {
        // A2: a scalar returns immediately, exactly as the bracket branch does —
        // a following `- item` line is never consumed as part of the field.
        let fm = "allowed-tools: Read\n  - Grep";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read"]);
    }

    #[test]
    fn extract_list_field_scalar_commas_are_not_separators() {
        // A2: the spec's scalar is whitespace-delimited; commas are token content.
        let fm = "allowed-tools: Read,Grep";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read,Grep"]);
    }

    // ── Story 19.2 code-review regressions ──────────────────────────────────

    /// The review's headline parser defect: a comment on the key line made the
    /// scalar branch invent a junk allowlist AND swallow the block list that
    /// parsed correctly before the branch existed (verified against `f7a002e`).
    #[test]
    fn commented_key_line_still_parses_the_block_list_beneath_it() {
        let fm = "allowed-tools: # deployment tools\n  - Read\n  - Grep";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    /// A value that is only a comment is an EMPTY value, never a restriction.
    /// Returning items here would silently restrict a user who declared nothing.
    #[test]
    fn comment_only_value_is_not_a_restriction() {
        assert_eq!(
            extract_list_field("allowed-tools: # nothing yet", "allowed-tools"),
            None
        );
    }

    /// A `#` inside a quoted scalar is content; cutting there used to leave an
    /// unbalanced quote inside the parsed tool name.
    #[test]
    fn hash_inside_a_quoted_scalar_is_content_not_a_comment() {
        let fm = "allowed-tools: \"Read # Grep\"";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert!(
            result.iter().all(|item| !item.contains('"')),
            "no item may carry an unbalanced quote: {result:?}"
        );
        assert!(result.contains(&"Read".to_string()));
        assert!(result.contains(&"Grep".to_string()));
    }

    /// A quoted scalar with a trailing comment still drops the comment.
    #[test]
    fn quoted_scalar_drops_a_trailing_comment() {
        let fm = "allowed-tools: \"Read Grep\"  # only these two";
        let result = extract_list_field(fm, "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
    }

    /// Per-item quoting must not be mangled by unquoting the whole value first.
    #[test]
    fn per_item_quoted_scalar_items_are_unquoted_cleanly() {
        let result = extract_list_field("allowed-tools: 'Read' 'Grep'", "allowed-tools").unwrap();
        assert_eq!(result, vec!["Read", "Grep"]);
        let mixed = extract_list_field("allowed-tools: Read \"Grep\"", "allowed-tools").unwrap();
        assert_eq!(mixed, vec!["Read", "Grep"]);
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
