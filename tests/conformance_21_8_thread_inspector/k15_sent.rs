//! K15 / AC18: sender reads occupy a frame, never a transcript row.
use super::support::*;
use rustain::adapters::tui::{keymap::Key, state::ThreadAccess, thread_body};
use rustain::infrastructure::runtime::{thread_open, transparency_bridge};

const PROJECTION: &str = "thread · ada · root\n  you       root\n      root body\n  3 earlier turns not shown — the full thread is on ada's host\n  ada       other\n      unrelated turn\n  ada       match\n      beta witness\n  you       opened\n      opened body\nThis host addresses ada with one credential and cannot tell which operator sent this item.";

fn open(rig: &mut Rig) -> u64 {
    thread_open::open_sent(
        &mut rig.f.h.state,
        ThreadAccess::Writer,
        "ada",
        "opened",
        "origin",
    )
}

#[tokio::test]
async fn ambiguous_sent_headers_never_offer_retract_in_local_paint() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 40).await;
    let base = "thread · ada · ri_open\n  you       ri_open\n      body";
    for (suffix, mine, retracted, rail) in [
        ("", true, false, true),
        (
            "\n  ada       other\n        you       ri_open  [retracted 99:99]",
            true,
            false,
            true,
        ),
        ("\n  ada       ri_open\n      reply", false, false, false),
        (
            "\n  ada       ri_open  [retracted 99:99]\n      reply",
            false,
            false,
            false,
        ),
        ("\n  you       ri_open\n      reply", false, false, false),
        (
            "\n  ada  ri_open       other\n      reply",
            false,
            false,
            false,
        ),
    ] {
        let generation = thread_open::open_sent(
            &mut rig.f.h.state,
            ThreadAccess::Writer,
            "ada",
            "ri_open",
            "origin",
        );
        assert!(thread_open::fill_sent(
            &mut rig.f.h.state,
            "origin",
            generation,
            &format!("{base}{suffix}")
        ));
        let facts = rig.inspector().sent.as_ref().unwrap();
        assert_eq!((facts.mine, facts.retracted), (mine, retracted), "{suffix}");
        rig.paint();
        assert_eq!(
            footer_rows(&rig).join("\n").contains("[x] Retract"),
            rail,
            "{suffix}"
        );
    }
    let generation = thread_open::open_sent(
        &mut rig.f.h.state,
        ThreadAccess::Writer,
        "ada",
        "ri_open",
        "origin",
    );
    assert!(thread_open::fill_sent(
        &mut rig.f.h.state,
        "origin",
        generation,
        "thread · ada · ri_open\n  you       ri_open  [retracted 12:34]\n      body"
    ));
    assert!(rig.inspector().sent.as_ref().unwrap().retracted);
    rig.paint();
    assert!(!footer_rows(&rig).join("\n").contains("[x] Retract"));
}

#[tokio::test]
async fn sent_loading_projection_safety_filter_and_back() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 40).await;
    rig.f.h.state.input_buffer = "draft".into();
    let blocks = rig.f.h.state.feedback_blocks.len();
    let generation = open(&mut rig);
    rig.paint();
    assert!(title_row(&rig.f).contains("Thread · sent · ada"));
    assert!(painted_body(&mut rig).join("\n").contains("reading…"));
    let hostile = PROJECTION.replace("opened body", "opened body \u{1b}[31mred\u{202e}tail");
    transparency_bridge::team_thread_ready(&mut rig.f.h.state, "origin", generation, &hostile);
    let text = painted_body(&mut rig).join("\n");
    for part in [
        "thread · ada · root",
        "root body",
        "opened body",
        "3 earlier turns not shown",
        "one credential",
        "\\u{1b}",
        "\\u{202e}",
    ] {
        assert!(text.contains(part), "missing {part}: {text}");
    }
    for line in thread_body::build(rig.inspector(), &rig.f.h.state.theme, 100).lines {
        for span in line.spans {
            assert!(
                !span
                    .content
                    .chars()
                    .any(|c| matches!(c, '\u{1b}' | '\u{202e}'))
            );
        }
    }
    for cell in &rig.f.buffer().content {
        assert!(
            !cell
                .symbol()
                .chars()
                .any(|c| matches!(c, '\u{1b}' | '\u{202e}'))
        );
    }
    rig.key(Key::Char('/'));
    for c in "beta".chars() {
        rig.key(Key::Char(c));
    }
    rig.key(Key::Enter);
    let filtered = painted_body(&mut rig).join("\n");
    assert!(
        filtered.contains("root body")
            && filtered.contains("opened body")
            && filtered.contains("beta witness"),
        "{filtered}"
    );
    assert!(!filtered.contains("unrelated turn"));
    rig.key(Key::Esc); // clear filter
    rig.key(Key::Ctrl('p'));
    assert_eq!(rig.depth(), 2);
    rig.key(Key::Esc);
    assert!(
        rig.is_open(),
        "back from the palette leaves the sent thread"
    );
    rig.key(Key::Esc); // back one frame
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(rig.f.h.state.input_buffer, "draft");
    assert_eq!(rig.f.h.state.feedback_blocks.len(), blocks);
}

#[tokio::test]
async fn stale_generation_completion_is_dropped() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 30).await;
    let before = rig.f.h.state.feedback_blocks.len();
    let stashed = rig.f.tabs.active_tab().conversation.id.clone();
    rig.f.tabs.create_tab();
    let stashed_blocks = rig
        .f
        .tabs
        .find_by_conversation(&stashed)
        .unwrap()
        .feedback_blocks
        .clone();
    let first = open(&mut rig);
    let second = open(&mut rig);
    rig.key(Key::Esc);
    transparency_bridge::team_thread_ready(
        &mut rig.f.h.state,
        "origin",
        second,
        "closed completion",
    );
    assert!(rig.f.h.state.modal_stack.is_empty());
    assert_eq!(rig.f.h.state.feedback_blocks.len(), before);
    assert_eq!(
        rig.f
            .tabs
            .find_by_conversation(&stashed)
            .unwrap()
            .feedback_blocks,
        stashed_blocks
    );
    let newest = open(&mut rig);
    transparency_bridge::team_thread_ready(
        &mut rig.f.h.state,
        "origin",
        newest,
        "newest completion",
    );
    transparency_bridge::team_thread_ready(&mut rig.f.h.state, "origin", first, "stale first");
    transparency_bridge::team_thread_ready(&mut rig.f.h.state, "origin", second, "stale second");
    transparency_bridge::team_thread_ready(
        &mut rig.f.h.state,
        "other-origin",
        newest,
        "wrong origin",
    );
    let text = painted_body(&mut rig).join("\n");
    assert!(text.contains("newest completion"), "{text}");
    assert!(!text.contains("stale") && !text.contains("wrong origin"));
    assert_eq!(rig.f.h.state.feedback_blocks.len(), before);
}

#[tokio::test]
async fn refusal_is_painted_in_the_thread_frame() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 30).await;
    let generation = open(&mut rig);
    assert!(thread_open::fill_sent(
        &mut rig.f.h.state,
        "origin",
        generation,
        "thread not found on ada's host"
    ));
    assert!(
        painted_body(&mut rig)
            .join("\n")
            .contains("thread not found on ada's host")
    );
}

#[cfg(not(feature = "a2a"))]
#[tokio::test]
async fn no_a2a_front_door_warns_without_opening() {
    let mut rig = Rig::plain(Workspace::new().await, 130, 30).await;
    transparency_bridge::team_command(
        &mut rig.f.h.state,
        "origin",
        Some("thread ada opened"),
        &rig.app,
    )
    .await;
    assert!(rig.f.h.state.modal_stack.is_empty());
    let warnings: Vec<_> = rig.f.h.state.notifications.visible().collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Warning
    );
    assert!(warnings[0].message.contains("needs the `a2a` feature"));
}
