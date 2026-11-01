//! K6 — generated docs, the portable-row fixture and the docs truth lint (AC7).
//! The committed `docs/keyboard.md` and `tests_tui/fixtures/keymap_portable_rows.json`
//! must equal the renderer output (regenerate with the printed command); every
//! key token in README.md and docs/*.md must resolve to a registry row.

use std::path::{Path, PathBuf};

use rustain::adapters::tui::keymap::govern::{RowDefect, validate_rows};
use rustain::adapters::tui::keymap::render_docs::{
    markdown_reference, portable_rows_json, unresolved_key_tokens,
};
use rustain::adapters::tui::keymap::table::GlobalCmd;
use rustain::adapters::tui::keymap::{CommandId, Context, HelpCat, Key, bind, table};

const REGEN: &str =
    "RUSTAIN_WRITE_KEYMAP_DOC=1 cargo test --test tui conformance_21_3_keymap::k6_docs";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Compare (or, with the env var set, rewrite) one generated artifact.
fn check_generated(relative: &str, expected: &str) {
    let path = root().join(relative);
    if std::env::var_os("RUSTAIN_WRITE_KEYMAP_DOC").is_some() {
        std::fs::write(&path, expected).expect("write the generated artifact");
        // A regeneration run compares nothing: it must never pass as the gate.
        panic!("{relative} regenerated; re-run without RUSTAIN_WRITE_KEYMAP_DOC");
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{relative} is missing ({e}); regenerate with: {REGEN}"));
    assert!(
        committed == expected,
        "{relative} differs from the registry output; regenerate with: {REGEN}"
    );
}

#[test]
fn keyboard_reference_equals_the_registry_output() {
    check_generated("docs/keyboard.md", &markdown_reference());
}

#[test]
fn portable_rows_fixture_equals_the_registry_output() {
    check_generated(
        "tests_tui/fixtures/keymap_portable_rows.json",
        &portable_rows_json(),
    );
}

fn scoped_docs() -> Vec<PathBuf> {
    let mut files = vec![root().join("README.md")];
    let mut docs: Vec<PathBuf> = std::fs::read_dir(root().join("docs"))
        .expect("docs dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter(|path| {
            !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ARCHITECTURE_"))
        })
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

#[test]
fn every_key_token_in_the_docs_resolves_to_a_registry_row() {
    let files = scoped_docs();
    assert!(
        files.len() > 5,
        "the lint must scan README and docs/*.md: {files:?}"
    );
    let mut failures = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        for token in unresolved_key_tokens(&text) {
            failures.push(format!("{}: `{token}`", relative(file)));
        }
    }
    assert!(
        failures.is_empty(),
        "docs name keys the registry does not know (rewrite the sentence; a retired key gets no exemption):\n{}",
        failures.join("\n")
    );
}

fn relative(path: &Path) -> String {
    path.strip_prefix(root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[test]
fn the_lint_has_a_positive_control_independent_of_the_scanned_files() {
    // An in-memory doc naming a retired binding fails the same scanner...
    assert_eq!(
        unresolved_key_tokens("Ctrl+J toggles history"),
        vec!["Ctrl+J".to_string()]
    );
    assert_eq!(
        unresolved_key_tokens("`Ctrl+Y` is unbound"),
        vec!["Ctrl+Y".to_string()],
        "an unbound key sentence is not exempt until it is rewritten"
    );
    // ...and a true sentence passes.
    assert!(
        unresolved_key_tokens("Press Ctrl+X, H for History, or Ctrl+P for commands.").is_empty()
    );
}

/// Lines that name `Esc` together with declining or cancelling without saying it never does.
fn stale_card_doc_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains("Esc"))
        .filter(|line| line.contains("declin") || line.contains("cancel"))
        .filter(|line| !line.contains("never"))
        .collect()
}

/// R6: the user docs of the cards say `Esc` never decides one.
#[test]
fn the_card_docs_never_say_esc_declines_or_cancels() {
    for file in ["docs/skills.md", "docs/profiles.md", "docs/a2a.md"] {
        let text = std::fs::read_to_string(root().join(file)).expect("a scoped doc");
        assert!(stale_card_doc_lines(&text).is_empty(), "{file}");
    }
    // Positive control: the retired wording fails the same scanner.
    assert_eq!(
        stale_card_doc_lines("`n` or `Esc` declines it\nfine").len(),
        1
    );
    assert!(stale_card_doc_lines("`Esc` never declines it").is_empty());
}

/// KG-1: the "Does" cell is plain words, never a Rust enum name (`Chat(OpenPalette)`).
#[test]
fn no_does_cell_is_a_debug_name() {
    let debug_name = regex::Regex::new(r"^\w+\(\w").expect("regex");
    let doc = markdown_reference();
    let mut cells = 0;
    for line in doc.lines().filter(|l| l.starts_with("| `")) {
        let does = line.split(" | ").nth(1).expect("a Does cell");
        assert!(!debug_name.is_match(does), "debug name in the docs: {line}");
        assert!(!does.is_empty(), "empty Does cell: {line}");
        cells += 1;
    }
    assert!(
        cells > 100,
        "the lint saw the whole reference ({cells} rows)"
    );
    // Positive control: the scanner does flag a leaked enum name.
    assert!(debug_name.is_match("Chat(OpenPalette)") && debug_name.is_match("Leader(Cancel)"));
}

/// A row with neither `does`, help nor label is rejected, with each reason
/// reported on its own; any one of the three descriptions makes it legal.
#[test]
fn a_row_without_a_description_is_rejected() {
    let command = CommandId::Global(GlobalCmd::QuitRequest);
    let keys: &'static [Key] = &[Key::Char('a')];
    let bare = bind(Context::Chat, keys, command, "");
    assert_eq!(
        validate_rows(&[bare]),
        Err(vec![RowDefect::NoDescription {
            context: Context::Chat,
            keys: keys.to_vec()
        }])
    );
    for described in [
        bind(Context::Chat, keys, command, "label"),
        bind(Context::Chat, keys, command, "").does("plain words"),
        bind(Context::Chat, keys, command, "").help(HelpCat::General, "a", "help text"),
    ] {
        assert_eq!(validate_rows(&[described]), Ok(()));
    }
    validate_rows(table()).expect("every shipped row says what it does");
}
