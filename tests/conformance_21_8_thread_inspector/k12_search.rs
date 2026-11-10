//! K12: search filters turns within the thread without touching the composer.
use super::support::*;
use rustain::adapters::tui::{
    decision_queue,
    keymap::Key,
    presentation::{self, Surface},
};
use rustain::domain::models::ImageAttachment;

async fn search_rig() -> Rig {
    let mut items = chain(30);
    for n in [4, 11, 20] {
        items[n].content = format!("beta witness {n:03}");
    }
    let mut rig = Rig::plain(Workspace::with(&items).await, 130, 40).await;
    rig.view("ri_029").await;
    rig.paint();
    rig
}
fn type_query(rig: &mut Rig, text: &str) {
    rig.key(Key::Char('/'));
    for c in text.chars() {
        rig.key(Key::Char(c));
    }
}

#[tokio::test]
async fn slash_filters_turns_keeping_identity_and_opened_item() {
    let mut rig = search_rig().await;
    let identity = header_rows(&rig);
    let before = painted_body(&mut rig).join("\n");
    assert!(before.contains("turn 010 body"));
    type_query(&mut rig, "BeTa");
    rig.key(Key::Enter);
    rig.paint();
    assert_eq!(header_rows(&rig), identity);
    let after = painted_body(&mut rig).join("\n");
    for n in [4, 11, 20] {
        assert!(after.contains(&format!("beta witness {n:03}")), "{after}");
    }
    assert!(!after.contains("turn 010 body"));
    assert!(after.contains("turn 029 body"));
    assert!(after.contains("turn 000 body"));
}

#[tokio::test]
async fn enter_keeps_the_filter_esc_clears_next_esc_backs() {
    let mut rig = search_rig().await;
    type_query(&mut rig, "beta");
    rig.key(Key::Enter);
    rig.paint();
    assert!(rig.is_open());
    assert!(!painted_body(&mut rig).join("\n").contains("turn 010 body"));
    rig.key(Key::Esc);
    rig.paint();
    assert!(rig.is_open());
    assert!(painted_body(&mut rig).join("\n").contains("turn 010 body"));
    rig.key(Key::Esc);
    assert!(!rig.is_open());
}

#[tokio::test]
async fn no_match_posts_a_notice_and_leaves_the_view_unchanged() {
    let mut rig = search_rig().await;
    let before = painted_body(&mut rig);
    type_query(&mut rig, "unfindable");
    rig.key(Key::Enter);
    rig.paint();
    assert_eq!(
        rig.f
            .h
            .state
            .notifications
            .records()
            .last()
            .unwrap()
            .message,
        "Search: no match for \"unfindable\""
    );
    assert_eq!(painted_body(&mut rig), before);
    type_query(&mut rig, "beta");
    rig.key(Key::Enter);
    rig.paint();
    assert_ne!(painted_body(&mut rig), before);
}

#[tokio::test]
async fn typing_never_reaches_the_composer() {
    let mut rig = search_rig().await;
    rig.f.h.state.input_buffer = "draft text".into();
    rig.f.h.state.cursor_position = 3;
    let scroll = rig.f.h.state.input_scroll_offset;
    type_query(&mut rig, "beta");
    assert_eq!(rig.inspector().search.query.as_str(), "beta");
    rig.key(Key::Enter);
    rig.paint();
    assert_eq!(rig.f.h.state.input_buffer, "draft text");
    assert_eq!(rig.f.h.state.cursor_position, 3);
    assert_eq!(rig.f.h.state.input_scroll_offset, scroll);
    assert!(painted_body(&mut rig).join("\n").contains("beta witness"));
}

#[tokio::test]
async fn arrival_while_typing_does_not_preempt_but_closed_field_does() {
    let image = || ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    };
    let mut typing = search_rig().await;
    type_query(&mut typing, "beta");
    assert!(!presentation::arrival_may_preempt(
        &typing.f.h.state,
        typing.f.h.state.notifications.now()
    ));
    decision_queue::raise_large_image(&mut typing.f.h.state, image(), "Image is large (6 MB).");
    assert!(typing.is_open());
    assert_eq!(decision_queue::waiting(&typing.f.h.state), 1);
    assert_eq!(typing.inspector().search.query.as_str(), "beta");
    let mut closed = search_rig().await;
    assert!(presentation::arrival_may_preempt(
        &closed.f.h.state,
        closed.f.h.state.notifications.now()
    ));
    decision_queue::raise_large_image(&mut closed.f.h.state, image(), "Image is large (6 MB).");
    assert_eq!(closed.top(), Surface::Decision);
    assert_eq!(closed.depth(), 2);
}

/// The live smoke seed (attempt 4): a 60-line opened body, two newer follow-up items, the two-word
/// query `line 40` + Enter, then `G`: the matching follow-up stays, the other goes.
#[tokio::test]
async fn live_seed_two_word_query_keeps_the_matching_follow_up_turn_at_the_end() {
    let body = (1..=60)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let items = vec![
        Item::new(
            "ri_open",
            rustain::domain::models::SemanticMessageType::ArchitectureUpdate,
            &body,
        ),
        Item::new(
            "ri_filter_3",
            rustain::domain::models::SemanticMessageType::ArchitectureUpdate,
            "line 3",
        )
        .reply_to("ri_open"),
        Item::new(
            "ri_filter_40",
            rustain::domain::models::SemanticMessageType::ArchitectureUpdate,
            "line 40",
        )
        .reply_to("ri_open"),
    ];
    let mut rig = Rig::plain(Workspace::with(&items).await, 80, 24).await;
    rig.view("ri_open").await;
    rig.paint();
    rig.key(Key::Char('G'));
    rig.paint();
    let unfiltered = painted_body(&mut rig).join("\n");
    assert!(
        unfiltered.contains("ri_filter_3 ·") && unfiltered.contains("ri_filter_40 ·"),
        "{unfiltered}"
    );
    rig.key(Key::Char('g'));
    rig.paint();
    type_query(&mut rig, "line 40");
    rig.key(Key::Enter);
    rig.paint();
    rig.key(Key::Char('G'));
    rig.paint();
    let filtered = painted_body(&mut rig).join("\n");
    assert!(
        filtered.contains("ri_filter_40 ·"),
        "matching follow-up kept: {filtered}"
    );
    assert!(!filtered.contains("ri_filter_3 ·"), "{filtered}");
}

/// 8B-02: `G` sent right after the field/filter transition, with NO paint in between, still
/// reaches the end once the next paint has the live dimensions.
#[tokio::test]
async fn tail_right_after_the_filter_transition_without_a_paint_reaches_the_end() {
    let body = (1..=60)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let kind = rustain::domain::models::SemanticMessageType::ArchitectureUpdate;
    let items = vec![
        Item::new("ri_open", kind, &body),
        Item::new("ri_filter_3", kind, "line 3").reply_to("ri_open"),
        Item::new("ri_filter_40", kind, "line 40").reply_to("ri_open"),
    ];
    let mut rig = Rig::plain(Workspace::with(&items).await, 80, 24).await;
    rig.view("ri_open").await;
    rig.paint();
    rig.key(Key::Char('g'));
    rig.paint();
    type_query(&mut rig, "line 40");
    rig.paint();
    rig.key(Key::Enter);
    rig.key(Key::Char('G'));
    rig.paint();
    assert_last_row_is_the_end(&rig);
    // Secondary: the matching follow-up is the last turn of the window.
    assert!(body_rows(&rig).join("\n").contains("line 40"));
}

/// The painted body's last row is the layout's last row: the jump-to-end resolved against the
/// live fit. (`painted_body` presses `g` and pages, so it cannot see a stale position.)
fn assert_last_row_is_the_end(rig: &Rig) {
    let (rows, viewport) = rig.fit();
    assert!(rows > viewport, "positive control: the body scrolls");
    let cache = rig.inspector().layout.borrow();
    let last: String = cache.body[rows - 1]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let shown = body_rows(rig);
    assert_eq!(shown[viewport - 1], last.trim_end(), "{shown:?}");
}

/// 8B-02 (growing direction): clearing a kept filter restores many rows; `G` sent right after
/// the `Esc`, before any paint, must still land on the real last rows (a stale finite position
/// computed from the filtered layout would stop short).
#[tokio::test]
async fn tail_right_after_clearing_the_filter_without_a_paint_reaches_the_real_end() {
    let body = (1..=60)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let kind = rustain::domain::models::SemanticMessageType::ArchitectureUpdate;
    let items = vec![
        Item::new("ri_open", kind, &body),
        Item::new("ri_filter_3", kind, "line 3").reply_to("ri_open"),
        Item::new("ri_filter_40", kind, "line 40").reply_to("ri_open"),
    ];
    let mut rig = Rig::plain(Workspace::with(&items).await, 80, 24).await;
    rig.view("ri_open").await;
    rig.paint();
    type_query(&mut rig, "line 40");
    rig.key(Key::Enter);
    rig.paint();
    rig.key(Key::Esc);
    rig.key(Key::Char('G'));
    rig.paint();
    assert_last_row_is_the_end(&rig);
}
