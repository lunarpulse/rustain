//! Story `agents-thread` — the host bridge of the Agents tab's `t Open thread`.
//!
//! The Agents roster knows only node ids; the A2A runtime alone can say which delegation row
//! binds to which exact peer exchange. This module keeps the three host rules around that
//! answer: the **offers** are recomputed on every roster refresh ([`refresh_offers`]); **`t`**
//! re-checks the offer, mints a generation and spawns one lookup ([`open`]); the **completion**
//! applies only while that very lookup is still the pending one for the selected row of the
//! top Mission Control frame ([`ready`]), opening the sent-thread frame above Mission Control
//! or posting one Warning that says why nothing opened.
//!
//! Opening is a read: no `message/send`, no `tasks/*`, no provider turn, and the composer is
//! never read or written. The frame it opens is the one `/team thread <peer> <item>` opens,
//! rendered by the same renderer.

use crate::adapters::tui::mission_control::{self, McTab};
use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::Surface;
use crate::adapters::tui::state::{ThreadAccess, TuiState};
use crate::adapters::tui::thread_inspector::painted_peer;
use crate::domain::models::{AgentId, AgentThreadOutcome};

use super::app_state::AppState;

/// AC3(e): the offer no longer verifies (the alias left the roster, the row is gone, or the
/// task is not one this host dispatched to that peer); nothing was requested.
pub const UNVERIFIABLE: &str = "this agent's thread can no longer be verified — nothing was opened";

/// D11: a thread frame already beneath Mission Control would be truncated to (dropping
/// Mission Control), so `t` refuses and changes nothing.
pub const THREAD_BENEATH: &str =
    "a thread window is already open beneath Mission Control — close it first";

/// The most candidate item ids the many-items sentence lists before it says `…`.
const MAX_CANDIDATES: usize = 8;

/// AC3(b): the peer's scoped list names no item for the task.
#[cfg(feature = "a2a")]
pub(crate) fn no_item_sentence(peer: &str) -> String {
    format!(
        "{}'s host lists no item for this agent's task — nothing was opened",
        painted_peer(peer)
    )
}

/// AC3(c): two or more items name the task; the exact ids in list order, never a pick.
pub(crate) fn candidates_sentence(peer: &str, items: &[String]) -> String {
    let peer = painted_peer(peer);
    let shown = items
        .iter()
        .take(MAX_CANDIDATES)
        .map(|item| painted_peer(item))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if items.len() > MAX_CANDIDATES {
        ", …"
    } else {
        ""
    };
    format!(
        "{peer}'s host lists {} items for this agent's task: {shown}{more} — open one with /team thread {peer} <item-id>",
        items.len()
    )
}

/// AC3(d): the list read failed, in the existing outcome words (`unreachable`, `no usable
/// answer — …`).
#[cfg(feature = "a2a")]
pub(crate) fn list_failed_sentence(
    peer: &str,
    outcome: &crate::domain::models::RecipientOutcome,
) -> String {
    format!(
        "could not read {}'s items — {}",
        painted_peer(peer),
        crate::adapters::tui::handlers::team_command::outcome_label(outcome)
    )
}

/// Recompute the offers of the roster the Agents tab just cached (D2, D3): every refresh site
/// calls this after `apply_roster`. Only `Peer` rows are examined; the computation reads the
/// roster binding and the seeded dispatch ledger, never the network and never the journal's
/// write side. Without the `a2a` feature (or without a session runtime) nothing is offered.
pub async fn refresh_offers(state: &mut TuiState, app_state: &AppState) {
    #[cfg(feature = "a2a")]
    {
        let peers: Vec<_> = state
            .agent_panel_state
            .cached_entries
            .iter()
            .filter(|row| matches!(row.ownership, crate::domain::models::OwnershipKind::Peer))
            .cloned()
            .collect();
        refresh_offers_from(state, &peers, app_state.a2a_send.as_deref()).await;
    }
    #[cfg(not(feature = "a2a"))]
    {
        let _ = app_state;
        if !state.agent_threads.offers.is_empty() {
            state.agent_threads.offers.clear();
            state.needs_redraw = true;
        }
    }
}

/// The seam [`refresh_offers`] calls: replace `state.agent_threads.offers` with exactly one
/// offer per row of `rows` that the A2A owner verifies (D3), keyed by the row's `AgentId`.
/// No runtime means no offers.
#[cfg(feature = "a2a")]
pub async fn refresh_offers_from(
    state: &mut TuiState,
    rows: &[crate::domain::models::AgentRowView],
    runtime: Option<&crate::adapters::a2a::driver::A2aDelegationRuntime>,
) {
    let mut offers = std::collections::BTreeMap::new();
    if let Some(runtime) = runtime {
        for row in rows {
            if let Some(offer) = crate::adapters::a2a::agent_thread::offer(runtime, row).await {
                offers.insert(row.agent_id.clone(), offer);
            }
        }
    }
    if state.agent_threads.offers != offers {
        state.agent_threads.offers = offers;
        state.needs_redraw = true;
    }
}

/// A thread frame sits beneath Mission Control on the stack (D11).
fn thread_beneath_mission_control(state: &TuiState) -> bool {
    let thread = state
        .modal_stack
        .iter()
        .position(|frame| frame.surface == Surface::ThreadInspector);
    let hub = state
        .modal_stack
        .iter()
        .position(|frame| frame.surface == Surface::MissionControl);
    matches!((thread, hub), (Some(thread), Some(hub)) if thread < hub)
}

/// `t` on an Agents row (`InputAction::OpenAgentThread`): refuse when a thread frame sits
/// beneath Mission Control (D11); re-read the offer **by the row's id** and the roster (a row
/// that left, or an alias that was never verified, opens nothing and says so before any I/O);
/// mint the lookup's generation, mark it pending and spawn the one lookup under the session's
/// cancellation. The answer arrives as `AppEvent::AgentThreadReady` and applies through
/// [`ready`]. Never a model turn, never a send, never the composer.
pub fn open(state: &mut TuiState, conversation_id: &str, app_state: &AppState, agent_id: AgentId) {
    if thread_beneath_mission_control(state) {
        notifications::warning(state, NoticeSource::Team, THREAD_BENEATH);
        return;
    }
    #[cfg(feature = "a2a")]
    {
        use crate::adapters::tui::state::AgentThreadOpening;

        let in_roster = state
            .agent_panel_state
            .cached_entries
            .iter()
            .any(|entry| entry.agent_id == agent_id);
        let offer = in_roster
            .then(|| state.agent_threads.offers.get(&agent_id).cloned())
            .flatten();
        let (Some(runtime), Some(offer)) = (app_state.a2a_send.clone(), offer) else {
            notifications::warning(state, NoticeSource::Team, UNVERIFIABLE);
            return;
        };
        state.agent_threads.generation += 1;
        let generation = state.agent_threads.generation;
        state.agent_threads.opening = Some(AgentThreadOpening {
            agent: agent_id.clone(),
            generation,
        });
        state.needs_redraw = true;
        drop(spawn_open(
            runtime,
            app_state.event_bus.clone(),
            conversation_id.to_owned(),
            generation,
            agent_id,
            offer,
            app_state.session_cancel.child_token(),
        ));
    }
    #[cfg(not(feature = "a2a"))]
    {
        let _ = (conversation_id, app_state, agent_id);
        notifications::warning(state, NoticeSource::Team, UNVERIFIABLE);
    }
}

/// The lookup's spawn: session-scoped (a cancelled session publishes nothing further, the
/// `spawn_team_thread` precedent), one `AgentThreadReady` per lookup. Returns the handle so an
/// in-crate test can await it instead of waiting on a timer.
#[cfg(feature = "a2a")]
fn spawn_open(
    runtime: std::sync::Arc<crate::adapters::a2a::driver::A2aDelegationRuntime>,
    event_bus: std::sync::Arc<super::event_bus::EventBus>,
    conversation_id: String,
    generation: u64,
    agent_id: AgentId,
    offer: crate::domain::models::SentThreadOffer,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            outcome = resolve(&runtime, &offer) => outcome,
        };
        if cancel.is_cancelled() {
            return;
        }
        let _ = event_bus.emit_domain(crate::domain::events::AppEvent::AgentThreadReady {
            conversation_id,
            generation,
            agent_id,
            outcome,
        });
    })
}

/// Re-verify `offer`, resolve its exact exchange (D4) and render the one item's thread with the
/// renderer `/team thread <peer> <item>` uses, taking the follow-up target from the structured
/// read (D6). Zero or several items open nothing.
#[cfg(feature = "a2a")]
pub async fn resolve(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    offer: &crate::domain::models::SentThreadOffer,
) -> AgentThreadOutcome {
    use crate::adapters::a2a::agent_thread::{AgentThreadRead, open};
    use crate::adapters::a2a::send::{TaskThreadRead, ThreadRead};
    use crate::adapters::tui::handlers::team_command::{follow_up_target, render_team_thread};

    let missing = AgentThreadOutcome::Missing;
    match open(runtime, offer).await {
        AgentThreadRead::Unverifiable => missing(UNVERIFIABLE.to_owned()),
        AgentThreadRead::Read(TaskThreadRead::NoItem) => missing(no_item_sentence(&offer.peer)),
        AgentThreadRead::Read(TaskThreadRead::Candidates(items)) => {
            AgentThreadOutcome::Candidates {
                peer: offer.peer.clone(),
                items,
            }
        }
        AgentThreadRead::Read(TaskThreadRead::Failed(ThreadRead::Failed(outcome))) => {
            missing(list_failed_sentence(&offer.peer, &outcome))
        }
        // Only a roster race can leave the peer unknown after verification.
        AgentThreadRead::Read(TaskThreadRead::Failed(_)) => missing(UNVERIFIABLE.to_owned()),
        AgentThreadRead::Read(TaskThreadRead::One { item, read }) => {
            let credentialed = runtime
                .peer_binding(&offer.peer)
                .is_some_and(|(spec, _)| spec.auth.is_some());
            AgentThreadOutcome::Opened {
                peer: offer.peer.clone(),
                follow_up: follow_up_target(&read, &item),
                message: render_team_thread(&offer.peer, &item, &read, credentialed),
                item,
            }
        }
    }
}

/// Apply a lookup's completion (`AppEvent::AgentThreadReady`). It applies only while it is the
/// very lookup `state.agent_threads.opening` names, with Mission Control on top on the Agents
/// tab and that agent still selected (AC5): anything else — a moved selection, another tab,
/// another frame on top, a newer `t` — drops it without a frame or a notice. A dropped
/// completion of a superseded or already voided lookup changes nothing; a dropped completion
/// of the very lookup still pending (Mission Control covered by another frame, or its row not
/// selected) clears that `opening` and nothing else: it was that lookup's one completion, so
/// keeping the token would leave the row `opening thread…` with `t` inert for good. Once
/// Mission Control is on top again the rail offers `t` afresh, and pressing it reads again.
/// Applying clears `opening`; `Opened` opens the sent-thread frame above Mission Control and
/// records its follow-up target through the same guarded steps the typed `/team thread` uses;
/// every other outcome is one Warning. Returns whether it applied.
pub fn ready(
    state: &mut TuiState,
    conversation_id: &str,
    generation: u64,
    agent_id: &AgentId,
    outcome: AgentThreadOutcome,
) -> bool {
    let pending = state
        .agent_threads
        .opening
        .as_ref()
        .is_some_and(|opening| opening.generation == generation && opening.agent == *agent_id);
    if !pending {
        return false;
    }
    state.agent_threads.opening = None;
    state.needs_redraw = true;
    if !mission_control::is_top(state)
        || state.mission_control.tab != McTab::Agents
        || state.mission_control.selected_id() != Some(agent_id.as_str())
    {
        return false;
    }
    match outcome {
        AgentThreadOutcome::Opened {
            peer,
            item,
            message,
            follow_up,
        } => {
            let read = super::thread_open::open_sent(
                state,
                ThreadAccess::Writer,
                &peer,
                &item,
                conversation_id,
            );
            if super::thread_open::fill_sent(state, conversation_id, read, &message) {
                super::thread_open::offer_follow_up(
                    state,
                    conversation_id,
                    read,
                    follow_up.as_deref(),
                );
            }
        }
        AgentThreadOutcome::Candidates { peer, items } => {
            notifications::warning(
                state,
                NoticeSource::Team,
                candidates_sentence(&peer, &items),
            );
        }
        AgentThreadOutcome::Missing(sentence) => {
            notifications::warning(state, NoticeSource::Team, sentence);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::mission_control::agents;
    use crate::adapters::tui::presentation::reducer;
    use crate::adapters::tui::presentation::viewer::ViewerCmd;
    use crate::adapters::tui::state::{AgentThreadOpening, ThreadTarget};
    use crate::domain::models::NodeState;
    use crate::domain::models::subagent_view::{AgentRowView, OwnershipKind};

    fn row() -> AgentRowView {
        AgentRowView {
            isolated: false,
            agent_id: AgentId::new(),
            parent_id: AgentId::root(),
            subagent_type: "a2a-peer".to_owned(),
            spawned_at: 0,
            depth: 1,
            current_status: NodeState::Running,
            ownership: OwnershipKind::Peer,
            effective_model: String::new(),
            tools_summary: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            turns: 0,
            wait_reason: None,
        }
    }

    /// Mission Control on Agents, two delegation rows, the first selected (explicitly: with
    /// no selection the first row is already the implicit one, so `LineDown` would select the
    /// second) and its lookup (generation 5) pending.
    fn pending() -> (TuiState, AgentId, AgentId) {
        let mut state = TuiState::new(100, 30);
        let (first, second) = (row(), row());
        let ids = (first.agent_id.clone(), second.agent_id.clone());
        state.agent_panel_state.cached_entries = vec![first, second];
        agents::sync(&mut state);
        reducer::open_mission_control(&mut state, McTab::Agents);
        state.mission_control.move_selection(ViewerCmd::Top);
        state.agent_threads.opening = Some(AgentThreadOpening {
            agent: ids.0.clone(),
            generation: 5,
        });
        (state, ids.0, ids.1)
    }

    fn opened() -> AgentThreadOutcome {
        AgentThreadOutcome::Opened {
            peer: "marcus".to_owned(),
            item: "ri_1".to_owned(),
            message: "thread · marcus · ri_1\n  you       ri_1\n      hello".to_owned(),
            follow_up: Some("ri_1".to_owned()),
        }
    }

    fn last_notice(
        state: &TuiState,
    ) -> Option<(crate::adapters::tui::notifications::NoticeSeverity, String)> {
        state
            .notifications
            .records()
            .next_back()
            .map(|record| (record.severity, record.message.clone()))
    }

    #[test]
    fn agents_thread_ready_opens_the_sent_frame_above_mission_control_and_clears_opening() {
        let (mut state, id, _) = pending();
        state.input_buffer = "main draft".to_owned();
        state.cursor_position = 4;
        assert!(ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.agent_threads.opening.is_none());
        let inspector = state.thread_inspector.as_ref().expect("a thread frame");
        assert!(matches!(
            &inspector.target,
            ThreadTarget::Sent { peer, item } if peer == "marcus" && item == "ri_1"
        ));
        assert_eq!(inspector.access, ThreadAccess::Writer);
        let sent = inspector.sent.as_ref().unwrap();
        assert!(!sent.loading, "the completed read filled the frame");
        assert_eq!(sent.follow_up.as_deref(), Some("ri_1"));
        assert_eq!(sent.origin, "conv");
        // Above Mission Control, which keeps its tab and its selection beneath.
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.surface),
            Some(Surface::ThreadInspector)
        );
        assert!(
            state
                .modal_stack
                .iter()
                .any(|frame| frame.surface == Surface::MissionControl)
        );
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
        // The Main draft is untouched.
        assert_eq!(state.input_buffer, "main draft");
        assert_eq!(state.cursor_position, 4);
        // Esc returns to Mission Control on the same row.
        reducer::back(&mut state);
        assert!(mission_control::is_top(&state));
        assert_eq!(state.mission_control.tab, McTab::Agents);
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
    }

    #[test]
    fn agents_thread_a_late_or_superseded_completion_opens_and_notifies_nothing() {
        // Wrong generation (a newer `t` minted another) and a different agent.
        let (mut state, id, other) = pending();
        assert!(!ready(&mut state, "conv", 4, &id, opened()));
        assert!(!ready(&mut state, "conv", 5, &other, opened()));
        assert!(state.thread_inspector.is_none());
        assert!(state.notifications.is_empty());
        assert!(
            state.agent_threads.opening.is_some(),
            "a dropped completion leaves the pending lookup alone"
        );

        // The selection moved away: the guard drops it even if `opening` survived, and as the
        // lookup's one completion it frees the row it was pressed on.
        let (mut state, id, _) = pending();
        state.mission_control.move_selection(ViewerCmd::LineDown);
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());
        assert!(state.notifications.is_empty());
        assert!(state.agent_threads.opening.is_none());

        // Another tab is showing.
        let (mut state, id, _) = pending();
        reducer::set_mission_tab(&mut state, McTab::Tasks);
        assert!(
            state.agent_threads.opening.is_none(),
            "switching tab clears the lookup at once"
        );
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());

        // Another frame is on top of Mission Control: dropped without a frame or a notice,
        // and the lookup it answered is no longer pending (no other completion will come).
        let (mut state, id, _) = pending();
        reducer::open(&mut state, Surface::Help);
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());
        assert!(state.notifications.is_empty());
        assert_eq!(
            state.modal_stack.top().map(|frame| frame.surface),
            Some(Surface::Help)
        );
        assert!(state.agent_threads.opening.is_none());

        // Mission Control closed.
        let (mut state, id, _) = pending();
        reducer::close(&mut state, Surface::MissionControl);
        assert!(state.agent_threads.opening.is_none());
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());
    }

    #[test]
    fn agents_thread_moving_the_selection_voids_the_pending_lookup_for_good() {
        let (mut state, id, other) = pending();
        let moved = crate::adapters::tui::mission_control::keys::on_key(
            &mut state,
            crate::adapters::tui::keymap::Key::Char('j'),
            crate::adapters::tui::keymap::Host::Local,
        );
        assert_eq!(moved, crate::adapters::tui::app::InputAction::Consumed);
        assert_eq!(state.mission_control.selected_id(), Some(other.as_str()));
        assert!(state.agent_threads.opening.is_none());
        // Coming back does not revive it.
        crate::adapters::tui::mission_control::keys::on_key(
            &mut state,
            crate::adapters::tui::keymap::Key::Char('k'),
            crate::adapters::tui::keymap::Host::Local,
        );
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());
    }

    #[test]
    fn agents_thread_every_other_outcome_is_one_warning_and_no_frame() {
        use crate::adapters::tui::notifications::NoticeSeverity;

        let missing = no_item_sentence_for_tests("marcus");
        for (outcome, expected) in [
            (
                AgentThreadOutcome::Missing(UNVERIFIABLE.to_owned()),
                UNVERIFIABLE.to_owned(),
            ),
            (
                AgentThreadOutcome::Missing(missing.clone()),
                missing.clone(),
            ),
            (
                AgentThreadOutcome::Candidates {
                    peer: "marcus".to_owned(),
                    items: vec!["ri_1".to_owned(), "ri_2".to_owned()],
                },
                "marcus's host lists 2 items for this agent's task: ri_1, ri_2 — open one with /team thread marcus <item-id>".to_owned(),
            ),
        ] {
            let (mut state, id, _) = pending();
            assert!(ready(&mut state, "conv", 5, &id, outcome));
            assert!(state.thread_inspector.is_none(), "no frame for {expected}");
            assert!(state.agent_threads.opening.is_none());
            assert_eq!(
                last_notice(&state),
                Some((NoticeSeverity::Warning, expected))
            );
            assert!(mission_control::is_top(&state), "Mission Control stays on top");
        }
    }

    /// The AC3(b) sentence (the a2a-only constructor is not linked in a default build).
    fn no_item_sentence_for_tests(peer: &str) -> String {
        format!("{peer}'s host lists no item for this agent's task — nothing was opened")
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn agents_thread_sentences_are_exact_sanitized_and_bounded() {
        use crate::domain::models::RecipientOutcome;

        assert_eq!(
            no_item_sentence("marcus"),
            no_item_sentence_for_tests("marcus")
        );
        assert_eq!(
            list_failed_sentence("marcus", &RecipientOutcome::Unreachable { cause: None }),
            "could not read marcus's items — unreachable"
        );
        assert_eq!(
            list_failed_sentence("marcus", &RecipientOutcome::NoUsableAnswer),
            "could not read marcus's items — no usable answer — '/team board' shows whether it arrived"
        );
        // A hostile peer-chosen id never reaches the notice raw.
        let hostile =
            candidates_sentence("marcus", &["ri_\u{1b}[31m".to_owned(), "ri_2".to_owned()]);
        assert!(!hostile.contains('\u{1b}'), "{hostile:?}");
        // Many candidates are bounded.
        let many: Vec<String> = (0..20).map(|n| format!("ri_{n}")).collect();
        let sentence = candidates_sentence("marcus", &many);
        assert!(sentence.contains("lists 20 items"), "{sentence}");
        assert!(sentence.contains("ri_7, …"), "{sentence}");
        assert!(!sentence.contains("ri_8"), "{sentence}");
    }

    #[test]
    fn agents_thread_t_refuses_when_a_thread_frame_sits_beneath_mission_control() {
        let mut state = TuiState::new(100, 30);
        assert!(!thread_beneath_mission_control(&state));
        // A sent thread, then Mission Control above it.
        crate::infrastructure::runtime::thread_open::open_sent(
            &mut state,
            ThreadAccess::Writer,
            "marcus",
            "ri_1",
            "conv",
        );
        assert!(!thread_beneath_mission_control(&state));
        reducer::open_mission_control(&mut state, McTab::Agents);
        assert!(thread_beneath_mission_control(&state));
        // Mission Control alone, or a thread above it, is not "beneath".
        let mut state = TuiState::new(100, 30);
        reducer::open_mission_control(&mut state, McTab::Agents);
        assert!(!thread_beneath_mission_control(&state));
        crate::infrastructure::runtime::thread_open::open_sent(
            &mut state,
            ThreadAccess::Writer,
            "marcus",
            "ri_1",
            "conv",
        );
        assert!(!thread_beneath_mission_control(&state));
    }

    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn agents_thread_no_runtime_means_no_offers() {
        use crate::domain::models::SentThreadOffer;

        let (mut state, id, _) = pending();
        state.agent_threads.offers.insert(
            id,
            SentThreadOffer {
                peer: "marcus".to_owned(),
                task: "t-1".to_owned(),
            },
        );
        let rows = state.agent_panel_state.cached_entries.clone();
        refresh_offers_from(&mut state, &rows, None).await;
        assert!(state.agent_threads.offers.is_empty());
    }

    fn offer() -> crate::domain::models::SentThreadOffer {
        crate::domain::models::SentThreadOffer {
            peer: "marcus".to_owned(),
            task: "t-1".to_owned(),
        }
    }

    /// The rail (the last inner row) of Mission Control painted over the whole 100×30 screen,
    /// on the local host. Painting records the fit `t` acts on.
    fn painted_rail(state: &TuiState) -> String {
        use ratatui::{Terminal, backend::TestBackend};

        let (width, height) = (100, 30);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                crate::adapters::tui::widgets::mission_control::render(
                    frame,
                    frame.area(),
                    state,
                    "Agents",
                    crate::adapters::tui::keymap::Host::Local,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (1..width - 1)
            .map(|x| buffer[(x, height - 2)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    fn press_t(state: &mut TuiState) -> crate::adapters::tui::app::InputAction {
        crate::adapters::tui::mission_control::keys::on_key(
            state,
            crate::adapters::tui::keymap::Key::Char('t'),
            crate::adapters::tui::keymap::Host::Local,
        )
    }

    /// AT-05: the one completion of a lookup lands while Help covers Mission Control. It
    /// opens nothing and says nothing, and it frees the row: back on top, the rail offers `t`
    /// again (not `opening thread…` for good) and `t` acts.
    #[test]
    fn agents_thread_a_completion_dropped_under_a_cover_frees_t_once_mission_control_is_back() {
        use crate::adapters::tui::app::InputAction;

        let (mut state, id, _) = pending();
        state.agent_threads.offers.insert(id.clone(), offer());
        let rail = painted_rail(&state);
        assert!(rail.contains("opening thread…"), "{rail}");
        assert_eq!(press_t(&mut state), InputAction::Consumed);

        reducer::open(&mut state, Surface::Help);
        assert!(!ready(&mut state, "conv", 5, &id, opened()));
        assert!(state.thread_inspector.is_none());
        assert!(state.notifications.is_empty());

        reducer::back(&mut state);
        assert!(mission_control::is_top(&state));
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
        let rail = painted_rail(&state);
        assert!(rail.contains("[t] Open thread"), "{rail}");
        assert!(!rail.contains("opening thread…"), "{rail}");
        assert_eq!(press_t(&mut state), InputAction::OpenAgentThread(id));
        assert!(
            state.thread_inspector.is_none(),
            "the dropped result never opens late"
        );
    }

    #[cfg(feature = "a2a")]
    struct Accepting;

    #[cfg(feature = "a2a")]
    #[async_trait::async_trait]
    impl crate::domain::ports::RoomJournal for Accepting {
        async fn record_event(
            &self,
            _event: crate::domain::models::RoomEvent,
        ) -> Result<(), crate::domain::ports::RoomJournalError> {
            Ok(())
        }
    }

    /// A real `AppState` with a real A2A runtime that binds no roster alias: every lookup the
    /// production `open` spawns re-verifies to "unverifiable" at once, without network I/O,
    /// and publishes its one `AgentThreadReady` on the real bus. Returns the bus receiver.
    #[cfg(feature = "a2a")]
    fn front_door(
        workspace: &std::path::Path,
    ) -> (
        AppState,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) {
        let (mut app, events) =
            crate::infrastructure::runtime::transparency_bridge::bridge_app_state(workspace);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.a2a_send = Some(std::sync::Arc::new(
            crate::adapters::a2a::driver::A2aDelegationRuntime::new(
                crate::infrastructure::subagent::NodeTree::new(),
                std::sync::Arc::new(Accepting),
                tx,
            ),
        ));
        (app, events)
    }

    /// The next `AgentThreadReady` the bus carries: `(conversation, generation, agent,
    /// outcome)`. The deadline only bounds a broken run.
    #[cfg(feature = "a2a")]
    async fn next_ready(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) -> (String, u64, AgentId, AgentThreadOutcome) {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let crate::domain::events::AppEvent::AgentThreadReady {
                    conversation_id,
                    generation,
                    agent_id,
                    outcome,
                } = events.recv().await.expect("the bus stays open")
                {
                    return (conversation_id, generation, agent_id, outcome);
                }
            }
        })
        .await
        .expect("the lookup published its completion")
    }

    /// AT-05 through the production `open` and `ready`: `t`, a cover (Help, the command
    /// palette, the agent window), the real completion lands under it and is dropped with no
    /// frame and no notice; back on top the rail offers `t` again, `t` reads afresh (a new
    /// generation, a new real completion) and that completion applies.
    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn agents_thread_a_lookup_answered_under_a_cover_is_retryable_through_the_front_door() {
        use crate::adapters::tui::app::InputAction;
        use crate::adapters::tui::notifications::NoticeSeverity;

        for cover in [Surface::Help, Surface::CommandPalette, Surface::AgentWindow] {
            let workspace = tempfile::tempdir().unwrap();
            let (app, mut events) = front_door(workspace.path());
            let (mut state, id, _) = pending();
            state.agent_threads.opening = None;
            state.agent_threads.offers.insert(id.clone(), offer());
            painted_rail(&state);
            assert_eq!(
                press_t(&mut state),
                InputAction::OpenAgentThread(id.clone())
            );
            open(&mut state, "conv", &app, id.clone());
            let first = state.agent_threads.generation;
            assert!(state.agent_threads.opening.is_some(), "{cover:?}");

            if cover == Surface::AgentWindow {
                crate::adapters::tui::agent_window::open(&mut state, id.clone());
            } else {
                reducer::open(&mut state, cover);
            }
            let (conversation, generation, agent, outcome) = next_ready(&mut events).await;
            assert_eq!((generation, &agent), (first, &id), "{cover:?}");
            assert!(!ready(
                &mut state,
                &conversation,
                generation,
                &agent,
                outcome
            ));
            assert!(state.thread_inspector.is_none(), "{cover:?}: no late frame");
            assert!(state.notifications.is_empty(), "{cover:?}: no notice");
            assert_eq!(
                state.modal_stack.top().map(|frame| frame.surface),
                Some(cover)
            );
            assert!(state.agent_threads.opening.is_none(), "{cover:?}");

            reducer::back(&mut state);
            assert!(mission_control::is_top(&state), "{cover:?}");
            let rail = painted_rail(&state);
            assert!(rail.contains("[t] Open thread"), "{cover:?}: {rail}");
            assert!(!rail.contains("opening thread…"), "{cover:?}: {rail}");
            assert_eq!(
                press_t(&mut state),
                InputAction::OpenAgentThread(id.clone())
            );
            open(&mut state, "conv", &app, id.clone());
            let retry = state.agent_threads.generation;
            assert!(retry > first, "{cover:?}");

            let (conversation, generation, agent, outcome) = next_ready(&mut events).await;
            assert_eq!((generation, &agent), (retry, &id), "{cover:?}");
            if cover == Surface::Help {
                // The real answer of this runtime (no bound alias) is the one Warning.
                assert!(ready(
                    &mut state,
                    &conversation,
                    generation,
                    &agent,
                    outcome
                ));
                assert_eq!(
                    last_notice(&state),
                    Some((NoticeSeverity::Warning, UNVERIFIABLE.to_owned()))
                );
                assert_eq!(state.notifications.len(), 1);
                assert!(state.thread_inspector.is_none());
            } else {
                // The retry's real completion, carrying a found exchange (this in-crate runtime
                // has no peer to list): it opens the sent frame above Mission Control.
                assert!(ready(
                    &mut state,
                    &conversation,
                    generation,
                    &agent,
                    opened()
                ));
                assert!(state.notifications.is_empty());
                assert_eq!(
                    state.modal_stack.top().map(|frame| frame.surface),
                    Some(Surface::ThreadInspector)
                );
                reducer::back(&mut state);
                assert!(mission_control::is_top(&state));
                assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
            }
            assert!(state.agent_threads.opening.is_none());
        }
    }

    /// A cover that comes and goes before the lookup answers changes nothing about it: the
    /// completion then finds Mission Control on top and applies.
    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn agents_thread_a_cover_lifted_before_the_completion_lets_it_apply() {
        use crate::adapters::tui::notifications::NoticeSeverity;

        let workspace = tempfile::tempdir().unwrap();
        let (app, mut events) = front_door(workspace.path());
        let (mut state, id, _) = pending();
        state.agent_threads.opening = None;
        state.agent_threads.offers.insert(id.clone(), offer());
        open(&mut state, "conv", &app, id.clone());
        reducer::open(&mut state, Surface::Help);
        reducer::back(&mut state);
        assert!(state.agent_threads.opening.is_some());
        let (conversation, generation, agent, outcome) = next_ready(&mut events).await;
        assert!(ready(
            &mut state,
            &conversation,
            generation,
            &agent,
            outcome
        ));
        assert!(state.agent_threads.opening.is_none());
        assert_eq!(
            last_notice(&state),
            Some((NoticeSeverity::Warning, UNVERIFIABLE.to_owned()))
        );
    }

    /// AC4 / D11 through the production `open`: with a thread window (and its kept follow-up
    /// text) beneath Mission Control, `t` posts the one Warning and nothing else: no lookup is
    /// minted or spawned (no completion ever reaches the bus), Mission Control stays on top,
    /// and the frame beneath, its field and the composer are untouched.
    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn agents_thread_t_beneath_a_thread_window_refuses_through_open_without_any_read() {
        use crate::adapters::tui::notifications::NoticeSeverity;
        use crate::adapters::tui::state::ThreadReplyField;

        let workspace = tempfile::tempdir().unwrap();
        let (app, mut events) = front_door(workspace.path());
        let mut state = TuiState::new(100, 30);
        crate::infrastructure::runtime::thread_open::open_sent(
            &mut state,
            ThreadAccess::Writer,
            "marcus",
            "ri_1",
            "conv",
        );
        let mut reply = ThreadReplyField {
            follow_up: Some("ri_1".to_owned()),
            ..ThreadReplyField::default()
        };
        reply.field.set("kept follow-up");
        state.thread_inspector.as_mut().unwrap().reply = Some(reply);
        let entry = row();
        let id = entry.agent_id.clone();
        state.agent_panel_state.cached_entries = vec![entry];
        agents::sync(&mut state);
        reducer::open_mission_control(&mut state, McTab::Agents);
        state.mission_control.move_selection(ViewerCmd::Top);
        state.agent_threads.offers.insert(id.clone(), offer());
        state.input_buffer = "main draft".to_owned();
        state.cursor_position = 4;
        assert!(thread_beneath_mission_control(&state));

        open(&mut state, "conv", &app, id.clone());

        assert_eq!(state.notifications.len(), 1);
        assert_eq!(
            last_notice(&state),
            Some((NoticeSeverity::Warning, THREAD_BENEATH.to_owned()))
        );
        assert_eq!(state.agent_threads.generation, 0, "no lookup minted");
        assert!(state.agent_threads.opening.is_none());
        assert!(mission_control::is_top(&state));
        assert!(thread_beneath_mission_control(&state));
        assert_eq!(state.mission_control.selected_id(), Some(id.as_str()));
        let inspector = state
            .thread_inspector
            .as_ref()
            .expect("the frame beneath stays");
        assert!(matches!(
            &inspector.target,
            ThreadTarget::Sent { peer, item } if peer == "marcus" && item == "ri_1"
        ));
        let reply = inspector.reply.as_ref().expect("its field stays");
        assert_eq!(reply.field.as_str(), "kept follow-up");
        assert_eq!(reply.follow_up.as_deref(), Some("ri_1"));
        assert_eq!(state.input_buffer, "main draft");
        assert_eq!(state.cursor_position, 4);
        // Nothing was spawned: once every ready task has run, the bus holds no completion.
        tokio::task::yield_now().await;
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
    }
}
