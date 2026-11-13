//! K8 (AC7, DF-297, DF-293): an assistant turn with nothing to paint paints no
//! bare gutter rail and counts no row; an Assistant message hosting a PlanCard
//! paints the card after the turn in both hosts and its height is its paint.
use std::sync::Arc;

use rustain::adapters::tui::{theme::Theme, widgets::plan_card::render_plan_card_lines};
use rustain::domain::models::{
    ContentBlockType, InvocationStatus, MessageRole, PartId, ProviderReplayItem, StopReason,
    StreamingState, ToolOutput, Turn, TurnPart,
    plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus},
};

use super::support::{Fixture, cached_height, call, part, row_starting, rows};
use crate::conformance_21_12_markdown::support::{
    Pane, paint_attached, paint_local, rows as pane_rows,
};
use crate::render_helpers::{make_conversation, make_msg, make_turn, prose};

fn replay_part(turn: &mut Turn) {
    turn.push_part(|id| TurnPart::ProviderReplay {
        id,
        item: ProviderReplayItem {
            provider: "p".into(),
            model: "m".into(),
            payload: Arc::new(serde_json::json!({"m": 1})),
            scope: None,
        },
    });
}

fn result_part(turn: &mut Turn) {
    turn.push_part(|id| TurnPart::ToolResult {
        id,
        refs: PartId(999),
        output: ToolOutput {
            content: "orphan".into(),
            is_error: false,
            diff: rustain::domain::models::WriteDiffState::NotAWrite,
            images: Vec::new(),
        },
    });
}

#[test]
fn a_turn_with_nothing_to_paint_has_no_rail_and_no_height() {
    let empty = Turn::new("fixture-model".into(), 0);
    let mut replay_only = Turn::new("fixture-model".into(), 0);
    replay_part(&mut replay_only);
    let mut result_only = Turn::new("fixture-model".into(), 0);
    result_part(&mut result_only);
    let mut both = Turn::new("fixture-model".into(), 0);
    replay_part(&mut both);
    result_part(&mut both);
    for (name, turn) in [
        ("empty", empty),
        ("replay only", replay_only),
        ("result only", result_only),
        ("both", both),
    ] {
        let f = Fixture::new(80, 24, turn);
        assert_eq!(cached_height(&f), 0, "{name}: layout height");
        assert_eq!(f.h.state.total_content_height, 0, "{name}: no rows at all");
        assert!(
            rows(&f).iter().all(|row| row != "│"),
            "{name}: no bare gutter: {:?}",
            rows(&f)
        );
    }
}

#[test]
fn a_turn_with_rows_keeps_its_trailing_rail() {
    // Control: one prose part keeps the rail (and counts it).
    let mut turn = Turn::new("fixture-model".into(), 0);
    part(&mut turn, "Only prose.");
    let f = Fixture::new(80, 24, turn);
    let prose_rows = rustain::adapters::tui::markdown::compute_height(
        "Only prose.",
        78,
        &Theme::dark(),
        &rustain::adapters::tui::markdown::RenderOptions::completed(),
    );
    assert_eq!(
        cached_height(&f),
        prose_rows + 1,
        "the prose rows plus the rail"
    );
    let painted = rows(&f);
    let at = row_starting(&painted, "│ Only prose.").expect("prose row");
    assert_eq!(painted[at + 1], "│");
    // A running tool keeps its rail too.
    let mut turn = Turn::new("fixture-model".into(), 0);
    call(
        &mut turn,
        "Bash",
        serde_json::json!({"command": "sleep 1"}),
        InvocationStatus::Running,
        None,
    );
    let f = Fixture::new(80, 24, turn);
    assert!(cached_height(&f) >= 3, "tool row, live rail, trailing rail");
    assert!(rows(&f).iter().any(|row| row == "│"));
}

#[test]
fn replay_beside_a_prose_part_adds_no_row() {
    let mut plain = Turn::new("fixture-model".into(), 0);
    part(&mut plain, "Answer.");
    let mut with_replay = Turn::new("fixture-model".into(), 0);
    part(&mut with_replay, "Answer.");
    replay_part(&mut with_replay);
    assert_eq!(
        cached_height(&Fixture::new(80, 24, plain)),
        cached_height(&Fixture::new(80, 24, with_replay))
    );
}

fn plan_task(number: u32, title: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_string(),
        description: String::new(),
        depends_on: vec![],
        status: PlanTaskStatus::Pending,
        started_at_ms: None,
        completed_at_ms: None,
        result: None,
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

fn plan() -> Plan {
    Plan {
        id: "k8-plan".to_string(),
        title: "Ship the polish".to_string(),
        tasks: vec![
            plan_task(1, "Write the reducer"),
            plan_task(2, "Paint the fold"),
        ],
        estimated_effort: None,
        status: PlanStatus::Pending,
        created_at: 0,
        resolved_at: None,
        host_message_id: Some("k".to_string()),
        origin_restrictions: None,
    }
}

/// An assistant message `k` with a committed turn; hosts the plan when asked.
fn hosting(with_plan: bool) -> rustain::domain::models::Conversation {
    let turn = make_turn("k", vec![prose("The answer.")], Some(StopReason::EndTurn));
    let mut message = make_msg("k", MessageRole::Assistant);
    let mut conversation = make_conversation(vec![message.clone()], vec![turn]);
    if with_plan {
        message.content_blocks.push(ContentBlockType::PlanCard);
        conversation.messages = vec![message];
        conversation.plans.insert("k8-plan".to_string(), plan());
    }
    conversation
}

fn paint(conversation: &rustain::domain::models::Conversation, attached: bool) -> Pane {
    if attached {
        paint_attached(
            conversation,
            &StreamingState::default(),
            &Theme::dark(),
            80,
            40,
        )
    } else {
        paint_local(
            conversation,
            None,
            &StreamingState::default(),
            None,
            &Theme::dark(),
            80,
            40,
        )
    }
}

#[test]
fn an_assistant_plan_card_paints_after_the_turn_in_both_hosts_and_height_is_paint() {
    let card_rows = render_plan_card_lines(&plan(), &Theme::dark(), 80, false).len();
    assert!(card_rows > 2);
    for attached in [false, true] {
        let bare = paint(&hosting(false), attached);
        let hosted = paint(&hosting(true), attached);
        assert_eq!(
            hosted.total_height,
            bare.total_height + card_rows,
            "attached {attached}: the card adds exactly its own rows"
        );
        let rows = pane_rows(&hosted.buffer);
        let turn = rows
            .iter()
            .position(|row| row.contains("The answer."))
            .expect("the turn paints");
        let card = rows
            .iter()
            .position(|row| row.contains("Ship the polish"))
            .expect("the card paints");
        assert!(
            card > turn,
            "attached {attached}: card after turn: {rows:?}"
        );
        let last_content = rows.iter().rposition(|row| !row.is_empty()).unwrap();
        assert!(
            last_content < hosted.total_height,
            "attached {attached}: nothing paints past the measured height"
        );
        // Control: without the hosted card the title is absent.
        assert!(
            pane_rows(&bare.buffer)
                .iter()
                .all(|r| !r.contains("Ship the polish"))
        );
    }
}
