//! K18 / AC21: one stable conversation row, with first-settlement notices.
use rustain::adapters::tui::{
    handlers::team_command::{TeamSendRow, render_team_send_summary},
    state::TuiState,
    widgets::feedback_block::render_feedback_lines,
};
use rustain::domain::models::{
    FeedbackBlock, FeedbackLevel, RecipientOutcome, SemanticMessageType, SendCause, tab::TabManager,
};
use rustain::infrastructure::runtime::transparency_bridge::team_send_settled;

fn row(alias: &str, kind: SemanticMessageType) -> TeamSendRow {
    TeamSendRow {
        alias: alias.into(),
        outcome: None,
        message_type: kind,
    }
}
fn setup(rows: Vec<TeamSendRow>) -> (TuiState, TabManager, String) {
    let tabs = TabManager::default();
    let conversation = tabs.active_tab().conversation.id.clone();
    let mut state = TuiState::new(60, 40);
    let message = render_team_send_summary(&rows);
    state.team_send_blocks.insert("team-send-1".into(), rows);
    state.feedback_blocks.insert(
        "team-send-1".into(),
        FeedbackBlock {
            id: "team-send-1".into(),
            level: FeedbackLevel::Info,
            message,
            seq: 0,
        },
    );
    (state, tabs, conversation)
}

#[test]
fn settlement_rewrites_the_one_line_in_place() {
    let (mut state, mut tabs, conversation) = setup(vec![
        row("lena-po", SemanticMessageType::ArchitectureUpdate),
        row("jun-dev", SemanticMessageType::ArchitectureUpdate),
    ]);
    let initial = &state.feedback_blocks["team-send-1"];
    assert_eq!(initial.id, "team-send-1");
    assert!(
        initial.message.contains("lena-po sending…")
            && initial.message.contains("jun-dev sending…")
    );
    assert_eq!(initial.message.lines().count(), 1);
    team_send_settled(
        &conversation,
        &mut state,
        &mut tabs,
        &conversation,
        "team-send-1",
        0,
        RecipientOutcome::AwaitingApproval,
    );
    assert_eq!(state.feedback_blocks.len(), 1);
    let block = &state.feedback_blocks["team-send-1"];
    assert_eq!(block.id, "team-send-1");
    assert_eq!(block.message.lines().count(), 1);
    assert!(
        block.message.contains("lena-po awaiting their approval")
            && block.message.contains("jun-dev sending…")
    );
    team_send_settled(
        &conversation,
        &mut state,
        &mut tabs,
        &conversation,
        "team-send-1",
        1,
        RecipientOutcome::Delivered,
    );
    assert_eq!(state.feedback_blocks.len(), 1);
    assert!(
        state.feedback_blocks["team-send-1"]
            .message
            .contains("● jun-dev delivered")
    );
}

#[test]
fn causes_and_retro_notices_are_posted_once() {
    let (mut state, mut tabs, conversation) = setup(vec![
        row("ada", SemanticMessageType::RetroRequest),
        row("bob", SemanticMessageType::RetroRequest),
    ]);
    let outcome = RecipientOutcome::Unreachable {
        cause: Some(SendCause::PeerInfluenced("network\u{1b}broken".into())),
    };
    team_send_settled(
        &conversation,
        &mut state,
        &mut tabs,
        &conversation,
        "team-send-1",
        0,
        outcome.clone(),
    );
    team_send_settled(
        &conversation,
        &mut state,
        &mut tabs,
        &conversation,
        "team-send-1",
        0,
        outcome,
    );
    let notices: Vec<_> = state.notifications.records().collect();
    assert_eq!(notices.len(), 2);
    assert_eq!(
        notices
            .iter()
            .filter(|n| n.message.contains("ada unreachable:"))
            .count(),
        1
    );
    assert_eq!(
        notices
            .iter()
            .filter(|n| n.message.contains("Read the report"))
            .count(),
        1
    );
    assert_eq!(
        notices
            .iter()
            .find(|n| n.message.contains("ada unreachable:"))
            .unwrap()
            .severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Warning
    );
    assert_eq!(
        notices
            .iter()
            .find(|n| n.message.contains("Read the report"))
            .unwrap()
            .severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Info
    );
    assert!(notices.iter().all(|n| !n.message.contains('\u{1b}')));
    team_send_settled(
        &conversation,
        &mut state,
        &mut tabs,
        &conversation,
        "team-send-1",
        1,
        RecipientOutcome::Delivered,
    );
    assert!(
        state.feedback_blocks["team-send-1"]
            .message
            .contains("● bob sent")
    );
    assert_eq!(state.notifications.records().count(), 2);
}

#[test]
fn asked_question_posts_one_warning_on_first_settlement() {
    let (mut state, mut tabs, conversation) = setup(vec![
        row("ada", SemanticMessageType::Consultation),
        row("bob", SemanticMessageType::Consultation),
    ]);
    let outcome = RecipientOutcome::AskedQuestion {
        task_id: "task-1".into(),
    };
    for _ in 0..2 {
        team_send_settled(
            &conversation,
            &mut state,
            &mut tabs,
            &conversation,
            "team-send-1",
            0,
            outcome.clone(),
        );
    }
    let notices: Vec<_> = state.notifications.records().collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].severity,
        rustain::adapters::tui::notifications::NoticeSeverity::Warning
    );
    assert!(notices[0].message.contains("asked a question"));
    assert!(state.feedback_blocks["team-send-1"].message.contains("ada"));
}

#[tokio::test]
async fn summary_wraps_at_sixty_without_losing_aliases() {
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Widget},
    };
    let aliases = ["lena-po", "jun-dev", "sasha-architect", "marin-reviewer"];
    let (state, _, _) = setup(
        aliases
            .iter()
            .map(|a| row(a, SemanticMessageType::ArchitectureUpdate))
            .collect(),
    );
    let block = &state.feedback_blocks["team-send-1"];
    let lines = render_feedback_lines(block, 60, &state.theme);
    assert!(lines.len() > 1);
    let area = Rect::new(0, 0, 60, lines.len() as u16);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(lines).render(area, &mut buffer);
    let painted: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    for alias in aliases {
        assert!(painted.contains(alias), "{alias}: {painted}");
    }
    let mut rig = super::support::Rig::plain(super::support::Workspace::new().await, 60, 40).await;
    rig.f
        .h
        .state
        .feedback_blocks
        .insert("team-send-1".into(), block.clone());
    rig.paint();
    let screen: String = rig
        .f
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    for alias in aliases {
        assert!(screen.contains(alias), "{alias}: {screen}");
    }
}

#[tokio::test]
async fn summary_survives_real_tab_save_and_load() {
    use super::support::{Rig, Workspace};
    use rustain::infrastructure::runtime::event_loop;
    let mut rig = Rig::plain(Workspace::new().await, 130, 30).await;
    let rows = vec![row("lena-po", SemanticMessageType::ArchitectureUpdate)];
    let message = render_team_send_summary(&rows);
    rig.f
        .h
        .state
        .team_send_blocks
        .insert("team-send-1".into(), rows);
    rig.f.h.state.feedback_blocks.insert(
        "team-send-1".into(),
        FeedbackBlock {
            id: "team-send-1".into(),
            level: FeedbackLevel::Info,
            message: message.clone(),
            seq: 0,
        },
    );
    let session = rig.f.tabs.active_tab().session.clone();
    let queue = rig.f.tabs.active_tab().turn_queue.clone();
    event_loop::save_active_tab(
        &mut rig.f.tabs,
        &rig.f.h.conversation,
        &rig.f.h.streaming,
        &session,
        &rig.f.h.state,
        &queue,
    );
    rig.f.tabs.create_tab();
    rig.f.load();
    assert!(!rig.f.h.state.feedback_blocks.contains_key("team-send-1"));
    rig.f.tabs.switch_to_prev();
    rig.f.load();
    assert_eq!(
        rig.f.h.state.feedback_blocks["team-send-1"].message,
        message
    );
    rig.paint();
    let painted: String = rig.f.buffer().content.iter().map(|c| c.symbol()).collect();
    assert!(painted.contains("lena-po"), "{painted}");
}
