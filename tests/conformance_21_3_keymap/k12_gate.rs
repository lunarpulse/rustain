//! K12 — the RU-31 cutover gate over the production code of BOTH hosts (AC18, AC9; complete at 3-C).
//!
//! A scoped static ratchet, not behavioural proof (K1-K4 drive the real keymap):
//!
//! * the PC-3 symbols (`CHORD_LABELS`, `help_data`, ...) and a `DomainKey::CtrlH`/`CtrlK` match
//!   arm outside `src/adapters/tui/keymap/` are ABSENT from every production part under `src/`;
//! * no string literal in a production part outside `src/adapters/tui/keymap/` matches the
//!   hard-coded key-hint grammar P1/P2 of AC18 unless its file is on the allowlist with an exact
//!   hit count and a named owner (21-4 or the permanent AC18 set).
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
//! `CARGO_MANIFEST_DIR` and nothing outside `rustain/`. Known limit (DF-21-3-K12-ONE-LITERAL-PER-LINE):
//! a hit is one physical line, so several literals on one line count once, and a pin matches by
//! `starts_with`: a second literal on a pinned line is not seen.

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

const OWNER_21_4: &str = "21-4";
const OWNER_PERMANENT: &str = "AC18 permanent";
const OWNERS: &[&str] = &[OWNER_21_4, OWNER_PERMANENT];

/// `(file, literal prefixes, owner, reason)`: every hit of the file must start with one of
/// the pinned prefixes (one pin per hit) and every pin must still match a hit, so a new
/// literal in an allowlisted file fails and a pin never outlives its literal
/// (DF-21-3-K12-LITERAL-PINNING). 3-C leaves no card row: every card rail is generated.
type Allow = (
    &'static str,
    &'static [&'static str],
    &'static str,
    &'static str,
);

#[rustfmt::skip]
const ALLOWLIST: &[Allow] = &[
    // permanent AC18 set: not TUI keys
    ("src/adapters/cli/doctor/checks.rs", &["tmux detected: Ctrl+B prefix"], OWNER_PERMANENT, "doctor note about a tmux prefix conflict, not a keymap row"),
    ("src/adapters/cli/init.rs", &["Sign in to a model provider now? [y/n] ", "{question} [y/n] "], OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompts"),
    ("src/adapters/cli/migrate.rs", &["[x][ ]"], OWNER_PERMANENT, "migrate checklist marker `[x]`/`[ ]`, not a key"),
    ("src/adapters/cli/profile/import.rs", &["Profile '{}' already exists at {}. Overwrite? [y/n] "], OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/cli/profile/install.rs", &["Profile '{}' already exists at {}. Overwrite? [y/n]: "], OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/cli/profile/prompt.rs", &["{} [y/n] "], OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/daemon/response_modes.rs", &["[y] Approve  [e] Edit  [n] Reject  [write] My own "], OWNER_PERMANENT, "line-client RAP text, not a TUI key"),
    ("src/adapters/self_update/orchestrator.rs", &["Update? [y/n] "], OWNER_PERMANENT, "non-TUI CLI `[y/n]` prompt"),
    ("src/adapters/tui/markdown/layout.rs", &["[x] "], OWNER_PERMANENT, "markdown task-list marker `[x]`, not a key"),
    ("src/domain/services/approval_presentation.rs", &["… more not shown — [r] Review first"], OWNER_PERMANENT, "domain `MORE_MARKER` (TM-09 T5), truth-checked by the card tests against the `r` row"),
    ("src/domain/services/team_policy.rs", &["Standing trust is granted from a sender's consent card ([a] Always allow)"], OWNER_PERMANENT, "standing-trust doctrine text, truth-checked against the consent card"),
    // `Tab` here is a browser-style tab (P2 matches the word), not a key
    ("src/adapters/tui/agent_inspector_keys.rs", &["Tab is read-only (subagent output view)"], OWNER_PERMANENT, "read-only subagent tab warning, not a key"),
    ("src/infrastructure/runtime/event_loop.rs", &["Tab is read-only (subagent output view)", "Tab save timed out"], OWNER_PERMANENT, "two tab warnings, not keys"),
    ("src/adapters/cli/peer/rows.rs", &["         Awaiting your decision.  [y] Confirm and pin", "         Awaiting your decision.  [y] Update the address"], OWNER_PERMANENT, "shared CLI/TUI peer-add card copy: `cli` may not import `tui` (adapter ratchet); the keys are truth-checked against the Confirm rows by K11"),
    // `[x]` / `[ ]` are checkbox markers, not keys
    ("src/adapters/tui/widgets/consolidation_card.rs", &["  [x]   [ ] "], OWNER_PERMANENT, "checkbox markers, not keys"),
    ("src/adapters/tui/widgets/forget_card.rs", &["  [x]   [ ] "], OWNER_PERMANENT, "checkbox markers, not keys"),
    // 21-4: the login overlay keeps `handlers::login::on_key` (security boundary)
    ("src/adapters/tui/widgets/login_dialog.rs", &[
        "Esc back", "Esc close", "↑↓ navigate · Enter select · Esc back", "↑↓ navigate · Enter select · Esc close",
        "Continue in your browser. If it didn't open, press Alt+C", "Enter this code on the sign-in page",
        "c copy link · l copy local link · Esc cancel", "c copy code · u copy link · Esc cancel",
        "Enter submit · Alt+C copy link · Alt+L copy local link · Esc cancel", "Enter submit · Esc cancel",
        "Enter submit · Alt+C copy code · Alt+U copy link · Esc cancel", "Esc cancel",
    ], OWNER_21_4, "login overlay literals"),
    // 21-4: a writer's action keys depend on the received item (NAMED EXCLUSION, K9 R5)
    ("src/adapters/tui/widgets/recipient_surface.rs", &[
        "[r] Respond", "[d] Discuss", "[b] Busy", "[a] Acknowledge", "[y] Approve", "[e] Edit", "[n] Reject",
        "[n] Decline", "[s] Share status", "[n] Decline", "[c] Use as context", "[d] Discuss concerns",
        "[y] Accept", "[s] Share", "[n] Decline", "[c] Stop using as context", "[s] Share with redactions",
    ], OWNER_21_4, "per-item writer action keys of the recipient surface"),
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

/// `(line, trimmed source line)` of every hand-written `Ctrl+W/U/H` handling in the production
/// part of `path`: a `Key::Ctrl('w'|'u'|'h')` pattern or a `DomainKey::CtrlW|CtrlU|CtrlH` path.
fn field_edit_hits(_path: &str, source: &str) -> Vec<(usize, String)> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(CTRL_WUH).unwrap());
    production_part(source)
        .lines()
        .enumerate()
        .filter(|(_, line)| RE.is_match(line))
        .map(|(index, line)| (index + 1, line.trim().to_owned()))
        .collect()
}

/// Pinned violations of `hit_fn`'s hits over `files` against `allow` (see [`Allow`]).
fn pinned_violations(
    files: &[(String, String)],
    allow: &[Allow],
    hit_fn: fn(&str, &str) -> Vec<(usize, String)>,
    what: &str,
) -> Vec<String> {
    let mut violations = Vec::new();
    let mut hits_by_file: BTreeMap<&str, Vec<(usize, String)>> = BTreeMap::new();
    for (path, source) in files {
        if path.starts_with(KEYMAP_DIR) {
            continue;
        }
        let hits = hit_fn(path, source);
        if !hits.is_empty() {
            hits_by_file.insert(path.as_str(), hits);
        }
    }
    for (path, hits) in &hits_by_file {
        if !allow.iter().any(|(file, ..)| file == path) {
            for (line, text) in hits {
                violations.push(format!("{path}:{line}: {what} {text:?}"));
            }
        }
    }
    for (file, pins, owner, _reason) in allow {
        if !files.iter().any(|(path, _)| path.as_str() == *file) {
            violations.push(format!(
                "stale allowlist entry {file} ({owner}): the file no longer exists"
            ));
            continue;
        }
        let mut unused: Vec<&str> = pins.to_vec();
        for (line, text) in hits_by_file.get(file).map(Vec::as_slice).unwrap_or(&[]) {
            match unused.iter().position(|pin| text.starts_with(pin)) {
                Some(index) => {
                    unused.remove(index);
                }
                None => violations.push(format!(
                    "{file}:{line}: unpinned {what} {text:?} in an allowlisted file ({owner})"
                )),
            }
        }
        for pin in unused {
            violations.push(format!(
                "stale allowlist entry {file} ({owner}): pin {pin:?} matches no hit"
            ));
        }
    }
    violations
}

/// Literal-gate violations over `files` (`src/…` path, source) against `allow`.
fn literal_violations(files: &[(String, String)], allow: &[Allow]) -> Vec<String> {
    pinned_violations(files, allow, literal_hits, "hard-coded key hint")
}

/// Hand-written `Ctrl+W/U/H` handling in a production part outside the keymap: the editing
/// keys of a text field are `FieldEdit` rows (DF-21-3-K12-LITERAL-PINNING). Pinned by line.
const CTRL_WUH: &str = r"Key::Ctrl\('[whu]'|DomainKey::Ctrl[WUH]\b";

#[rustfmt::skip]
const FIELD_EDIT_ALLOWLIST: &[Allow] = &[
    ("src/adapters/tui/app.rs", &[
        "FieldCmd::DeleteWordBack => DomainKey::CtrlW,", "FieldCmd::Clear => DomainKey::CtrlU,", "FieldCmd::DeleteBack => DomainKey::CtrlH,",
        "DomainKey::CtrlW => text_edit::kill_word_back", "DomainKey::CtrlU => text_edit::kill_to_line_start",
        "return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlH));", "return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlU));", "return Some(DomainInputEvent::SpecialKey(DomainKey::CtrlW));",
    ], OWNER_PERMANENT, "the card text fields (feedback, answer: FR-21-3-b, deleted with 21-6), the FieldEdit executor and the decoder"),
];

/// The same line scanner as the literal gate, over the editing keys.
fn field_edit_violations(files: &[(String, String)], allow: &[Allow]) -> Vec<String> {
    pinned_violations(
        files,
        allow,
        field_edit_hits,
        "hand-written Ctrl+W/U/H handling",
    )
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
    "src/adapters/tui/decision_queue/cards_tests.rs",
    "src/adapters/tui/story21_2_slash_tests.rs",
    "src/infrastructure/runtime/attach_loop/inspect_hint_tests.rs",
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
    for (file, pins, owner, reason) in ALLOWLIST {
        assert!(seen.insert(*file), "{file} is listed twice");
        assert!(file.starts_with("src/") && file.ends_with(".rs"), "{file}");
        assert!(
            !file.starts_with(KEYMAP_DIR),
            "{file}: the keymap dir is not scanned"
        );
        assert!(!pins.is_empty(), "{file}: an entry without a pin is stale");
        assert!(pins.iter().all(|pin| !pin.is_empty()), "{file}: empty pin");
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
        (
            "src/a.rs",
            &["Press Ctrl+B"],
            OWNER_PERMANENT,
            "listed, exact",
        ),
        (
            "src/b.rs",
            &["Press Ctrl+B"],
            OWNER_21_4,
            "listed, but a second literal appeared",
        ),
        (
            "src/c.rs",
            &["Press Ctrl+B"],
            OWNER_21_4,
            "listed, but now clean",
        ),
        ("src/gone.rs", &["x"], OWNER_21_4, "listed, file vanished"),
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
        text.contains("src/b.rs:2: unpinned hard-coded key hint"),
        "{text}"
    );
    assert!(
        text.contains("stale allowlist entry src/c.rs") && text.contains("matches no hit"),
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

#[test]
fn k12_ctrl_w_u_h_handling_is_pinned_to_the_card_fields_and_the_editor() {
    let (files, skipped) = production_sources();
    assert_whole_tree_scanned(&files, &skipped);
    let violations = field_edit_violations(&files, FIELD_EDIT_ALLOWLIST);
    assert!(violations.is_empty(), "{}", violations.join("\n"));
    // Positive control through the same scanner: a new hand-written arm and a stale pin fail.
    let allow: &[Allow] = &[(
        "src/a.rs",
        &["let k = Key::Ctrl('w');", "gone"],
        OWNER_PERMANENT,
        "pinned",
    )];
    let files = [
        src(
            "src/a.rs",
            "fn f() { let k = Key::Ctrl('w'); }\nfn g() {\n    let k = Key::Ctrl('w');\n}\n",
        ),
        src(
            "src/b.rs",
            "fn f() { match k { DomainKey::CtrlH => {} } }\n#[cfg(test)]\nmod t { fn x() { Key::Ctrl('u'); } }\n",
        ),
    ];
    let text = field_edit_violations(&files, allow).join("\n");
    assert!(
        text.contains("src/b.rs:1: hand-written Ctrl+W/U/H handling"),
        "{text}"
    );
    assert!(text.contains("src/a.rs:1: unpinned"), "{text}");
    assert!(
        text.contains("stale allowlist entry src/a.rs") && text.contains("\"gone\""),
        "{text}"
    );
    assert!(
        !text.contains("src/b.rs:3") && !text.contains("src/a.rs:3"),
        "{text}"
    );
}
