//! K13: horizontal block navigation is isolated from the thread pager.
use super::support::*;
use rustain::adapters::tui::{app::InputAction, keymap::Key};
use rustain::domain::models::SemanticMessageType;

fn wide_body() -> String {
    format!(
        "prose before block\n\n```text\n{}\n```\n\nprose after block",
        "0123456789".repeat(20)
    )
}
async fn wide_rig() -> Rig {
    let mut items = chain(60);
    items[59].content = wide_body();
    let mut rig = Rig::plain(Workspace::with(&items).await, 100, 40).await;
    rig.view(NEWEST).await;
    rig.paint();
    rig
}
async fn earlier(rig: &mut Rig) {
    assert_eq!(
        rig.key(Key::Char('p')),
        InputAction::ThreadPage { earlier: true }
    );
    rig.navigate(true).await;
    rig.paint();
    assert!(rig.inspector().omitted_after > 0);
}
fn show_block(rig: &mut Rig) {
    rig.key(Key::Char('g'));
    rig.paint();
    assert!(body_rows(rig).join("\n").contains("prose after block"));
}

#[tokio::test]
async fn enter_is_painted_only_when_a_wide_block_exists() {
    let mut wide = wide_rig().await;
    assert!(compact(&footer_rows(&wide).join("\n")).contains("Blockviewport"));
    wide.key(Key::Enter);
    wide.paint();
    assert!(wide.inspector().viewport);
    let ws = Workspace::with(&[Item::new(
        "ri_c",
        SemanticMessageType::Consultation,
        "plain narrow prose",
    )])
    .await;
    let mut narrow = Rig::plain(ws, 100, 40).await;
    narrow.view("ri_c").await;
    narrow.paint();
    assert!(!compact(&footer_rows(&narrow).join("\n")).contains("Blockviewport"));
    narrow.key(Key::Enter);
    narrow.paint();
    assert!(!narrow.inspector().viewport);
    assert!(
        painted_body(&mut narrow)
            .join("\n")
            .contains("plain narrow prose")
    );
}

#[tokio::test]
async fn l_never_pages_inside_the_viewport() {
    let mut rig = wide_rig().await;
    earlier(&mut rig).await;
    assert_eq!(
        rig.key(Key::Char('l')),
        InputAction::ThreadPage { earlier: false }
    );
    rig.navigate(false).await;
    rig.paint();
    earlier(&mut rig).await;
    show_block(&mut rig);
    let before = body_rows(&rig);
    let content = rig.inspector().content.clone();
    let omitted = rig.inspector().omitted_after;
    rig.key(Key::Enter);
    rig.paint();
    assert!(rig.inspector().viewport);
    for _ in 0..2 {
        assert!(matches!(
            rig.key(Key::Char('l')),
            InputAction::Consumed | InputAction::Ignored
        ));
        rig.paint();
        assert_eq!(rig.inspector().content, content);
        assert_eq!(rig.inspector().omitted_after, omitted);
    }
    let after = body_rows(&rig);
    assert_ne!(before, after, "wide block must actually move");
    for prose in ["prose before block", "prose after block"] {
        assert_eq!(
            before.iter().find(|r| r.contains(prose)),
            after.iter().find(|r| r.contains(prose))
        );
    }
    rig.key(Key::Esc);
    rig.paint();
    assert!(!rig.inspector().viewport);
    assert_eq!(
        rig.key(Key::Char('l')),
        InputAction::ThreadPage { earlier: false }
    );
    rig.navigate(false).await;
    rig.paint();
    assert_eq!(rig.inspector().omitted_after, 0);
}

#[tokio::test]
async fn h_0_dollar_edges() {
    let mut rig = wide_rig().await;
    show_block(&mut rig);
    rig.key(Key::Enter);
    rig.paint();
    let start = body_rows(&rig);
    rig.key(Key::Char('$'));
    rig.paint();
    let end = body_rows(&rig);
    assert_ne!(start, end);
    rig.key(Key::Char('$'));
    rig.paint();
    assert_eq!(body_rows(&rig), end);
    rig.key(Key::Char('h'));
    rig.paint();
    assert_ne!(body_rows(&rig), end);
    rig.key(Key::Char('0'));
    rig.paint();
    assert_eq!(body_rows(&rig), start);
    rig.key(Key::Char('h'));
    rig.paint();
    assert_eq!(body_rows(&rig), start);
}
