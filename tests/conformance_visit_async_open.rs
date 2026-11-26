#![cfg(feature = "session-visit")]
//! Session Visit asynchronous open: production-front-door witnesses (story visit-async-open).
//!
//! The Stage, the Inspector, the route, the compositor and the runtime renderer are the
//! witnesses; the verified read is the real `SessionHistory` over a seeded workspace, held with
//! the production writer exclusion (an exclusive `flock` on the journal's lock sidecar). Keys go
//! through the local front door; the loop's own steps (`visit_open::begin_*`, `reconcile`,
//! `apply`, `reauthorize_with`) run in the order `render_local!` and the select branch run them.

use crate::conformance_21_5_tool_inspector::support::{Fixture, call, part_for, prose, rows};
use ratatui::buffer::Buffer;
use rustain::adapters::artifact::FileSystemArtifactStore;
use rustain::adapters::tui::{
    app::InputAction, keymap::Key, presentation::Surface, tool_inspector,
};
use rustain::domain::models::orchestration_room::{HostBinding, MainVisitKey, VisitReport};
use rustain::domain::models::{
    AgentId, InvocationStatus as S, PartId, TabManager, Turn, TurnPart, WriteDiffState as D,
};
use rustain::infrastructure::runtime::mission_bridge;
use rustain::infrastructure::runtime::visit_open::{
    self, VisitOpenCtx, VisitOpenDone, VisitOpenSender,
};
use rustain::infrastructure::subagent::NodeJournal;
use rustain::infrastructure::subagent::session_history::{SessionHistory, VisitRead, hash_body};
use serde_json::json;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MISSING: &str = "No readable retained session for this exact source.";
const CUE: &str = "[Esc] Cancel  Opening Session Visit…";

/// An exclusive `flock` on the journal's lock sidecar; every verified read waits for it.
struct Hold(std::fs::File);

impl Hold {
    fn acquire(lock: &Path) -> Self {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .expect("the writer created the lock sidecar");
        // SAFETY: the descriptor is owned by `file` for the whole call.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        // The exclusion is real: a second descriptor on this same path is refused a shared
        // lock without waiting, exactly what the read's `LOCK_SH` would meet.
        let probe = std::fs::OpenOptions::new()
            .read(true)
            .open(lock)
            .expect("the sidecar opens read-only");
        // SAFETY: the descriptor is owned by `probe` for the whole call.
        let refused = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        assert_eq!(
            refused, -1,
            "a shared lock on the held sidecar must be refused"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        );
        Self(file)
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        // SAFETY: as in `acquire`.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Wait for a read task to end; a task that ignores its token fails instead of hanging.
async fn joined(handle: JoinHandle<()>) {
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect("the read task ends")
        .unwrap();
}

fn ctx<'a>(tabs: &'a TabManager, cancel: &'a CancellationToken) -> VisitOpenCtx<'a> {
    VisitOpenCtx {
        conversation: &tabs.active_tab().conversation,
        tab_id: tabs.active_tab_id(),
        local_allowed: true,
        session_cancel: cancel,
        tab_count: tabs.tabs().len(),
    }
}

/// One Main turn with three committed local `task` results under one root, each with its own
/// linked attempt (the RH1 shape, smaller), and the Inspector open on the middle one.
struct World {
    f: Fixture,
    history: Arc<SessionHistory>,
    keys: Vec<MainVisitKey>,
    cancel: CancellationToken,
    tx: VisitOpenSender,
    rx: mpsc::Receiver<VisitOpenDone>,
    lock: PathBuf,
    _dir: tempfile::TempDir,
}

impl World {
    async fn new(width: u16, height: u16) -> Self {
        let mut turn = Turn::new("fixture-model".into(), 0);
        let mut calls = Vec::new();
        for n in 0..3 {
            calls.push(call(
                &mut turn,
                "task",
                json!({ "description": format!("task {n}") }),
                S::Success,
                Some((&format!("literal output {n}"), D::NotAWrite)),
            ));
            prose(&mut turn, &rows(22));
        }
        let mut f = Fixture::new(width, height, turn);

        let conversation = f.conversation().clone();
        let turn = &conversation.turns[0];
        let keys: Vec<MainVisitKey> = calls
            .iter()
            .map(|call| {
                let invocation = part_for(turn, call);
                let (result, content) = result_of(turn, invocation);
                MainVisitKey {
                    conversation_id: conversation.id.clone(),
                    turn_id: turn.id.clone(),
                    invocation,
                    result,
                    revision: hash_body(&content),
                }
            })
            .collect();

        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
        let history = Arc::new(SessionHistory::new(
            journal.clone(),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        ));
        for (n, key) in keys.iter().enumerate() {
            let attempt = AgentId::new();
            history.begin(&key.conversation_id, &attempt).unwrap();
            history
                .record_turn(
                    &attempt,
                    &AgentId::root(),
                    1,
                    &["Read: ok".into()],
                    &format!("PUBLIC_{n}"),
                )
                .await
                .unwrap();
            let (_, content) = result_of(turn, key.invocation);
            history
                .offer(
                    &key.conversation_id,
                    &attempt,
                    &content,
                    VisitReport {
                        report: None,
                        unstructured: true,
                        attempt: Some(attempt.clone()),
                        expires_at_ms: None,
                    },
                )
                .unwrap();
            history.commit_link(key.clone(), attempt).await.unwrap();
        }
        history
            .load_visit_deadlines(&mut f.tabs.active_tab_mut().conversation, true)
            .await;

        f.focus_call(&calls[1]);
        f.open();
        let (tx, rx) = mpsc::channel(4);
        Self {
            f,
            history,
            keys,
            cancel: CancellationToken::new(),
            tx,
            rx,
            lock: journal.path().with_extension("lock"),
            _dir: dir,
        }
    }

    fn begin(&mut self) -> Option<JoinHandle<()>> {
        visit_open::begin_current(
            &mut self.f.h.state,
            Some(self.history.clone()),
            ctx(&self.f.tabs, &self.cancel),
            &self.tx,
        )
    }

    fn applied(&mut self, done: VisitOpenDone) -> bool {
        visit_open::apply(&mut self.f.h.state, done, ctx(&self.f.tabs, &self.cancel))
    }

    async fn done(&mut self) -> VisitOpenDone {
        tokio::time::timeout(Duration::from_secs(30), self.rx.recv())
            .await
            .expect("the read finishes")
            .expect("the channel is open")
    }

    /// With the hold taken the read is genuinely in flight: no completion arrives within a
    /// bounded wait and the task is still running. A hold on the wrong path would let the read
    /// finish inside the bound and fail this.
    async fn assert_held_read_blocked(&mut self, handle: &JoinHandle<()>) {
        let early = tokio::time::timeout(Duration::from_millis(250), self.rx.recv()).await;
        assert!(early.is_err(), "the held read must not complete");
        assert!(!handle.is_finished(), "the held read is still in flight");
    }

    /// What `render_local!` does for every paint: reconcile, reauthorize, then draw.
    fn paint(&mut self) -> String {
        visit_open::reconcile(&mut self.f.h.state, ctx(&self.f.tabs, &self.cancel));
        mission_bridge::reauthorize_with(
            &mut self.f.h.state,
            Some(self.history.as_ref()),
            &self.f.tabs.active_tab().conversation,
            true,
        );
        self.f.paint()
    }

    fn rows(&self) -> Vec<String> {
        let buffer: &Buffer = self.f.buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    fn rail_y(&self) -> usize {
        usize::from(tool_inspector::layout::parts(self.f.frame_rect()).rail.y)
    }

    fn visit_open(&self) -> bool {
        self.f
            .h
            .state
            .modal_stack
            .iter()
            .any(|frame| frame.surface == Surface::SessionVisit)
    }

    fn notices(&self) -> usize {
        self.f.h.state.notifications.len()
    }

    /// The literal output of the middle result changes, as an edit after the key would.
    fn edit_middle_result(&mut self) {
        let invocation = self.keys[1].invocation;
        let turn = &mut self.f.tabs.active_tab_mut().conversation.turns[0];
        for part in turn.parts.iter_mut() {
            if let TurnPart::ToolResult { refs, output, .. } = part
                && *refs == invocation
            {
                output.content = "edited".into();
            }
        }
    }
}

fn result_of(turn: &Turn, invocation: PartId) -> (PartId, String) {
    turn.parts
        .iter()
        .find_map(|part| match part {
            TurnPart::ToolResult { id, refs, output } if *refs == invocation => {
                Some((*id, output.content.clone()))
            }
            _ => None,
        })
        .expect("the call has a result")
}

#[tokio::test]
async fn k1_the_key_paints_the_cue_in_its_own_dispatch_and_esc_restores_the_frame() {
    for (width, height) in [(80, 24), (60, 16)] {
        let mut w = World::new(width, height).await;
        let before = w.rows();
        let rail = w.rail_y();
        assert!(
            before[rail].contains("[Esc] Back") && before[rail].contains("[o] Visit source"),
            "{width}x{height}: {}",
            before[rail]
        );

        assert_eq!(w.f.press(Key::Char('o')), InputAction::VisitCurrentSource);
        let handle = w.begin().expect("the checks pass and one read starts");
        w.paint();
        let during = w.rows();
        assert!(
            during[rail].contains(CUE),
            "{width}x{height}: {}",
            during[rail]
        );
        assert!(!during[rail].contains("[o] Visit source"));
        let changed: Vec<usize> = (0..before.len())
            .filter(|y| before[*y] != during[*y])
            .collect();
        assert_eq!(changed, vec![rail], "every other row is byte-identical");

        assert_eq!(w.f.press(Key::Esc), InputAction::Consumed);
        w.paint();
        assert_eq!(w.rows(), before, "{width}x{height}: cancel restores it");
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn k1_an_ineligible_result_paints_no_cue_and_posts_the_missing_sentence() {
    let mut w = World::new(80, 24).await;
    w.edit_middle_result();
    let notices = w.notices();
    assert_eq!(w.f.press(Key::Char('o')), InputAction::VisitCurrentSource);
    assert!(w.begin().is_none(), "no read starts");
    w.paint();
    assert!(!w.rows()[w.rail_y()].contains(CUE));
    assert_eq!(w.notices(), notices + 1);
    let last = w.f.h.state.notifications.records().next_back().unwrap();
    assert_eq!(last.message, MISSING);
    assert!(w.rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k2_esc_cancels_a_held_read_and_no_completion_ever_paints_or_notifies() {
    let mut w = World::new(80, 24).await;
    let before = w.rows();
    let notices = w.notices();

    let hold = Hold::acquire(&w.lock);
    let handle = w.begin().expect("the read starts");
    w.assert_held_read_blocked(&handle).await;
    w.paint();
    assert!(
        w.rows()[w.rail_y()].contains(CUE),
        "the cue paints while held"
    );
    assert_eq!(w.f.press(Key::Esc), InputAction::Consumed);
    w.paint();
    assert_eq!(w.rows(), before, "the origin frame, byte for byte");
    joined(handle).await;
    drop(hold);
    assert!(w.rx.try_recv().is_err(), "the channel holds no completion");
    assert!(!w.visit_open());
    assert_eq!(w.notices(), notices);

    // A completion that finished before the cancel is dropped by the generation guard.
    let handle = w.begin().expect("the read starts");
    let done = w.done().await;
    joined(handle).await;
    assert_eq!(w.f.press(Key::Esc), InputAction::Consumed);
    assert!(!w.applied(done));
    w.paint();
    assert_eq!(w.rows(), before);
    assert!(!w.visit_open());
    assert_eq!(w.notices(), notices);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k3_a_context_change_drops_the_open_and_the_key_acts() {
    let mut w = World::new(80, 24).await;
    let notices = w.notices();
    let hold = Hold::acquire(&w.lock);
    let handle = w.begin().expect("the read starts");
    w.assert_held_read_blocked(&handle).await;
    assert!(w.f.step(Key::Char(']')), "`]` steps in the same dispatch");
    let screen = w.paint();
    let title = w.f.h.state.modal_stack.top().unwrap().title().to_owned();
    assert!(title.ends_with("#3"), "{title}");
    assert!(!screen.contains("Opening"), "no cue under the next call");
    assert!(!screen.contains("Session Visit"));
    joined(handle).await;
    drop(hold);
    assert!(w.rx.try_recv().is_err());
    assert!(!w.visit_open());
    assert_eq!(w.notices(), notices);

    // A completion that arrives after the step is dropped.
    w.f.step(Key::Char('['));
    let handle = w.begin().expect("the read starts");
    let done = w.done().await;
    joined(handle).await;
    assert!(w.f.step(Key::Char(']')));
    assert!(!w.applied(done));
    let screen = w.paint();
    assert!(!screen.contains("Session Visit"));
    assert!(!w.visit_open());
}

#[tokio::test]
async fn k4_the_live_completion_opens_the_same_visit_the_read_resolves() {
    let mut w = World::new(80, 24).await;
    let read = w
        .history
        .resolve(&w.keys[1], &w.f.tabs.active_tab().conversation, true)
        .await;
    let (label, attempt) = match read {
        VisitRead::Available(body) => ("Read-only", body.attempt),
        VisitRead::Partial(body) => ("Partial", body.attempt),
        other => panic!("the seeded result is readable, got {other:?}"),
    };

    let handle = w.begin().expect("the read starts");
    let done = w.done().await;
    joined(handle).await;
    assert!(w.applied(done));
    let screen = w.paint();
    assert!(w.visit_open());
    assert!(
        screen.contains(&format!("Session Visit · {} · {label}", attempt.as_str())),
        "{screen}"
    );
    assert!(screen.contains("Turn 1 · Tools:"));
    assert!(screen.contains("PUBLIC_1"));
}

#[tokio::test]
async fn k5_a_result_edited_during_the_read_shows_changed_and_no_private_text() {
    let mut w = World::new(80, 24).await;
    let handle = w.begin().expect("the read starts");
    let done = w.done().await;
    joined(handle).await;
    w.edit_middle_result();
    assert!(w.applied(done));
    let screen = w.paint();
    assert!(screen.contains("Session Visit · Changed"), "{screen}");
    assert!(!screen.contains("PUBLIC_1"), "no retained body is painted");
    assert!(!screen.contains("Turn 1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k6_main_draft_and_caret_survive_open_cancel_and_back() {
    let mut w = World::new(80, 24).await;
    w.f.h.state.input_buffer = "parked draft".into();
    w.f.h.state.cursor_position = 4;
    let hold = Hold::acquire(&w.lock);
    assert_eq!(w.f.press(Key::Char('o')), InputAction::VisitCurrentSource);
    let handle = w.begin().expect("the read starts");
    w.assert_held_read_blocked(&handle).await;
    assert_eq!(w.f.press(Key::Esc), InputAction::Consumed);
    assert!(
        matches!(
            w.f.h.state.modal_stack.top().unwrap().surface,
            Surface::ToolInspector(_)
        ),
        "the first Esc only cancels"
    );
    assert_eq!(w.f.press(Key::Esc), InputAction::Consumed);
    w.paint();
    assert!(w.f.h.state.modal_stack.is_empty(), "the second Esc is Back");
    joined(handle).await;
    drop(hold);
    assert_eq!(w.f.h.state.input_buffer, "parked draft");
    assert_eq!(w.f.h.state.cursor_position, 4);
}

/// AC3 positive control: the same hold without `Esc`, once released, yields exactly one
/// completion that opens and paints the Visit once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k2_the_held_read_completes_and_opens_once_when_the_hold_is_released() {
    let mut w = World::new(80, 24).await;
    let hold = Hold::acquire(&w.lock);
    let handle = w.begin().expect("the read starts");
    w.assert_held_read_blocked(&handle).await;
    assert!(!w.visit_open(), "nothing opens while held");
    drop(hold);
    let done = w.done().await;
    joined(handle).await;
    assert!(w.applied(done));
    let screen = w.paint();
    assert!(w.visit_open());
    assert!(screen.contains("Session Visit · "), "{screen}");
    assert!(w.rx.try_recv().is_err(), "exactly one completion");
}
