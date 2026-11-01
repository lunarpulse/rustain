//! K12 (3-A part) — the RU-31 cutover gate over the production code of BOTH hosts (AC18, AC9).
//!
//! A scoped static ratchet, not behavioural proof (K1-K4 drive the real keymap):
//!
//! * the PC-3 symbols (`CHORD_LABELS`, `help_data`, ...) and a `DomainKey::CtrlH`/`CtrlK` match
//!   arm outside `src/adapters/tui/keymap/` are ABSENT from every production part under `src/`;
//! * no string literal in a production part outside `src/adapters/tui/keymap/` matches the
//!   hard-coded key-hint grammar P1/P2 of AC18 unless its file is on the allowlist with an exact
//!   hit count and a named owner (3-C, 21-4, or the permanent AC18 set).
//!
//! Gate hygiene (21-1 K11 / 21-11 K7 pattern): production part only (an item gated by a
//! column-0 `#[cfg(test)]` item is blanked, so test modules neither hide nor add hits and
//! production items after a test module are still scanned; an item header the scanner cannot
//! place is refused); every `.rs` file under `src/` is scanned except the seven pinned
//! test-only files; a stale allowlist entry (different count,
//! including zero, or a vanished file) fails; in-memory positive controls feed synthetic sources
//! through the same scanner functions.
//!
//! P1/P2 apply to string literals only. The literal extractor ignores `//`, `///`, `//!` and
//! block comments and every non-literal token (so `v[i]` indexing is never a hit); a hit is a
//! physical source line whose literal text matches. The tests read `src/**/*.rs` relative to
//! `CARGO_MANIFEST_DIR` and nothing outside `rustain/`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

const KEYMAP_DIR: &str = "src/adapters/tui/keymap/";

/// PC-3 symbols that must not appear (whole word) in any production part under `src/`.
const PC3_SYMBOLS: &[&str] = &[
    "CHORD_LABELS",
    "SLASH_SHORTCUTS",
    "RETAINED_CHORDS",
    "chord_map",
    "ChordAction",
    "chord_menu",
    "help_data",
    "HelpBinding",
    "HelpCategory",
    "FeedbackAction",
    "key_label",
    "dispatch_key",
    "ToggleSidebar",
];

/// A `DomainKey::CtrlH`/`CtrlK` match arm (the TextEntry editing fn in `keymap/` owns them).
const CTRL_HK_ARM: &str = r"DomainKey::Ctrl[HK]\b[^\n]*=>";

/// AC18 P1: hard-coded key hints, anywhere in `src/`.
const P1: &str = r"(?:Ctrl|Alt|Shift)\+[A-Za-z0-9?/\[\]]|\^X\b|\[(?:[A-Za-z?'/]|Esc|Enter|Tab|Space|PgUp|PgDn|PgUp/PgDn|Enter/Esc|j/k|y/n|n/Esc[^\]]*)\]";

/// AC18 P2: `Esc to close`-style prose, in the TUI adapter and the three runtime key files.
const P2: &str = r"\b(?:Esc|Enter|Tab|PgUp|PgDn)\s+(?:to\s+)?[a-z]+";

const P2_RUNTIME_FILES: &[&str] = &[
    "src/infrastructure/runtime/event_loop.rs",
    "src/infrastructure/runtime/attach_loop.rs",
    "src/infrastructure/runtime/attach_keys.rs",
];

const OWNER_3C: &str = "21-3 slice 3-C";
const OWNER_21_4: &str = "21-4";
const OWNER_PERMANENT: &str = "AC18 permanent";
const OWNERS: &[&str] = &[OWNER_3C, OWNER_21_4, OWNER_PERMANENT];

/// `(file, exact literal-hit-line count, owner, reason)`. 3-B2 burned down every 3-B row
/// (the footers and notices are generated from the registry); 3-C empties the card rows.
type Allow = (&'static str, usize, &'static str, &'static str);

#[rustfmt::skip]
const ALLOWLIST: &[Allow] = &[
    // permanent AC18 set: not TUI keys
    ("src/adapters/cli/doctor/checks.rs", 1, OWNER_PERMANENT, "doctor note about a tmux prefix conflict, not a keymap row"),
    ("src/adapters/cli/init.rs", 2, OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/cli/migrate.rs", 1, OWNER_PERMANENT, "migrate checklist marker `[x]`/`[ ]`, not a key"),
    ("src/adapters/cli/profile/import.rs", 1, OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/cli/profile/install.rs", 1, OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/cli/profile/prompt.rs", 1, OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/daemon/response_modes.rs", 1, OWNER_PERMANENT, "line-client RAP text, not a TUI key"),
    ("src/adapters/self_update/orchestrator.rs", 1, OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/tui/markdown/layout.rs", 1, OWNER_PERMANENT, "markdown task-list marker `[x]`, not a key"),
    ("src/domain/services/approval_presentation.rs", 1, OWNER_PERMANENT, "domain `MORE_MARKER` (TM-09 T5), truth-checked by the card tests"),
    ("src/domain/services/team_policy.rs", 1, OWNER_PERMANENT, "standing-trust doctrine text, truth-checked against the consent card"),
    // 21-4
    ("src/adapters/tui/widgets/login_dialog.rs", 12, OWNER_21_4, "login overlay literals"),
    // 3-C: cards, decision surfaces, catalogue usage rows, attach card arms
    // `Tab` here is a browser-style tab (P2 matches the word): read-only subagent tabs, tab saves
    ("src/adapters/tui/agent_inspector_keys.rs", 1, OWNER_PERMANENT, "read-only subagent tab warning, not a key"),
    ("src/infrastructure/runtime/event_loop.rs", 3, OWNER_3C, "two tab warnings (permanent, not keys) and the `[n]` deviation card string (3-C)"),
    ("src/adapters/cli/peer/rows.rs", 2, OWNER_3C, "peer add / team retract prompt rows"),
    ("src/adapters/command_registry.rs", 4, OWNER_3C, "catalogue usage rows kept verbatim (A11); 3-C generates their key text"),
    ("src/adapters/tui/decision_pane.rs", 15, OWNER_3C, "decision pane rails"),
    ("src/adapters/tui/handlers/peer_command.rs", 1, OWNER_3C, "peer add prompt text"),
    ("src/adapters/tui/handlers/team_command.rs", 2, OWNER_3C, "consent-card `[a]` text and `Ctrl+X, L` pointer"),
    ("src/adapters/tui/notification_log.rs", 8, OWNER_3C, "notification log footer"),
    ("src/adapters/tui/quit_confirm.rs", 3, OWNER_3C, "quit confirm rail"),
    ("src/adapters/tui/state.rs", 2, OWNER_3C, "recipient reply banners"),
    ("src/adapters/tui/widgets/cancel_plan_confirm_card.rs", 1, OWNER_3C, "cancel-plan confirm card"),
    ("src/adapters/tui/widgets/consolidation_card.rs", 2, OWNER_3C, "consolidation card (line 49 is a `[x]` checkbox marker, not a key)"),
    ("src/adapters/tui/widgets/delegation_card.rs", 1, OWNER_3C, "delegation card"),
    ("src/adapters/tui/widgets/forget_card.rs", 2, OWNER_3C, "forget card (line 49 is a `[x]` checkbox marker, not a key)"),
    ("src/adapters/tui/widgets/fork_confirm.rs", 1, OWNER_3C, "fork confirm card"),
    ("src/adapters/tui/widgets/permission_prompt.rs", 12, OWNER_3C, "permission prompt rails"),
    ("src/adapters/tui/widgets/plan_approval.rs", 4, OWNER_3C, "plan approval rail"),
    ("src/adapters/tui/widgets/plan_card.rs", 5, OWNER_3C, "plan card actions"),
    ("src/adapters/tui/widgets/plan_deviation_card.rs", 1, OWNER_3C, "plan deviation card"),
    ("src/adapters/tui/widgets/recipient_surface.rs", 22, OWNER_3C, "recipient surface actions"),
    ("src/adapters/tui/widgets/rewind_confirm.rs", 1, OWNER_3C, "rewind confirm card"),
    ("src/adapters/tui/widgets/skill_trust_prompt.rs", 4, OWNER_3C, "skill trust prompt"),
    ("src/adapters/tui/widgets/task_skip_cascade_card.rs", 1, OWNER_3C, "skip-cascade card"),
    ("src/adapters/tui/widgets/team_proposal_card.rs", 2, OWNER_3C, "team proposal card (`MORE` repeats the domain marker)"),
    ("src/infrastructure/runtime/attach_loop.rs", 5, OWNER_3C, "recipient tab labels and team-retract prompt"),
    ("src/infrastructure/runtime/recipient_view.rs", 1, OWNER_3C, "recipient view `[p]` hint"),
    ("src/infrastructure/runtime/transparency_bridge.rs", 6, OWNER_3C, "consent / retract card text"),
];

// ── scanner ───────────────────────────────────────────────────────────────────

/// A column-0 `#[cfg(test)]` item is blanked (attribute run, header, body up to the first
/// column-0 `}`); line numbers are preserved. A one-line item (`...;` or `...}`) ends at its header.
fn production_part(source: &str) -> String {
    enum St {
        Prod,
        Attrs,
        Body,
    }
    let mut st = St::Prod;
    let mut out: Vec<&str> = Vec::new();
    for line in source.split('\n') {
        let keep = match st {
            St::Prod => {
                if line.starts_with("#[cfg(test)]") {
                    st = St::Attrs;
                    false
                } else {
                    true
                }
            }
            St::Attrs => {
                if !(line.starts_with("#[") || line.starts_with("//")) {
                    let header = line.trim_end();
                    st = if header.ends_with(';') || header.ends_with('}') {
                        St::Prod
                    } else if header.ends_with('{') || header.ends_with('(') {
                        St::Body
                    } else {
                        panic!("unrecognised `#[cfg(test)]` item header: {header:?}");
                    };
                }
                false
            }
            St::Body => {
                if line.starts_with('}') {
                    st = St::Prod;
                }
                false
            }
        };
        out.push(if keep { line } else { "" });
    }
    out.join("\n")
}

/// The text of the string literals on each physical line (1-based). Comments, char literals,
/// lifetimes and all other tokens are skipped; escapes other than `\"`, `\\`, `\'` become a space;
/// a `\`-newline continuation joins nothing; raw strings (`r#"…"#`) are literal text.
fn literal_lines(source: &str) -> BTreeMap<usize, String> {
    let b: Vec<char> = source.chars().collect();
    let n = b.len();
    let mut out: BTreeMap<usize, String> = BTreeMap::new();
    let mut line = 1usize;
    let mut i = 0usize;
    while i < n {
        let c = b[i];
        let next = b.get(i + 1).copied().unwrap_or('\0');
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c == '/' && next == '/' {
            while i < n && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && next == '*' {
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if b[i] == '/' && b.get(i + 1) == Some(&'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == '*' && b.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    if b[i] == '\n' {
                        line += 1;
                    }
                    i += 1;
                }
            }
        } else if c == '"' {
            i += 1;
            while i < n && b[i] != '"' {
                if b[i] == '\\' && i + 1 < n {
                    let escaped = b[i + 1];
                    if escaped == '\n' {
                        line += 1;
                    } else if matches!(escaped, '"' | '\\' | '\'') {
                        out.entry(line).or_default().push(escaped);
                    } else {
                        out.entry(line).or_default().push(' ');
                    }
                    i += 2;
                } else {
                    if b[i] == '\n' {
                        line += 1;
                    } else {
                        out.entry(line).or_default().push(b[i]);
                    }
                    i += 1;
                }
            }
            i += 1;
        } else if c == 'r' && (next == '#' || next == '"') {
            let mut j = i + 1;
            let mut hashes = 0usize;
            while j < n && b[j] == '#' {
                hashes += 1;
                j += 1;
            }
            if j < n && b[j] == '"' {
                i = j + 1;
                while i < n {
                    if b[i] == '"' && (0..hashes).all(|k| b.get(i + 1 + k) == Some(&'#')) {
                        i += 1 + hashes;
                        break;
                    }
                    if b[i] == '\n' {
                        line += 1;
                    } else {
                        out.entry(line).or_default().push(b[i]);
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
        } else if c == '\'' {
            if next == '\\' {
                // `'\n'`, `'\''`, `'\u{..}'`: skip the quote, the backslash and the escaped char
                i += 3;
                while i < n && b[i] != '\'' {
                    i += 1;
                }
                i += 1;
            } else if b.get(i + 2) == Some(&'\'') {
                i += 3; // `'x'`
            } else {
                i += 1; // a lifetime
            }
        } else {
            i += 1;
        }
    }
    out
}

fn p2_applies(path: &str) -> bool {
    path.starts_with("src/adapters/tui/") || P2_RUNTIME_FILES.contains(&path)
}

/// `(line, literal text)` of every P1/P2 hit in the production part of `path`.
fn literal_hits(path: &str, source: &str) -> Vec<(usize, String)> {
    static P1_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(P1).unwrap());
    static P2_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(P2).unwrap());
    let (p1, p2) = (&*P1_RE, &*P2_RE);
    let use_p2 = p2_applies(path);
    literal_lines(&production_part(source))
        .into_iter()
        .filter(|(_, text)| p1.is_match(text) || (use_p2 && p2.is_match(text)))
        .collect()
}

/// Literal-gate violations over `files` (`src/…` path, source) against `allow`.
fn literal_violations(files: &[(String, String)], allow: &[Allow]) -> Vec<String> {
    let mut violations = Vec::new();
    let mut hits_by_file: BTreeMap<&str, Vec<(usize, String)>> = BTreeMap::new();
    for (path, source) in files {
        if path.starts_with(KEYMAP_DIR) {
            continue;
        }
        let hits = literal_hits(path, source);
        if !hits.is_empty() {
            hits_by_file.insert(path.as_str(), hits);
        }
    }
    for (path, hits) in &hits_by_file {
        if allow.iter().any(|(file, ..)| file == path) {
            continue;
        }
        for (line, text) in hits {
            violations.push(format!("{path}:{line}: hard-coded key hint {text:?}"));
        }
    }
    for (file, count, owner, _reason) in allow {
        if !files.iter().any(|(path, _)| path.as_str() == *file) {
            violations.push(format!(
                "stale allowlist entry {file} ({owner}): the file no longer exists"
            ));
            continue;
        }
        let found = hits_by_file.get(file).map(Vec::as_slice).unwrap_or(&[]);
        if found.len() != *count {
            let lines: Vec<String> = found
                .iter()
                .map(|(l, t)| format!("{file}:{l}: {t:?}"))
                .collect();
            violations.push(format!(
                "stale allowlist entry {file} ({owner}): expected {count} hit(s), found {}: {lines:?}",
                found.len()
            ));
        }
    }
    violations
}

/// PC-3 violations over `files`: a banned symbol anywhere in a production part, or a
/// `DomainKey::CtrlH`/`CtrlK` match arm outside `src/adapters/tui/keymap/`.
fn symbol_violations(files: &[(String, String)]) -> Vec<String> {
    let symbols: Vec<(&str, Regex)> = PC3_SYMBOLS
        .iter()
        .map(|s| (*s, Regex::new(&format!(r"\b{s}\b")).unwrap()))
        .collect();
    let arm = Regex::new(CTRL_HK_ARM).unwrap();
    let mut violations = Vec::new();
    for (path, source) in files {
        let production = production_part(source);
        let line_of = |at: usize| production[..at].matches('\n').count() + 1;
        for (name, re) in &symbols {
            for m in re.find_iter(&production) {
                violations.push(format!(
                    "{path}:{}: PC-3 symbol `{name}`",
                    line_of(m.start())
                ));
            }
        }
        if !path.starts_with(KEYMAP_DIR) {
            for m in arm.find_iter(&production) {
                violations.push(format!(
                    "{path}:{}: `DomainKey::CtrlH`/`CtrlK` match arm outside the keymap",
                    line_of(m.start())
                ));
            }
        }
    }
    violations
}

// ── the real tree ─────────────────────────────────────────────────────────────

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Test-only by name: `*_tests.rs` and `tests.rs`. The skipped files are pinned below.
fn test_only_name(path: &Path) -> bool {
    path.file_name().is_some_and(|n| {
        let n = n.to_string_lossy();
        n.ends_with("_tests.rs") || n == "tests.rs"
    })
}

/// The only files the scan skips (test-only by name): a new one must be named here.
const SKIPPED_TEST_FILES: &[&str] = &[
    "src/adapters/auth/flows/api_key/tests.rs",
    "src/adapters/daemon/approval_lifecycle_tests.rs",
    "src/adapters/daemon/server/turn_interrupt/tests.rs",
    "src/adapters/responses/tests.rs",
    "src/adapters/tui/auth_outcome_tests.rs",
    "src/adapters/tui/story21_2_slash_tests.rs",
    "src/infrastructure/runtime/state_extraction_tests.rs",
];

/// `(files scanned as ("src/…", source), the skipped test-only paths)`.
fn production_sources() -> (Vec<(String, String)>, Vec<String>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = Vec::new();
    rust_files(&root.join("src"), &mut paths);
    paths.sort();
    let relative = |p: &Path| {
        p.strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    let skipped = paths
        .iter()
        .filter(|p| test_only_name(p))
        .map(|p| relative(p))
        .collect();
    let files = paths
        .iter()
        .filter(|p| !test_only_name(p))
        .map(|p| (relative(p), fs::read_to_string(p).unwrap()))
        .collect();
    (files, skipped)
}

fn assert_whole_tree_scanned(files: &[(String, String)], skipped: &[String]) {
    assert_eq!(
        skipped, SKIPPED_TEST_FILES,
        "only the named test-only files may be skipped"
    );
    assert!(files.len() > 500, "{} files scanned", files.len());
    for must in [
        "src/infrastructure/runtime/event_loop.rs",
        "src/infrastructure/runtime/attach_loop.rs",
        "src/infrastructure/runtime/attach_keys.rs",
        "src/adapters/tui/app.rs",
        "src/adapters/tui/keymap/table.rs",
    ] {
        assert_eq!(
            files.iter().filter(|(p, _)| p == must).count(),
            1,
            "{must} must be scanned exactly once"
        );
    }
}

#[test]
fn k12_pc3_symbols_are_absent_from_the_production_code_of_both_hosts() {
    let (files, skipped) = production_sources();
    assert_whole_tree_scanned(&files, &skipped);
    let violations = symbol_violations(&files);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn k12_no_hard_coded_key_hint_literal_outside_the_registry_except_the_owned_allowlist() {
    let (files, skipped) = production_sources();
    assert_whole_tree_scanned(&files, &skipped);
    let violations = literal_violations(&files, ALLOWLIST);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn k12_allowlist_is_well_formed_and_every_entry_names_an_owner() {
    let mut seen = std::collections::BTreeSet::new();
    for (file, count, owner, reason) in ALLOWLIST {
        assert!(seen.insert(*file), "{file} is listed twice");
        assert!(file.starts_with("src/") && file.ends_with(".rs"), "{file}");
        assert!(
            !file.starts_with(KEYMAP_DIR),
            "{file}: the keymap dir is not scanned"
        );
        assert!(
            *count > 0,
            "{file}: a zero-count entry is stale by definition"
        );
        assert!(OWNERS.contains(owner), "{file}: unknown owner {owner:?}");
        assert!(!reason.is_empty(), "{file}: a reason is required");
    }
}

// ── positive controls (synthetic in-memory sources through the same scanners) ──

fn src(path: &str, source: &str) -> (String, String) {
    (path.to_owned(), source.to_owned())
}

/// Control 1 — the symbol scanner: every PC-3 symbol is found whole-word (also inside
/// the keymap dir), never in a test part; a `CtrlH`/`CtrlK` match arm is banned outside
/// the keymap only.
#[test]
fn k12_positive_control_symbol_scanner() {
    for symbol in PC3_SYMBOLS {
        let found = symbol_violations(&[src(
            "src/new/file.rs",
            &format!("fn f() {{ let _ = {symbol}; }}"),
        )]);
        assert!(
            found.len() == 1
                && found[0].contains(symbol)
                && found[0].starts_with("src/new/file.rs:1:"),
            "`{symbol}` must be found: {found:?}"
        );
        for ok in [
            format!("fn f() {{ let _ = pre_{symbol}_post; }}"),
            format!(
                "fn ok() {{}}\n#[cfg(test)]\nmod tests {{\n    fn t() {{ let _ = {symbol}; }}\n}}\n"
            ),
        ] {
            assert!(
                symbol_violations(&[src("src/new/file.rs", &ok)]).is_empty(),
                "{ok}"
            );
        }
    }
    let arm = "fn g(k: DomainKey) { match k { DomainKey::CtrlK => kill(), _ => {} } }";
    let both = "match k { DomainKey::CtrlH | DomainKey::CtrlK => x(), _ => {} }";
    assert_eq!(
        symbol_violations(&[src("src/adapters/tui/state.rs", arm)]).len(),
        1
    );
    // No file is exempt any more: app.rs resolves its editing keys through the registry.
    assert_eq!(
        symbol_violations(&[src("src/adapters/tui/app.rs", arm)]).len(),
        1
    );
    assert_eq!(
        symbol_violations(&[src("src/infrastructure/runtime/attach_keys.rs", both)]).len(),
        1
    );
    assert!(symbol_violations(&[src("src/adapters/tui/keymap/model.rs", arm)]).is_empty());
    for ok in [
        "let k = DomainKey::CtrlH;",
        "match k { DomainKey::CtrlHome => a(), _ => {} }",
        "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { match k { DomainKey::CtrlH => a(), _ => {} } }\n}\n",
    ] {
        assert!(
            symbol_violations(&[src("src/adapters/tui/state.rs", ok)]).is_empty(),
            "{ok}"
        );
    }
}

/// Control 2 — the literal scanner: test items are blanked (later production kept, line
/// numbers survive, an unrecognised header is refused), only literals hit (right line),
/// and the allowlist fails a new file, a stale count, a clean file and a vanished file.
#[test]
fn k12_positive_control_literal_scanner_production_part_and_allowlist() {
    let source = "fn prod() { let _ = \"Ctrl+Q\"; }\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = \"Ctrl+T\"; let _ = chord_map; }\n}\n#[cfg(test)]\nuse std::fmt;\nfn after() { let _ = \"Alt+Z\"; }\n#[cfg(test)]\npub fn gated(\n    a: u8,\n) {\n    s(\"Ctrl+G\");\n}\nfn last() {}\n";
    let production = production_part(source);
    assert!(production.contains("Ctrl+Q") && production.contains("Alt+Z"));
    assert!(
        !production.contains("Ctrl+T")
            && !production.contains("Ctrl+G")
            && !production.contains("chord_map")
    );
    assert_eq!(production.lines().count(), source.lines().count());
    assert_eq!(
        literal_hits("src/x.rs", source)
            .iter()
            .map(|(l, _)| *l)
            .collect::<Vec<_>>(),
        vec![1, 8]
    );
    assert!(symbol_violations(&[src("src/x.rs", source)]).is_empty());
    // A header the scanner cannot place ends neither in `{`, `(`, `;` nor `}`: refused.
    assert!(
        std::panic::catch_unwind(|| production_part("#[cfg(test)]\nfn f() -> u8\nwhere T: X\n"))
            .is_err()
    );

    let p1 = "fn a() {}\nfn b() { s(\"Press Ctrl+X, T\"); }\nfn c() { s(\"[y/n]\"); }\nfn d() { s(\"^X menu\"); }\nfn e() { s(\"a \\\n   [Esc] b\"); }\nfn f() { s(r#\"[j/k] move\"#); }\n";
    let lines: Vec<usize> = literal_hits("src/domain/x.rs", p1)
        .into_iter()
        .map(|(l, _)| l)
        .collect();
    assert_eq!(
        lines,
        vec![2, 3, 4, 6, 7],
        "the continuation line keeps its own number"
    );
    let not_literals = "// Ctrl+X in a comment\n/// Press [Esc] (doc)\n/* Shift+Tab\n [y/n] */\nfn f<'a>(v: &'a [u8], i: usize) -> u8 { let _c = '['; let _q = '\"'; v[i] }\n";
    assert!(literal_hits("src/domain/x.rs", not_literals).is_empty());
    let quote = "fn f() { let _q = '\"'; let _e = '\\''; s(\"Alt+L\"); }";
    assert_eq!(
        literal_hits("src/domain/x.rs", quote).len(),
        1,
        "a quote char literal keeps the scanner in sync"
    );
    let p2 = "fn f() { s(\"Esc to close\"); }";
    assert_eq!(literal_hits("src/adapters/tui/widgets/new.rs", p2).len(), 1);
    assert!(literal_hits("src/domain/models/x.rs", p2).is_empty());

    let hit = "fn f() { s(\"Press Ctrl+B\"); }";
    let two = "fn f() { s(\"Press Ctrl+B\"); }\nfn g() { s(\"Alt+Q\"); }";
    let allow: &[Allow] = &[
        ("src/a.rs", 1, OWNER_PERMANENT, "listed, exact"),
        ("src/b.rs", 1, OWNER_3C, "listed, but the count changed"),
        ("src/c.rs", 1, OWNER_3C, "listed, but now clean"),
        ("src/gone.rs", 1, OWNER_21_4, "listed, file vanished"),
    ];
    let files = [
        src("src/a.rs", hit),
        src("src/b.rs", two),
        src("src/c.rs", "fn f() { s(\"nothing here\"); }"),
        src("src/new.rs", hit),
        src(
            "src/adapters/tui/keymap/table.rs",
            "const L: &str = \"Ctrl+X\";",
        ),
        src(
            "src/t.rs",
            "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn t() { s(\"Ctrl+Z\"); }\n}\n",
        ),
    ];
    let violations = literal_violations(&files, allow);
    let text = violations.join("\n");
    assert!(text.contains("src/new.rs:1: hard-coded key hint"), "{text}");
    assert!(
        text.contains("stale allowlist entry src/b.rs")
            && text.contains("expected 1 hit(s), found 2"),
        "{text}"
    );
    assert!(
        text.contains("stale allowlist entry src/c.rs") && text.contains("found 0"),
        "{text}"
    );
    assert!(
        text.contains("stale allowlist entry src/gone.rs") && text.contains("no longer exists"),
        "{text}"
    );
    assert!(
        !text.contains("src/a.rs:")
            && !text.contains("src/adapters/tui/keymap")
            && !text.contains("src/t.rs"),
        "{text}"
    );
    assert_eq!(violations.len(), 4, "{text}");
}
