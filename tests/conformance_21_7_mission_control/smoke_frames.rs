//! Desk-check frames for the native smoke file (`tests_tui/test_story_21_7_mission_control.py`,
//! legs A and B): the production host paints the hub at 60x16, 80x24 and 130x30 with a seeded
//! room journal (leg A: the Room tab), a seeded A2A log page (leg B: the Messages tab) and one
//! artifact window (leg B: `Artifact · <name>`), and the frames are written as plain text for
//! the Python predicates to be proven against offline (every predicate passes on these frames
//! and fails on torn controls).
//!
//! `#[ignore]`d: it writes files. Run it with `RUSTAIN_MISSION_FRAMES=<dir>`:
//! `cargo test --test tui conformance_21_7_mission_control::smoke_frames -- --ignored`.
use super::support::*;
use rustain::adapters::tui::artifact_window::{ArtifactBody, ArtifactWindowState};
use rustain::adapters::tui::keymap::Key;
use rustain::adapters::tui::presentation::reducer;

/// The seeded A2A rows the Messages frame paints (the closing claim last, as `log_blocks`
/// builds it).
fn seeded_log_blocks() -> Vec<String> {
    vec![
        "2023-11-14 22:13:20Z ↓ inbound ✗ refused smoke-00".to_owned(),
        "2023-11-14 22:13:21Z ↓ inbound ✗ refused smoke-01".to_owned(),
        "— append-only, crash-safe, structurally replayable (not payload- or provenance-authenticated)"
            .to_owned(),
    ]
}

const ARTIFACT_BODY: &[u8] = b"proof artifact body line\n";

/// The seeded plan of legs D and F: three tasks, the second with a markdown-free result.
fn seeded_plan() -> rustain::domain::models::plan::Plan {
    use rustain::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus, TaskResult};
    let task = |number: u32, title: &str, status: PlanTaskStatus, result: Option<&str>| PlanTask {
        number,
        title: title.to_owned(),
        description: String::new(),
        depends_on: vec![],
        status,
        started_at_ms: None,
        completed_at_ms: None,
        result: result.map(|text| TaskResult {
            text: text.to_owned(),
            tool_call_count: 1,
            token_count: None,
        }),
        error: None,
        waiting_on: vec![],
        delegated_to: None,
        sub_tasks: vec![],
    };
    Plan {
        id: "p1".to_owned(),
        title: "Release".to_owned(),
        tasks: vec![
            task(
                1,
                "Draft notes",
                PlanTaskStatus::Completed,
                Some("Notes **drafted**"),
            ),
            task(
                2,
                "Review result",
                PlanTaskStatus::Completed,
                Some("Reviewed OK"),
            ),
            task(3, "Publish", PlanTaskStatus::Pending, None),
        ],
        estimated_effort: None,
        status: PlanStatus::Completed,
        created_at: 1,
        resolved_at: None,
        host_message_id: None,
    }
}

#[tokio::test]
#[ignore = "writes the desk-check frames named by RUSTAIN_MISSION_FRAMES"]
async fn story21_7_smoke_frames_export() {
    let dir = std::path::PathBuf::from(
        std::env::var("RUSTAIN_MISSION_FRAMES")
            .expect("RUSTAIN_MISSION_FRAMES names the output directory"),
    );
    std::fs::create_dir_all(&dir).expect("create the frames directory");
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        let before = lines(&rig).join("\n");
        open_room(&mut rig).await;
        let open = lines(&rig).join("\n");
        rig.f.press(Key::Esc);
        rig.f.paint();
        let after = lines(&rig).join("\n");
        std::fs::write(
            dir.join(format!("stage-before-{width}x{height}.txt")),
            before,
        )
        .expect("write");
        std::fs::write(dir.join(format!("room-open-{width}x{height}.txt")), open).expect("write");
        std::fs::write(dir.join(format!("stage-after-{width}x{height}.txt")), after)
            .expect("write");
    }
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rustain::adapters::tui::mission_control::open_messages(
            &mut rig.f.h.state,
            false,
            &seeded_log_blocks(),
            None,
        );
        rig.f.paint();
        std::fs::write(
            dir.join(format!("messages-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");

        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        let id = "a".repeat(64);
        let header = "evidence aaaaaa\n  producer  proof-node\n".to_owned();
        reducer::open_artifact_window(
            &mut rig.f.h.state,
            ArtifactWindowState::new(id, header, ArtifactBody::Source(ARTIFACT_BODY.to_vec())),
            "Artifact · evidence aaaaaa".to_owned(),
        );
        rig.f.paint();
        std::fs::write(
            dir.join(format!("artifact-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
    }
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let dir_tmp = tempfile::tempdir().expect("scratch spool dir");
        let tree = rustain::infrastructure::subagent::NodeTree::new();
        let spool =
            rustain::infrastructure::subagent::SubagentSpool::new(dir_tmp.path().join("spool"))
                .await
                .expect("spool");
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rig.key(Key::Ctrl('x'));
        rig.key(Key::Char('s'));
        rustain::infrastructure::runtime::mission_bridge::open_agents_tab_from(
            &mut rig.f.h.state,
            &tree,
            &spool,
        )
        .await;
        rig.f.paint();
        std::fs::write(
            dir.join(format!("agents-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");

        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rustain::infrastructure::runtime::mission_bridge::open_adapters(
            &mut rig.f.h.state,
            &rustain::infrastructure::runtime::agent_core::AgentCore::test_noop(),
        );
        rig.f.paint();
        std::fs::write(
            dir.join(format!("adapters-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
    }
    // Leg D (7-D): the Tasks tab and the task window of the seeded plan (the live leg seeds the
    // same tasks through a saved session).
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rig.f
            .tabs
            .active_tab_mut()
            .conversation
            .plans
            .insert("p1".to_owned(), seeded_plan());
        rig.key(Key::Ctrl('x'));
        rig.key(Key::Char('t'));
        rig.f.paint();
        std::fs::write(
            dir.join(format!("tasks-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
        rig.key(Key::Char('j'));
        rig.key(Key::Char('j'));
        rig.f.paint();
        let conversation = rig.f.conversation().clone();
        let reference = rustain::adapters::tui::mission_control::tasks::TaskRef {
            conversation_id: conversation.id.clone(),
            plan_id: "p1".to_owned(),
            number: 2,
        };
        rustain::adapters::tui::task_window::open(&mut rig.f.h.state, &conversation, &reference);
        rig.f.paint();
        std::fs::write(
            dir.join(format!("task-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
    }
    // Leg E (7-E): the launchpad key line and the History frame over two saved sessions (the
    // live leg seeds the same two titles through saved session files).
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        use rustain::domain::models::ConversationSummary;
        use rustain::domain::services::session_index::SessionIndex;
        let summary = |id: &str, title: &str, updated: i64| ConversationSummary {
            id: id.to_owned(),
            title: title.to_owned(),
            created_at: updated,
            updated_at: updated,
            message_count: 2,
            has_fork_source: false,
        };
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rig.f.sessions = SessionIndex::build(vec![
            summary("alpha", "Alpha history session", 1_700_000),
            summary("bravo", "Bravo history session", 1_700_100),
        ]);
        // The launchpad shows on an empty conversation (and needs its environment facts).
        rig.f.h.state.launchpad_env = Some(rustain::adapters::tui::launchpad::LaunchpadEnv::new(
            rig.ws.path().to_path_buf(),
            |_| false,
        ));
        let tab = rig.f.tabs.active_tab_mut();
        tab.conversation.turns.clear();
        tab.conversation.rebuild_messages_mirror();
        rig.f.paint();
        std::fs::write(
            dir.join(format!("launchpad-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
        rustain::infrastructure::runtime::history_bridge::open(&mut rig.f.h.state, &rig.f.sessions);
        rig.f.paint();
        std::fs::write(
            dir.join(format!("history-open-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
    }
    // Leg F (7-F): the read-only monitor beside the main column. The plan is the leg D plan (the
    // live leg seeds it through a saved session). `Ctrl+X w` paints `Monitor · Tasks` at 130x30
    // and answers the width remedy (painting no column) below 120 columns; a second `Ctrl+X w`
    // at 130x30 takes the column away again.
    for (width, height) in [(60u16, 16u16), (80, 24), (130, 30)] {
        let mut rig = Rig::plain(room_workspace(&NODES).await, width, height).await;
        rig.f
            .tabs
            .active_tab_mut()
            .conversation
            .plans
            .insert("p1".to_owned(), seeded_plan());
        rig.f.paint();
        std::fs::write(
            dir.join(format!("monitor-off-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
        rig.key(Key::Ctrl('x'));
        rig.key(Key::Char('w'));
        rig.f.paint();
        std::fs::write(
            dir.join(format!("monitor-on-{width}x{height}.txt")),
            lines(&rig).join("\n"),
        )
        .expect("write");
        if width >= 120 {
            rig.key(Key::Ctrl('x'));
            rig.key(Key::Char('w'));
            rig.f.paint();
            std::fs::write(
                dir.join(format!("monitor-toggled-off-{width}x{height}.txt")),
                lines(&rig).join("\n"),
            )
            .expect("write");
        }
    }
}
