//! K5: copy is the block's exact source (D3), never painted rows, borders or markers.
use std::cell::RefCell;

use rustain::adapters::tui::{
    app::InputAction, block_viewer, clipboard::ClipboardResult, handlers::clipboard, keymap::Key,
    notifications::NoticeSource,
};

use super::support::{Fixture, focus, mixed_turn, open, single};

/// Press `key` and run the production copy effect with an injected copier.
fn copy(f: &mut Fixture, key: Key) -> Vec<String> {
    let action = f.press(key);
    let copied = RefCell::new(Vec::new());
    let copier = |text: &str| {
        copied.borrow_mut().push(text.to_owned());
        ClipboardResult::Osc52Success
    };
    let tab = f.tabs.active_tab();
    match action {
        InputAction::CopyBlockSource => {
            assert!(block_viewer::copy_source(
                &mut f.h.state,
                &tab.conversation,
                tab.reducer.open_turn.as_ref(),
                &copier,
            ));
        }
        InputAction::CopyToClipboard(content) => clipboard::requested_in(
            &mut f.h.state,
            &tab.conversation,
            tab.reducer.open_turn.as_ref(),
            content,
            &copier,
        ),
        other => panic!("{key:?} must request a copy, got {other:?}"),
    }
    copied.into_inner()
}

fn toast(f: &Fixture) -> String {
    f.h.state
        .notifications
        .visible()
        .filter(|notice| notice.source == NoticeSource::Clipboard)
        .last()
        .map(|notice| notice.message.clone())
        .unwrap_or_default()
}

#[test]
fn c_copies_the_block_source_byte_exact() {
    let (mut f, id) = single(80, 24, "```rust\n\tlet a = 1;  \n  b\n```\n");
    open(&mut f, &id);
    let copied = copy(&mut f, Key::Char('c'));
    assert_eq!(
        copied,
        ["\tlet a = 1;  \n  b"],
        "tabs and trailing spaces kept, no box"
    );
    assert_eq!(toast(&f), "Copied to clipboard");
    // Control: the painted rows differ from the source, so a row copy would fail.
    assert!(f.h.screen_text().contains("│"));
}

#[test]
fn trailing_blank_lines_and_quoted_tables_copy_unrepaired() {
    let (mut f, id) = single(80, 24, "```\na\n\n```\n");
    open(&mut f, &id);
    assert_eq!(
        copy(&mut f, Key::Char('c')),
        ["a\n"],
        "one trailing blank line is content"
    );
    let (mut f, id) = single(80, 24, "> | a | b |\n> |---|---|\n> | 1 | 2 |\n");
    open(&mut f, &id);
    let copied = copy(&mut f, Key::Char('c'));
    assert_eq!(copied.len(), 1);
    assert!(
        copied[0].starts_with("| a | b |\n|---|---|\n| 1 | 2 |"),
        "{copied:?}"
    );
    assert!(
        !copied[0].contains('>'),
        "the container prefix is not copied"
    );
}

#[test]
fn an_empty_block_copies_nothing_and_never_the_last_answer() {
    let (mut f, id) = single(80, 24, "Some answer text.\n\n```\n```\n");
    open(&mut f, &id);
    assert!(
        copy(&mut f, Key::Char('c')).is_empty(),
        "the copier is never called"
    );
    assert_eq!(toast(&f), "Nothing to copy");
}

#[test]
fn a_block_of_the_open_turn_copies_its_source_and_an_unresolvable_one_copies_nothing() {
    let (mut f, id) = single(80, 24, "Intro.\n\n```rust\nlet open = 1;\n```\n");
    // A streaming turn's committed prose parts are Tab-focusable too.
    let turn = f.tabs.active_tab_mut().conversation.turns.pop().unwrap();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    f.tabs.active_tab_mut().reducer.open_turn = Some(turn);
    f.paint();
    focus(&mut f, &id);
    f.h.state.select_item_explicitly(id.clone());
    assert_eq!(copy(&mut f, Key::Char('c')), ["let open = 1;"]);
    assert_eq!(toast(&f), "Copied to clipboard");
    // A block id that resolves nowhere never falls back to the last answer.
    f.tabs.active_tab_mut().reducer.open_turn = None;
    assert!(copy(&mut f, Key::Char('c')).is_empty());
    assert_eq!(toast(&f), "Nothing to copy");
}

#[test]
fn block_copy_is_bounded_at_eight_mebibytes_and_never_calls_the_copier_above_it() {
    for (bytes, expect_copy) in [(8 * 1024 * 1024, true), (8 * 1024 * 1024 + 1, false)] {
        let (mut f, id) = single(80, 24, &format!("```\n{}\n```\n", "x".repeat(bytes)));
        open(&mut f, &id);
        let copied = copy(&mut f, Key::Char('c'));
        if expect_copy {
            assert_eq!(copied.len(), 1);
            assert_eq!(copied[0].len(), bytes);
            assert_eq!(toast(&f), "Copied to clipboard");
        } else {
            assert!(
                copied.is_empty(),
                "the copier is never called above the limit"
            );
            assert_eq!(
                toast(&f),
                "Copy failed: block source exceeds the 8 MiB copy limit"
            );
        }
    }
}

#[test]
fn chat_c_follows_the_selected_item_and_keeps_its_legacy_rules() {
    let (turn, ids) = mixed_turn();
    let mut f = Fixture::new(80, 24, turn);
    focus(&mut f, &ids[0]);
    f.h.state.select_item_explicitly(ids[0].clone());
    let copied = copy(&mut f, Key::Char('c'));
    assert!(
        copied[0].starts_with("fn line_01() {}\nfn line_02() {}"),
        "{copied:?}"
    );
    assert!(!copied[0].contains("```"), "no fences");
    // Positive controls: a selected tool copies its output; no selection copies
    // the last assistant message.
    f.h.state.select_item_explicitly(ids[1].clone());
    assert_eq!(copy(&mut f, Key::Char('c')), ["output"]);
    f.h.state.clear_item_selection();
    let last = f
        .conversation()
        .messages
        .iter()
        .rev()
        .find(|message| message.role == rustain::domain::models::MessageRole::Assistant)
        .unwrap()
        .content
        .clone();
    assert_eq!(copy(&mut f, Key::Char('c')), [last]);
}
