//! Whole-frame witnesses for the Mission Control surfaces at 80x24.
use super::support::*;
use crate::conformance_21_8_thread_inspector::support::Item;
use async_trait::async_trait;
use ratatui::layout::Rect;
use rustain::adapters::tui::app::{InputAction, submit_message_for_test};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::mission_control::McTab;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult};
use rustain::domain::models::{
    AgentId, ArtifactId, ArtifactKind, ArtifactRef, CapabilityTokenId, ContentHash,
    ConversationSummary, EvidenceArtifact, HostBinding, PermissionMode, ProvenanceTag,
    ReviewVerdict, RoomEvent, SemanticMessageType,
};
use rustain::domain::ports::{PatchReviewError, PatchReviewRecorder};
use rustain::domain::services::patch_review::MergeBackPolicy;
use rustain::domain::services::session_index::SessionIndex;
use rustain::infrastructure::runtime::agent_core::AgentCore;
use rustain::infrastructure::runtime::{
    artifact_bridge, history_bridge, mission_bridge, room_bridge, transparency_bridge,
};
use rustain::infrastructure::subagent::{NodeTree, SubagentSpool};
use std::sync::Arc;

/// Find the full-width frame in the paint, independent of the layout's frame rectangle.
fn whole_frame(rig: &Rig, title: &str) -> Vec<String> {
    assert_eq!(rig.f.buffer().area, Rect::new(0, 0, 80, 24));
    let mut rows = lines(rig);
    let top = rows
        .iter()
        .position(|row| {
            let Some(border) = row.strip_prefix('┌').and_then(|row| row.strip_suffix('┐'))
            else {
                return false;
            };
            let label = border.trim_end_matches('─');
            let child = label
                .strip_prefix(' ')
                .and_then(|label| label.strip_suffix(' '))
                .and_then(|label| label.rsplit_once(" › "));
            label.len() < border.len()
                && (label == title || child.is_some_and(|(_, current)| current == title))
        })
        .unwrap_or_else(|| panic!("missing continuous top border for {title}: {rows:?}"));
    let bottom = (top + 1..rows.len())
        .find(|&y| rows[y].starts_with('└'))
        .unwrap_or_else(|| panic!("missing bottom border for {title}: {rows:?}"));
    assert_eq!(rows[bottom], format!("└{}┘", "─".repeat(78)));
    assert!(
        rows[bottom + 1..]
            .iter()
            .any(|row| row.contains("│ Normal │")),
        "missing painted status below {title}: {rows:?}"
    );
    let inner = &rows[top + 1..bottom];
    for line in inner {
        assert!(
            line.starts_with('│') && line.ends_with('│'),
            "broken side rails: {line:?}"
        );
    }
    let rail = inner.last().expect("frame has a rail");
    assert!(
        rail.trim_end_matches('│')
            .trim_end()
            .ends_with("[Esc] Back"),
        "missing Back on the row above the bottom border: {rail:?}"
    );
    rows.truncate(bottom);
    drop(rows.drain(..=top));
    rows
}

fn has_body_row(rows: &[String], expected: &str) -> bool {
    // `whole_frame` leaves the rail as its last row; only the body can supply the text.
    rows[..rows.len() - 1].iter().any(|row| {
        row.strip_prefix('│')
            .and_then(|row| row.strip_suffix('│'))
            .is_some_and(|row| row.trim_matches(' ') == expected)
    })
}

struct Body(ArtifactId);

#[async_trait]
impl PatchReviewRecorder for Body {
    async fn record_verdict(
        &self,
        _artifact: ArtifactRef,
        _reviewer: AgentId,
        _verdict: ReviewVerdict,
    ) -> Result<ArtifactRef, PatchReviewError> {
        Err(PatchReviewError::Refused("not under test".to_owned()))
    }
    async fn body(&self, id: &ArtifactId) -> Result<Vec<u8>, PatchReviewError> {
        if id == &self.0 {
            Ok(b"proof artifact body line\n".to_vec())
        } else {
            Err(PatchReviewError::Refused("no stored body".to_owned()))
        }
    }
    fn effective_policy(&self) -> MergeBackPolicy {
        MergeBackPolicy::default()
    }
}

fn task(number: u32, title: &str) -> PlanTask {
    PlanTask {
        number,
        title: title.to_owned(),
        description: String::new(),
        depends_on: vec![],
        status: PlanTaskStatus::Completed,
        started_at_ms: None,
        completed_at_ms: None,
        result: Some(TaskResult {
            text: "Reviewed OK".to_owned(),
            tool_call_count: 0,
            token_count: None,
        }),
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    }
}

#[tokio::test]
async fn mission_surfaces_are_whole_frames_at_80_by_24() {
    let item = Item::new(
        "ri_oldest",
        SemanticMessageType::Consultation,
        "oldest message",
    );
    let ws = Workspace::with(&[item]).await;
    let id = ArtifactId::from(ContentHash::parse_hex(&"a".repeat(64)).expect("hash"));
    let artifact = EvidenceArtifact {
        id: id.clone(),
        kind: ArtifactKind::Evidence,
        producer: AgentId::parse("spoke-1").expect("agent id"),
        content_hash: ContentHash::parse_hex(&"0".repeat(64)).expect("hash"),
        authority: CapabilityTokenId::default(),
        provenance: vec![ProvenanceTag::UserOriginated],
        depends_on: vec![],
        review: None,
        host: HostBinding::new(
            rustain::infrastructure::subagent::current_host_id(ws.path()),
            "ws",
        ),
    };
    ws.node
        .append_room(RoomEvent::ArtifactCreated { artifact })
        .await
        .expect("append artifact");
    let mut rig = Rig::plain(ws, 80, 24).await;
    rig.app.patch_review = Some(Arc::new(Body(id.clone())));
    attach_journal(&mut rig);

    let mut scratch = TuiState::new(80, 24);
    scratch.input_buffer = "/team log".to_owned();
    let InputAction::ExecuteCommand { name, args } = submit_message_for_test(&mut scratch) else {
        panic!("/team log must parse")
    };
    assert_eq!(name, "team");
    transparency_bridge::team_command(&mut rig.f.h.state, "origin", args.as_deref(), &rig.app)
        .await;
    rig.key(Key::Char('g'));
    rig.paint();
    let rows = whole_frame(&rig, "Mission Control · Messages");
    assert!(rows[0].contains("▸Messages"), "{rows:?}");
    assert!(rows.iter().any(|row| row.contains("ri_oldest")), "{rows:?}");

    assert_eq!(
        rig.key(Key::Right),
        InputAction::OpenMissionControl(McTab::Room)
    );
    assert_eq!(
        rig.key(Key::Right),
        InputAction::OpenMissionControl(McTab::Artifacts)
    );
    room_bridge::refresh_mission_tab(
        &rig.app,
        &mut rig.f.h.state,
        CONVERSATION,
        McTab::Artifacts,
        PermissionMode::Yolo,
    )
    .await;
    rig.paint();
    let rows = whole_frame(&rig, "Mission Control · Artifacts");
    assert!(
        rows.iter().any(|row| row.contains("evidence aaaaaa")),
        "{rows:?}"
    );
    assert_eq!(
        rig.key(Key::Enter),
        InputAction::OpenArtifactWindow(id.as_str().to_owned())
    );
    artifact_bridge::open_window(
        &rig.app,
        &mut rig.f.h.state,
        CONVERSATION,
        id.as_str(),
        PermissionMode::Yolo,
    )
    .await;
    rig.paint();
    let rows = whole_frame(&rig, "Artifact · evidence aaaaaa");
    assert!(
        rows.last().is_some_and(|row| row.contains("[c] Copy")),
        "{rows:?}"
    );
    assert!(has_body_row(&rows, "proof artifact body line"), "{rows:?}");

    rig.key(Key::Esc);
    rig.key(Key::Esc);
    let dir = tempfile::tempdir().expect("scratch spool");
    let tree = NodeTree::new();
    let spool = SubagentSpool::new(dir.path().join("spool"))
        .await
        .expect("spool");
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.key(Key::Char('s')),
        InputAction::OpenMissionControl(McTab::Agents)
    );
    mission_bridge::open_agents_tab_from(&mut rig.f.h.state, &tree, &spool).await;
    rig.paint();
    let rows = whole_frame(&rig, "Mission Control · Agents");
    assert_eq!(
        rows[0].trim_matches('│').split_whitespace().next(),
        Some("▸Agents")
    );
    assert!(
        rows.iter().any(|row| row.contains("No agents active.")),
        "{rows:?}"
    );

    rig.key(Key::Esc);
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('a')), InputAction::OpenAdapters);
    mission_bridge::open_adapters(&mut rig.f.h.state, &AgentCore::test_noop());
    rig.paint();
    let rows = whole_frame(&rig, "Adapters");
    assert!(rows.iter().any(|row| row.contains("persona")), "{rows:?}");

    rig.key(Key::Esc);
    rig.f.tabs.active_tab_mut().conversation.plans.insert(
        "p1".to_owned(),
        Plan {
            id: "p1".to_owned(),
            title: "Release".to_owned(),
            tasks: vec![task(1, "Compile"), task(2, "Review result")],
            estimated_effort: None,
            status: PlanStatus::Executing,
            created_at: 1,
            resolved_at: None,
            host_message_id: None,
            origin_restrictions: None,
        },
    );
    rig.f.h.state.task_panel_state.last_executed_plan_id = Some("p1".to_owned());
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(
        rig.key(Key::Char('t')),
        InputAction::OpenMissionControl(McTab::Tasks)
    );
    rig.paint();
    rig.key(Key::Char('j'));
    rig.paint();
    let rows = whole_frame(&rig, "Mission Control · Tasks");
    assert!(
        rows[0]
            .trim_matches('│')
            .split_whitespace()
            .take(2)
            .eq(["Agents", "▸Tasks"]),
        "{rows:?}"
    );
    let selected: Vec<_> = rows
        .iter()
        .skip(1)
        .filter(|row| row.contains('▸'))
        .collect();
    assert_eq!(selected.len(), 1, "{rows:?}");
    assert!(selected[0].contains("Review result"), "{selected:?}");
    let InputAction::OpenTaskWindow(task_ref) = rig.key(Key::Enter) else {
        panic!("Enter must open the selected task")
    };
    let conversation = rig.f.conversation().clone();
    assert!(rustain::adapters::tui::task_window::open(
        &mut rig.f.h.state,
        &conversation,
        &task_ref
    ));
    rig.paint();
    let rows = whole_frame(&rig, "Task 2 · Review result");
    assert!(
        rows.last().is_some_and(|row| row.contains("[c] Copy")),
        "{rows:?}"
    );
    assert!(has_body_row(&rows, "Reviewed OK"), "{rows:?}");

    rig.key(Key::Esc);
    rig.key(Key::Esc);
    let now = rustain::domain::models::session_meta::now_unix();
    rig.f.sessions = SessionIndex::build(
        [("a", "Alpha session"), ("b", "Beta plans")]
            .iter()
            .enumerate()
            .map(|(n, (id, title))| ConversationSummary {
                id: (*id).to_owned(),
                title: (*title).to_owned(),
                created_at: now - 10_000,
                updated_at: now - i64::try_from(n).unwrap(),
                message_count: 3,
                has_fork_source: false,
            })
            .collect(),
    );
    assert_eq!(rig.key(Key::Ctrl('x')), InputAction::Consumed);
    assert_eq!(rig.key(Key::Char('h')), InputAction::OpenHistory);
    history_bridge::open(&mut rig.f.h.state, &rig.f.sessions);
    rig.paint();
    let rows = whole_frame(&rig, "History");
    assert!(
        rows.last().is_some_and(|row| row.contains("[Enter] Open")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Alpha session") && row.contains("msgs")),
        "{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("Beta plans") && row.contains("msgs")),
        "{rows:?}"
    );
}
