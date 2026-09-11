//! Matching for `allowed-tools` entries that carry command specifiers.
//!
//! Parsing is intentionally independent of frontmatter syntax: by the time an
//! item reaches this module, scalar/list parsing has already produced one
//! opaque string such as `Bash(kubectl:*)`.

/// A borrowed view of one `allowed-tools` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllowedToolPattern<'a> {
    pub tool_name: &'a str,
    pub specifier: Option<&'a str>,
}

/// Parse `Tool` or `Tool(specifier)`, using the final `)` as the delimiter.
pub fn parse_allowed_tool_pattern(item: &str) -> Option<AllowedToolPattern<'_>> {
    if item.is_empty() {
        return None;
    }
    let Some(open) = item.find('(') else {
        return (!item.contains(')')).then_some(AllowedToolPattern {
            tool_name: item,
            specifier: None,
        });
    };
    let close = item.rfind(')')?;
    if open == 0 || close != item.len() - 1 {
        return None;
    }
    Some(AllowedToolPattern {
        tool_name: &item[..open],
        specifier: Some(&item[open + 1..close]),
    })
}

/// Whether an item safely identifies a tool at offer/disclosure time.
pub fn allowed_item_matches_tool(item: &str, tool_name: &str) -> bool {
    parse_allowed_tool_pattern(item)
        .is_some_and(|pattern| pattern.tool_name == tool_name && pattern.specifier != Some(""))
}

/// Whether every shell segment is admitted by at least one item for `tool_name`.
pub fn command_matches_allowed_items(items: &[String], tool_name: &str, command: &str) -> bool {
    if command.trim().is_empty() {
        return false;
    }

    let mut has_restricted_pattern = false;
    for item in items {
        let Some(pattern) = parse_allowed_tool_pattern(item) else {
            continue;
        };
        if pattern.tool_name != tool_name {
            continue;
        }
        match pattern.specifier {
            None | Some("*") => return true,
            Some("") => {}
            Some(_) => has_restricted_pattern = true,
        }
    }
    if !has_restricted_pattern {
        return false;
    }

    command_segments_match(command, |segment| {
        items.iter().any(|item| {
            parse_allowed_tool_pattern(item).is_some_and(|pattern| {
                pattern.tool_name == tool_name
                    && pattern
                        .specifier
                        .is_some_and(|specifier| specifier_matches_segment(specifier, segment))
            })
        })
    })
}

fn normalized_chars(value: &str) -> impl Iterator<Item = char> + '_ {
    let mut in_whitespace = false;
    value.trim().chars().filter_map(move |character| {
        if character.is_whitespace() {
            if in_whitespace {
                None
            } else {
                in_whitespace = true;
                Some(' ')
            }
        } else {
            in_whitespace = false;
            Some(character)
        }
    })
}

fn normalized_starts_with(value: &str, prefix: &str) -> bool {
    let mut value = normalized_chars(value);
    normalized_chars(prefix).all(|expected| value.next() == Some(expected))
}

fn normalized_word_prefix(value: &str, prefix: &str) -> bool {
    let mut value = normalized_chars(value);
    if !normalized_chars(prefix).all(|expected| value.next() == Some(expected)) {
        return false;
    }
    matches!(value.next(), None | Some(' '))
}

fn specifier_matches_segment(specifier: &str, segment: &str) -> bool {
    if specifier == "*" {
        return true;
    }
    if let Some(prefix) = specifier.strip_suffix(":*") {
        return !prefix.is_empty() && normalized_word_prefix(segment, prefix);
    }
    if let Some(prefix) = specifier.strip_suffix('*') {
        if prefix.ends_with(char::is_whitespace) {
            return normalized_word_prefix(segment, prefix.trim_end());
        }
        return normalized_starts_with(segment, prefix);
    }
    normalized_chars(segment).eq(normalized_chars(specifier))
}

fn command_segments_match(command: &str, mut segment_matches: impl FnMut(&str) -> bool) -> bool {
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut matched_segment = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }

        if byte == b'`'
            || (matches!(byte, b'$' | b'<' | b'>') && bytes.get(index + 1) == Some(&b'('))
        {
            return false;
        }

        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = None;
                }
                index += 1;
                continue;
            }
            _ => {}
        }

        match byte {
            b'\\' => {
                escaped = true;
                index += 1;
                continue;
            }
            b'\'' | b'"' => {
                quote = Some(byte);
                index += 1;
                continue;
            }
            b'<' if bytes.get(index + 1) == Some(&b'<') => return false,
            _ => {}
        }

        let separator_len = if byte == b'\n' || byte == b';' {
            1
        } else if byte == b'&'
            && bytes
                .get(index.wrapping_sub(1))
                .is_some_and(|b| matches!(b, b'<' | b'>'))
        {
            0
        } else if byte == b'&'
            && bytes
                .get(index + 1)
                .is_some_and(|b| matches!(b, b'<' | b'>'))
        {
            0
        } else if bytes.get(index..index + 2) == Some(b"&&")
            || bytes.get(index..index + 2) == Some(b"||")
            || bytes.get(index..index + 2) == Some(b"|&")
        {
            2
        } else if matches!(byte, b'|' | b'&') {
            1
        } else {
            0
        };

        if separator_len == 0 {
            index += 1;
            continue;
        }

        let segment = command[start..index].trim();
        let logical = bytes.get(index..index + separator_len) == Some(b"&&")
            || bytes.get(index..index + separator_len) == Some(b"||");
        if logical && segment.is_empty() {
            return false;
        }
        if !segment.is_empty() {
            if !segment_matches(segment) {
                return false;
            }
            matched_segment = true;
        }
        index += separator_len;
        start = index;
        if logical && command[start..].trim().is_empty() {
            return false;
        }
    }

    if quote.is_some() || escaped {
        return false;
    }
    let segment = command[start..].trim();
    if !segment.is_empty() {
        if !segment_matches(segment) {
            return false;
        }
        matched_segment = true;
    }
    matched_segment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_documented_forms_without_splitting_inner_colons() {
        assert_eq!(
            parse_allowed_tool_pattern("Bash"),
            Some(AllowedToolPattern {
                tool_name: "Bash",
                specifier: None,
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("Bash(*)"),
            Some(AllowedToolPattern {
                tool_name: "Bash",
                specifier: Some("*"),
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("Bash(kubectl:*)"),
            Some(AllowedToolPattern {
                tool_name: "Bash",
                specifier: Some("kubectl:*"),
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("Bash(git:* push)"),
            Some(AllowedToolPattern {
                tool_name: "Bash",
                specifier: Some("git:* push"),
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("Bash(echo \"a (b)\")"),
            Some(AllowedToolPattern {
                tool_name: "Bash",
                specifier: Some("echo \"a (b)\""),
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("Read"),
            Some(AllowedToolPattern {
                tool_name: "Read",
                specifier: None,
            })
        );
        assert_eq!(
            parse_allowed_tool_pattern("mcp__srv__tool:variant"),
            Some(AllowedToolPattern {
                tool_name: "mcp__srv__tool:variant",
                specifier: None,
            })
        );
    }

    #[test]
    fn malformed_or_empty_items_fail_closed() {
        for item in ["", "(x)", "Bash(", "Bash)x(", "Bash(kubectl)junk"] {
            assert_eq!(parse_allowed_tool_pattern(item), None, "item={item:?}");
        }
        assert!(!allowed_item_matches_tool("Bash()", "Bash"));
        assert!(!command_matches_allowed_items(
            &items(&["Bash()"]),
            "Bash",
            "kubectl get pods"
        ));
        assert!(!command_matches_allowed_items(
            &items(&["Bash(kubectl:*)"]),
            "Bash",
            ""
        ));
    }

    #[test]
    fn documented_prefix_and_boundary_semantics_hold() {
        let restricted = items(&["Bash(kubectl:*)", "Bash(helm:*)"]);
        for command in [
            "kubectl",
            "kubectl get pods",
            "helm diff release chart",
            "  kubectl   get\tpods",
            "kubectl get pods 2>&1",
        ] {
            assert!(
                command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
        for command in [
            "printf x",
            "kubectl-evil --all",
            "kubectlfoo",
            "KUBECTL get pods",
        ] {
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }

        assert!(command_matches_allowed_items(
            &items(&["Bash(git log:*)"]),
            "Bash",
            "git log"
        ));
        assert!(command_matches_allowed_items(
            &items(&["Bash(git log *)"]),
            "Bash",
            "git log --oneline"
        ));
        assert!(command_matches_allowed_items(
            &items(&["Bash(ls*)"]),
            "Bash",
            "lsof"
        ));
        assert!(!command_matches_allowed_items(
            &items(&["Bash(ls *)"]),
            "Bash",
            "lsof"
        ));
        assert!(!command_matches_allowed_items(
            &items(&["Bash(git:* push)"]),
            "Bash",
            "git status"
        ));
    }

    #[test]
    fn bare_tool_and_star_admit_every_command_at_the_allowlist_step() {
        for item in ["Bash", "Bash(*)"] {
            assert!(command_matches_allowed_items(
                &items(&[item]),
                "Bash",
                "printf x && anything"
            ));
        }
    }

    #[test]
    fn every_unquoted_shell_segment_must_match() {
        let restricted = items(&["Bash(kubectl:*)", "Bash(helm:*)"]);
        assert!(command_matches_allowed_items(
            &restricted,
            "Bash",
            "kubectl get pods && helm diff release chart"
        ));
        for command in [
            "kubectl get pods && printf x",
            "kubectl get pods; printf x",
            "kubectl get pods | tee /tmp/x",
            "printf x && kubectl get pods",
            "kubectl get pods &&",
        ] {
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
    }

    #[test]
    fn quoted_separators_are_text_but_escaped_quotes_do_not_open_quotes() {
        let restricted = items(&["Bash(kubectl:*)"]);
        assert!(command_matches_allowed_items(
            &restricted,
            "Bash",
            "kubectl get -o jsonpath='{.a && .b}'"
        ));
        assert!(!command_matches_allowed_items(
            &restricted,
            "Bash",
            r"kubectl get pods --note=\'x;printf SECOND-CMD-RAN\'z"
        ));
    }

    #[test]
    fn constructs_the_scanner_cannot_safely_interpret_are_denied() {
        let restricted = items(&["Bash(kubectl:*)", "Bash(echo:*)"]);
        for command in [
            "echo $(printf x)",
            "kubectl get `printf x`",
            "kubectl get \"unbalanced",
            "kubectl apply -f - <<EOF",
            "KUBECONFIG=/tmp/x kubectl get pods",
        ] {
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
    }

    #[test]
    fn empty_segments_are_vacuous() {
        let restricted = items(&["Bash(kubectl:*)"]);
        for command in [
            "; kubectl get pods",
            "kubectl get pods;",
            "kubectl;;get pods",
        ] {
            let expected = command != "kubectl;;get pods";
            assert_eq!(
                command_matches_allowed_items(&restricted, "Bash", command),
                expected,
                "command={command:?}"
            );
        }
    }

    #[test]
    fn rm_chains_are_rejected_by_the_matcher_even_without_the_blocklist() {
        let restricted = items(&["Bash(kubectl:*)"]);
        for command in [
            "kubectl get pods && rm -rf /",
            "kubectl get pods; rm -rf /",
            "kubectl get pods | rm -rf /",
        ] {
            assert!(!command_matches_allowed_items(&restricted, "Bash", command));
        }
    }
}
