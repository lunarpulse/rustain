use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::tui::{
    app::InputAction,
    layout,
    widgets::{chat_pane, tab_bar, team_proposal_card},
};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{ResponseMode, StreamingState, TeamProposalDecision, ViewState};
use serde_json::json;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

#[tokio::test]
async fn k03_overflow_keeps_complete_aliases_and_gates_send_until_footer() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let aliases = (0..12)
        .map(|index| format!("member-{index:02}-dev"))
        .collect::<Vec<_>>();
    let mut f =
        ProposalFixture::start(aliases.iter().map(|alias| host.peer(alias)).collect()).await;
    host.reset_counts();
    let result = f
        .submit(json!({
            "recipients": aliases, "type": "architecture_update", "body": "Exact overflow proposal."
        }))
        .await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    let first = f.paint(60, 16);
    assert!(
        first.iter().any(|row| row.contains("PgUp/PgDn")),
        "{first:#?}"
    );
    assert!(f.state.team_proposal_visible.is_some());
    assert!(f.state.team_proposal_scroll_max > 0);
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::Consumed
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    assert!(f.state.team_send_blocks.is_empty());

    let mut shown = first;
    while f.state.team_proposal_scroll < f.state.team_proposal_scroll_max {
        let previous = f.state.team_proposal_scroll;
        assert!(matches!(
            f.key(DomainInputEvent::SpecialKey(DomainKey::PageDown))
                .await,
            InputAction::Consumed
        ));
        assert!(f.state.team_proposal_scroll > previous);
        if f.state.team_proposal_scroll == f.state.team_proposal_scroll_max {
            assert!(
                matches!(
                    f.key(DomainInputEvent::KeyPress('y')).await,
                    InputAction::Consumed
                ),
                "a queued page must not authorize Send before its footer is painted"
            );
            assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
        }
        shown.extend(f.paint(60, 16));
    }
    let text = shown
        .iter()
        .filter_map(|row| row.split('║').nth(1))
        .flat_map(|row| row.chars().filter(|character| !character.is_whitespace()))
        .collect::<String>();
    for alias in &aliases {
        assert!(
            text.contains(alias),
            "complete alias missing: {alias}: {shown:#?}"
        );
    }
    let last = f.paint(60, 16);
    assert!(
        last.iter().any(|row| row.contains("[n] Dismiss")),
        "{last:#?}"
    );
    f.key(DomainInputEvent::SpecialKey(DomainKey::PageUp)).await;
    f.paint(60, 16);
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('r')).await,
        InputAction::TeamProposal { decision: None, .. }
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    f.paint(60, 16);
    while f.state.team_proposal_scroll < f.state.team_proposal_scroll_max {
        f.key(DomainInputEvent::SpecialKey(DomainKey::PageDown))
            .await;
        f.paint(60, 16);
    }
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('y')).await,
        InputAction::TeamProposal {
            decision: Some(TeamProposalDecision::Send),
            ..
        }
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
async fn k03_review_keeps_tab_bar_visible_above_chat() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    let result = f.submit(json!({
        "recipients": ["lena-po"], "type": "architecture_update", "body": "Readable current review."
    })).await;
    assert!(
        result.starts_with("Proposal shown to the operator."),
        "{result}"
    );
    f.conversation.title = "OWNER_TAB".into();
    f.tabs.active_tab_mut().conversation = f.conversation.clone();
    f.tabs.create_tab();
    f.tabs.active_tab_mut().conversation.title = "OTHER_TAB".into();
    f.tabs.switch_to_prev();
    f.paint(80, 24);
    f.key(DomainInputEvent::KeyPress('r')).await;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let mut layout = layout::compute_layout(
                area,
                &f.state.theme,
                "",
                f.tabs.tab_count(),
                false,
                f.state.density_mode,
            )
            .unwrap();
            let card = team_proposal_card::CardView::from_state(&f.state).prepare(
                &mut layout,
                area,
                &f.state.theme,
            );
            tab_bar::render_tab_bar(
                &f.tabs,
                f.tabs.active_tab_index(),
                layout.tab_bar.unwrap(),
                frame.buffer_mut(),
                &f.state.theme,
            );
            let render = f
                .state
                .tab_render_states
                .entry(f.state.active_tab_id)
                .or_default();
            chat_pane::render_with_search(
                frame,
                layout.chat_pane,
                &f.conversation,
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &rustain::domain::clock::SystemClock::default(),
                0,
                true,
                &f.state.theme,
                render,
                &f.state.tool_block_states,
                &f.state.feedback_blocks,
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                None,
                f.state.active_feedback_id.as_deref(),
            );
            team_proposal_card::render(card, frame.buffer_mut(), &f.state.theme);
        })
        .unwrap();
    let rows = terminal
        .backend()
        .buffer()
        .content
        .chunks(80)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>();
    assert!(
        rows[0].contains("OWNER_TAB") && rows[0].contains("OTHER_TAB"),
        "{rows:#?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("│ Readable current review.")),
        "{rows:#?}"
    );
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_dashboard_keeps_a_proposal_decision_surface() {
    let host = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut f = ProposalFixture::start(vec![host.peer("lena-po")]).await;
    host.reset_counts();
    f.submit(
        json!({"recipients":["lena-po"],"type":"architecture_update","body":"Dashboard draft."}),
    )
    .await;
    f.state.density_mode = rustain::domain::models::visual::DensityMode::Dashboard;
    let rows = f.paint(80, 24);
    assert!(
        rows.iter().any(|row| row.contains("Dashboard draft.")),
        "{rows:#?}"
    );
    assert!(matches!(
        f.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::TeamProposal {
            decision: Some(TeamProposalDecision::Dismiss),
            ..
        }
    ));
    assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
    host.stop().await;
}

#[tokio::test]
async fn k03_overflow_review_stays_readable_while_streaming_and_scrolled() {
    fn paint(f: &mut ProposalFixture, streaming: &StreamingState) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let mut layout = layout::compute_layout(
                    area,
                    &f.state.theme,
                    "",
                    1,
                    false,
                    f.state.density_mode,
                )
                .unwrap();
                let card = team_proposal_card::CardView::from_state(&f.state).prepare(
                    &mut layout,
                    area,
                    &f.state.theme,
                );
                let scroll_offset = f.state.scroll_offset();
                let auto_scroll = f.state.auto_scroll();
                let render = f
                    .state
                    .tab_render_states
                    .entry(f.state.active_tab_id)
                    .or_default();
                let result = chat_pane::render_with_search(
                    frame,
                    layout.chat_pane,
                    &f.conversation,
                    None,
                    streaming,
                    &ViewState::default(),
                    &rustain::domain::clock::SystemClock::default(),
                    scroll_offset,
                    auto_scroll,
                    &f.state.theme,
                    render,
                    &f.state.tool_block_states,
                    &f.state.feedback_blocks,
                    None,
                    None,
                    &[],
                    &[],
                    None,
                    None,
                    None,
                    None,
                    f.state.active_feedback_id.as_deref(),
                );
                f.state.viewport_height = layout.chat_pane.height;
                f.state.total_content_height = result.total_content_height;
                f.state.team_proposal_scroll_max =
                    card.as_ref().map_or(0, |card| card.scroll_max());
                f.state.team_proposal_viewport_rows =
                    card.as_ref().map_or(0, |card| card.viewport_rows());
                f.state.team_proposal_footer_visible =
                    card.as_ref().is_some_and(|card| card.footer_visible());
                f.state.team_proposal_visible =
                    team_proposal_card::render(card, frame.buffer_mut(), &f.state.theme);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(60)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }
    let live_text = (0..60)
        .map(|index| format!("LIVE_STREAM_{index}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    for text in [String::new(), live_text] {
        let host =
            ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
        let aliases = (0..12)
            .map(|index| format!("member-{index:02}-dev"))
            .collect::<Vec<_>>();
        let mut f =
            ProposalFixture::start(aliases.iter().map(|alias| host.peer(alias)).collect()).await;
        host.reset_counts();
        let body = (0..20)
            .map(|index| format!("STREAM_REVIEW_ROW_{index:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        f.submit(json!({"recipients":aliases,"type":"architecture_update","body":body}))
            .await;
        f.paint(60, 16);
        f.key(DomainInputEvent::KeyPress('r')).await;
        let streaming = StreamingState {
            is_streaming: true,
            current_text_buffer: text,
            ..Default::default()
        };
        let mut pages = paint(&mut f, &streaming);
        for _ in 0..20 {
            let action = f.key(DomainInputEvent::SpecialKey(DomainKey::PageUp)).await;
            let InputAction::MouseScroll(delta) = action else {
                panic!("Review boundary paging must scroll its transcript: {action:?}");
            };
            rustain::adapters::tui::handlers::scroll::handle_apply_scroll_intent(
                &mut f.tabs,
                &mut f.state,
                &f.conversation,
                delta,
            );
            pages.extend(paint(&mut f, &streaming));
        }
        for index in 0..20 {
            assert!(
                pages
                    .iter()
                    .any(|row| row.contains(&format!("STREAM_REVIEW_ROW_{index:02}"))),
                "full Review row {index} is inaccessible: {pages:#?}"
            );
        }
        assert_eq!(host.counts(), HttpCounts { get: 0, post: 0 });
        host.stop().await;
    }
}
