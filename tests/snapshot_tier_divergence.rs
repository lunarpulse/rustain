//! Locks the Tier-1 vs Tier-2 visual divergence — the durable differentiator
//! vs codex/gemini-cli/opencode (UX-DR-COLLAPSED-TIER, ADR-16-01 §Consequences).
//! Re-record only when ADR-16-01 §Q3 LLM-polish lands.

use rustain::adapters::tui::state::TabRenderState;
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::turn::{PartId, TurnPart};
use rustain::domain::models::{
    ChatMessage, Conversation, InvocationStatus, MessageRole, StopReason, StreamingState,
    SummaryTier, Turn, ViewState,
};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use std::collections::{BTreeMap, HashMap};

fn render_to_string(
    conversation: &Conversation,
    open_turn: Option<&Turn>,
    view_state: &ViewState,
    clock: &dyn rustain::domain::clock::Clock,
    width: u16,
    height: u16,
) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let _ = terminal.draw(|frame| {
        let area = Rect::new(0, 0, width, height);
        let streaming = StreamingState::default();
        let mut tab_render_state = TabRenderState::default();
        let _ = chat_pane::render_with_search(
            frame,
            area,
            conversation,
            open_turn,
            &streaming,
            view_state,
            clock,
            0,
            true,
            &Theme::dark(),
            &mut tab_render_state,
            &HashMap::new(),
            &BTreeMap::new(),
            None,
            None,
            &[],
            &[],
            None,
        );
    });
    use ratatui::buffer::Buffer;
    let buffer: Buffer = terminal.backend().buffer().clone();
    let mut lines: Vec<String> = Vec::new();
    for y in 0..buffer.area().height {
        let row: String = (0..buffer.area().width)
            .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
            .collect();
        let trimmed = row.trim_end().to_string();
        if !trimmed.is_empty() {
            lines.push(trimmed);
        }
    }
    lines.join("\n")
}

#[test]
fn tier1_and_tier2_render_different_collapsed_lines() {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);

    // Build a turn: 1 prose + 3 Read invocations with shared path prefix
    let mut turn = Turn::new("claude".into(), 1_700_000_000_000);
    turn.id = rustain::domain::models::TurnId("dt".to_string());
    turn.push_part(move |_id| TurnPart::Prose {
        id: PartId(0),
        text: "I need to read the auth files.".to_string(),
    });
    for path in &[
        "src/auth/login.rs",
        "src/auth/jwt.rs",
        "src/auth/session.rs",
    ] {
        let path = path.to_string();
        turn.push_part(move |id| TurnPart::ToolInvocation {
            id,
            tool: "Read".to_string(),
            args: serde_json::json!({"file_path": path}),
            status: InvocationStatus::Success,
            started_at: 1_700_000_000_000,
            ended_at: Some(1_700_000_005_000),
        });
    }
    turn.stop_reason = Some(StopReason::EndTurn);

    let msg = ChatMessage {
        id: "dt".to_string(),
        role: MessageRole::Assistant,
        content: String::new(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
    };
    let conversation = Conversation {
        id: "test-conv-divergence".to_string(),
        title: "Test".to_string(),
        messages: vec![msg],
        turns: vec![turn.clone()],
        created_at: 1_700_000,
        updated_at: 1_700_000,
        last_response_at: Some(1_700_000),
        session_id: Some("test-session".to_string()),
        usage: None,
        plans: HashMap::new(),
        fork_source: None,
    };

    // Tier-1 render
    let mut vs_tier1 = ViewState::default();
    vs_tier1.collapsed.insert(turn.id.clone(), true);
    vs_tier1.summary_tier = SummaryTier::Tier1;
    let tier1_str = render_to_string(&conversation, None, &vs_tier1, &clock, 80, 20);
    insta::assert_snapshot!("tier1", tier1_str);

    // Tier-2 render
    let mut vs_tier2 = ViewState::default();
    vs_tier2.collapsed.insert(turn.id.clone(), true);
    vs_tier2.summary_tier = SummaryTier::Tier2;
    let tier2_str = render_to_string(&conversation, None, &vs_tier2, &clock, 80, 20);
    insta::assert_snapshot!("tier2", tier2_str);

    // Durable differentiator lock: the two must not be equal
    assert_ne!(
        tier1_str, tier2_str,
        "Tier-1 and Tier-2 collapsed lines must differ — durable differentiator lock"
    );

    // Sanity check directionality
    assert!(
        tier1_str.contains("3 tools"),
        "Tier-1 must contain '3 tools': {tier1_str}"
    );
    assert!(
        tier2_str.contains("3 reads"),
        "Tier-2 must contain '3 reads': {tier2_str}"
    );
}
