//! K7 (second half): a scoped static gate over the migrated production
//! functions. It rejects the arithmetic that caused the 21-11 defects (scalar
//! count as a width, scalar clipping, per-scalar width sums, one-cell-per-scalar
//! painting, byte-length padding) INSIDE the named functions only.
//!
//! This is a ratchet, not proof: K2-K6 drive the real renderers and remain the
//! behavioral authority. Legitimate source/index/list/ASCII arithmetic is not
//! banned globally: the function list is explicit and the few allowed
//! expressions are listed in `EXEMPT` with a reason, so a new exemption is a
//! reviewed change. The positive controls prove the gate sees what it bans and
//! leaves the legitimate forms alone.

/// The source before its trailing unit-test module, so test helpers that share
/// a production function's name can neither shadow nor duplicate it.
fn production_part(source: &'static str) -> &'static str {
    source
        .find("\n#[cfg(test)]\nmod ")
        .map_or(source, |at| &source[..at])
}

struct Site {
    file: &'static str,
    source: &'static str,
    functions: &'static [&'static str],
}

macro_rules! site {
    ($file:literal, [$($function:literal),+ $(,)?]) => {
        Site {
            file: $file,
            source: include_str!(concat!("../../src/adapters/tui/", $file)),
            functions: &[$($function),+],
        }
    };
}

const SITES: &[Site] = &[
    site!("widgets/sidebar.rs", ["truncate_to_width"]),
    site!("widgets/modal.rs", ["ellipsize"]),
    site!(
        "widgets/chat_pane/word_wrap.rs",
        ["wrap_text", "wrap_line_words", "char_wrap_word"]
    ),
    site!("markdown/layout.rs", ["clip_str", "display_width"]),
    site!(
        "widgets/chat_pane/mod.rs",
        ["truncate_with_ellipsis", "apply_search_highlights"]
    ),
    site!("widgets/login_dialog.rs", ["single_line"]),
    site!("widgets/permission_prompt.rs", ["key_rows"]),
    site!(
        "widgets/tab_bar.rs",
        ["truncate_to_cells", "render_tab_bar"]
    ),
    site!("widgets/search_bar.rs", ["render"]),
    site!("widgets/reverse_search.rs", ["render"]),
    site!(
        "widgets/cross_search.rs",
        ["render", "build_excerpt_spans", "shorten_to_cells"]
    ),
    site!("widgets/bookmark_list.rs", ["render"]),
    site!("widgets/command_palette.rs", ["format_palette_entry"]),
    site!(
        "widgets/plan_card.rs",
        [
            "render_plan_card_lines",
            "render_sub_task_card_line",
            "wrap_description"
        ]
    ),
    site!(
        "widgets/ask_user_question.rs",
        ["render_ask_user_lines", "wrap_question"]
    ),
    site!(
        "widgets/fork_confirm.rs",
        ["render_fork_confirmation_lines"]
    ),
    site!(
        "widgets/rewind_confirm.rs",
        [
            "panel_top_row",
            "panel_row",
            "render_rewind_confirmation_lines"
        ]
    ),
    site!(
        "widgets/consolidation_card.rs",
        ["render_consolidation_card_lines"]
    ),
    site!("widgets/forget_card.rs", ["render_forget_card_lines"]),
    site!("widgets/peer_add_prompt.rs", ["render_peer_add_lines"]),
    site!(
        "widgets/team_retract_prompt.rs",
        ["render_team_retract_lines", "wrap_chars"]
    ),
    site!(
        "widgets/apply_card.rs",
        [
            "fits",
            "wrap_words",
            "wrapped_file_chunks",
            "push_apply_body"
        ]
    ),
    site!(
        "widgets/skill_trust_prompt.rs",
        ["render_trust_lines", "render_inspect_lines"]
    ),
    site!("widgets/usage_panel.rs", ["truncate_model"]),
    site!("widgets/result_row.rs", ["render_result_row"]),
    site!("widgets/adapter_status_panel.rs", ["mcp_health_line"]),
    site!("widgets/agent_panel.rs", ["render"]),
    site!("widgets/task_panel.rs", ["render_task_panel"]),
    site!("handlers/team_command.rs", ["render_team_send_rows"]),
    site!("handlers/config.rs", ["short_reason"]),
    site!("widgets/input_box.rs", ["input_area_height", "render"]),
];

/// Expressions that would be flagged but are legitimate in the named function:
/// `(file, function, snippet, reason)`. Anything not listed here is a failure.
const EXEMPT: &[(&str, &str, &str, &str)] = &[
    // Add an entry only with a reason that names the unit (source index, byte
    // cap, list count, ASCII constant).
    (
        "widgets/permission_prompt.rs",
        "key_rows",
        ".chars().count()",
        "fixed key prefix and body delimiter are single-cell ASCII/box glyph constants, not arbitrary input",
    ),
    (
        "handlers/config.rs",
        "short_reason",
        ".chars().count()",
        "converts the byte cut to a scalar source index for grapheme snapping; the 80-byte bound is a diagnostic cap, not a column count",
    ),
    (
        "widgets/input_box.rs",
        "render",
        ".chars().count()",
        "the 500-scalar token-estimate hint threshold is a heuristic, not a width",
    ),
];

/// Substrings that are width/clip arithmetic on scalars or bytes.
const PROHIBITED: &[(&str, &str)] = &[
    (".chars().count()", "scalar count is not a terminal width"),
    (".chars().take(", "scalar clipping can split a grapheme"),
    (
        ".chars().rev().take(",
        "scalar tail clipping can split a grapheme",
    ),
    (".chars().skip(", "scalar skipping can split a grapheme"),
    (
        "UnicodeWidthChar",
        "a sum of scalar widths is not the string width",
    ),
    ("width_cjk", "the pinned policy is the default string width"),
    ("chars().map(|c| c.len_utf8", "bytes are not cells"),
];

/// A `set_char(` call is per-scalar painting unless it writes a literal blank.
const ALLOWED_SET_CHAR: &str = "set_char(' ')";

/// Body of `fn name` in `source`: from its opening `{` to the matching `}`,
/// skipping strings, char literals and comments. `None` if absent.
fn function_body<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("fn {name}");
    let mut from = 0;
    let mut starts = Vec::new();
    while let Some(found) = source[from..].find(&needle) {
        let at = from + found;
        let after = source[at + needle.len()..].chars().next();
        let before = source[..at].chars().next_back();
        if matches!(after, Some('(' | '<'))
            && !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            starts.push(at);
        }
        from = at + needle.len();
    }
    // A name that matches twice would silently gate the wrong body.
    assert!(
        starts.len() <= 1,
        "fn {name} matches {} definitions: scope the site to a unique production fn",
        starts.len()
    );
    let start = *starts.first()?;
    let bytes = source.as_bytes();
    let mut i = start;
    // Signature up to the body brace (generic bounds never contain a `{`).
    while i < bytes.len() && bytes[i] != b'{' {
        if bytes[i] == b';' {
            return None; // trait method declaration without a body
        }
        i += 1;
    }
    let open = i;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'\'' => {
                // A char literal ('x', '\n', '{') versus a lifetime ('a).
                let rest = &source[i + 1..];
                let mut it = rest.chars();
                match it.next() {
                    Some('\\') => {
                        if let Some(end) = rest[1..].find('\'') {
                            i += 1 + 1 + end;
                        }
                    }
                    Some(c) if it.next() == Some('\'') => i += 1 + c.len_utf8(),
                    _ => {}
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[open..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Every prohibited construct in `body`, as `(snippet, reason)`.
fn violations(body: &str) -> Vec<(String, &'static str)> {
    let mut found = Vec::new();
    for (pattern, reason) in PROHIBITED {
        if body.contains(pattern) {
            found.push(((*pattern).to_owned(), *reason));
        }
    }
    let mut from = 0;
    while let Some(at) = body[from..].find("set_char(") {
        let at = from + at;
        if !body[at..].starts_with(ALLOWED_SET_CHAR) {
            found.push((
                body[at..(at + 24).min(body.len())].to_owned(),
                "one cell per scalar painting",
            ));
        }
        from = at + "set_char(".len();
    }
    // `x.repeat(n - something.len())`: bytes (or scalars) as a padding width.
    let mut from = 0;
    while let Some(at) = body[from..].find(".repeat(") {
        let start = from + at + ".repeat(".len();
        let mut depth = 1usize;
        let mut end = start;
        for (offset, c) in body[start..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        let argument = &body[start..end];
        if argument.contains(".len()") {
            found.push((
                format!("repeat({argument})"),
                "byte length as a padding width",
            ));
        }
        from = start;
    }
    found
}

#[test]
fn k7_gate_finds_every_named_function_and_the_migrated_bodies_are_clean() {
    let mut checked = 0;
    let total: usize = SITES.iter().map(|site| site.functions.len()).sum();
    let mut used = vec![false; EXEMPT.len()];
    let mut failures = Vec::new();
    for site in SITES {
        for function in site.functions {
            let body = function_body(production_part(site.source), function).unwrap_or_else(|| {
                panic!("{}: fn {function} not found: the gate is stale", site.file)
            });
            checked += 1;
            for (snippet, reason) in violations(body) {
                let exempt = EXEMPT.iter().position(|(file, name, allowed, _)| {
                    *file == site.file && name == function && snippet.contains(allowed)
                });
                match exempt {
                    Some(index) => used[index] = true,
                    None => failures.push(format!(
                        "{} :: {function}: `{snippet}` ({reason})",
                        site.file
                    )),
                }
            }
        }
    }
    assert_eq!(checked, total, "every named function must be checked");
    assert!(checked >= 45, "the gate must stay scoped ({checked})");
    let stale: Vec<_> = EXEMPT
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((file, name, snippet, _), _)| format!("{file} :: {name}: `{snippet}`"))
        .collect();
    assert!(
        stale.is_empty(),
        "stale exemptions (remove them):\n{}",
        stale.join("\n")
    );
    assert!(
        failures.is_empty(),
        "width arithmetic regressed:\n{}",
        failures.join("\n")
    );
}

#[test]
fn k7_gate_positive_controls_detect_the_banned_forms_and_spare_legitimate_ones() {
    let banned = r#"
        fn pad(name: &str, width: usize) -> String {
            let used = name.chars().count();
            format!("{}{}", name, " ".repeat(width - name.len()))
        }
        fn clip(text: &str, max: usize) -> String { text.chars().take(max).collect() }
        fn paint(buf: &mut Buffer, c: char) { buf[(0, 0)].set_char(c); }
        fn sum(text: &str) -> usize { text.chars().map(|c| UnicodeWidthChar::width(c).unwrap_or(1)).sum() }
    "#;
    let pad = violations(function_body(banned, "pad").unwrap());
    assert!(pad.iter().any(|(s, _)| s == ".chars().count()"), "{pad:?}");
    assert!(pad.iter().any(|(s, _)| s.starts_with("repeat(")), "{pad:?}");
    assert!(!violations(function_body(banned, "clip").unwrap()).is_empty());
    assert!(!violations(function_body(banned, "paint").unwrap()).is_empty());
    assert!(!violations(function_body(banned, "sum").unwrap()).is_empty());

    // Legitimate forms: list counts and byte caps, literal-blank clears,
    // a source-index cursor that is NOT inside a gated function, braces in
    // strings and char literals do not confuse the extractor.
    let legit = r#"
        fn rows(items: &[String]) -> usize { items.len() + "}{".len() }
        fn clear(buf: &mut Buffer) { buf[(0, 0)].set_char(' '); let _b = '{'; let _l: &'static str = "}"; }
        fn cap(text: &str) -> &str { &text[..text.len().min(120)] }
        fn source_index(text: &str) -> usize { text.chars().count() }
    "#;
    for name in ["rows", "clear", "cap"] {
        let v = violations(function_body(legit, name).unwrap());
        assert!(v.is_empty(), "{name}: {v:?}");
    }
    assert!(function_body(legit, "missing").is_none());
    // The scalar-source function is only exempt because it is not gated.
    assert!(!violations(function_body(legit, "source_index").unwrap()).is_empty());
    assert!(
        SITES
            .iter()
            .all(|site| site.functions.iter().all(|f| *f != "source_index"))
    );
}
