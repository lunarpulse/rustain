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
///
/// Bare items (`Read`) match by exact name. Pattern items
/// (`Tool(specifier)`) are honoured for `Bash` only — the one tool whose
/// commands carry an execution-time gate (Story 19.11 review: expanding a
/// non-Bash pattern offers a tool that is then denied on every call, with
/// the FR42-a disclosure silenced) — and only with a specifier that is
/// neither empty nor whitespace (`Bash()` and `Bash(   )` are junk, not
/// grants; a junk item stays unmatched and is disclosed).
pub fn allowed_item_matches_tool(item: &str, tool_name: &str) -> bool {
    parse_allowed_tool_pattern(item).is_some_and(|pattern| {
        pattern.tool_name == tool_name
            && match pattern.specifier {
                None => true,
                Some(specifier) => {
                    pattern.tool_name == "Bash" && specifier_is_usable_grant(specifier)
                }
            }
    })
}

/// Whether a declared specifier can ever serve as a grant — used at
/// offer/disclosure time so junk stays unmatched and is disclosed instead
/// of offering a tool that no command can satisfy. Rejects whitespace-only
/// specifiers (`Bash( )`), glob-leading prefixes (`Bash(**)`) and
/// whitespace prefixes before a trailing star (`Bash( *)`, which would
/// otherwise behave as a full wildcard). `Bash(*)`, `Tool(prefix*)`,
/// `Tool(prefix:*)` and star-free literal specifiers are grants.
fn specifier_is_usable_grant(specifier: &str) -> bool {
    if specifier.trim().is_empty() {
        return false;
    }
    if specifier == "*" {
        return true;
    }
    let Some(prefix) = specifier.strip_suffix('*') else {
        return true; // exact literal specifier — no trailing star
    };
    let trimmed = prefix.trim_end();
    !trimmed.is_empty() && !trimmed.starts_with('*')
}

/// How a list of `allowed-tools` items admits one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolAdmission {
    /// A bare item or `Tool(*)`: every call to the tool is admitted.
    Unrestricted,
    /// Only through specifiers (`Bash(kubectl:*)`): a command must match one.
    Patterns,
    /// No item grants the tool.
    Nothing,
}

/// The one predicate for "does this list admit `tool_name` only through
/// command patterns" — shared by the segment gate and the redirect step so
/// they cannot drift (Story 19.29, R1).
pub fn tool_admission<'a>(items: impl Iterator<Item = &'a str>, tool_name: &str) -> ToolAdmission {
    let mut has_restricted_pattern = false;
    for item in items {
        let Some(pattern) = parse_allowed_tool_pattern(item) else {
            continue;
        };
        if pattern.tool_name != tool_name {
            continue;
        }
        match pattern.specifier {
            None | Some("*") => return ToolAdmission::Unrestricted,
            Some("") => {}
            Some(_) => has_restricted_pattern = true,
        }
    }
    if has_restricted_pattern {
        ToolAdmission::Patterns
    } else {
        ToolAdmission::Nothing
    }
}

/// Whether every shell segment is admitted by at least one item for `tool_name`.
pub fn command_matches_allowed_items(items: &[String], tool_name: &str, command: &str) -> bool {
    command_matches_allowed_item_refs(items.iter().map(String::as_str), tool_name, command)
}

pub fn command_matches_allowed_item_refs<'a>(
    items: impl Iterator<Item = &'a str> + Clone,
    tool_name: &str,
    command: &str,
) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    match tool_admission(items.clone(), tool_name) {
        ToolAdmission::Unrestricted => true,
        ToolAdmission::Nothing => false,
        ToolAdmission::Patterns => {
            segments_admitted(items, tool_name, scan_command(command).as_ref())
        }
    }
}

/// [`command_matches_allowed_item_refs`] over a scan the caller already holds,
/// so the permission chain scans a command once per call.
pub fn command_matches_scanned_items<'a>(
    items: impl Iterator<Item = &'a str> + Clone,
    tool_name: &str,
    command: &str,
    scan: Option<&CommandScan<'_>>,
) -> bool {
    if command.trim().is_empty() {
        return false;
    }
    match tool_admission(items.clone(), tool_name) {
        ToolAdmission::Unrestricted => true,
        ToolAdmission::Nothing => false,
        ToolAdmission::Patterns => segments_admitted(items, tool_name, scan),
    }
}

fn segments_admitted<'a>(
    items: impl Iterator<Item = &'a str> + Clone,
    tool_name: &str,
    scan: Option<&CommandScan<'_>>,
) -> bool {
    scan.is_some_and(|scan| {
        scan.segments.iter().all(|segment| {
            items.clone().any(|item| {
                parse_allowed_tool_pattern(item).is_some_and(|pattern| {
                    pattern.tool_name == tool_name
                        && pattern
                            .specifier
                            .is_some_and(|specifier| specifier_matches_segment(specifier, segment))
                })
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
        return !prefix.is_empty()
            && !prefix.starts_with('*')
            && normalized_word_prefix(segment, prefix);
    }
    if let Some(prefix) = specifier.strip_suffix('*') {
        // A star-leading stripped prefix (e.g. `Bash(**)`) is a glob, not a
        // declared prefix: bash glob-expands `*.sh` at command position and
        // executes it, so no segment may ever be admitted by one. A
        // whitespace-only prefix (`Bash( *)`) would vacuously match every
        // command as a full wildcard — also never a grant.
        let trimmed = prefix.trim_end();
        if trimmed.is_empty() || trimmed.starts_with('*') {
            return false;
        }
        if prefix.ends_with(char::is_whitespace) {
            return normalized_word_prefix(segment, trimmed);
        }
        return normalized_starts_with(segment, prefix);
    }
    normalized_chars(segment).eq(normalized_chars(specifier))
}

/// One redirect found by [`scan_command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The redirect as written: fd prefix, operator and target.
    pub text: String,
    pub target: RedirectTarget,
}

/// Which workspace operation a redirect target needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    Read,
    Write,
}

/// Why a redirect target is refused instead of workspace-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectRefusal {
    /// The operator has no target word.
    Empty,
    /// The target needs shell expansion (or quote handling the scanner does
    /// not mirror), so it cannot be known before the command runs.
    Expansion,
}

/// A redirect's classification. Descriptor duplications (`2>&1`, `>&-`) have
/// no file and are not reported at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectTarget {
    /// A literal path (quote removal applied) and the operation it needs.
    File {
        op: RedirectOp,
        target: String,
    },
    Refused(RedirectRefusal),
}

/// The result of the single scan of a shell command (Story 19.29, R3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandScan<'a> {
    /// Non-empty command segments, trimmed. The segment text keeps its
    /// redirects, so a prefix pattern still matches `kubectl get x > f`.
    pub segments: Vec<&'a str>,
    /// Every redirect that has, or may have, a file target, in command order.
    pub redirects: Vec<Redirect>,
    /// An unquoted word `cd`, `pushd` or `popd` occurs anywhere in the
    /// command (quote removal applied): a relative path may no longer name
    /// the workspace when a redirect opens it.
    pub changes_directory: bool,
}

struct PendingRedirect {
    start: usize,
    op: RedirectOp,
    /// `>&word` / `<&word`: a descriptor duplication when `word` is a number.
    dup: bool,
    word: Vec<u8>,
    started: bool,
    expansion: bool,
}

fn push_word_byte(pending: &mut Option<PendingRedirect>, word: &mut Vec<u8>, byte: u8) {
    word.push(byte);
    if let Some(pending) = pending {
        pending.started = true;
        pending.word.push(byte);
    }
}

/// `>`/`<`/`&>` operator at `index` (unquoted, unescaped): operation, byte
/// length, and whether it is the `>&`/`<&` duplication form.
fn redirect_operator(bytes: &[u8], index: usize) -> Option<(RedirectOp, usize, bool)> {
    let next = bytes.get(index + 1).copied();
    match bytes[index] {
        b'>' => Some(match next {
            Some(b'>' | b'|') => (RedirectOp::Write, 2, false),
            Some(b'&') => (RedirectOp::Write, 2, true),
            _ => (RedirectOp::Write, 1, false),
        }),
        b'<' => Some(match next {
            Some(b'>') => (RedirectOp::Write, 2, false),
            Some(b'&') => (RedirectOp::Read, 2, true),
            _ => (RedirectOp::Read, 1, false),
        }),
        b'&' if next == Some(b'>') => Some(if bytes.get(index + 2) == Some(&b'>') {
            (RedirectOp::Write, 3, false)
        } else {
            (RedirectOp::Write, 2, false)
        }),
        _ => None,
    }
}

fn is_word_boundary(bytes: &[u8], index: usize) -> bool {
    index == 0
        || matches!(
            bytes[index - 1],
            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'(' | b')' | b'<' | b'>'
        )
}

/// Start of the redirect as written: a decimal fd or `{name}` directly before
/// the operator belongs to it (display only; it never changes the class).
fn redirect_text_start(bytes: &[u8], operator: usize) -> usize {
    if operator > 0 && bytes[operator - 1] == b'}' {
        if let Some(open) = bytes[..operator - 1].iter().rposition(|b| *b == b'{') {
            let name = &bytes[open + 1..operator - 1];
            if !name.is_empty()
                && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
                && is_word_boundary(bytes, open)
            {
                return open;
            }
        }
        return operator;
    }
    let mut start = operator;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start < operator && is_word_boundary(bytes, start) {
        start
    } else {
        operator
    }
}

/// `n`, `-` or `n-`: the descriptor forms of `>&word` / `<&word`.
fn is_descriptor_word(word: &str) -> bool {
    let digits = word.strip_suffix('-').unwrap_or(word);
    word == "-" || (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn finish_redirect(
    pending: &mut Option<PendingRedirect>,
    scan: &mut CommandScan<'_>,
    command: &str,
    end: usize,
) {
    let Some(pending) = pending.take() else {
        return;
    };
    let word = String::from_utf8_lossy(&pending.word).into_owned();
    let target = if word.is_empty() {
        RedirectTarget::Refused(RedirectRefusal::Empty)
    } else if pending.expansion {
        RedirectTarget::Refused(RedirectRefusal::Expansion)
    } else if pending.dup && is_descriptor_word(&word) {
        return;
    } else {
        RedirectTarget::File {
            op: pending.op,
            target: word,
        }
    };
    scan.redirects.push(Redirect {
        text: command[pending.start..end].trim_end().to_string(),
        target,
    });
}

fn finish_word(word: &mut Vec<u8>, word_quoted: &mut bool, scan: &mut CommandScan<'_>) {
    if matches!(word.as_slice(), b"cd" | b"pushd" | b"popd") {
        scan.changes_directory = true;
    }
    word.clear();
    *word_quoted = false;
}

/// The byte after `index` as Bash sees it: backslash-newline pairs are deleted
/// before any construct is recognised, so every unsupported-opener lookahead
/// goes through this (Story 19.29 patch round 2, G1).
fn effective_next(bytes: &[u8], mut index: usize) -> Option<u8> {
    while bytes.get(index) == Some(&b'\\') && bytes.get(index + 1) == Some(&b'\n') {
        index += 2;
    }
    bytes.get(index).copied()
}

/// Split a shell command into segments and redirects in ONE pass, with one
/// quote/escape state. `None` means the command has a construct the scanner
/// cannot interpret safely (`$(…)`, backticks, process substitution,
/// here-docs/strings, an unquoted comment, `$'…'`, `$[…]`, `((…))`, `${…}`
/// inside double quotes, an unterminated quote or escape, an empty `&&`/`||`
/// operand, or no segment at all). Story 19.29 patch round 1: every construct
/// that can change quote state or hide text from Bash is either modelled
/// exactly or refused here; see the census in the story.
pub fn scan_command(command: &str) -> Option<CommandScan<'_>> {
    let bytes = command.as_bytes();
    let mut scan = CommandScan {
        segments: Vec::new(),
        redirects: Vec::new(),
        changes_directory: false,
    };
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut escaped = false;
    // Index of the byte most recently consumed as backslash-escaped; the
    // redirect-adjacency test below must not treat an escaped `>`/`<` as a
    // redirect operator (it is a literal word to bash).
    let mut escaped_at: Option<usize> = None;
    let mut pending: Option<PendingRedirect> = None;
    // The current shell word with quote removal (for the `cd` rule).
    let mut word: Vec<u8> = Vec::new();
    // Whether a quote opened inside the current word (`''#` is mid-word).
    let mut word_quoted = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            escaped_at = Some(index);
            // An unquoted backslash-newline is a line continuation: deleted.
            if byte != b'\n' {
                push_word_byte(&mut pending, &mut word, byte);
            }
            index += 1;
            continue;
        }

        if byte == b'`'
            || (matches!(byte, b'$' | b'<' | b'>')
                && effective_next(bytes, index + 1) == Some(b'('))
        {
            return None;
        }

        // `$[…]` (arithmetic) and `${…}` inside double quotes nest their own
        // quoting; the scanner does not mirror it, so refuse them. Inside
        // single quotes both are literal text.
        if byte == b'$'
            && quote != Some(b'\'')
            && (effective_next(bytes, index + 1) == Some(b'[')
                || (quote == Some(b'"') && effective_next(bytes, index + 1) == Some(b'{')))
        {
            return None;
        }

        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                } else {
                    push_word_byte(&mut pending, &mut word, byte);
                }
                index += 1;
                continue;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    escaped = true;
                    // Bash keeps or drops the backslash by the next byte;
                    // refuse the target instead of mirroring that.
                    if let Some(pending) = pending.as_mut() {
                        pending.expansion = true;
                    }
                } else if byte == b'"' {
                    quote = None;
                } else {
                    if byte == b'$'
                        && let Some(pending) = pending.as_mut()
                    {
                        pending.expansion = true;
                    }
                    push_word_byte(&mut pending, &mut word, byte);
                }
                index += 1;
                continue;
            }
            _ => {}
        }

        // A backslash-newline directly before the operator was deleted by
        // Bash, which can turn the preceding word into an fd prefix
        // (`2\⏎>x`); that form is refused rather than mirrored.
        let continued_before =
            index > 0 && escaped_at == Some(index - 1) && bytes[index - 1] == b'\n';
        let word_before = !word.is_empty();
        let terminator = matches!(
            byte,
            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')'
        );
        // An extended-glob opener (`@(`, `+(`, `!(`) ends the target word at
        // `(`; the truncated name would be checked instead of what Bash opens.
        if byte == b'('
            && let Some(pending) = pending.as_mut()
            && pending.started
        {
            pending.expansion = true;
        }
        if terminator {
            if matches!(byte, b' ' | b'\t') && pending.as_ref().is_some_and(|p| !p.started) {
                // Blanks between the operator and its target.
                index += 1;
                continue;
            }
            finish_redirect(&mut pending, &mut scan, command, index);
            finish_word(&mut word, &mut word_quoted, &mut scan);
        }

        // An unquoted `#` that starts a word is a comment to Bash, and
        // nothing inside it affects quote state. Modelling it exactly is
        // unsafe (`${x:- # }; echo > /out` is not a comment to Bash), so the
        // whole command is refused.
        if byte == b'#' && word.is_empty() && !word_quoted {
            return None;
        }
        // ANSI-C quoting (`$'…'`) has its own escape rules for `'`; refused
        // wherever it appears unquoted. `((…))` is arithmetic whose quoting
        // Bash handles apart from commands; refused too.
        if (byte == b'$' && effective_next(bytes, index + 1) == Some(b'\''))
            || (byte == b'(' && effective_next(bytes, index + 1) == Some(b'('))
        {
            return None;
        }

        match byte {
            b'\\' => {
                escaped = true;
                index += 1;
                continue;
            }
            b'\'' | b'"' => {
                quote = Some(byte);
                word_quoted = true;
                if let Some(pending) = pending.as_mut() {
                    pending.started = true;
                }
                index += 1;
                continue;
            }
            b'<' if effective_next(bytes, index + 1) == Some(b'<') => return None,
            _ => {}
        }

        if let Some((op, len, dup)) = redirect_operator(bytes, index) {
            let text_start = if byte == b'&' {
                index
            } else {
                redirect_text_start(bytes, index)
            };
            pending = Some(PendingRedirect {
                start: text_start,
                op,
                dup,
                word: Vec::new(),
                started: false,
                expansion: continued_before && word_before && byte != b'&',
            });
            index += len;
            continue;
        }

        if !terminator {
            if matches!(byte, b'$' | b'~' | b'*' | b'?' | b'[' | b']' | b'{' | b'}')
                && let Some(pending) = pending.as_mut()
            {
                pending.expansion = true;
            }
            push_word_byte(&mut pending, &mut word, byte);
            index += 1;
            continue;
        }

        let separator_len = if byte == b'\n' || byte == b';' {
            1
        } else if byte == b'&'
            && ((bytes
                .get(index.wrapping_sub(1))
                .is_some_and(|b| matches!(b, b'<' | b'>'))
                && escaped_at != Some(index.wrapping_sub(1)))
                || bytes
                    .get(index + 1)
                    .is_some_and(|b| matches!(b, b'<' | b'>')))
        {
            // `&` adjacent to `>`/`<` is a redirect operator — but only if
            // that neighbouring byte was not consumed as a backslash
            // escape. `\>` is a literal word to bash, so the `&` after it
            // is a real control operator and the command must be split
            // (Story 19.11 review: `kubectl get pods \>& touch /tmp/x`
            // runs `touch` as a second command; verified by execution).
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
            return None;
        }
        if !segment.is_empty() {
            scan.segments.push(segment);
        }
        index += separator_len;
        start = index;
        if logical && command[start..].trim().is_empty() {
            return None;
        }
    }

    if quote.is_some() || escaped {
        return None;
    }
    finish_redirect(&mut pending, &mut scan, command, bytes.len());
    finish_word(&mut word, &mut word_quoted, &mut scan);
    let segment = command[start..].trim();
    if !segment.is_empty() {
        scan.segments.push(segment);
    }
    (!scan.segments.is_empty()).then_some(scan)
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
    fn escaped_redirect_before_ampersand_is_a_real_separator() {
        // Story 19.11 review, executed: bash runs the second command here —
        // `\>` is a literal word, so the `&` is a control operator. The
        // redirect-adjacency suppression must not fire across an escape.
        let restricted = items(&["Bash(kubectl:*)"]);
        for command in [
            r"kubectl get pods \>& touch /tmp/pwned",
            r"kubectl get pods \<& sh -c env",
            r"kubectl get pods 2\>&1 printf PWNED",
        ] {
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
        // Unescaped redirect-adjacent `&` stays a redirect (A24), never a
        // split.
        assert!(command_matches_allowed_items(
            &restricted,
            "Bash",
            "kubectl get pods >& /tmp/log"
        ));
        assert!(command_matches_allowed_items(
            &restricted,
            "Bash",
            "kubectl get pods 2>&1"
        ));
    }

    #[test]
    fn junk_and_non_bash_specifiers_are_unmatched_never_grants() {
        // Whitespace-only specifiers, star-leading glob prefixes and
        // non-Bash patterns never make a tool "matched" at offer or
        // disclosure time (Story 19.11 review): they stay unmatched, are
        // named by the FR42-a advisory, and can never admit a segment.
        for (item, tool) in [
            ("Bash(   )", "Bash"),
            ("Bash(**)", "Bash"),
            ("Bash(*:*)", "Bash"),
            ("Bash( *)", "Bash"),
            ("Read(docs/*)", "Read"),
            ("Write(/tmp/*)", "Write"),
        ] {
            assert!(!allowed_item_matches_tool(item, tool), "item={item:?}");
        }
        assert!(allowed_item_matches_tool("Bash(kubectl:*)", "Bash"));
        assert!(allowed_item_matches_tool("Bash(*)", "Bash"));
        assert!(allowed_item_matches_tool("Read", "Read"));
        // And the matcher itself never admits a glob-leading segment.
        assert!(!command_matches_allowed_items(
            &items(&["Bash(**)"]),
            "Bash",
            "*.sh"
        ));
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

    // ---- Story 19.29: redirects from the one scanner ----------------------

    fn scanned(command: &str) -> CommandScan<'_> {
        scan_command(command).unwrap_or_else(|| panic!("command must scan: {command:?}"))
    }

    fn targets(command: &str) -> Vec<RedirectTarget> {
        scanned(command)
            .redirects
            .into_iter()
            .map(|redirect| redirect.target)
            .collect()
    }

    fn file(op: RedirectOp, target: &str) -> RedirectTarget {
        RedirectTarget::File {
            op,
            target: target.to_string(),
        }
    }

    #[test]
    fn redirect_forms_classify_to_file_target_with_operation() {
        use RedirectOp::{Read, Write};
        let rows: &[(&str, RedirectOp, &str)] = &[
            ("echo >x", Write, "x"),
            ("echo > x", Write, "x"),
            ("echo >\tx", Write, "x"),
            ("kubectl version>x", Write, "x"),
            ("echo 1>x", Write, "x"),
            ("echo 10>x", Write, "x"),
            ("echo {fd}>x", Write, "x"),
            ("echo >|x", Write, "x"),
            ("echo >>x", Write, "x"),
            ("echo 2>>x", Write, "x"),
            ("echo &>x", Write, "x"),
            ("echo &>>x", Write, "x"),
            ("echo >&x", Write, "x"),
            ("echo 2>&x", Write, "x"),
            ("echo >&1x", Write, "1x"),
            ("echo <>x", Write, "x"),
            ("echo <x", Read, "x"),
            ("echo 0<x", Read, "x"),
            ("echo <&x", Read, "x"),
            ("kubectl get pods >& /tmp/log", Write, "/tmp/log"),
        ];
        for (command, op, target) in rows {
            assert_eq!(
                targets(command),
                vec![file(*op, target)],
                "command={command:?}"
            );
        }
    }

    #[test]
    fn descriptor_duplications_have_no_file() {
        for command in [
            "kubectl get pods 2>&1",
            "echo >&2",
            "echo 2>&-",
            "echo 1>&2-",
            "echo <&0",
            "echo <&-",
            "echo 2>&1 >&-",
        ] {
            assert!(scanned(command).redirects.is_empty(), "command={command:?}");
        }
    }

    #[test]
    fn quote_removal_matches_what_bash_opens() {
        // Every spelling below is the absolute path `/abs` (or `a b`) to bash.
        for command in [
            "echo > \"/abs\"",
            "echo > '/abs'",
            "echo > \\/abs",
            "echo > ''/abs",
            "echo > \"\"/abs",
            "echo > '/'abs",
            "echo > \\\n/abs",
            "echo >\\\n/abs",
            "echo > /a\"\"bs",
        ] {
            assert_eq!(
                targets(command),
                vec![file(RedirectOp::Write, "/abs")],
                "command={command:?}"
            );
        }
        for command in ["echo > \"a b\"", "echo > 'a b'", "echo > a\\ b"] {
            assert_eq!(
                targets(command),
                vec![file(RedirectOp::Write, "a b")],
                "command={command:?}"
            );
        }
    }

    #[test]
    fn targets_that_cannot_be_known_before_the_run_are_refused() {
        use RedirectRefusal::{Empty, Expansion};
        let rows: &[(&str, RedirectRefusal)] = &[
            ("echo >", Empty),
            ("echo > ;", Empty),
            ("echo > &x", Empty),
            ("echo > ''", Empty),
            ("echo > $HOME/x", Expansion),
            ("echo > \"$HOME\"/x", Expansion),
            ("echo > \"/a\\b\"", Expansion),
            ("echo > ~/x", Expansion),
            ("echo > /tmp/*", Expansion),
            ("echo > /tmp/?", Expansion),
            ("echo > /tmp/[ab]", Expansion),
            ("echo > /tmp/{a,b}", Expansion),
            ("echo >&$fd", Expansion),
        ];
        for (command, reason) in rows {
            assert_eq!(
                targets(command).first(),
                Some(&RedirectTarget::Refused(*reason)),
                "command={command:?}"
            );
        }
        // A line continuation inside the operator joins `>` and `>` into
        // `>>`; refusing the first half is the safe over-reading.
        assert_eq!(
            targets("echo >\\\n>x"),
            vec![RedirectTarget::Refused(Empty), file(RedirectOp::Write, "x")]
        );
    }

    #[test]
    fn quoted_or_escaped_operators_are_not_redirects() {
        for command in [
            r"kubectl get pods \> x",
            "kubectl get -o jsonpath='{.a > .b}'",
            "echo \"a > b\"",
            r"echo a\<b",
        ] {
            assert!(scanned(command).redirects.is_empty(), "command={command:?}");
        }
    }

    #[test]
    fn quoted_expansion_characters_are_literal_not_refused() {
        assert_eq!(
            targets("echo > 'a$b*c'"),
            vec![file(RedirectOp::Write, "a$b*c")]
        );
        assert_eq!(
            targets("echo > \"~x\""),
            vec![file(RedirectOp::Write, "~x")]
        );
    }

    #[test]
    fn redirect_text_is_as_written_including_the_fd_prefix() {
        for (command, text) in [
            ("kubectl version 2>>x", "2>>x"),
            ("kubectl version {fd}>x", "{fd}>x"),
            ("kubectl version > ~/.bashrc", "> ~/.bashrc"),
            ("kubectl version &>>x", "&>>x"),
            ("kubectl version a2>x", ">x"),
        ] {
            assert_eq!(
                scanned(command).redirects[0].text,
                text,
                "command={command:?}"
            );
        }
    }

    #[test]
    fn redirects_after_a_separator_belong_to_the_command() {
        let scan = scanned("kubectl get pods && kubectl version > /etc/x; echo ok 2>/dev/null");
        assert_eq!(scan.segments.len(), 3);
        assert_eq!(
            scan.redirects
                .iter()
                .map(|r| r.target.clone())
                .collect::<Vec<_>>(),
            vec![
                file(RedirectOp::Write, "/etc/x"),
                file(RedirectOp::Write, "/dev/null")
            ]
        );
    }

    #[test]
    fn any_directory_changing_word_is_flagged() {
        for command in [
            "cd /tmp && kubectl version > x",
            "builtin cd /tmp",
            "command pushd x",
            "if popd; then echo ok; fi",
            "{ cd x; }",
            "(cd x)",
            "c\\d x",
            "c'd' x",
        ] {
            assert!(scanned(command).changes_directory, "command={command:?}");
        }
        for command in ["kubectl version > x", "echo cdx", "echo mycd", "echo a=cd"] {
            assert!(!scanned(command).changes_directory, "command={command:?}");
        }
    }

    #[test]
    fn command_level_refusals_are_unchanged_by_redirect_extraction() {
        for command in [
            "echo $(printf x)",
            "echo `x`",
            "echo <(x)",
            "echo >(x)",
            "cat <<EOF",
            "cat <<<x",
            "echo 'open",
            "echo trail\\",
            "&& echo x",
            "echo x &&",
            "",
            " ; ",
        ] {
            assert!(scan_command(command).is_none(), "command={command:?}");
        }
    }

    #[test]
    fn tool_admission_is_the_shared_pattern_predicate() {
        let admission = |values: &[&str]| tool_admission(values.iter().copied(), "Bash");
        assert_eq!(admission(&["Bash"]), ToolAdmission::Unrestricted);
        assert_eq!(admission(&["Bash(*)"]), ToolAdmission::Unrestricted);
        assert_eq!(
            admission(&["Bash(kubectl:*)", "Bash(*)"]),
            ToolAdmission::Unrestricted
        );
        assert_eq!(
            admission(&["Bash(kubectl:*)", "Read"]),
            ToolAdmission::Patterns
        );
        assert_eq!(admission(&["Read"]), ToolAdmission::Nothing);
        assert_eq!(admission(&["Bash()"]), ToolAdmission::Nothing);
        assert_eq!(admission(&[]), ToolAdmission::Nothing);
    }

    #[test]
    fn scanned_and_unscanned_matchers_agree() {
        let restricted = items(&["Bash(kubectl:*)"]);
        for command in [
            "kubectl get pods 2>&1",
            "kubectl get pods > x",
            "kubectl get pods && rm x",
            "kubectl get $(x)",
            "",
        ] {
            let scan = scan_command(command);
            assert_eq!(
                command_matches_scanned_items(
                    restricted.iter().map(String::as_str),
                    "Bash",
                    command,
                    scan.as_ref()
                ),
                command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
    }

    // ---- Story 19.29 patch round 1: the lexical census ---------------------
    //
    // R19-29-01/02/04. Every construct that can change quote state or hide
    // text from Bash is modelled exactly (`modelled_*`) or the whole command
    // is refused (rows below).

    #[test]
    fn constructs_that_can_hide_a_redirect_refuse_the_whole_command() {
        let restricted = items(&["Bash(echo:*)", "Bash(cat:*)"]);
        for command in [
            // R19-29-01 (review payload): quotes inside comments.
            "echo ok # '\necho escaped > /outside/19-29-comment # '",
            // R19-29-02 (review payload): ANSI-C `$'\''`.
            "echo $'\\'' > /outside/19-29-ansi # '",
            // Comment introducers at every word start.
            "echo # c",
            "echo ok #",
            "echo a;#c",
            "echo a &&#c",
            "echo a |#c",
            "echo >#x",
            "echo > #x",
            "# only a comment",
            // A comment is not a comment inside `${…}`.
            "echo ${x:- # }; echo > /outside/x",
            // ANSI-C quoting anywhere unquoted, target or argument.
            "echo $'a'",
            "echo a$'b'",
            "echo > $'/abs'",
            "echo $'\\x27' > /outside/x",
            // Arithmetic and `${…}` in double quotes nest their own quoting.
            "echo $[1+1]",
            "echo \"$[1+1]\"",
            "(( x > 1 ))",
            "echo $((1+1))",
            "echo \"${x:-'}\" > /outside/x '\"",
            "echo \"${x:-\"a b\"}\"",
            // Pre-existing refusals stay (substitution, heredoc, here-string).
            "echo $(x)",
            "echo `x`",
            "echo <(x)",
            "echo >(x)",
            "cat <<EOF",
            "cat <<-EOF",
            "cat <<'EOF'",
            "cat <<<x",
        ] {
            assert!(scan_command(command).is_none(), "command={command:?}");
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
    }

    #[test]
    fn modelled_constructs_keep_exact_quote_state() {
        let rows: &[&str] = &[
            // `#` that does not start a word is an ordinary byte.
            "echo a#b > x",
            "echo $# > x",
            "echo ''# > x",
            "echo \\# > x",
            "echo a\\ #b > x",
            // `#`, `$[` and `${` are literal inside quotes.
            "echo '# c' > x",
            "echo '$[1]' > x",
            "echo '${x}' > x",
            "echo \"# c\" > x",
            // `$"…"` is a double-quoted string; `\$'` is a literal `$`.
            "echo $\"a b\" > x",
            "echo \\$'q' > x",
            // Unquoted `${…}` quotes exactly like the surrounding word.
            "echo ${x:-'}'} > x",
            "echo ${x:-\"}\"} > x",
            // A line continuation inside a word is deleted.
            "echo a\\\nb > x",
            // `case … in pattern)` has no lexical effect of its own.
            "case x in a) echo > x ;; esac",
        ];
        for command in rows {
            assert_eq!(
                targets(command),
                vec![file(RedirectOp::Write, "x")],
                "command={command:?}"
            );
        }
    }

    #[test]
    fn a_continuation_between_an_fd_prefix_and_the_operator_is_refused() {
        // R19-29-04: Bash deletes `\⏎`, so `2\⏎>inside` is `2>inside`.
        for command in [
            "echo 2\\\n>inside",
            "echo 2\\\n<inside",
            "echo {fd}\\\n>inside",
            "echo 2\\\n>>inside",
        ] {
            assert_eq!(
                targets(command),
                vec![RedirectTarget::Refused(RedirectRefusal::Expansion)],
                "command={command:?}"
            );
        }
        // A continuation after the operator, or before a separate word, is
        // still deleted exactly.
        assert_eq!(
            targets("echo 2> \\\ninside"),
            vec![file(RedirectOp::Write, "inside")]
        );
        assert_eq!(
            targets("echo \\\n>inside"),
            vec![file(RedirectOp::Write, "inside")]
        );
    }

    // ---- Story 19.29 patch round 2: continued openers, extglob, cwd -------

    #[test]
    fn continued_unsupported_openers_refuse_the_whole_command() {
        // Bash deletes backslash-newline BEFORE recognising `$'`, `$[`, `$(`,
        // `((`, `<(`, `>(`, `<<`, and `${` in double quotes (G1).
        let restricted = items(&["Bash(echo:*)", "Bash(cat:*)"]);
        for command in [
            "echo $\\\n'\\'' > /outside/leak # '",
            "echo $\\\n'a'",
            "echo $\\\n\\\n'a'",
            "echo $\\\n[1+1]",
            "echo $\\\n(x)",
            "echo $(\\\n(1+1))",
            "(\\\n( x > 1 ))",
            "echo \"$\\\n{x}\"",
            "echo \"$\\\n[1]\"",
            "echo <\\\n(x)",
            "echo >\\\n(x)",
            "cat <\\\n<EOF",
            "cat <\\\n<<x",
        ] {
            assert!(scan_command(command).is_none(), "command={command:?}");
            assert!(
                !command_matches_allowed_items(&restricted, "Bash", command),
                "command={command:?}"
            );
        }
        // The supported literal-target continuations are unchanged.
        assert_eq!(
            targets("echo >\\\n/abs"),
            vec![file(RedirectOp::Write, "/abs")]
        );
        assert_eq!(
            targets("echo $\\\nHOME > x"),
            vec![file(RedirectOp::Write, "x")]
        );
    }

    #[test]
    fn extended_glob_targets_are_refused_not_truncated() {
        // G2: the target word ended at `(`, so `@` was checked as a filename.
        for command in [
            "echo > @(a)",
            "echo > +(a)",
            "echo > !(a)",
            "echo >> @(a)",
            "echo >@(a)",
            "echo &>+(a)",
            "echo >&!(a)",
            "echo < @(a)",
        ] {
            assert_eq!(
                targets(command).first(),
                Some(&RedirectTarget::Refused(RedirectRefusal::Expansion)),
                "command={command:?}"
            );
        }
    }

    #[test]
    fn a_double_quoted_continuation_is_deleted_from_ordinary_words() {
        // G3: `"c\⏎d"` is `cd` to Bash.
        for command in [
            "builtin \"c\\\nd\" /outside && echo x > leak",
            "\"c\\\nd\" /outside",
            "command \"pu\\\nshd\" /outside",
        ] {
            assert!(scanned(command).changes_directory, "command={command:?}");
        }
    }
}
