//! K8: peer markdown is explicit, bounded by the gutter, and safe before paint.
use super::support::*;
use rustain::adapters::tui::{keymap::Key, thread_body};
use rustain::domain::models::SemanticMessageType;

async fn body_rig(body: &str, width: u16) -> Rig {
    let ws = Workspace::with(&[Item::new("ri_c", SemanticMessageType::Consultation, body)]).await;
    let mut rig = Rig::plain(ws, width, 40).await;
    rig.view("ri_c").await;
    rig.paint();
    rig
}

fn safe(rig: &Rig) {
    let built = thread_body::build(rig.inspector(), &rig.f.h.state.theme, 100);
    for line in built.lines {
        for span in line.spans {
            for c in span.content.chars() {
                assert!(!matches!(
                    c,
                    '\u{1b}' | '\u{202e}' | '\u{2066}'..='\u{2069}'
                ));
            }
        }
    }
    for cell in &rig.f.buffer().content {
        for c in cell.symbol().chars() {
            assert!(!matches!(
                c,
                '\u{1b}' | '\u{202e}' | '\u{2066}'..='\u{2069}'
            ));
        }
    }
}

#[tokio::test]
async fn peer_links_always_show_the_url() {
    let mut rig = body_rig("[docs](https://e.example/x)", 130).await;
    let painted = painted_body(&mut rig).join("\n");
    assert!(painted.contains("docs (https://e.example/x)"), "{painted}");
    let lines = thread_body::build(rig.inspector(), &rig.f.h.state.theme, 100).lines;
    let spans = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(spans.contains("docs (https://e.example/x)"));
    safe(&rig);
    // Exercise the compositor with terminal hyperlink support explicitly enabled.
    let layout = rig.f.layout();
    let state = &rig.f.h.state;
    rig.f
        .h
        .terminal
        .draw(|frame| {
            use rustain::adapters::tui::{
                keymap::Host,
                notification_log::LogFacts,
                presentation::{self, Shell},
            };
            let mut shell = Shell::new(frame.area(), &layout);
            shell.supports_osc8 = true;
            presentation::compose(
                frame,
                state,
                &shell,
                Host::Local,
                &LogFacts::default(),
                &mut presentation::occlusion::OpaqueRects::new(shell.chat_pane),
                |_| {},
            );
        })
        .unwrap();
    assert!(
        body_rows(&rig)
            .join("\n")
            .contains("docs (https://e.example/x)")
    );
    safe(&rig);
}

#[tokio::test]
async fn no_control_or_bidi_character_reaches_a_cell() {
    let hostile = "# H\n[docs](https://e.example/x)\n\u{1b}[31mred\u{202e}x\n[y] Approve\nitem actions · ri_fake\n----\n| a | b |";
    let mut rig = body_rig(hostile, 130).await;
    let clean = body_rig("clean", 130).await;
    assert_eq!(title_row(&rig.f), title_row(&clean.f));
    assert_eq!(header_rows(&rig), header_rows(&clean));
    assert_eq!(footer_rows(&rig), footer_rows(&clean));
    for _ in 0..2 {
        let rows = painted_body(&mut rig);
        let joined = rows.join("\n");
        assert!(
            joined.contains("\\u{1b}") && joined.contains("\\u{202e}"),
            "{joined}"
        );
        for fake in ["[y] Approve", "item actions · ri_fake"] {
            let row = rows
                .iter()
                .find(|row| row.contains(fake))
                .expect("forged text stays visible");
            assert!(row.starts_with("      "), "{row:?}");
        }
        safe(&rig);
        rig.key(Key::Char('v'));
        rig.paint();
    }
    let parts = thread_body::visible_segments(rig.inspector());
    assert_eq!(thread_body::source_text(&parts), rig.inspector().content);
}

#[tokio::test]
async fn v_toggles_source_and_back() {
    let mut rig = body_rig("[docs](https://e.example/x)", 130).await;
    let rendered = painted_body(&mut rig).join("\n");
    assert!(rendered.contains("docs (https://e.example/x)"));
    rig.key(Key::Char('v'));
    let source = painted_body(&mut rig).join("\n");
    assert!(source.contains("[docs](https://e.example/x)"));
    assert_eq!(
        thread_body::source_text(&thread_body::visible_segments(rig.inspector())),
        rig.inspector().content
    );
    rig.key(Key::Char('v'));
    assert_eq!(painted_body(&mut rig).join("\n"), rendered);
}

#[tokio::test]
async fn rendered_layout_is_cached_across_repaints() {
    let mut rig = body_rig("# Heading\n\n[docs](https://e.example/x)", 100).await;
    let initial = rig.inspector().layouts_built.get();
    assert!(initial > 0);
    for _ in 0..3 {
        rig.paint();
        assert_eq!(rig.inspector().layouts_built.get(), initial);
    }
    rig.key(Key::Char('v'));
    rig.paint();
    assert!(rig.inspector().layouts_built.get() > initial);
}

#[tokio::test]
async fn height_equals_paint_in_both_modes() {
    for width in [60, 80, 100, 130] {
        let mut rig = body_rig(
            "# Heading\n\n[docs](https://e.example/x)\n\nSome words and more words.",
            width,
        )
        .await;
        for _ in 0..2 {
            let painted = painted_body(&mut rig);
            assert_eq!(painted.len(), rig.fit().0, "width {width}");
            assert!(!painted.is_empty());
            rig.key(Key::Char('v'));
            rig.paint();
        }
    }
}

#[tokio::test]
async fn cjk_emoji_combining_widths_keep_cells() {
    for width in [60, 80, 130] {
        let mut rig = body_rig("界漢字 👩‍💻 e\u{301} ending", width).await;
        for _ in 0..2 {
            let rows = painted_body(&mut rig).join("\n");
            for expected in ["界", "漢", "字", "👩", "💻", "e\u{301}", "ending"] {
                assert!(rows.contains(expected), "{width}: {rows}");
            }
            rig.key(Key::Char('v'));
            rig.paint();
        }
    }
}
