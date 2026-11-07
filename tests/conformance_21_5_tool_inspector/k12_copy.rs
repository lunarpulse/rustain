//! K12 / M11: clipboard requests stay scoped to the real frame source.
use super::support::*;
use rustain::adapters::tui::{
    app::InputAction,
    clipboard::ClipboardResult,
    handlers::clipboard,
    keymap::Key,
    notifications::{NoticeSeverity, NoticeSource},
    tool_inspector::{self, ToolTab, project, source},
};
use rustain::domain::{
    models::{
        Conversation, DiffKind, DiffLine, InvocationStatus as S, StreamChunk, Turn, TurnPart,
        WriteDiffState as D,
    },
    services::reducer,
};
use serde_json::{Value, json};
use std::cell::RefCell;

#[test]
fn copy_key_requests_canonical_source_instead_of_the_chat_sentinel() {
    let mut turn = Turn::new("copy-model".into(), 0);
    let id = call(
        &mut turn,
        "Bash",
        json!({}),
        S::Success,
        Some(("", D::NotAWrite)),
    );
    prose(&mut turn, "assistant fallback must not be copied");
    let mut f = Fixture::new(80, 24, turn);
    f.focus_call(&id);
    f.open();
    // A stale explicit copy selection is a real observable Chat-fallback control,
    // never the target of the frame that Enter actually resolved.
    f.h.state.selected_item_id = Some("stale-copy-selection".into());
    assert!(
        f.h.state
            .resolve_copy_content(f.conversation())
            .contains("assistant fallback")
    );
    assert_eq!(
        format!("{:?}", f.press(Key::Char('c'))),
        "CopyToolSource",
        "the canonical effect must precede source resolution, never the empty Chat sentinel"
    );
}

/// The host's real two-effect dispatcher, with only the platform copier injected.
/// Keeping generic requested() here makes M11 exercise the actual Chat fallback,
/// rather than a test-only imitation of empty-string resolution.
fn dispatch(f: &mut Fixture, key: Key, result: ClipboardResult) -> Vec<String> {
    let copied = RefCell::new(Vec::new());
    let result = RefCell::new(Some(result));
    let copier = |text: &str| {
        copied.borrow_mut().push(text.to_owned());
        result
            .borrow_mut()
            .take()
            .expect("one platform attempt per key")
    };
    let action = f.press(key);
    let tab = f.tabs.active_tab();
    match action {
        InputAction::CopyToolSource => assert!(tool_inspector::copy::copy_source(
            &mut f.h.state,
            &tab.conversation,
            Some(&tab.reducer),
            &copier,
        )),
        InputAction::CopyToClipboard(content) => {
            clipboard::requested(&mut f.h.state, &tab.conversation, content, &copier)
        }
        other => panic!("copy key emitted {other:?}"),
    }
    copied.into_inner()
}

fn copy(f: &mut Fixture) -> Vec<String> {
    dispatch(f, Key::Char('c'), ClipboardResult::Osc52Success)
}

fn notice(f: &Fixture, severity: NoticeSeverity, message: &str) {
    let record =
        f.h.state
            .notifications
            .records()
            .last()
            .expect("copy notice");
    assert_eq!(record.source, NoticeSource::Clipboard);
    assert_eq!(
        (record.severity, record.message.as_str()),
        (severity, message)
    );
}

fn baseline(f: &Fixture, tab: ToolTab) -> String {
    let active = f.tabs.active_tab();
    let view = project::project(
        &active.conversation.turns,
        active.reducer.open_turn.as_ref(),
        &active.reducer,
        &f.tool().target,
    )
    .unwrap();
    source::text(&view, tab).into_owned()
}

fn result_mut(f: &mut Fixture) -> &mut rustain::domain::models::ToolOutput {
    let tab = f.tabs.active_tab_mut();
    let turn = match tab.reducer.open_turn.as_mut() {
        Some(turn) => turn,
        None => tab.conversation.turns.last_mut().unwrap(),
    };
    turn.parts
        .iter_mut()
        .rev()
        .find_map(|part| match part {
            TurnPart::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .expect("completed fixture")
}

#[test]
fn empty_tab_never_copies_the_last_assistant_message() {
    for status in [S::Pending, S::Running, S::Success] {
        let mut turn = Turn::new("copy-model".into(), 0);
        let completed = status == S::Success;
        let id = call(
            &mut turn,
            "Bash",
            json!({}),
            status,
            completed.then_some(("", D::NotAWrite)),
        );
        prose(&mut turn, "last assistant positive control");
        let mut f = Fixture::new(80, 24, turn);
        f.focus_call(&id);
        f.open();
        f.h.state.selected_item_id = None;
        assert!(
            f.h.state
                .resolve_copy_content(f.conversation())
                .contains("last assistant positive control")
        );
        assert!(
            copy(&mut f).is_empty(),
            "empty source must not invoke the copier"
        );
        notice(&f, NoticeSeverity::Info, "Nothing to copy");
        f.press(Key::Esc);
        // Enter intentionally selected a tool. Explicitly remove that selection
        // so this is a genuine last-assistant Chat positive control.
        f.h.state.selected_item_id = None;
        let expected = f.h.state.resolve_copy_content(f.conversation());
        assert!(!expected.is_empty());
        assert_eq!(copy(&mut f), [expected]);
        notice(&f, NoticeSeverity::Success, "Copied to clipboard");
    }
}

#[test]
fn output_copy_is_original_bytes_not_sanitized_pixels_and_changes_no_model_or_cache() {
    let original = "\x1b[31mred\x1b[0m\t界\u{202e}bidi\r\nsecond\rthird\x07";
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Bash",
        json!({}),
        S::Success,
        Some((original, D::NotAWrite)),
    );
    f.focus_call(&id);
    f.h.state.input_buffer = "draft text".into();
    f.h.state.cursor_position = 3;
    f.paint();
    f.open();
    let screen = f.h.screen_text();
    assert!(screen.contains("red") && screen.contains("second"));
    for forbidden in ['\x1b', '\x07', '\u{202e}', '\r', '\t'] {
        assert!(!screen.contains(forbidden));
    }
    let transcript = serde_json::to_vec(f.conversation()).unwrap();
    let view = f.view();
    let cache = f.cache();
    let draft = (
        f.h.state.input_buffer.clone(),
        f.h.state.cursor_position,
        f.h.state.input_scroll_offset,
    );
    let bodies = format!("{:?}", f.tool().bodies);
    let status = f.h.state.status.clone();
    assert_eq!(copy(&mut f), [original]);
    assert_eq!(serde_json::to_vec(f.conversation()).unwrap(), transcript);
    assert_eq!(f.view(), view);
    assert_eq!(f.cache(), cache);
    assert_eq!(
        (
            f.h.state.input_buffer.clone(),
            f.h.state.cursor_position,
            f.h.state.input_scroll_offset
        ),
        draft
    );
    assert_eq!(format!("{:?}", f.tool().bodies), bodies);
    assert_eq!(f.h.state.status, status);
}

#[test]
fn input_raw_and_diff_copy_exact_sources_before_the_next_paint() {
    let args = json!({"old_string":"old 界\r\n", "new_string":"new \u{202e}\r\n", "extra":null});
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Edit",
        args,
        S::Success,
        Some(("result\x1b[0m", D::NotAWrite)),
    );
    f.focus_call(&id);
    f.open();
    for tab in [ToolTab::Input, ToolTab::Raw, ToolTab::Diff] {
        f.press(Key::Right);
        let expected = baseline(&f, tab);
        assert!(!expected.is_empty());
        assert_eq!(copy(&mut f), [expected]);
    }
    if let TurnPart::ToolInvocation { args, .. } = f.invocation_mut(&id) {
        *args = json!({"old_string":"fresh old", "new_string":"fresh new"});
    }
    assert_eq!(copy(&mut f), ["- fresh old\n+ fresh new"]);
    f.press(Key::Left);
    result_mut(&mut f).content = "fresh result\r\n\x1b[32m".into();
    let expected = baseline(&f, ToolTab::Raw);
    assert!(expected.contains("fresh result"));
    assert_eq!(copy(&mut f), [expected]);
    f.press(Key::Left);
    let expected = baseline(&f, ToolTab::Input);
    assert_eq!(copy(&mut f), [expected]);
}

#[test]
fn copy_resolves_initial_tab_keys_before_the_first_inspector_paint() {
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Write",
        json!({"content":"界\r\nnew"}),
        S::Success,
        Some(("output", D::NewFile)),
    );
    f.focus_call(&id);
    assert_eq!(f.press(Key::Enter), InputAction::InspectTool);
    assert!(f.inspect());
    assert!(
        f.tool().bodies.iter().all(Option::is_none),
        "control: no inspector body has been prepared"
    );
    for tab in [ToolTab::Input, ToolTab::Raw, ToolTab::Diff] {
        f.press(Key::Right);
        let expected = baseline(&f, tab);
        assert_eq!(copy(&mut f), [expected]);
        assert!(
            f.tool().bodies.iter().all(Option::is_none),
            "copy must not prepare display bodies"
        );
    }
}

fn running() -> (Fixture, String, String) {
    let (mut f, _) = Fixture::single(80, 24, "Bash", json!({}), S::Success, None);
    f.tabs.active_tab_mut().conversation.turns.clear();
    f.tabs
        .active_tab_mut()
        .conversation
        .rebuild_messages_mirror();
    for wire in ["wire-copy-a", "wire-copy-b"] {
        reducer::reduce(
            &mut f.tabs.active_tab_mut().reducer,
            StreamChunk::ToolUse {
                id: wire.into(),
                name: "Bash".into(),
                input: json!({"command":wire}),
            },
            &*f.clock,
        );
    }
    let tab = f.tabs.active_tab_mut();
    let turn = tab.reducer.open_turn.as_ref().unwrap();
    let ids: Vec<_> = turn
        .parts
        .iter()
        .filter_map(|part| match part {
            TurnPart::ToolInvocation { id, .. } => Some(
                rustain::domain::models::turn::tool_call_id_for(&turn.id, *id),
            ),
            _ => None,
        })
        .collect();
    tab.reducer.set_tail("wire-copy-a", "A original\r\n".into());
    tab.reducer
        .set_tail("wire-copy-b", "B original\x1b[0m".into());
    reducer::update_streaming_mirror(&tab.reducer, &mut tab.streaming);
    f.paint();
    (f, ids[0].clone(), ids[1].clone())
}

#[test]
fn running_copy_resolves_only_its_own_fresh_tail_and_honors_empty_tail() {
    let (mut f, a, b) = running();
    f.focus_call(&a);
    f.open();
    f.tabs
        .active_tab_mut()
        .reducer
        .set_tail("wire-copy-a", "A fresh\r\n".into());
    assert_eq!(copy(&mut f), ["A fresh\r\n"]);
    assert!(f.step(Key::Char(']')));
    assert_eq!(f.tool().target.tool_call_id, b);
    assert_eq!(copy(&mut f), ["B original\x1b[0m"]);
    f.tabs
        .active_tab_mut()
        .reducer
        .set_tail("wire-copy-b", String::new());
    assert!(copy(&mut f).is_empty());
    notice(&f, NoticeSeverity::Info, "Nothing to copy");
    reducer::reduce(
        &mut f.tabs.active_tab_mut().reducer,
        StreamChunk::ToolResult {
            id: "wire-copy-b".into(),
            operator_private: false,
            content: "completed B".into(),
            is_error: false,
            diff: D::NotAWrite,
            images: vec![],
        },
        &*f.clock,
    );
    assert_eq!(copy(&mut f), ["completed B"]);
}

#[test]
fn typed_copy_success_fallback_and_failure_use_the_existing_notices() {
    let fallback = std::env::temp_dir().join("rustain-k12-clipboard.txt");
    let cases = [
        (
            ClipboardResult::Osc52Success,
            NoticeSeverity::Success,
            "Copied to clipboard".into(),
        ),
        (
            ClipboardResult::FallbackSuccess(fallback.clone()),
            NoticeSeverity::Success,
            format!("Copied to {}", fallback.display()),
        ),
        (
            ClipboardResult::Failed("platform unavailable".into()),
            NoticeSeverity::Error,
            "Copy failed: platform unavailable".into(),
        ),
    ];
    for (result, severity, message) in cases {
        let (mut f, id) = Fixture::single(
            80,
            24,
            "Bash",
            json!({}),
            S::Success,
            Some(("source", D::NotAWrite)),
        );
        f.focus_call(&id);
        f.open();
        assert_eq!(dispatch(&mut f, Key::Char('c'), result), ["source"]);
        notice(&f, severity, &message);
        if severity == NoticeSeverity::Error {
            assert!(
                f.h.state
                    .notifications
                    .records()
                    .all(|r| r.source != NoticeSource::Clipboard
                        || r.severity != NoticeSeverity::Success)
            );
        }
    }
}

#[test]
fn output_input_raw_and_diff_accept_exactly_eight_mib_and_refuse_one_more_byte() {
    for tab in [ToolTab::Output, ToolTab::Input, ToolTab::Raw, ToolTab::Diff] {
        for extra in [0, 1] {
            let (mut f, id) = Fixture::single(
                80,
                24,
                "Write",
                json!(""),
                S::Success,
                Some((
                    "",
                    D::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Added,
                            content: "small".into(),
                        }],
                        more: 0,
                    },
                )),
            );
            f.focus_call(&id);
            f.open();
            for _ in 0..tab.index() {
                f.press(Key::Right);
            }
            assert_eq!(f.tool().tab, tab);
            let overhead = match tab {
                ToolTab::Output => 0,
                ToolTab::Input => 2, // a JSON string's opening/closing quotes
                ToolTab::Raw => baseline(&f, ToolTab::Raw).len(),
                ToolTab::Diff => 2, // "+ "
            };
            let size = source::COPY_MAX_BYTES - overhead + extra;
            // Unicode proves this is a byte boundary, not a character limit.
            let payload = format!("{}{}", "界".repeat(size / 3), "x".repeat(size % 3));
            match tab {
                ToolTab::Output | ToolTab::Raw => result_mut(&mut f).content = payload,
                ToolTab::Input => {
                    if let TurnPart::ToolInvocation { args, .. } = f.invocation_mut(&id) {
                        *args = Value::String(payload);
                    }
                }
                ToolTab::Diff => {
                    result_mut(&mut f).diff = D::Diff {
                        lines: vec![DiffLine {
                            kind: DiffKind::Added,
                            content: payload,
                        }],
                        more: 0,
                    }
                }
            }
            // No refresh/paint: large Input/Raw must be bounded at the effect,
            // not protected accidentally by a prior display serialization.
            let copied = copy(&mut f);
            if extra == 0 {
                assert_eq!(copied.len(), 1);
                assert_eq!(copied[0].len(), source::COPY_MAX_BYTES);
                assert_eq!(copied[0], baseline(&f, tab));
                notice(&f, NoticeSeverity::Success, "Copied to clipboard");
            } else {
                assert!(
                    copied.is_empty(),
                    "an oversized source must never reach the copier"
                );
                notice(
                    &f,
                    NoticeSeverity::Error,
                    "Copy failed: tool source exceeds the 8 MiB copy limit",
                );
            }
        }
    }
}

#[test]
fn prepaint_derived_diff_is_bounded_and_never_truncated() {
    for name in ["Edit", "Write"] {
        let args = if name == "Edit" {
            json!({"old_string":"old", "new_string":"small"})
        } else {
            json!({"content":"small"})
        };
        let (mut f, id) = Fixture::single(80, 24, name, args, S::Success, Some(("", D::NewFile)));
        f.focus_call(&id);
        f.open();
        for _ in 0..3 {
            f.press(Key::Right);
        }
        if let TurnPart::ToolInvocation { args, .. } = f.invocation_mut(&id) {
            let huge = "x".repeat(source::COPY_MAX_BYTES + 1);
            *args = if name == "Edit" {
                json!({"old_string":"old", "new_string":huge})
            } else {
                json!({"content":huge})
            };
        }
        assert!(copy(&mut f).is_empty());
        notice(
            &f,
            NoticeSeverity::Error,
            "Copy failed: tool source exceeds the 8 MiB copy limit",
        );
    }
}

#[test]
fn raw_shape_order_and_values_match_the_original_borrowed_adapters() {
    for completed in [false, true] {
        for content in ["界\u{202e}\r\n\x1b[0m".into(), "large 界".repeat(20_000)] {
            let (mut f, id) = Fixture::single(
                80,
                24,
                "Bash",
                json!({"optional":null, "unicode":"界", "nested":[true,42]}),
                if completed { S::Success } else { S::Running },
                completed.then_some(("", D::NotAWrite)),
            );
            f.focus_call(&id);
            f.open();
            if completed {
                result_mut(&mut f).content = content.clone();
            }
            f.press(Key::Right);
            f.press(Key::Right);
            let expected = baseline(&f, ToolTab::Raw);
            let copied = copy(&mut f);
            assert_eq!(copied.as_slice(), std::slice::from_ref(&expected));
            let value: Value = serde_json::from_str(&copied[0]).unwrap();
            assert_eq!(value["id"], id);
            assert_eq!(value["tool"], "Bash");
            assert_eq!(
                value["input"],
                json!({"optional":null, "unicode":"界", "nested":[true,42]})
            );
            assert_eq!(value["started_at"], 0);
            assert_eq!(
                value["ended_at"],
                if completed { json!(1) } else { Value::Null }
            );
            assert_eq!(
                value["result"],
                if completed {
                    json!({"content":content, "is_error":false})
                } else {
                    Value::Null
                }
            );
            let fields = [
                "id",
                "tool",
                "status",
                "input",
                "result",
                "started_at",
                "ended_at",
            ];
            let positions: Vec<_> = fields
                .iter()
                .map(|field| expected.find(&format!("\n  \"{field}\":")).unwrap())
                .collect();
            assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }
}

#[test]
fn missing_non_tool_and_wrong_conversation_have_no_clipboard_or_notice_effect() {
    let (mut f, id) = Fixture::single(
        80,
        24,
        "Bash",
        json!({}),
        S::Success,
        Some(("source", D::NotAWrite)),
    );
    let attempts = RefCell::new(0);
    let copier = |_: &str| {
        *attempts.borrow_mut() += 1;
        ClipboardResult::Osc52Success
    };
    let before = f.h.state.notifications.records().len();
    let active = f.tabs.active_tab();
    assert!(!tool_inspector::copy::copy_source(
        &mut f.h.state,
        &active.conversation,
        Some(&active.reducer),
        &copier
    ));
    f.focus_call(&id);
    f.open();
    f.press(Key::Ctrl('x'));
    f.press(Key::Char('?'));
    assert_eq!(
        f.h.state.modal_stack.top().unwrap().surface,
        rustain::adapters::tui::presentation::Surface::Help
    );
    let active = f.tabs.active_tab();
    assert!(!tool_inspector::copy::copy_source(
        &mut f.h.state,
        &active.conversation,
        Some(&active.reducer),
        &copier
    ));
    f.press(Key::Esc);
    let other = Conversation {
        id: format!("{}-not-owner", f.conversation().id),
        ..Conversation::default()
    };
    assert!(!tool_inspector::copy::copy_source(
        &mut f.h.state,
        &other,
        None,
        &copier
    ));
    f.tabs.active_tab_mut().conversation.turns.clear();
    let active = f.tabs.active_tab();
    assert!(!tool_inspector::copy::copy_source(
        &mut f.h.state,
        &active.conversation,
        Some(&active.reducer),
        &copier
    ));
    assert_eq!(*attempts.borrow(), 0);
    assert_eq!(f.h.state.notifications.records().len(), before);
}
