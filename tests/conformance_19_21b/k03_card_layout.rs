use ratatui::layout::Rect;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::{
    app::InputAction,
    decision_queue::DecisionEffect,
    layout,
    presentation::geometry::{frame_rect, modal_region},
};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{ResponseMode, StreamingState};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

#[tokio::test]
async fn story21_6_overflow_aliases_scroll_in_frame_and_send_once() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let aliases = (0..12)
        .map(|n| format!("member-{n:02}-dev"))
        .collect::<Vec<_>>();
    let mut f =
        ProposalFixture::start(aliases.iter().map(|alias| host.peer(alias)).collect()).await;
    host.reset_counts();
    let result = f.submit(json!({"recipients": aliases, "type": "architecture_update", "body": "Exact overflow proposal."})).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    let mut rows = f.paint(60, 16);
    for _ in 0..40 {
        assert!(matches!(
            f.key(DomainInputEvent::SpecialKey(DomainKey::PageDown))
                .await,
            InputAction::Consumed
        ));
        rows.extend(f.paint(60, 16));
    }
    let text = rows
        .iter()
        .flat_map(|row| row.chars().filter(|c| !c.is_whitespace()))
        .collect::<String>();
    for alias in &aliases {
        assert!(text.contains(alias), "missing {alias}: {rows:#?}");
    }
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(f.state.team_send_blocks.is_empty());
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Decision(DecisionEffect::TeamProposalSend)
    ));
    f.settle(aliases.len()).await;
    assert_eq!(
        host.counts(),
        HttpCounts {
            get: 0,
            post: aliases.len()
        }
    );
    host.stop().await;
}

#[tokio::test]
async fn story21_6_review_uses_frame_without_reserving_chat() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let area = Rect::new(0, 0, 80, 24);
    let before = layout::compute_layout(area, &f.state.theme, "", 2, false).unwrap();
    f.conversation.title = "OWNER_TAB".into();
    f.tabs.active_tab_mut().conversation = f.conversation.clone();
    f.tabs.create_tab();
    f.tabs.active_tab_mut().conversation.title = "OTHER_TAB".into();
    f.tabs.switch_to_prev();
    let result = f.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":"Readable current review."})).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.paint(80, 24);
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::Consumed
    ));
    let rows = f.paint(80, 24);
    let after = layout::compute_layout(area, &f.state.theme, "", 2, false).unwrap();
    assert_eq!(before.chat_pane, after.chat_pane);
    assert_eq!(
        frame_rect(modal_region(&after, area)),
        modal_region(&after, area)
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Readable current review.")),
        "{rows:#?}"
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn story21_6_frame_rect_matches_painted_height_at_all_widths() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    f.submit(
        json!({"recipients":["lena-po"],"type":"architecture_update","body":"Geometry witness."}),
    )
    .await;
    for width in [60, 80, 130] {
        let area = Rect::new(0, 0, width, 24);
        let shell = layout::compute_layout(area, &f.state.theme, "", 1, false).unwrap();
        let rect = frame_rect(modal_region(&shell, area));
        let rows = f.paint(width, 24);
        let top = &rows[rect.y as usize];
        let bottom = &rows[(rect.bottom() - 1) as usize];
        assert!(top.contains("Decision:"), "{rect:?}: {rows:#?}");
        assert_eq!(
            top.chars().nth(rect.x as usize),
            Some('┌'),
            "{rect:?}: {rows:#?}"
        );
        assert_eq!(
            bottom.chars().nth(rect.x as usize),
            Some('└'),
            "{rect:?}: {rows:#?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Dismiss")),
            "{rect:?}: {rows:#?}"
        );
    }
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_monitor_column_keeps_a_proposal_decision_surface() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    f.submit(
        json!({"recipients":["lena-po"],"type":"architecture_update","body":"Monitor draft."}),
    )
    .await;
    f.state.monitor = true;
    let rows = f.paint(130, 30);
    assert!(
        rows.iter().any(|row| row.contains("Monitor draft.")),
        "{rows:#?}"
    );
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::Decision(DecisionEffect::TeamProposalDismiss)
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn story21_6_overflow_review_scrolls_inside_frame() {
    let live_text = (0..60)
        .map(|n| format!("LIVE_STREAM_{n}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    for text in [String::new(), live_text] {
        let host =
            ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
        let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
        host.reset_counts();
        let body = (0..20)
            .map(|n| format!("STREAM_REVIEW_ROW_{n:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        f.submit(json!({"recipients":["lena-po"],"type":"architecture_update","body":body}))
            .await;
        f.paint(60, 16);
        assert!(matches!(
            f.key(DomainInputEvent::KeyPress('r')).await,
            InputAction::Consumed
        ));
        let streaming = StreamingState {
            is_streaming: true,
            current_text_buffer: text,
            ..Default::default()
        };
        let mut pages = f.paint_streaming(60, 16, &streaming);
        for _ in 0..40 {
            assert!(matches!(
                f.key(DomainInputEvent::SpecialKey(DomainKey::PageDown))
                    .await,
                InputAction::Consumed
            ));
            pages.extend(f.paint_streaming(60, 16, &streaming));
        }
        for index in 0..20 {
            assert!(
                pages
                    .iter()
                    .any(|row| row.contains(&format!("STREAM_REVIEW_ROW_{index:02}"))),
                "row {index}: {pages:#?}"
            );
        }
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
        host.stop().await;
    }
}
