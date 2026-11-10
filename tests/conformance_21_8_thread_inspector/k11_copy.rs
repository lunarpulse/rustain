//! K11: copy uses the original item, never the painted projection or Chat fallback.
use super::support::*;
use rustain::adapters::tui::{
    app::InputAction, clipboard::ClipboardResult, handlers::clipboard, keymap::Key,
    tool_inspector::source::COPY_MAX_BYTES,
};
use rustain::domain::models::SemanticMessageType;
use std::sync::{Arc, Mutex};

fn recorder(rig: &mut Rig) -> Arc<Mutex<Vec<String>>> {
    let copies = Arc::new(Mutex::new(Vec::new()));
    let sink = copies.clone();
    rig.f.h.state.clipboard_copier = Some(Arc::new(move |text| {
        sink.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    }));
    copies
}

async fn opened(body: &str) -> Rig {
    let ws = Workspace::with(&[Item::new("ri_c", SemanticMessageType::Consultation, body)]).await;
    let mut rig = Rig::plain(ws, 100, 30).await;
    rig.view("ri_c").await;
    rig.paint();
    rig
}

#[tokio::test]
async fn c_copies_the_original_body_bytes_not_the_display() {
    let original = "raw \u{1b}[31mred\u{202e}x";
    let mut rig = opened(original).await;
    let copies = recorder(&mut rig);
    let painted = painted_body(&mut rig).join("\n");
    assert!(painted.contains("\\u{1b}"));
    assert!(matches!(rig.key(Key::Char('c')), InputAction::Consumed));
    assert_eq!(&*copies.lock().unwrap(), &[original]);
}

#[tokio::test]
async fn removed_item_c_says_nothing_to_copy_and_not_the_last_assistant_message() {
    let ws = Workspace::with(&[Item::new(
        "ri_c",
        SemanticMessageType::Consultation,
        "removed body",
    )])
    .await;
    ws.remove("ri_c").await;
    let mut rig = Rig::plain(ws, 100, 30).await;
    let copies = recorder(&mut rig);
    rig.view("ri_c").await;
    rig.paint();
    rig.key(Key::Char('c'));
    assert!(copies.lock().unwrap().is_empty());
    assert_eq!(
        rig.f
            .h
            .state
            .notifications
            .records()
            .last()
            .unwrap()
            .message,
        "Nothing to copy"
    );
    rig.key(Key::Esc);
    let action = rig.key(Key::Char('c'));
    let InputAction::CopyToClipboard(content) = action else {
        panic!("Chat copy: {action:?}")
    };
    let conversation = rig.f.conversation().clone();
    let sink = copies.clone();
    clipboard::requested(&mut rig.f.h.state, &conversation, content, &|text| {
        sink.lock().unwrap().push(text.to_owned());
        ClipboardResult::Osc52Success
    });
    assert!(
        copies
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("Stage answer paragraph")
    );
}

#[tokio::test]
async fn body_over_eight_mebibytes_is_refused_before_copying() {
    let mut rig = opened("small positive fixture").await;
    let copies = recorder(&mut rig);
    rig.f.h.state.thread_inspector.as_mut().unwrap().original =
        Some("x".repeat(COPY_MAX_BYTES + 1));
    rig.key(Key::Char('c'));
    assert!(copies.lock().unwrap().is_empty());
    assert_eq!(
        rig.f
            .h
            .state
            .notifications
            .records()
            .last()
            .unwrap()
            .message,
        "Copy failed: item body exceeds the 8 MiB copy limit"
    );
    rig.f.h.state.thread_inspector.as_mut().unwrap().original = Some("x".repeat(COPY_MAX_BYTES));
    rig.key(Key::Char('c'));
    assert_eq!(copies.lock().unwrap()[0].len(), COPY_MAX_BYTES);
}

#[tokio::test]
async fn c_sends_no_frame_and_selects_no_context() {
    let mut rig = opened("copy me").await;
    let copies = recorder(&mut rig);
    let selected = rig.inspector().context_selected;
    let journal = rig.ws.journal_rows().await;
    assert!(matches!(rig.key(Key::Char('c')), InputAction::Consumed));
    assert_eq!(&*copies.lock().unwrap(), &["copy me"]);
    assert_eq!(rig.inspector().context_selected, selected);
    assert_eq!(rig.ws.journal_rows().await, journal);
    assert!(rig.events.try_recv().is_err());
}
