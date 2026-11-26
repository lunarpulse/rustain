//! Asynchronous, cancelable open of a Session Visit.
//!
//! The event loop never awaits a Visit read. [`begin_current`] and [`begin_related`] run the
//! synchronous checks the former inline open ran on the loop, record one pending open on the
//! frame whose key started it, and spawn the verified read; [`apply`] turns the completion
//! into the same Visit (or the same notice) the inline open produced, and only when it is
//! still the live open. [`reconcile`] drops a pending open whose context changed. Completions
//! travel on a private channel the loop owns, never on the event bus, and no function here
//! takes the bus, the app state, the agent core or a provider.
//!
//! The read itself is the unchanged `SessionHistory::resolve` on an owned snapshot of the
//! conversation ([`snapshot`]); authority, revision, backing and expiry are rechecked against
//! the live conversation before the first paint by `reauthorize_session_visit`. Every read,
//! and the reader it prepares, runs on the one Visit reader thread (`READER`).

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::mission_bridge;
use crate::adapters::tui::presentation::Surface;
use crate::adapters::tui::session_visit::{self, OpeningOrigin, VisitOpening, VisitWindowState};
use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::tool_inspector::{self, ToolFrameKey};
use crate::domain::models::turn::TurnId;
use crate::domain::models::{Conversation, TabId, Turn, TurnPart};
use crate::infrastructure::subagent::session_history::{MainVisitKey, SessionHistory, VisitRead};

/// What the loop knows when it begins, reconciles or applies an open.
#[derive(Clone, Copy)]
pub struct VisitOpenCtx<'a> {
    pub conversation: &'a Conversation,
    pub tab_id: TabId,
    /// The active tab is a local, writable one and host authority is live.
    pub local_allowed: bool,
    pub session_cancel: &'a CancellationToken,
    pub tab_count: usize,
}

/// A finished read. It carries the generation and frame it was started for; [`apply`] drops
/// it unless both are still live. It is never printed: it may hold a retained body.
pub struct VisitOpenDone {
    generation: u64,
    frame_id: u64,
    outcome: Outcome,
}

/// The loop's private channel for finished reads.
pub type VisitOpenSender = mpsc::Sender<VisitOpenDone>;

/// A prepared Visit, or the read that was not readable (boxed: `VisitRead` is large).
type Prepared = Result<Box<VisitWindowState>, Box<VisitRead>>;

struct Outcome {
    /// A related open only: the fresh read of the source the Visit already shows.
    current: Option<VisitRead>,
    /// The target read. `None`: a related open whose fresh current read did not name it.
    target: Option<Prepared>,
}

/// Everything the read needs, owned, so it can leave the loop.
struct Job {
    history: Option<Arc<SessionHistory>>,
    snapshot: Conversation,
    local_allowed: bool,
    session_cancel: CancellationToken,
    layout: Option<(Theme, u16)>,
    key: MainVisitKey,
    current: Option<MainVisitKey>,
}

/// The part of `conversation` that `SessionHistory::exact` can look at: its id and every turn
/// whose id equals the id of some turn holding a `task` invocation. `exact` reads only the
/// active id and the turns with the key's turn id, and rejects any turn without a `task`
/// invocation, so its answer on the snapshot equals its answer on the live conversation for
/// every key; turns sharing an id are all kept.
pub fn snapshot(conversation: &Conversation) -> Conversation {
    let holds_task = |turn: &Turn| {
        turn.parts
            .iter()
            .any(|part| matches!(part, TurnPart::ToolInvocation { tool, .. } if tool == "task"))
    };
    let ids: HashSet<&TurnId> = conversation
        .turns
        .iter()
        .filter(|turn| holds_task(turn))
        .map(|turn| &turn.id)
        .collect();
    Conversation {
        id: conversation.id.clone(),
        turns: conversation
            .turns
            .iter()
            .filter(|turn| ids.contains(&turn.id))
            .cloned()
            .collect(),
        ..Default::default()
    }
}

/// `o` on an Inspector: the P2 checks of the former inline open, then one pending open and
/// one read task. `None` when a check failed (its notice is posted) or nothing started. The
/// loop drops the handle; tests await it.
pub fn begin_current(
    state: &mut TuiState,
    history: Option<Arc<SessionHistory>>,
    ctx: VisitOpenCtx<'_>,
    tx: &VisitOpenSender,
) -> Option<JoinHandle<()>> {
    if !mission_bridge::session_visit_access_allowed(state, ctx.local_allowed, ctx.session_cancel) {
        mission_bridge::visit_unavailable(state, VisitRead::Denied);
        return None;
    }
    let Some(Surface::ToolInspector(ToolFrameKey(frame_id))) =
        state.modal_stack.top().map(|frame| frame.surface)
    else {
        return None;
    };
    let inspector = state.tool_inspectors.get(&frame_id)?;
    // `o` is idempotent while its own read is pending.
    if inspector.visit_opening.is_some() {
        return None;
    }
    let tab = inspector.tab;
    let Some(key) = tool_inspector::visit_target(state, ctx.conversation) else {
        mission_bridge::visit_unavailable(state, VisitRead::Missing);
        return None;
    };
    Some(start(
        state,
        history,
        ctx,
        tx,
        key,
        OpeningOrigin::Inspector { frame_id, tab },
    ))
}

/// `←`/`→` inside a Visit: the related open of the former inline open, started the same way.
pub fn begin_related(
    state: &mut TuiState,
    history: Option<Arc<SessionHistory>>,
    ctx: VisitOpenCtx<'_>,
    tx: &VisitOpenSender,
    key: MainVisitKey,
) -> Option<JoinHandle<()>> {
    if !mission_bridge::session_visit_access_allowed(state, ctx.local_allowed, ctx.session_cancel) {
        mission_bridge::visit_unavailable(state, VisitRead::Denied);
        return None;
    }
    let frame = state.modal_stack.top()?;
    if frame.surface != Surface::SessionVisit {
        return None;
    }
    let frame_id = frame.id;
    let visit = state.session_visit.as_ref()?;
    if visit.opening.is_some() {
        return None;
    }
    let cursor_key = visit.cursor.key.clone();
    Some(start(
        state,
        history,
        ctx,
        tx,
        key,
        OpeningOrigin::Visit {
            frame_id,
            cursor_key,
        },
    ))
}

/// Mint the generation, record the pending open on the top frame (which clears any other
/// one: at most one exists) and spawn the read under a child of the session token.
fn start(
    state: &mut TuiState,
    history: Option<Arc<SessionHistory>>,
    ctx: VisitOpenCtx<'_>,
    tx: &VisitOpenSender,
    key: MainVisitKey,
    origin: OpeningOrigin,
) -> JoinHandle<()> {
    let frame_id = origin.frame_id();
    let job = Job {
        history,
        snapshot: snapshot(ctx.conversation),
        local_allowed: ctx.local_allowed,
        session_cancel: ctx.session_cancel.clone(),
        // The body width the Visit and the Inspector both paint at, so the first paint
        // reuses the layout the preparation built.
        layout: tool_inspector::frame_rect(state, ctx.tab_count)
            .map(|rect| (state.theme.clone(), rect.width.saturating_sub(2))),
        key: key.clone(),
        current: match &origin {
            OpeningOrigin::Visit { cursor_key, .. } => Some(cursor_key.clone()),
            OpeningOrigin::Inspector { .. } => None,
        },
    };
    session_visit::clear_pending(state);
    state.visit_open_generation += 1;
    let generation = state.visit_open_generation;
    let cancel = ctx.session_cancel.child_token();
    let opening = VisitOpening::new(
        generation,
        key,
        origin,
        ctx.tab_id,
        ctx.conversation.id.clone(),
        cancel.clone().drop_guard(),
    );
    *session_visit::top_slot(state).expect("the origin frame owns its payload") = Some(opening);
    state.needs_redraw = true;
    spawn(job, generation, frame_id, cancel, tx.clone())
}

/// Name of the one thread every Visit read runs on.
const READER_THREAD: &str = "rustain-visit-reader";

/// The one thread every Visit read and reader preparation runs on: a current-thread runtime
/// on a named, detached thread, started by the first open and kept for the process.
///
/// An open allocates a few MiB (artifact bodies, parsed turns, the projected text, its search
/// fold and first layout) that the loop frees when the Visit closes or a completion is
/// dropped. glibc returns freed memory to the arena of the thread that allocated it. On the
/// multi-threaded runtime a read moved between worker threads at its awaits and prepared on
/// any blocking-pool thread, so each open spread its allocations over many arenas, each of
/// which can keep its freed share resident. One thread keeps that memory in one arena, where
/// the next open reuses it.
/// Blocking steps inside the read (the journal projection's `flock`, file reads) still run on
/// this runtime's own blocking pool, so a held lock never stalls the reader thread.
static READER: LazyLock<tokio::runtime::Handle> = LazyLock::new(|| {
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
    // Detached: the runtime drives Visit reads for the life of the process.
    std::thread::Builder::new()
        .name(READER_THREAD.into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the Visit reader runtime");
            let _ = handle_tx.send(runtime.handle().clone());
            runtime.block_on(std::future::pending::<()>());
        })
        .expect("spawn the Visit reader thread");
    handle_rx
        .recv()
        .expect("the Visit reader thread reports its runtime")
});

/// The read task: cooperative cancellation at the read's own await points, no timeout, no
/// retry. A cancelled open never sends. The returned task belongs to the caller's runtime
/// (the loop never waits for the reader thread to start) and ends when the read on the
/// reader thread ends; a panic there resumes here.
fn spawn(
    job: Job,
    generation: u64,
    frame_id: u64,
    cancel: CancellationToken,
    tx: VisitOpenSender,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let read = READER.spawn(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = async {
                    let outcome = run(job).await;
                    let _ = tx.send(VisitOpenDone { generation, frame_id, outcome }).await;
                } => {}
            }
        });
        if let Err(error) = read.await
            && let Ok(panic) = error.try_into_panic()
        {
            std::panic::resume_unwind(panic);
        }
    })
}

/// `read_visit` of the former inline open, on the snapshot: the same refusals and the same
/// mapping of a session cancelled during the read to `Denied`.
async fn read_visit(job: &Job, key: &MainVisitKey) -> VisitRead {
    if !job.local_allowed
        || job.session_cancel.is_cancelled()
        || job.snapshot.id != key.conversation_id
    {
        return VisitRead::Denied;
    }
    let read = match &job.history {
        Some(history) => history.resolve(key, &job.snapshot, job.local_allowed).await,
        None => VisitRead::Missing,
    };
    if job.session_cancel.is_cancelled() {
        VisitRead::Denied
    } else {
        read
    }
}

/// Read the target and, when readable, build its reader (text, search fold, first layout)
/// here on the reader thread, in the arena the next open reuses. A cancel during the build
/// takes effect when it returns.
async fn read_prepared(job: &Job) -> Prepared {
    let read = read_visit(job, &job.key).await;
    if !matches!(read, VisitRead::Available(_) | VisitRead::Partial(_)) {
        return Err(Box::new(read));
    }
    #[cfg(test)]
    let window =
        probe::in_read(|| VisitWindowState::prepare(job.key.clone(), read, job.layout.clone()));
    #[cfg(not(test))]
    let window = VisitWindowState::prepare(job.key.clone(), read, job.layout.clone());
    Ok(Box::new(window))
}

/// An Inspector origin reads its key once. A related origin reads the source the Visit shows
/// first and the target only when that fresh read names it.
async fn run(job: Job) -> Outcome {
    let mut current = None;
    let mut wanted = true;
    if let Some(cursor_key) = &job.current {
        let read = read_visit(&job, cursor_key).await;
        wanted = job.key != *cursor_key && session_visit::edge_names(&read, cursor_key, &job.key);
        current = Some(read);
    }
    let target = if wanted {
        Some(read_prepared(&job).await)
    } else {
        None
    };
    Outcome { current, target }
}

/// Whether `opening` may still be waited for in `state` under `ctx`. `fresh_source`: also
/// re-read the Inspector's source from the live conversation (the paint-time rule).
fn holds(
    state: &TuiState,
    opening: &VisitOpening,
    ctx: &VisitOpenCtx<'_>,
    fresh_source: bool,
) -> bool {
    if opening.tab_id != ctx.tab_id || opening.conversation_id != ctx.conversation.id {
        return false;
    }
    let Some(top) = state.modal_stack.top() else {
        return false;
    };
    if top.id != opening.origin.frame_id() {
        return false;
    }
    match &opening.origin {
        OpeningOrigin::Inspector { .. } => {
            matches!(top.surface, Surface::ToolInspector(_))
                && state.tool_inspectors.get(&top.id).is_some_and(|inspector| {
                    let source = fresh_source.then_some(ctx.conversation);
                    inspector.opening_still_current(opening, source)
                })
        }
        OpeningOrigin::Visit { cursor_key, .. } => {
            top.surface == Surface::SessionVisit
                && state.session_visit.as_ref().is_some_and(|visit| {
                    visit.cursor.key == *cursor_key && !visit.text.search.editing
                })
        }
    }
}

/// Drop a pending open whose context changed (silently: its token is cancelled by the drop).
/// A pending open survives only while its origin frame is the top frame and still shows what
/// it started on, the session tab and conversation are unchanged, the editors are closed and
/// host authority holds. The loop runs this before every paint: a source that changed or
/// expired is judged from the live conversation there, so a stale `Opening…` cue is never
/// painted. [`apply`] runs the same rule without the source check: a completion that arrives
/// before any repaint opens the Visit and the reauthorize before its first paint shows
/// `Changed` (AC6).
pub fn reconcile(state: &mut TuiState, ctx: VisitOpenCtx<'_>) {
    reconcile_with(state, ctx, true);
}

fn reconcile_with(state: &mut TuiState, ctx: VisitOpenCtx<'_>, fresh_source: bool) {
    let Some(held) =
        session_visit::pending_open(state).map(|opening| holds(state, opening, &ctx, fresh_source))
    else {
        return;
    };
    let live =
        mission_bridge::session_visit_access_allowed(state, ctx.local_allowed, ctx.session_cancel);
    if !(held && live) {
        session_visit::clear_pending(state);
        // The cue was painted for this open: repaint so the rail shows the binding that applies.
        state.needs_redraw = true;
    }
}

/// The pending open of the top frame, if it is the one `generation` started there.
fn take_live(state: &mut TuiState, generation: u64, frame_id: u64) -> Option<VisitOpening> {
    let slot = session_visit::top_slot(state)?;
    let live = slot.as_ref().is_some_and(|opening| {
        opening.generation == generation && opening.origin.frame_id() == frame_id
    });
    if live { slot.take() } else { None }
}

fn finish(state: &mut TuiState, target: Option<Prepared>) {
    match target {
        Some(Ok(window)) => session_visit::open_visit(state, *window),
        Some(Err(read)) => mission_bridge::visit_unavailable(state, *read),
        None => mission_bridge::visit_unavailable(state, VisitRead::Missing),
    }
}

/// Apply a finished read. Returns `false`, having changed nothing, unless `done` belongs to
/// the live pending open of the top frame. Then the unchanged tail of the former inline open
/// runs: authority recheck, then the Visit (or the same four sentences); for a related open
/// the fresh current read refreshes the Visit first, and only a readable source that still
/// names the target opens it. The caller paints through `reauthorize_session_visit`, which
/// verifies source, revision, backing and expiry against the live conversation first.
pub fn apply(state: &mut TuiState, done: VisitOpenDone, ctx: VisitOpenCtx<'_>) -> bool {
    reconcile_with(state, ctx, false);
    let VisitOpenDone {
        generation,
        frame_id,
        outcome: Outcome { current, target },
    } = done;
    let Some(opening) = take_live(state, generation, frame_id) else {
        return false;
    };
    if !mission_bridge::session_visit_access_allowed(state, ctx.local_allowed, ctx.session_cancel) {
        return true;
    }
    match &opening.origin {
        OpeningOrigin::Inspector { .. } => finish(state, target),
        OpeningOrigin::Visit { cursor_key, .. } => {
            let Some(current) = current else {
                return true;
            };
            let readable = matches!(&current, VisitRead::Available(_) | VisitRead::Partial(_));
            session_visit::refresh_visit(state, current);
            if opening.key == *cursor_key || !readable {
                return true;
            }
            if session_visit::can_visit_related(state, &opening.key) {
                finish(state, target);
            } else {
                mission_bridge::visit_unavailable(state, VisitRead::Missing);
            }
        }
    }
    true
}

/// A Visit or a pending open exists.
pub fn active(state: &TuiState) -> bool {
    session_visit::visit_cursor(state).is_some() || session_visit::pending_open(state).is_some()
}

/// The same-dispatch redraw rule: a Visit or a pending open existed before the dispatch or
/// exists after it.
pub fn redraw_needed(before: bool, state: &TuiState) -> bool {
    before || active(state)
}

/// One finished read and one already-admitted input event, held while the loop decides which
/// goes first. A completion never applies (and so never paints) ahead of input that was
/// already ready when it arrived: an `Esc` or a context change in that input must first get
/// the chance to cancel the open the completion belongs to. Only the completion waits; every
/// other event source is unaffected, and an input that is not ready never delays it.
pub struct Parked<T> {
    done: Option<VisitOpenDone>,
    input: Option<T>,
}

impl<T> Default for Parked<T> {
    fn default() -> Self {
        Self {
            done: None,
            input: None,
        }
    }
}

impl<T> Parked<T> {
    /// A completion arrived; a newer one replaces an older one (the older open was
    /// superseded, so its completion could not apply anyway).
    pub fn park_done(&mut self, done: VisitOpenDone) {
        self.done = Some(done);
    }

    /// The parked input event, to be dispatched before anything else.
    pub fn take_input(&mut self) -> Option<T> {
        self.input.take()
    }

    /// The completion to apply now, if any. With input parked, or ready right now
    /// (`ready_input` polls the terminal without waiting), the completion keeps waiting and
    /// that input is parked to be dispatched first.
    pub fn next_apply(&mut self, ready_input: impl FnOnce() -> Option<T>) -> Option<VisitOpenDone> {
        if self.input.is_some() || self.done.is_none() {
            return None;
        }
        if let Some(input) = ready_input() {
            self.input = Some(input);
            return None;
        }
        self.done.take()
    }
}

/// Test oracle: which thread builds a reader. `prepare` reports itself here, but only while a
/// read task runs it under [`probe::in_read`], so moving the build to another thread leaves
/// the reader-thread count short.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static IN_READ: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) static ON_READER: AtomicUsize = AtomicUsize::new(0);
    pub(crate) static ELSEWHERE: AtomicUsize = AtomicUsize::new(0);

    pub(super) fn in_read<T>(build: impl FnOnce() -> T) -> T {
        IN_READ.with(|flag| flag.set(true));
        let built = build();
        IN_READ.with(|flag| flag.set(false));
        built
    }

    pub(crate) fn prepared() {
        if IN_READ.with(Cell::get) {
            let counter = if std::thread::current().name() == Some(super::READER_THREAD) {
                &ON_READER
            } else {
                &ELSEWHERE
            };
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    #[cfg(unix)]
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use crate::adapters::artifact::FileSystemArtifactStore;
    use crate::adapters::tui::app::{InputAction, convert_crossterm_event};
    use crate::adapters::tui::artifact_window::ArtifactBody;
    use crate::adapters::tui::keymap::Host;
    use crate::adapters::tui::notification_log::LogFacts;
    use crate::adapters::tui::presentation::{self, RouteEffect, reducer};
    use crate::adapters::tui::tool_inspector::ToolTab;
    use crate::domain::events::DomainInputEvent;
    use crate::domain::models::orchestration_room::{HostBinding, VisitReport};
    use crate::domain::models::{
        AgentId, InvocationStatus, MessageRole, TabManager, ToolOutput, WriteDiffState,
    };
    use crate::infrastructure::subagent::NodeJournal;
    use crate::infrastructure::subagent::session_history::hash_body;

    const MISSING: &str = "No readable retained session for this exact source.";

    fn retained_deadline() -> i64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        i64::try_from(now.as_millis()).unwrap() + 60_000
    }

    /// Wait for a read task to end; a task that ignores its token fails instead of hanging.
    #[cfg(unix)]
    async fn joined(handle: JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(30), handle)
            .await
            .expect("the read task ends")
            .unwrap();
    }

    /// An exclusive `flock` on the journal's lock sidecar: the production writer exclusion,
    /// which blocks the shared lock every verified read takes until it is released.
    #[cfg(unix)]
    struct Hold(std::fs::File);

    #[cfg(unix)]
    impl Hold {
        fn acquire(lock: &std::path::Path) -> Self {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(lock)
                .expect("the writer created the lock sidecar");
            // SAFETY: the descriptor is owned by `file` for the whole call.
            assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
            // The exclusion is real: a second descriptor on this same path is refused a
            // shared lock without waiting, exactly what the read's `LOCK_SH` would meet.
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

    #[cfg(unix)]
    impl Drop for Hold {
        fn drop(&mut self) {
            // SAFETY: as in `acquire`.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }

    fn route_key(state: &mut TuiState, code: KeyCode) -> Option<RouteEffect> {
        route_chord(state, code, KeyModifiers::NONE)
    }

    fn route_chord(
        state: &mut TuiState,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<RouteEffect> {
        let event = convert_crossterm_event(
            &Event::Key(KeyEvent::new(code, modifiers)),
            &crate::domain::models::MouseConfig::default(),
        )
        .expect("a key press converts");
        presentation::route(state, &event, Host::Local, &LogFacts::default())
    }

    fn ctx<'a>(
        conversation: &'a Conversation,
        cancel: &'a CancellationToken,
        tab_id: TabId,
        local_allowed: bool,
    ) -> VisitOpenCtx<'a> {
        VisitOpenCtx {
            conversation,
            tab_id,
            local_allowed,
            session_cancel: cancel,
            tab_count: 1,
        }
    }

    /// One Main turn with three committed `task` results under the root `main`, each with its
    /// own linked attempt in a real `SessionHistory`, and the Inspector open on the middle one.
    struct Rig {
        _dir: tempfile::TempDir,
        history: Arc<SessionHistory>,
        #[cfg_attr(not(unix), allow(dead_code))]
        journal: Arc<NodeJournal>,
        #[cfg_attr(not(unix), allow(dead_code))]
        lock: PathBuf,
        state: TuiState,
        conversation: Conversation,
        cancel: CancellationToken,
        tx: VisitOpenSender,
        rx: mpsc::Receiver<VisitOpenDone>,
        keys: Vec<MainVisitKey>,
        calls: Vec<String>,
        tab_id: TabId,
        local_allowed: bool,
        tabs: TabManager,
    }

    impl Rig {
        async fn new() -> Self {
            Self::seeded(1).await
        }

        /// The middle result's attempt retains `middle_turns` ordinals (128 reaches the cap
        /// marker slot, so the real resolve is `Partial` with turns 1..=127).
        async fn seeded(middle_turns: u32) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut turn = Turn::new("model".into(), 0);
            let mut pairs = Vec::new();
            for n in 0..3 {
                let invocation = turn.push_part(|id| TurnPart::ToolInvocation {
                    id,
                    tool: "task".into(),
                    args: serde_json::json!({ "description": format!("task {n}") }),
                    status: InvocationStatus::Success,
                    started_at: 0,
                    ended_at: Some(1),
                });
                let content = format!("literal output {n}");
                let result = turn.push_part(|id| TurnPart::ToolResult {
                    id,
                    refs: invocation,
                    output: ToolOutput {
                        content: content.clone(),
                        is_error: false,
                        diff: WriteDiffState::NotAWrite,
                        images: Vec::new(),
                    },
                });
                turn.visit_deadlines.insert(result, retained_deadline());
                pairs.push((invocation, result, content));
            }
            let keys: Vec<MainVisitKey> = pairs
                .iter()
                .map(|(invocation, result, content)| MainVisitKey {
                    conversation_id: "main".into(),
                    turn_id: turn.id.clone(),
                    invocation: *invocation,
                    result: *result,
                    revision: hash_body(content),
                })
                .collect();
            let calls: Vec<String> = pairs
                .iter()
                .map(|(invocation, ..)| {
                    crate::domain::models::tool_call_id_for(&turn.id, *invocation)
                })
                .collect();

            let journal = Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap());
            let history = Arc::new(SessionHistory::new(
                journal.clone(),
                Arc::new(FileSystemArtifactStore::new(dir.path())),
                HostBinding::new("host", "workspace"),
            ));
            for (n, (key, (_, _, content))) in keys.iter().zip(&pairs).enumerate() {
                let attempt = AgentId::new();
                history.begin(&key.conversation_id, &attempt).unwrap();
                let ordinals = if n == 1 { middle_turns } else { 1 };
                for ordinal in 1..=ordinals {
                    let text = if middle_turns == 1 || n != 1 {
                        format!("PUBLIC_{n}")
                    } else {
                        format!("t{ordinal}")
                    };
                    history
                        .record_turn(
                            &attempt,
                            &AgentId::root(),
                            ordinal,
                            &["Read: ok".into()],
                            &text,
                        )
                        .await
                        .unwrap();
                }
                history
                    .offer(
                        &key.conversation_id,
                        &attempt,
                        content,
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

            let (tx, rx) = mpsc::channel(4);
            let tabs = TabManager::new(CancellationToken::new());
            let mut rig = Self {
                lock: journal.path().with_extension("lock"),
                _dir: dir,
                history,
                journal,
                state: TuiState::new(80, 24),
                conversation: Conversation {
                    id: "main".into(),
                    turns: vec![turn],
                    ..Default::default()
                },
                cancel: CancellationToken::new(),
                tx,
                rx,
                keys,
                calls,
                tab_id: tabs.active_tab_id(),
                local_allowed: true,
                tabs,
            };
            rig.reset();
            rig
        }

        /// A fresh state with the Inspector open on the middle result, and a clean context.
        fn reset(&mut self) {
            self.conversation.id = "main".into();
            self.state = TuiState::new(80, 24);
            self.state.focused_item_id = Some(self.calls[1].clone());
            assert!(tool_inspector::inspect_focused(
                &mut self.state,
                &self.conversation,
                None,
            ));
            self.cancel = CancellationToken::new();
            self.tab_id = self.tabs.active_tab_id();
            self.local_allowed = true;
        }

        fn begin(&mut self) -> Option<JoinHandle<()>> {
            let history = Some(self.history.clone());
            self.begin_with(history)
        }

        fn begin_with(&mut self, history: Option<Arc<SessionHistory>>) -> Option<JoinHandle<()>> {
            begin_current(
                &mut self.state,
                history,
                ctx(
                    &self.conversation,
                    &self.cancel,
                    self.tab_id,
                    self.local_allowed,
                ),
                &self.tx,
            )
        }

        fn begin_related(&mut self, key: MainVisitKey) -> Option<JoinHandle<()>> {
            begin_related(
                &mut self.state,
                Some(self.history.clone()),
                ctx(
                    &self.conversation,
                    &self.cancel,
                    self.tab_id,
                    self.local_allowed,
                ),
                &self.tx,
                key,
            )
        }

        fn reconciled(&mut self) {
            reconcile(
                &mut self.state,
                ctx(
                    &self.conversation,
                    &self.cancel,
                    self.tab_id,
                    self.local_allowed,
                ),
            );
        }

        fn applied(&mut self, done: VisitOpenDone) -> bool {
            apply(
                &mut self.state,
                done,
                ctx(
                    &self.conversation,
                    &self.cancel,
                    self.tab_id,
                    self.local_allowed,
                ),
            )
        }

        /// Wait for the read's completion (a structural wait on the loop's channel).
        async fn done(&mut self) -> VisitOpenDone {
            tokio::time::timeout(Duration::from_secs(30), self.rx.recv())
                .await
                .expect("the read finishes")
                .expect("the channel is open")
        }

        /// With the hold taken the read is genuinely in flight: no completion arrives within a
        /// bounded wait and the task is still running. A hold on the wrong path would let the
        /// read finish inside the bound and fail this.
        #[cfg(unix)]
        async fn assert_held_read_blocked(&mut self, handle: &JoinHandle<()>) {
            let early = tokio::time::timeout(Duration::from_millis(250), self.rx.recv()).await;
            assert!(early.is_err(), "the held read must not complete");
            assert!(!handle.is_finished(), "the held read is still in flight");
        }

        fn key(&mut self, code: KeyCode) -> Option<RouteEffect> {
            route_key(&mut self.state, code)
        }

        fn top_id(&self) -> u64 {
            self.state.modal_stack.top().expect("a frame is open").id
        }

        fn visit_frame_open(&self) -> bool {
            self.state
                .modal_stack
                .iter()
                .any(|frame| frame.surface == Surface::SessionVisit)
        }

        /// One production paint at 80x24 (`render_with_visit`: reconcile, reauthorize, then the
        /// draw that refreshes the Inspector), as the screen text.
        fn paint(&mut self) -> String {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            crate::infrastructure::runtime::event_loop::render_with_visit(
                &mut terminal,
                &mut self.state,
                &self.conversation,
                &crate::domain::models::StreamingState::default(),
                "model",
                "provider",
                &crate::adapters::noop::NoOpSecurity,
                &mut self.tabs,
                &crate::domain::services::session_index::SessionIndex::new(),
                128_000,
                Some(self.history.as_ref()),
                &self.cancel,
            )
            .unwrap();
            let buffer = terminal.backend().buffer();
            (0..24_u16)
                .map(|y| {
                    (0..80_u16)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn last_notice(&self) -> String {
            self.state
                .notifications
                .records()
                .next_back()
                .expect("a notice")
                .message
                .clone()
        }

        fn pending_generation(&self) -> Option<u64> {
            session_visit::pending_open(&self.state).map(|opening| opening.generation)
        }

        /// The result's literal output changes, as an edit after the key would.
        fn edit_middle_result(&mut self) {
            let TurnPart::ToolResult { output, .. } = &mut self.conversation.turns[0].parts[3]
            else {
                panic!("the middle result");
            };
            output.content = "edited".into();
        }

        /// Applying `done` must change nothing: no Visit, no frame, no notice; a redraw only
        /// when `expect_redraw` (a pending open was dropped and its cue must be repainted).
        fn assert_quiet(&mut self, done: VisitOpenDone, why: &str, expect_redraw: bool) {
            let notices = self.state.notifications.len();
            let frames = self.state.modal_stack.len();
            self.state.needs_redraw = false;
            assert!(!self.applied(done), "{why}: the completion is dropped");
            assert!(self.state.session_visit.is_none(), "{why}: no Visit");
            assert_eq!(self.state.modal_stack.len(), frames, "{why}: no frame");
            assert_eq!(self.state.notifications.len(), notices, "{why}: no notice");
            assert_eq!(self.state.needs_redraw, expect_redraw, "{why}: redraw");
        }
    }

    #[tokio::test]
    async fn visit_async_open_begin_records_one_pending_open_and_the_live_completion_opens_it() {
        let mut rig = Rig::new().await;
        let frame = rig.top_id();
        rig.state.needs_redraw = false;
        let handle = rig
            .begin()
            .expect("the checks pass and one read is spawned");
        assert_eq!(rig.state.visit_open_generation, 1);
        let pending = session_visit::pending_open(&rig.state).expect("a pending open");
        assert_eq!(pending.generation, 1);
        assert_eq!(pending.key, rig.keys[1]);
        assert_eq!(
            pending.origin,
            OpeningOrigin::Inspector {
                frame_id: frame,
                tab: ToolTab::Output
            }
        );
        assert!(rig.state.needs_redraw, "the cue paints in this dispatch");
        assert!(active(&rig.state));
        assert!(redraw_needed(false, &rig.state));
        assert!(
            rig.state.session_visit.is_none(),
            "nothing opens before the read ends"
        );
        assert!(
            rig.begin().is_none(),
            "`o` is idempotent while its read is pending"
        );
        assert_eq!(rig.state.visit_open_generation, 1);

        let done = rig.done().await;
        assert!(rig.applied(done));
        let visit = rig.state.session_visit.as_ref().expect("the Visit opens");
        assert!(visit.title().starts_with("Session Visit · "));
        assert!(visit.title().ends_with("Read-only"));
        assert!(matches!(
            &visit.text.body,
            ArtifactBody::Source(bytes) if bytes.as_slice() == "Turn 1 · Tools: Read: ok\nPUBLIC_1".as_bytes()
        ));
        assert_eq!(
            visit.text.layouts_built(),
            1,
            "the first layout was prepared off the loop"
        );
        assert!(session_visit::pending_open(&rig.state).is_none());
        assert_ne!(rig.top_id(), frame, "the Visit is a new frame on top");
        assert_eq!(
            rig.state.modal_stack.top().unwrap().surface,
            Surface::SessionVisit
        );
        // A real paint at the frame's width reuses the layout built off the loop.
        let rect = tool_inspector::frame_rect(&rig.state, 1).expect("the frame fits 80x24");
        let title = rig.state.modal_stack.top().unwrap().title().to_owned();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                crate::adapters::tui::widgets::agent_window::render_visit(
                    frame, rect, &rig.state, &title, false,
                );
            })
            .unwrap();
        assert_eq!(
            rig.state
                .session_visit
                .as_ref()
                .unwrap()
                .text
                .layouts_built(),
            1,
            "the first paint reuses the prewarmed layout"
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn visit_async_open_failed_checks_post_the_existing_notice_and_start_nothing() {
        let mut rig = Rig::new().await;
        let notices = rig.state.notifications.len();
        rig.local_allowed = false;
        assert!(rig.begin().is_none());
        assert_eq!(
            rig.last_notice(),
            "Session Visit is not allowed for this source."
        );
        assert_eq!(rig.state.notifications.len(), notices + 1);

        rig.local_allowed = true;
        rig.edit_middle_result();
        assert!(rig.begin().is_none(), "an ineligible result starts no read");
        assert_eq!(rig.last_notice(), MISSING);
        assert_eq!(rig.state.notifications.len(), notices + 2);
        assert_eq!(rig.state.visit_open_generation, 0);
        assert!(session_visit::pending_open(&rig.state).is_none());
        assert!(!active(&rig.state));
        assert!(rig.rx.try_recv().is_err(), "no task exists to complete");
    }

    fn turn_with(id: &str, tool: &str, role: MessageRole) -> (Turn, MainVisitKey) {
        let mut turn = Turn::new("model".into(), 0);
        turn.id = TurnId(id.into());
        turn.role = role;
        let invocation = turn.push_part(|part| TurnPart::ToolInvocation {
            id: part,
            tool: tool.into(),
            args: serde_json::json!({}),
            status: InvocationStatus::Success,
            started_at: 0,
            ended_at: Some(1),
        });
        let result = turn.push_part(|part| TurnPart::ToolResult {
            id: part,
            refs: invocation,
            output: ToolOutput {
                content: "out".into(),
                is_error: false,
                diff: WriteDiffState::NotAWrite,
                images: Vec::new(),
            },
        });
        let key = MainVisitKey {
            conversation_id: "main".into(),
            turn_id: turn.id.clone(),
            invocation,
            result,
            revision: hash_body("out"),
        };
        (turn, key)
    }

    #[tokio::test]
    async fn visit_async_open_snapshot_gives_the_same_exactness_for_every_key() {
        let dir = tempfile::tempdir().unwrap();
        let history = SessionHistory::new(
            Arc::new(NodeJournal::open_workspace(dir.path()).await.unwrap()),
            Arc::new(FileSystemArtifactStore::new(dir.path())),
            HostBinding::new("host", "workspace"),
        );
        let (task, task_key) = turn_with("task", "task", MessageRole::Assistant);
        let (duplicate, duplicate_key) = turn_with("duplicate", "task", MessageRole::Assistant);
        let (read, read_key) = turn_with("read", "Read", MessageRole::Assistant);
        let (shared, shared_key) = turn_with("shared", "task", MessageRole::Assistant);
        let (user, _) = turn_with("shared", "task", MessageRole::User);
        let (mixed, mixed_key) = turn_with("mixed", "task", MessageRole::Assistant);
        let (mixed_read, _) = turn_with("mixed", "Read", MessageRole::Assistant);
        let mut missing_key = task_key.clone();
        missing_key.turn_id = TurnId("nowhere".into());
        let mut foreign_key = task_key.clone();
        foreign_key.conversation_id = "other".into();

        let live = Conversation {
            id: "main".into(),
            turns: vec![
                task,
                duplicate.clone(),
                duplicate,
                read,
                shared,
                user,
                mixed,
                mixed_read,
            ],
            ..Default::default()
        };
        let small = snapshot(&live);
        assert_eq!(small.id, live.id);
        assert_eq!(
            small.turns.len(),
            live.turns.len() - 1,
            "only the non-task turn is dropped"
        );
        for (name, key, exact) in [
            ("a unique task turn", &task_key, true),
            ("a turn id held twice", &duplicate_key, false),
            ("a non-task invocation", &read_key, false),
            ("a user turn sharing the id", &shared_key, false),
            ("a non-task turn sharing the id", &mixed_key, false),
            ("a missing turn", &missing_key, false),
            ("another conversation", &foreign_key, false),
        ] {
            assert_eq!(
                history.check_source(key, &live, true).is_none(),
                exact,
                "{name} on the live conversation"
            );
            assert_eq!(
                history.check_source(key, &small, true).is_none(),
                exact,
                "{name} on the snapshot"
            );
        }
    }

    #[tokio::test]
    async fn visit_async_open_reconcile_drops_each_context_change_and_keeps_the_rest() {
        type Change = fn(&mut Rig);
        let mut rig = Rig::new().await;
        let drops: [(&str, Change); 7] = [
            ("tool tab", |rig| {
                rig.key(KeyCode::Right);
            }),
            ("search editor", |rig| {
                rig.key(KeyCode::Char('/'));
            }),
            ("frame above", |rig| {
                reducer::open(&mut rig.state, Surface::Help);
            }),
            ("session tab", |rig| rig.tab_id = 7),
            ("conversation", |rig| rig.conversation.id = "other".into()),
            ("read-only tab", |rig| rig.local_allowed = false),
            ("session cancel", |rig| rig.cancel.cancel()),
        ];
        for (name, change) in drops {
            rig.reset();
            let handle = rig.begin().expect(name);
            assert!(rig.pending_generation().is_some(), "{name}: pending first");
            change(&mut rig);
            rig.reconciled();
            assert!(rig.pending_generation().is_none(), "{name}: dropped");
            handle.await.unwrap();
            assert!(
                rig.rx.try_recv().is_err(),
                "{name}: a cancelled read never sends"
            );
        }

        let keeps: [(&str, Change); 4] = [
            ("scroll", |rig| {
                rig.key(KeyCode::Down);
            }),
            ("wrap", |rig| {
                rig.key(KeyCode::Char('w'));
            }),
            ("copy", |rig| {
                rig.key(KeyCode::Char('c'));
            }),
            ("focus change", |rig| {
                let effect = presentation::route(
                    &mut rig.state,
                    &DomainInputEvent::FocusLost,
                    Host::Local,
                    &LogFacts::default(),
                );
                assert_eq!(effect, None, "focus is not a context change");
            }),
        ];
        for (name, change) in keeps {
            rig.reset();
            let handle = rig.begin().expect(name);
            let generation = rig.pending_generation();
            change(&mut rig);
            rig.reconciled();
            assert_eq!(rig.pending_generation(), generation, "{name}: kept");
            session_visit::clear_pending(&mut rig.state);
            handle.await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_held_read_esc_cancels_at_once_and_nothing_late_appears() {
        let mut rig = Rig::new().await;
        rig.state.input_buffer = "parked draft".into();
        rig.state.cursor_position = 4;
        let inspector = rig.top_id();
        let hold = Hold::acquire(&rig.lock);
        let handle = rig.begin().expect("the read is spawned");
        rig.assert_held_read_blocked(&handle).await;
        assert!(rig.pending_generation().is_some());
        let notices = rig.state.notifications.len();

        rig.state.needs_redraw = false;
        assert_eq!(
            rig.key(KeyCode::Esc),
            Some(RouteEffect::Action(InputAction::Consumed)),
            "the key's dispatch is redrawn by the loop"
        );
        assert!(rig.pending_generation().is_none());
        assert!(rig.state.needs_redraw);
        assert_eq!(rig.top_id(), inspector, "`Esc` only cancels: no Back");
        // The task ends although the read is still blocked on the held lock.
        joined(handle).await;
        drop(hold);
        assert!(
            rig.rx.try_recv().is_err(),
            "the channel holds no completion"
        );
        assert!(rig.state.session_visit.is_none());
        assert!(!rig.visit_frame_open());
        assert_eq!(rig.state.notifications.len(), notices, "no notice");

        rig.key(KeyCode::Esc);
        assert!(
            rig.state.modal_stack.top().is_none(),
            "the second `Esc` is the ordinary Back"
        );
        assert_eq!(
            (rig.state.input_buffer.as_str(), rig.state.cursor_position),
            ("parked draft", 4)
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_held_read_step_key_cancels_and_shows_the_next_call() {
        let mut rig = Rig::new().await;
        let hold = Hold::acquire(&rig.lock);
        let handle = rig.begin().expect("the read is spawned");
        rig.assert_held_read_blocked(&handle).await;
        let Some(RouteEffect::Action(InputAction::StepTool { forward })) =
            rig.key(KeyCode::Char(']'))
        else {
            panic!("`]` is the step key");
        };
        assert!(forward);
        assert!(tool_inspector::step(
            &mut rig.state,
            &rig.conversation,
            None,
            forward,
        ));
        assert!(
            rig.pending_generation().is_none(),
            "the replaced payload dropped its open"
        );
        assert!(rig.state.modal_stack.top().unwrap().title().ends_with("#3"));
        joined(handle).await;
        drop(hold);
        assert!(rig.rx.try_recv().is_err());
        assert!(rig.state.session_visit.is_none());
    }

    /// AC3 positive control: the same hold without `Esc`, once released, yields exactly one
    /// completion that opens the Visit once.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_held_read_completes_and_opens_once_after_the_hold_is_released() {
        let mut rig = Rig::new().await;
        let hold = Hold::acquire(&rig.lock);
        let handle = rig.begin().expect("the read is spawned");
        rig.assert_held_read_blocked(&handle).await;
        assert!(
            rig.state.session_visit.is_none(),
            "nothing opens while held"
        );
        drop(hold);
        let done = rig.done().await;
        joined(handle).await;
        assert!(rig.applied(done));
        let visits = rig
            .state
            .modal_stack
            .iter()
            .filter(|frame| frame.surface == Surface::SessionVisit)
            .count();
        assert_eq!(visits, 1, "one Visit frame");
        assert!(rig.state.session_visit.is_some());
        assert!(rig.rx.try_recv().is_err(), "exactly one completion");
    }

    #[tokio::test]
    async fn visit_async_open_reconcile_redraws_when_an_edited_result_drops_the_open() {
        let mut rig = Rig::new().await;
        let handle = rig.begin().expect("the read starts");
        rig.edit_middle_result();
        // The paint that recomputes `visit_current` (`render` refreshes before drawing).
        tool_inspector::refresh(
            &mut rig.state,
            &rig.conversation,
            None,
            &crate::domain::clock::MockClock::at_wall_ms(0),
            1,
        );
        rig.state.needs_redraw = false;
        rig.reconciled();
        assert!(rig.pending_generation().is_none(), "the open is dropped");
        assert!(
            rig.state.needs_redraw,
            "the stale cue is repainted as the ordinary Back rail"
        );
        handle.await.unwrap();
        assert!(rig.rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn visit_async_open_only_the_live_completion_applies() {
        let mut rig = Rig::new().await;

        // A completion of a superseded generation.
        let first = rig.begin().expect("first read");
        let stale = rig.done().await;
        first.await.unwrap();
        assert!(session_visit::cancel_opening(&mut rig.state));
        let second = rig.begin().expect("second read");
        assert_eq!(rig.state.visit_open_generation, 2);
        rig.assert_quiet(stale, "superseded generation", false);
        assert_eq!(
            rig.pending_generation(),
            Some(2),
            "the live open is untouched"
        );
        let live = rig.done().await;
        assert!(rig.applied(live), "only the live generation applies");
        assert!(rig.state.session_visit.is_some());
        second.await.unwrap();

        // The origin payload replaced, closed, covered, or the open cancelled.
        for case in [
            "replaced",
            "closed",
            "covered",
            "cancelled",
            "authority lost",
        ] {
            rig.reset();
            let handle = rig.begin_with(if case == "cancelled" {
                None
            } else {
                Some(rig.history.clone())
            });
            let done = rig.done().await;
            handle.expect("read").await.unwrap();
            match case {
                "replaced" => assert!(tool_inspector::step(
                    &mut rig.state,
                    &rig.conversation,
                    None,
                    true
                )),
                "closed" => {
                    rig.key(KeyCode::Esc);
                    rig.key(KeyCode::Esc);
                    assert!(rig.state.modal_stack.is_empty());
                }
                "covered" => {
                    reducer::open(&mut rig.state, Surface::Help);
                }
                "cancelled" => {
                    // A failing read: its sentence must stay unposted once cancelled.
                    rig.key(KeyCode::Esc);
                }
                _ => rig.local_allowed = false,
            }
            rig.assert_quiet(done, case, matches!(case, "covered" | "authority lost"));
        }

        // A live failing completion posts exactly the existing sentence, only when live.
        rig.reset();
        let handle = rig.begin_with(None).expect("a read without custody");
        let done = rig.done().await;
        handle.await.unwrap();
        assert!(rig.applied(done));
        assert_eq!(rig.last_notice(), MISSING);
        assert!(rig.state.session_visit.is_none());
        assert!(rig.pending_generation().is_none());
    }

    #[tokio::test]
    async fn visit_async_open_result_edited_during_the_read_shows_changed_before_the_paint() {
        let mut rig = Rig::new().await;
        let handle = rig.begin().expect("the read is spawned");
        let done = rig.done().await;
        handle.await.unwrap();
        rig.edit_middle_result();
        assert!(rig.applied(done), "the pending open is still the live one");
        assert!(rig.state.session_visit.is_some());

        // What `render_local!` does before the first paint.
        rig.reconciled();
        mission_bridge::reauthorize_with(
            &mut rig.state,
            Some(rig.history.as_ref()),
            &rig.conversation,
            true,
        );
        let visit = rig.state.session_visit.as_ref().unwrap();
        assert_eq!(visit.title(), "Session Visit · Changed");
        assert!(!visit.text.has_source(), "no retained text is painted");
    }

    #[tokio::test]
    async fn visit_async_open_related_open_is_asynchronous_cancelable_and_guarded() {
        let mut rig = Rig::new().await;
        let handle = rig.begin().expect("the open starts");
        let done = rig.done().await;
        assert!(rig.applied(done));
        handle.await.unwrap();
        let visit_frame = rig.top_id();
        let next = rig.keys[2].clone();
        let previous = rig.keys[0].clone();

        // Esc cancels the related open only; the current Visit stays open and on top.
        let related = rig
            .begin_related(next.clone())
            .expect("the related read starts");
        assert_eq!(rig.state.visit_open_generation, 2);
        assert_eq!(
            session_visit::pending_open(&rig.state).map(|opening| opening.origin.clone()),
            Some(OpeningOrigin::Visit {
                frame_id: visit_frame,
                cursor_key: rig.keys[1].clone()
            })
        );
        assert!(
            rig.begin_related(previous.clone()).is_none(),
            "one pending open at a time"
        );
        assert_eq!(
            rig.key(KeyCode::Esc),
            Some(RouteEffect::Action(InputAction::Consumed))
        );
        related.await.unwrap();
        assert!(rig.rx.try_recv().is_err());
        assert_eq!(rig.top_id(), visit_frame);
        assert_eq!(
            session_visit::visit_cursor(&rig.state).unwrap().key,
            rig.keys[1]
        );

        // A stale related completion is dropped.
        let stale = rig.begin_related(previous).expect("another related read");
        let stale_done = rig.done().await;
        stale.await.unwrap();
        assert!(session_visit::cancel_opening(&mut rig.state));
        let notices = rig.state.notifications.len();
        assert!(!rig.applied(stale_done));
        assert_eq!(
            session_visit::visit_cursor(&rig.state).unwrap().key,
            rig.keys[1]
        );
        assert_eq!(rig.state.notifications.len(), notices);

        // The live related completion refreshes the current Visit, then opens the target in
        // the same frame.
        let frames = rig.state.modal_stack.len();
        let live = rig
            .begin_related(next.clone())
            .expect("the related read starts");
        let done = rig.done().await;
        live.await.unwrap();
        assert!(rig.applied(done));
        assert_eq!(session_visit::visit_cursor(&rig.state).unwrap().key, next);
        assert_eq!(
            rig.state.modal_stack.len(),
            frames,
            "the related open reuses the frame"
        );
        let title = rig.state.session_visit.as_ref().unwrap().title();
        assert!(title.ends_with("Read-only"));
        assert!(session_visit::pending_open(&rig.state).is_none());
    }

    #[tokio::test]
    async fn visit_async_open_ctrl_c_cancels_like_esc_and_o_starts_nothing_while_pending() {
        let mut rig = Rig::new().await;
        let handle = rig.begin().expect("the read starts");
        assert_eq!(
            rig.key(KeyCode::Char('o')),
            Some(RouteEffect::Action(InputAction::VisitCurrentSource))
        );
        assert!(rig.begin().is_none(), "the loop's `o` effect is a no-op");
        assert_eq!(rig.state.visit_open_generation, 1, "no second generation");

        let effect = route_chord(&mut rig.state, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(effect, Some(RouteEffect::Action(InputAction::Consumed)));
        assert!(rig.pending_generation().is_none());
        assert!(
            !rig.state.modal_stack.is_empty(),
            "the Inspector stays open"
        );
        handle.await.unwrap();
        assert!(rig.rx.try_recv().is_err());

        // Without a pending open the same chord is the ordinary Back.
        route_chord(&mut rig.state, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(rig.state.modal_stack.is_empty());
    }

    /// M6: the read and the reader preparation run on the one named reader thread, never on
    /// the caller's runtime or an arbitrary blocking-pool thread, so their memory stays in one
    /// allocator arena that the next open reuses.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_reads_and_prepares_on_the_one_reader_thread() {
        let (reader, name) = READER
            .spawn(async {
                let thread = std::thread::current();
                (thread.id(), thread.name().map(str::to_owned))
            })
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some(READER_THREAD));
        assert_ne!(reader, std::thread::current().id());
        let before = probe::ON_READER.load(Ordering::SeqCst);
        let mut rig = Rig::new().await;
        for round in 0..2 {
            rig.reset();
            let handle = rig.begin().expect("the read starts");
            let done = rig.done().await;
            handle.await.unwrap();
            assert!(
                rig.applied(done),
                "round {round}: the live completion applies"
            );
            assert!(rig.state.session_visit.is_some());
        }
        assert!(
            probe::ON_READER.load(Ordering::SeqCst) >= before + 2,
            "both opens prepared their reader on the reader thread"
        );
        assert_eq!(
            probe::ELSEWHERE.load(Ordering::SeqCst),
            0,
            "no reader is prepared on any other thread"
        );
    }

    /// AC9: a read's job (snapshot, history handle, token) is released when the read ends,
    /// whether it applied, was cancelled with `Esc` while its journal step is still held, or
    /// was superseded by a newer open; a closed Visit keeps no reader.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_applied_cancelled_and_superseded_reads_release_their_job() {
        let mut rig = Rig::new().await;
        assert_eq!(Arc::strong_count(&rig.history), 1);

        let handle = rig.begin().expect("the read starts");
        let done = rig.done().await;
        joined(handle).await;
        assert_eq!(
            Arc::strong_count(&rig.history),
            1,
            "a finished read keeps no job"
        );
        assert!(rig.applied(done));
        reducer::close(&mut rig.state, Surface::SessionVisit);
        assert!(
            rig.state.session_visit.is_none(),
            "a closed Visit keeps no reader"
        );

        let hold = Hold::acquire(&rig.lock);
        let handle = rig.begin().expect("the read starts");
        rig.assert_held_read_blocked(&handle).await;
        assert_eq!(
            rig.key(KeyCode::Esc),
            Some(RouteEffect::Action(InputAction::Consumed))
        );
        joined(handle).await;
        assert_eq!(
            Arc::strong_count(&rig.history),
            1,
            "a cancelled read keeps nothing while its held step finishes"
        );
        drop(hold);

        let first = rig.begin().expect("the first read starts");
        let stale = rig.done().await;
        joined(first).await;
        assert!(session_visit::cancel_opening(&mut rig.state));
        let second = rig.begin().expect("the newer read starts");
        let live = rig.done().await;
        joined(second).await;
        assert!(!rig.applied(stale), "the superseded completion is dropped");
        assert!(rig.applied(live));
        assert_eq!(
            Arc::strong_count(&rig.history),
            1,
            "superseded and live reads keep no job"
        );
    }

    /// IR-1/IR-4: the loop's arbitration step (`Parked`, the production type the loop uses)
    /// with the completion and an `Esc` / `]` both ready. The completion must not apply or
    /// paint before the key is dispatched; afterwards it is judged by what the key left.
    #[tokio::test]
    async fn visit_async_open_a_ready_esc_or_step_key_is_dispatched_before_the_completion_applies()
    {
        for code in [KeyCode::Esc, KeyCode::Char(']')] {
            let mut rig = Rig::new().await;
            let handle = rig.begin().expect("the read starts");
            let done = rig.done().await;
            handle.await.unwrap();
            let notices = rig.state.notifications.len();

            // Branch 6 parks the completion; the input is already ready in the terminal.
            let mut parked: Parked<KeyCode> = Parked::default();
            parked.park_done(done);
            let mut terminal = VecDeque::from([code]);
            assert!(
                parked.next_apply(|| terminal.pop_front()).is_none(),
                "{code:?}: the completion yields to the ready key"
            );
            assert!(
                rig.state.session_visit.is_none(),
                "{code:?}: nothing applied"
            );
            assert!(
                !rig.paint().contains("Session Visit ·"),
                "{code:?}: nothing painted"
            );

            // Branch 1 dispatches the parked key the way the loop does.
            let code = parked.take_input().expect("the key was parked");
            match rig.key(code) {
                Some(RouteEffect::Action(InputAction::StepTool { forward })) => {
                    assert!(tool_inspector::step(
                        &mut rig.state,
                        &rig.conversation,
                        None,
                        forward
                    ));
                }
                Some(RouteEffect::Action(InputAction::Consumed)) => {}
                other => panic!("{code:?}: unexpected dispatch {other:?}"),
            }

            // Nothing else is ready: the completion is now judged by the pending open left.
            let done = parked
                .next_apply(|| terminal.pop_front())
                .expect("no input is waiting any more");
            assert!(
                !rig.applied(done),
                "{code:?}: the late completion is dropped"
            );
            let screen = rig.paint();
            assert!(rig.state.session_visit.is_none(), "{code:?}: no Visit");
            assert!(!rig.visit_frame_open());
            assert!(
                !screen.contains("Session Visit ·"),
                "{code:?}: no Visit paint"
            );
            assert!(!screen.contains("PUBLIC_1"), "{code:?}: no retained body");
            assert_eq!(
                rig.state.notifications.len(),
                notices,
                "{code:?}: no notice"
            );
            if code == KeyCode::Esc {
                assert!(screen.contains("[Esc] Back"), "the ordinary rail");
            } else {
                let title = rig.state.modal_stack.top().unwrap().title();
                assert!(title.ends_with("#3"), "`]` reached result #3: {title}");
            }
        }
    }

    /// IR-3: a source that changes or expires while the read is pending is cancelled by the
    /// first production paint (`render_with_visit`: reconcile before the draw that refreshes
    /// the Inspector), which already shows the ordinary rail.
    #[tokio::test]
    async fn visit_async_open_a_stale_source_drops_the_open_on_the_first_production_paint() {
        let mut rig = Rig::new().await;
        for change in ["edited", "expired"] {
            rig.reset();
            let handle = rig.begin().expect(change);
            assert!(
                rig.paint().contains("Opening Session Visit"),
                "{change}: the cue paints while the source is current"
            );
            if change == "edited" {
                rig.edit_middle_result();
            } else {
                let result = rig.keys[1].result;
                rig.conversation.turns[0].visit_deadlines.insert(result, 0);
            }
            let screen = rig.paint();
            assert!(
                rig.pending_generation().is_none(),
                "{change}: the open is dropped"
            );
            assert!(
                !screen.contains("Opening Session Visit"),
                "{change}: no stale cue"
            );
            assert!(screen.contains("[Esc] Back"), "{change}: the ordinary rail");
            handle.await.unwrap();
            assert!(rig.rx.try_recv().is_err(), "{change}: no completion");
        }
    }

    /// IR-4 / AC6: loop iterations around a source change while a read is pending. A repaint
    /// first (the tick) drops the open on the live source and paints the ordinary rail, and
    /// the completion that arrives afterwards is dropped; a completion that arrives before
    /// any repaint applies and shows `Changed` (`..._result_edited_during_the_read_*`).
    #[tokio::test]
    async fn visit_async_open_a_completion_after_a_live_context_change_is_dropped_and_paints_plain()
    {
        let mut rig = Rig::new().await;
        let handle = rig.begin().expect("the read starts");
        let done = rig.done().await;
        handle.await.unwrap();
        rig.edit_middle_result();
        let plain = rig.paint();
        assert!(
            rig.pending_generation().is_none(),
            "the repaint dropped the open"
        );
        assert!(!plain.contains("Opening") && plain.contains("[Esc] Back"));

        let mut parked: Parked<KeyCode> = Parked::default();
        parked.park_done(done);
        let done = parked
            .next_apply(|| None)
            .expect("no input is ready: the completion is judged");
        assert!(!rig.applied(done));
        let screen = rig.paint();
        assert!(rig.state.session_visit.is_none());
        assert!(!screen.contains("Session Visit ·") && !screen.contains("Opening"));
        assert!(screen.contains("[Esc] Back"));
    }

    /// IR-2: repeated open/`Esc` cycles while the writer exclusion is held start at most one
    /// blocking projection; its descriptor and admission are released only when it ends.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visit_async_open_repeated_cancel_under_a_held_lock_keeps_one_blocked_worker() {
        use std::sync::atomic::Ordering;
        let mut rig = Rig::new().await;
        let probe = rig.journal.projection_probe();
        let hold = Hold::acquire(&rig.lock);
        for cycle in 0..4 {
            let handle = rig.begin().expect("the read starts");
            rig.assert_held_read_blocked(&handle).await;
            assert_eq!(
                rig.key(KeyCode::Esc),
                Some(RouteEffect::Action(InputAction::Consumed)),
                "cycle {cycle}"
            );
            joined(handle).await;
        }
        assert_eq!(
            probe.max.load(Ordering::SeqCst),
            1,
            "never more than one blocked projection"
        );
        assert_eq!(
            probe.live.load(Ordering::SeqCst),
            1,
            "the one worker still waits on the lock after every wrapper ended"
        );

        drop(hold);
        while probe.live.load(Ordering::SeqCst) != 0 {
            tokio::time::timeout(Duration::from_secs(30), probe.ended.notified())
                .await
                .expect("the blocked worker ends once the lock is released");
        }

        // Admission is free again: the next open completes and applies.
        let handle = rig.begin().expect("the read starts");
        let done = rig.done().await;
        joined(handle).await;
        assert!(rig.applied(done));
        assert_eq!(probe.live.load(Ordering::SeqCst), 0);
        assert_eq!(probe.max.load(Ordering::SeqCst), 1);
    }

    /// IR-5: a real history capped at slot 128 (written through `record_turn`) opens
    /// `Partial` through `begin_current` with exactly the 127 retained turns.
    #[tokio::test]
    async fn visit_async_open_a_capped_attempt_opens_partial_with_exactly_the_retained_turns() {
        let mut rig = Rig::seeded(128).await;
        let handle = rig.begin().expect("the read starts");
        let done = rig.done().await;
        handle.await.unwrap();
        assert!(rig.applied(done));
        let visit = rig.state.session_visit.as_ref().expect("the Visit opens");
        assert!(visit.title().ends_with("Partial"), "{}", visit.title());
        let expected = (1..=127)
            .map(|n| format!("Turn {n} · Tools: Read: ok\nt{n}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert!(matches!(
            &visit.text.body,
            ArtifactBody::Source(bytes) if bytes.as_slice() == expected.as_bytes()
        ));
        assert!(!expected.contains("Turn 128"), "the cap marker is no turn");
    }

    /// IR-6: a related open pending in a Visit is dropped, with its real completion rejected
    /// silently, when the Visit's cursor changes, its filter editor opens or a frame covers it.
    #[tokio::test]
    async fn visit_async_open_related_pending_drops_on_cursor_filter_editor_and_covering_frame() {
        let mut rig = Rig::new().await;
        for case in ["cursor", "filter editor", "covering frame"] {
            rig.reset();
            let first = rig.begin().expect(case);
            let opened = rig.done().await;
            first.await.unwrap();
            assert!(rig.applied(opened), "{case}: the Visit opens first");

            let related = rig
                .begin_related(rig.keys[2].clone())
                .expect("the related read starts");
            let done = rig.done().await;
            related.await.unwrap();
            assert!(rig.pending_generation().is_some(), "{case}: pending first");
            match case {
                "cursor" => {
                    let moved = rig.keys[0].clone();
                    rig.state.session_visit.as_mut().unwrap().cursor.key = moved;
                }
                "filter editor" => rig.state.session_visit.as_mut().unwrap().text.search.open(),
                _ => {
                    reducer::open(&mut rig.state, Surface::Help);
                }
            }
            rig.state.needs_redraw = false;
            rig.reconciled();
            assert!(rig.pending_generation().is_none(), "{case}: dropped");
            assert!(rig.state.needs_redraw, "{case}: repainted");

            let notices = rig.state.notifications.len();
            let frames = rig.state.modal_stack.len();
            let cursor = session_visit::visit_cursor(&rig.state).map(|c| c.key.clone());
            assert!(!rig.applied(done), "{case}: the completion is rejected");
            assert_eq!(rig.state.notifications.len(), notices, "{case}: no notice");
            assert_eq!(rig.state.modal_stack.len(), frames, "{case}: no frame");
            assert_eq!(
                session_visit::visit_cursor(&rig.state).map(|c| c.key.clone()),
                cursor,
                "{case}: the Visit is untouched"
            );
        }
    }
}
