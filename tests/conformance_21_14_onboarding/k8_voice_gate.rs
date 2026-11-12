//! K8 — the voice gate (AC16, AC17, AC18; D19, D21, D26).
//!
//! The second rule of the 21-3 user-visible string-literal scanner (`conformance_21_3_keymap::
//! k12_gate`, reused, not copied: the literal extractor, the production-part blanking and the
//! pin accounting):
//!
//! * no production string literal under `src/` (the keymap dir included) names an internal
//!   story, epic, requirement or decision identifier, and none speaks in the first person
//!   (`I`, `I'm`, `I'll`, `I've`, `I'd`; `I/O` is not first person);
//! * literals that never reach a user surface (tracing text, assert messages, model-facing tool
//!   descriptions, code markers) are pinned by exact prefix with a reason and an owner; a stale or
//!   miscounted pin fails;
//! * no production code outside `notifications.rs` builds an Error-severity notice: every Error
//!   goes through `notifications::failure` / `NoticeDraft::failure`, which state what happened and
//!   the next step (match arms that only map a severity to a colour are not producers);
//! * in-memory positive controls feed synthetic sources through the same functions.

use std::sync::LazyLock;

use regex::Regex;

use crate::conformance_21_3_keymap::k12_gate::{
    Allow, assert_whole_tree_scanned, literal_lines, pinned_violations_scanning, production_part,
    production_sources,
};

/// Internal identifiers: stories, epics, requirements, architecture and decision records.
pub(crate) const IDENTIFIER: &str =
    r"\b(?:(?:Story|Stories|Epic)\s+\d|(?:FR|NFR|UX-DR|ADR|AD|RU|DF|KG|MW|LR|OD|TM|AC)-?\d)";

/// The product speaking as `I`. `I/O` is checked after the match (the `regex` crate has no
/// look-ahead).
pub(crate) const FIRST_PERSON: &str = r"(?:^|[^\w/])I(?:'m|'ll|'ve|'d)?\b";

const OWNER: &str = "21-14 permanent";

static IDENTIFIER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(IDENTIFIER).unwrap());
static FIRST_PERSON_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(FIRST_PERSON).unwrap());

/// What a text breaks, one word per broken rule (`identifier`, `first person`).
pub(crate) fn text_rules(text: &str) -> Vec<&'static str> {
    let mut rules = Vec::new();
    if IDENTIFIER_RE.is_match(text) {
        rules.push("identifier");
    }
    if FIRST_PERSON_RE
        .find_iter(text)
        .any(|m| !text[m.end()..].starts_with("/O"))
    {
        rules.push("first person");
    }
    rules
}

/// One message per line of `text` that breaks a voice rule (the CLI help gate, K9, feeds every
/// rendered help text through this same function).
pub(crate) fn text_violations(label: &str, text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| {
            text_rules(line)
                .into_iter()
                .map(move |rule| format!("{label}: {rule} in {:?}", line.trim()))
        })
        .collect()
}

/// `(line, literal text)` of every voice hit in the production part of `source`; the text is
/// trimmed at the start so a pin names the words, not the continuation indent.
fn voice_hits(_path: &str, source: &str) -> Vec<(usize, String)> {
    literal_lines(&production_part(source))
        .into_iter()
        .filter(|(_, text)| !text_rules(text).is_empty())
        .map(|(line, text)| (line, text.trim_start().to_owned()))
        .collect()
}

/// `NoticeSeverity::Error` in a production part, except in a match arm (`... => ...`) that only
/// maps the severity to a colour and in comments.
fn error_severity_hits(path: &str, source: &str) -> Vec<(usize, String)> {
    const NOTIFICATIONS: &str = "src/adapters/tui/notifications.rs";
    if path == NOTIFICATIONS {
        return Vec::new();
    }
    production_part(source)
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| {
            line.match_indices("NoticeSeverity::Error")
                .any(|(at, found)| {
                    let rest = line[at + found.len()..].trim_start();
                    !rest.starts_with("=>") && !rest.starts_with('|')
                })
        })
        .map(|(index, line)| (index + 1, line.trim().to_owned()))
        .collect()
}

fn voice_violations(files: &[(String, String)], allow: &[Allow]) -> Vec<String> {
    pinned_violations_scanning(
        files,
        allow,
        voice_hits,
        "internal identifier or first person",
        None,
    )
}

/// The producers that cannot call `failure`: the auth-handler boundary checker
/// (`.github/scripts/check-auth-handler-boundary.py`, sha-pinned by the story) admits only
/// `notifications::notify` in the login and account handlers. Their messages are built as
/// fact / next step / detail (the account runtime and `format!` in the handler; pinned by the
/// in-file `story21_14_a_failed_login_*` test and `conformance_20_1e2_tui_accounts`).
#[rustfmt::skip]
const ERROR_ALLOWLIST: &[Allow] = &[
    ("src/adapters/tui/handlers/login.rs", &["NoticeSeverity::Error,", "NoticeSeverity::Error,"], OWNER, "auth-handler boundary admits only `notifications::notify`; both failures carry fact, next step and detail"),
    ("src/adapters/tui/handlers/accounts.rs", &["(FeedbackLevel::Error, _) => NoticeSeverity::Error,"], OWNER, "auth-handler boundary admits only `notifications::notify`; the account runtime builds fact, next step and detail"),
];

fn error_violations(files: &[(String, String)], allow: &[Allow]) -> Vec<String> {
    pinned_violations_scanning(
        files,
        allow,
        error_severity_hits,
        "Error notice built outside `failure`",
        None,
    )
}

/// Literals that never reach a user surface. `(file, prefixes of the hit text, owner, reason)`.
#[rustfmt::skip]
const ALLOWLIST: &[Allow] = &[
    ("src/adapters/a2a/auth.rs", &["is not an accepted scheme (DF-18-1-OAUTH2)"], OWNER, "`tracing::debug!` text of a rejected credential scheme"),
    ("src/adapters/acp/translate.rs", &["permanently (ADR-06-08"], OWNER, "`tracing::warn!` text of an ignored ACP server"),
    ("src/adapters/daemon/lifecycle.rs", &["daemon lifecycle ready (Story 12.4", "AC3 honor per boundary", "AC4 on_session_end per boundary"], OWNER, "`tracing::info!` text and two test assert messages"),
    ("src/adapters/tui/handlers/compaction.rs", &["NFR58: memory flush before compaction failed"], OWNER, "`tracing::warn!` text"),
    ("src/domain/models/tool_descriptor.rs", &["ToolDescriptor.to_search_hit produced name", "ToolDescriptor.to_search_hit produced name"], OWNER, "a `debug_assert!` message and the `tracing::warn!` text beside it"),
    ("src/domain/services/team_policy.rs", &["DF-18-3b-CONSENT-PROJECTION"], OWNER, "code marker naming the deferral that owns the consent projection; never painted"),
    ("src/adapters/toolset_adapter.rs", &["Propose a structured multi-step plan"], OWNER, "model-facing tool description of `propose_plan`"),
    ("src/infrastructure/runtime/event_loop.rs", &["Sandbox policy updated on permission mode change (Story 9.5"], OWNER, "`tracing::info!` text"),
    ("src/infrastructure/composition/mod.rs", &["(ADR-06-04"], OWNER, "`tracing::warn!` text of the sandbox fallback"),
    ("src/infrastructure/startup.rs", &["ADR-06-04 §Negative — documented as known limitation"], OWNER, "`tracing::warn!` text of the sandbox restriction"),
    ("src/adapters/tui/keymap/table.rs", &["Ctrl+X, I"], OWNER, "the key label of the `I` leader key, not first person"),
];

fn src(path: &str, source: &str) -> (String, String) {
    (path.to_owned(), source.to_owned())
}

// ── the real tree ─────────────────────────────────────────────────────────────

#[test]
fn production_literals_carry_no_internal_identifier_or_first_person_except_the_pinned() {
    let (files, skipped) = production_sources();
    assert_whole_tree_scanned(&files, &skipped);
    let violations = voice_violations(&files, ALLOWLIST);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn the_voice_allowlist_is_well_formed_and_every_entry_names_a_reason() {
    let mut seen = std::collections::BTreeSet::new();
    for (file, pins, owner, reason) in ALLOWLIST {
        assert!(seen.insert(*file), "{file} is listed twice");
        assert!(file.starts_with("src/") && file.ends_with(".rs"), "{file}");
        assert!(
            !pins.is_empty() && pins.iter().all(|p| !p.is_empty()),
            "{file}: pins"
        );
        assert_eq!(*owner, OWNER, "{file}");
        assert!(reason.len() > 10, "{file}: a reason is required");
    }
}

#[test]
fn no_production_code_outside_notifications_builds_an_error_notice() {
    let (files, skipped) = production_sources();
    assert_whole_tree_scanned(&files, &skipped);
    let violations = error_violations(&files, ERROR_ALLOWLIST);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
    let notifications = files
        .iter()
        .find(|(path, _)| path == "src/adapters/tui/notifications.rs")
        .expect("the store module is scanned");
    assert!(
        !notifications.1.contains("pub fn error("),
        "the `error` shorthand is deleted"
    );
}

// ── positive controls (synthetic sources through the same functions) ──────────

#[test]
fn single_line_literals_are_scanned() {
    for (source, rules) in [
        (
            "fn f() { s(\"Start it (Story 1.2, FR9)\"); }",
            vec!["identifier"],
        ),
        (
            "fn f() { s(\"see ADR-09-02 for why\"); }",
            vec!["identifier"],
        ),
        ("fn f() { s(\"wait for DF-17-4a-2\"); }", vec!["identifier"]),
        ("fn f() { s(\"deferred to Epic 12\"); }", vec!["identifier"]),
        ("fn f() { s(\"I will draft it\"); }", vec!["first person"]),
        ("fn f() { s(\"I'm sorry\"); }", vec!["first person"]),
        (
            "fn f() { s(\"reply for me; I always see it\"); }",
            vec!["first person"],
        ),
    ] {
        let hits = voice_hits("src/x.rs", source);
        assert_eq!(hits.len(), 1, "{source}");
        assert_eq!(text_rules(&hits[0].1), rules, "{source}");
    }
}

#[test]
fn plain_words_and_lookalikes_are_not_hits() {
    for ok in [
        "I/O error",
        "an I/O problem",
        "the ADAPTER list",
        "Add a story to the list",
        "Ctrl+X, then press a key",
        "FRAME 3",
        "mAC-less",
        "Storyboard 1",
        "Hi there",
        "Use `--all` to see more",
    ] {
        assert!(text_rules(ok).is_empty(), "{ok}: {:?}", text_rules(ok));
    }
    // Comments, char literals and test items are not user text.
    let not_literals = "// Story 1.2 in a comment\n/// FR9 in a doc\nfn f() { let _c = 'I'; }\n#[cfg(test)]\nmod tests {\n    fn t() { s(\"I am a test (Story 3.1)\"); }\n}\nfn after() { s(\"clean\"); }\n";
    assert!(voice_hits("src/x.rs", not_literals).is_empty());
}

/// M10's killing test: a literal that spans several physical lines is scanned on every one of
/// them, whichever line carries the identifier (the continuation lines hold no quote).
#[test]
fn multi_line_literals_are_scanned() {
    let continued = "fn f() {\n    s(\"first line, \\\n       then (Story 9.7) on the second, \\\n       and a third\");\n}\n";
    let hits = voice_hits("src/x.rs", continued);
    assert_eq!(
        hits.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        vec![3]
    );
    assert!(hits[0].1.starts_with("then (Story 9.7)"), "{hits:?}");
    let raw = "fn g() {\n    s(r#\"a raw string\n    that says I always\n    ends here\"#);\n}\n";
    let hits = voice_hits("src/x.rs", raw);
    assert_eq!(
        hits.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        vec![3]
    );
}

#[test]
fn format_and_raw_string_literals_are_scanned() {
    let source =
        "fn f(x: u8) { s(format!(\"{x} failed (see ADR-06-04)\")); t(r#\"quote \"I\" said\"#); }\n";
    let hits = voice_hits("src/x.rs", source);
    assert_eq!(hits.len(), 2, "{hits:?}");
}

#[test]
fn an_unpinned_hit_a_stale_pin_and_a_vanished_file_fail() {
    let hit = "fn f() { s(\"deferred to Epic 12+\"); }";
    let two = "fn f() { s(\"deferred to Epic 12+\"); }\nfn g() { s(\"wait for DF-9-9\"); }";
    let allow: &[Allow] = &[
        (
            "src/a.rs",
            &["deferred to Epic 12+"],
            OWNER,
            "listed, exact",
        ),
        (
            "src/b.rs",
            &["deferred to Epic 12+"],
            OWNER,
            "listed, but a second literal appeared",
        ),
        (
            "src/c.rs",
            &["deferred to Epic 12+"],
            OWNER,
            "listed, but now clean",
        ),
        ("src/gone.rs", &["x"], OWNER, "listed, file vanished"),
    ];
    let files = [
        src("src/a.rs", hit),
        src("src/b.rs", two),
        src("src/c.rs", "fn f() { s(\"nothing here\"); }"),
        src("src/new.rs", hit),
        src(
            "src/adapters/tui/keymap/table.rs",
            "const L: &str = \"see FR1\";",
        ),
    ];
    let text = voice_violations(&files, allow).join("\n");
    assert!(
        text.contains("src/new.rs:1: internal identifier or first person"),
        "{text}"
    );
    assert!(text.contains("src/b.rs:2: unpinned"), "{text}");
    assert!(
        text.contains("stale allowlist entry src/c.rs") && text.contains("matches no hit"),
        "{text}"
    );
    assert!(
        text.contains("stale allowlist entry src/gone.rs") && text.contains("no longer exists"),
        "{text}"
    );
    assert!(
        text.contains("src/adapters/tui/keymap/table.rs:1"),
        "the keymap dir is scanned: {text}"
    );
    assert!(!text.contains("src/a.rs:"), "{text}");
    assert_eq!(voice_violations(&files, allow).len(), 5, "{text}");
}

#[test]
fn an_error_notice_built_outside_the_store_is_caught() {
    let bad = "fn f(s: &mut TuiState) {\n    notify(s, NoticeSource::Export, NoticeSeverity::Error, \"x\");\n}\n";
    let in_store = "fn f() { let _ = NoticeSeverity::Error; }";
    let arm = "fn c(s: NoticeSeverity) -> u8 {\n    match s {\n        NoticeSeverity::Error => 1,\n        NoticeSeverity::Warning | NoticeSeverity::Error => 2,\n    }\n}\n";
    let comment = "// NoticeSeverity::Error is built by `failure`\nfn f() {}\n";
    let test_only =
        "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = NoticeSeverity::Error; }\n}\n";
    let files = [
        src("src/adapters/tui/handlers/x.rs", bad),
        src("src/adapters/tui/notifications.rs", in_store),
        src("src/adapters/tui/widgets/toast_layer.rs", arm),
        src("src/adapters/tui/y.rs", comment),
        src("src/adapters/tui/z.rs", test_only),
        src(
            "src/infrastructure/runtime/attach_loop.rs",
            "fn g() { d(NoticeSeverity::Error); }",
        ),
    ];
    let text = error_violations(&files, &[]).join("\n");
    assert!(text.contains("src/adapters/tui/handlers/x.rs:2"), "{text}");
    assert!(
        text.contains("src/infrastructure/runtime/attach_loop.rs:1"),
        "{text}"
    );
    assert_eq!(error_violations(&files, &[]).len(), 2, "{text}");
    // A pin is exact: a stale one fails and a second Error in a pinned file is seen.
    let pinned: &[Allow] = &[
        (
            "src/pinned.rs",
            &["NoticeSeverity::Error,"],
            OWNER,
            "pinned",
        ),
        ("src/stale.rs", &["NoticeSeverity::Error,"], OWNER, "stale"),
    ];
    let two = "fn f() {\n    d(\n        NoticeSeverity::Error,\n        NoticeSeverity::Error,\n    );\n}\n";
    let text = error_violations(
        &[src("src/pinned.rs", two), src("src/stale.rs", "fn g() {}")],
        pinned,
    )
    .join("\n");
    assert!(text.contains("src/pinned.rs:4: unpinned"), "{text}");
    assert!(
        text.contains("stale allowlist entry src/stale.rs"),
        "{text}"
    );
}

/// UX-mandated and receipt-bound literals are never reworded by the voice pass (AC18): the
/// stale-approval copy, the unbound-key toast, the monitor refusal's witness head (read by the
/// generic-lane keymap-truth test) and the decision field label `My own`, which the Conflict
/// receipts show as `[w] My own`. None of them breaks a voice rule, so none is pinned in the
/// allowlist; this test keeps them exactly as they are.
#[test]
fn receipt_bound_and_ux_mandated_literals_stay_verbatim() {
    let (files, _) = production_sources();
    for (file, literal) in [
        (
            "src/adapters/daemon/protocol.rs",
            "approval refused: that request is no longer pending",
        ),
        (
            "src/adapters/tui/decision_queue/kinds.rs",
            "approval refused: that request is no longer pending",
        ),
        (
            "src/adapters/tui/keymap/resolve.rs",
            "No binding for Ctrl+X {} in {context_name}",
        ),
        (
            "src/adapters/tui/keymap/table.rs",
            "The monitor needs a terminal at least 120",
        ),
        ("src/adapters/tui/keymap/table_card.rs", "My own"),
    ] {
        let source = &files
            .iter()
            .find(|(path, _)| path == file)
            .unwrap_or_else(|| panic!("{file} is scanned"))
            .1;
        let found = literal_lines(&production_part(source))
            .into_iter()
            .any(|(_, text)| text.trim_start().starts_with(literal));
        assert!(found, "{file} no longer carries the literal {literal:?}");
    }
}
