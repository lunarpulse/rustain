//! Effect shell between the event loop and runtime bridge seams
//! (Stories 18.2 and 18.3c).
//!
//! Exists to keep `event_loop.rs` inside its line ratchet
//! (`EVENT_LOOP_HARD_BUDGET`) and to keep `adapters/tui/handlers/` free of
//! `crate::infrastructure::*` imports, which its module contract forbids. The
//! handlers receive data; this module performs the I/O.
//!
//! Deliberately **not** in `handlers/`: it performs journal reads and file
//! writes, and the handler contract is data-in/data-out.

use crate::adapters::tui::handlers::team_command::{LogRail, TeamLogArgs, TeamLogInput};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::AppConfig;
use crate::domain::services::transparency::{
    TransparencyExport, TransparencyFilter, TransparencyReport, TransparencyRow,
};
use crate::infrastructure::runtime::app_state::AppState;

/// Message shown when the workspace has no orchestration journal at all.
const NO_JOURNAL: &str =
    "this session has no orchestration journal (the subagent subsystem is not composed)";

/// Refresh the panel's rows from the durable journal.
///
/// Awaited inline at the dispatch site rather than spawned: the read happens
/// only when the operator opens or refreshes the panel, and a spawn would need
/// a new `AppEvent` round trip that the event-loop line budget cannot pay for.
/// `NodeJournal::load()` is O(whole file) with no tail read — if that ever
/// becomes a stall, it is the journal that needs an index, not this call that
/// needs a thread.
///
/// **Never live.** This is one consistent read under a shared `flock`; the
/// panel renders "as of <time>" and counts rows it has not shown.
pub(crate) async fn refresh_panel(app_state: &AppState, state: &mut TuiState) {
    let Some(service) = app_state.transparency.as_ref() else {
        state.transparency_panel.error = Some(NO_JOURNAL.to_owned());
        state.transparency_panel.pending_visit = None;
        return;
    };
    // Story 19.16h: resolved BEFORE the read (see `durable_reset_revision`).
    let revision = durable_reset_revision(service).await;
    match service.report().await {
        Ok(report) => {
            let now = chrono::Utc::now().timestamp_millis();
            // Story 19.16g: bound to THIS report, not a later head.
            state.transparency_panel.pending_visit =
                Some(visit_candidate(&report.rows, revision, state));
            state.transparency_panel.apply_report(report, now);
        }
        Err(error) => {
            state.transparency_panel.error = Some(error.to_string());
            state.transparency_panel.pending_visit = None;
        }
    }
    state.sidebar_entry_count = state.transparency_panel.visible_rows().len();
}

/// Story 19.16g — the seen-through boundary a freshly read report would
/// contribute once presented: its maximum row `seq`, bound to the reset
/// revision loaded before the read (Story 19.16h), or to the observer's
/// projection when that load was not trusted.
fn visit_candidate(
    rows: &[TransparencyRow],
    revision: Option<u64>,
    state: &TuiState,
) -> crate::domain::models::LogVisitCandidate {
    crate::domain::models::LogVisitCandidate {
        seen_through: rows.iter().map(|row| row.seq).max().unwrap_or(0),
        reset_revision: revision.unwrap_or(state.log_awareness.reset_revision),
    }
}

/// Story 19.16h — the reset revision a log read about to start belongs to:
/// the durable seen preference, loaded **before** the journal read. The
/// observer's projection lags a fresh start and another client's confirmed
/// reset, and a visit bound to it is rejected as `Stale`. Order is the
/// soundness argument: a reset is confirmed only after the replacement
/// journal exists, so a load that sees it precedes a read of the new
/// journal, while a reset confirmed after the load leaves the candidate on
/// the older revision — rejected, never old rows labelled with the new one.
///
/// `None` when the preference is untrusted or the load failed; the caller
/// then binds the projection (19.16g behavior).
async fn durable_reset_revision(
    service: &crate::infrastructure::transparency::TransparencyService,
) -> Option<u64> {
    use crate::infrastructure::transparency_awareness::{SeenLoad, SeenStore};

    let store = SeenStore::for_workspace(service.workspace());
    match tokio::task::spawn_blocking(move || store.load()).await {
        Ok(SeenLoad::Valid(boundary)) => Some(boundary.reset_revision),
        Ok(SeenLoad::Missing) => Some(0),
        Ok(SeenLoad::Untrusted(_)) | Err(_) => None,
    }
}

/// Story 19.16g — the session's transparency-log reminder observer. Hidden
/// (no observation) when no journal is composed, matching `/team log`'s own
/// "no orchestration journal" answer.
pub(crate) fn log_awareness_observer(
    app_state: &AppState,
) -> crate::infrastructure::transparency_awareness::LogAwarenessObserver {
    use crate::infrastructure::transparency_awareness::LogAwarenessObserver;
    if app_state.transparency.is_some() {
        LogAwarenessObserver::for_workspace(&app_state.compose_snapshot.workspace_path)
    } else {
        LogAwarenessObserver::disabled()
    }
}

/// Write exactly one already-read transparency report to the regenerable
/// export. Keeping the report argument explicit prevents a panel export from
/// racing a fresh journal read and claiming a different snapshot.
pub(crate) async fn export_report(
    app_state: &AppState,
    report: &TransparencyReport,
) -> Result<TransparencyExport, String> {
    let service = app_state
        .transparency
        .as_ref()
        .ok_or_else(|| NO_JOURNAL.to_owned())?;
    service
        .export_report(report)
        .await
        .map_err(|error| error.to_string())
}

/// Open the Transparency Log panel: read the journal, park the cursor on the
/// newest row, and size the sidebar.
///
/// Story 18.3 Task 0.5 pulled this out of the `InputAction::OpenPanel` arm.
/// Same reason as the rest of this module: `EVENT_LOOP_HARD_BUDGET` is a
/// ceiling, and Story 18.2 spent the last of its headroom (11_354 > 11_321).
pub(crate) async fn open_panel(app_state: &AppState, state: &mut TuiState) {
    refresh_panel(app_state, state).await;
    state
        .transparency_panel
        .open_at_tail(&mut state.sidebar_selected);
    state.sidebar_entry_count = state.transparency_panel.visible_len();
}

/// Export the snapshot the panel is currently showing, then report the outcome
/// as an operator notice.
///
/// Reads the report out of the panel rather than re-reading the journal, so the
/// exported bytes are exactly the snapshot on screen — the same anti-race
/// reasoning as `export_report`'s explicit report argument.
///
/// Story 18.3 Task 0.5 pulled this out of the `InputAction::ExportTransparency`
/// arm, which was 31 inline lines of the event loop's line budget.
pub(crate) async fn export_command(
    app_state: &AppState,
    state: &mut TuiState,
    conversation_id: &str,
) {
    let export = match state.transparency_panel.report.as_ref() {
        Some(report) => export_report(app_state, report).await,
        None => Err("open the Transparency Log before exporting its snapshot".to_owned()),
    };
    let (level, message) = match export {
        Ok(export) => (
            crate::domain::models::NoticeLevel::Warning,
            format!(
                "Transparency export written to {} ({} unfiltered rows).",
                export.path.display(),
                export.rows
            ),
        ),
        Err(error) => (
            crate::domain::models::NoticeLevel::Error,
            format!("Transparency export failed: {error}"),
        ),
    };
    let _ = app_state
        .event_bus
        .emit_domain(crate::domain::events::AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level,
            message,
        });
    state.needs_redraw = true;
}

/// Gather everything `/team log` needs: the filtered rows, the divergence
/// report, and the export result when `--export` was asked for.
pub(crate) async fn team_log_input(app_state: &AppState, args: &TeamLogArgs) -> TeamLogInput {
    team_log_input_from(app_state.transparency.as_deref(), args, LogRail::Standalone).await
}

/// The rail-independent core of [`team_log_input`]: one report read, shared
/// by the standalone dispatch arm and the attached client's local log.
pub(crate) async fn team_log_input_from(
    service: Option<&crate::infrastructure::transparency::TransparencyService>,
    args: &TeamLogArgs,
    rail: LogRail,
) -> TeamLogInput {
    let failed = |error: String, divergence: Option<String>| TeamLogInput {
        rows: Err(error),
        divergence,
        export: None,
        snapshot_max_seq: 0,
        reset_revision: None,
        rail,
    };
    let Some(service) = service else {
        return failed(NO_JOURNAL.to_owned(), None);
    };
    // Filtered views cannot contribute a visit; load only for an unfiltered
    // read, and always BEFORE that read (Story 19.16h).
    let reset_revision = if args.filter.is_none() {
        durable_reset_revision(service).await
    } else {
        None
    };
    let report = match service.report().await {
        Ok(report) => report,
        Err(error) => return failed(error.to_string(), None),
    };
    let divergence = report.structural_divergence_report();
    let rows = match filter_rows(&report.rows, args.filter.as_deref()) {
        Ok(rows) => rows,
        Err(error) => return failed(error, divergence),
    };
    // The export renders the WHOLE supplied report, never the filtered view:
    // a filtered export would be a different file every time and would stop
    // being byte-identically regenerable.
    let export = if args.export {
        Some(
            service
                .export_report(&report)
                .await
                .map_err(|error| error.to_string()),
        )
    } else {
        None
    };
    TeamLogInput {
        rows: Ok(rows),
        divergence,
        export,
        snapshot_max_seq: report.rows.iter().map(|row| row.seq).max().unwrap_or(0),
        reset_revision,
        rail,
    }
}

/// Story 19.16g — `/team log` on the daemon-attached client: a **local**
/// read of the same workspace journal (attach is a Unix-socket client of this
/// filesystem, not a journal replica), rendered into the same stable block.
/// ⛔ Never a model turn and never daemon write authority. The handler's
/// notices come back as plain strings for the client's own non-turn-fatal
/// transcript line — never the daemon's turn-control stream.
pub(crate) async fn attached_team_log(
    service: &crate::infrastructure::transparency::TransparencyService,
    state: &mut TuiState,
    args: &TeamLogArgs,
) -> Vec<String> {
    use crate::adapters::tui::handlers::team_command as handler;

    let input = team_log_input_from(Some(service), args, LogRail::Attached).await;
    handler::team_command(state, "", args, input)
        .into_iter()
        .filter_map(|event| match event {
            crate::domain::events::AppEvent::SystemNotice { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn filter_rows(
    rows: &[TransparencyRow],
    spec: Option<&str>,
) -> Result<Vec<TransparencyRow>, String> {
    let Some(spec) = spec else {
        return Ok(rows.to_vec());
    };
    let filter = TransparencyFilter::parse(spec)?;
    Ok(rows
        .iter()
        .filter(|row| filter.matches(row))
        .cloned()
        .collect())
}

/// One-call shell for the `/fanout` dispatch arm.
///
/// Story 18.3c Task 0.7 moved the established command path here unchanged so
/// `event_loop.rs` retains budget for new behavior. This is an effect shell,
/// not a second decision core: parsing, request construction, spawn-gate
/// selection, cancellation, and notice text remain the shipped implementations.
pub(crate) fn fanout_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    is_streaming: bool,
    config: &AppConfig,
    app_state: &AppState,
) {
    use crate::adapters::tui::widgets::exceptional_spawn_gate::{GateDecision, gate_decision};
    use crate::domain::events::AppEvent;
    use crate::domain::models::NoticeLevel;

    if cmd_arg.map(str::trim) == Some("cancel") {
        if let Some(cancel) = &state.wave_cancel {
            if !cancel.is_cancelled() {
                cancel.cancel();
                state.rerunning_slot = None;
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Info,
                    message: "Fan-out wave cancelled.".to_string(),
                });
            } else {
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Info,
                    message: "Wave already cancelled.".to_string(),
                });
            }
        } else {
            let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                conversation_id: Some(conversation_id.to_owned()),
                level: NoticeLevel::Info,
                message: "No active wave to cancel.".to_string(),
            });
        }
        state.needs_redraw = true;
    } else if is_streaming {
        let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level: NoticeLevel::Info,
            message:
                "/fanout unavailable while a turn is in progress — try again after it finishes."
                    .to_string(),
        });
        state.needs_redraw = true;
    } else if state.wave_state.is_some() {
        let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level: NoticeLevel::Info,
            message: "A fan-out wave is already in flight — wait for it to finish.".to_string(),
        });
        state.needs_redraw = true;
    } else {
        match crate::adapters::tui::fanout_spec::parse_fanout(cmd_arg) {
            Ok(spec) => {
                let effective_model = state.selected_model.as_deref().unwrap_or(&config.model);
                match crate::adapters::tui::fanout_spec::to_request(&spec, effective_model) {
                    Ok(request) => {
                        let requested = request.spokes.len();
                        let threshold = config.fanout_spawn_gate_threshold;
                        match gate_decision(requested, threshold) {
                            GateDecision::Allow => {
                                super::event_loop::launch_wave_request(
                                    state,
                                    app_state,
                                    conversation_id.to_owned(),
                                    request,
                                );
                            }
                            GateDecision::Refuse => {
                                state.pending_spawn_gate =
                                    Some(crate::adapters::tui::state::PendingSpawnGate {
                                        spec,
                                        requested,
                                        threshold,
                                        adjusted: None,
                                    });
                                state.needs_redraw = true;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                            conversation_id: Some(conversation_id.to_owned()),
                            level: NoticeLevel::Warning,
                            message: err.to_string(),
                        });
                        state.needs_redraw = true;
                    }
                }
            }
            Err(msg) => {
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Warning,
                    message: msg.to_string(),
                });
                state.needs_redraw = true;
            }
        }
    }
}

/// One-call shell for the `/team` dispatch arm: parse, read, render, emit.
///
/// The parse and the rendering are pure and live in
/// `handlers::team_command`; this wrapper exists so the event-loop arm is two
/// lines instead of twenty — `EVENT_LOOP_HARD_BUDGET` is a ceiling, not a
/// budget, and Story 18.2 had 24 lines for all of Cluster B.
///
/// Emits its own notices rather than returning them (Story 18.3 Task 0.5): the
/// caller's `for … emit_domain` loop cost three lines of a budget that Story
/// 18.2 had already overrun. The pure `handlers::team_command` still returns
/// its events, so the behavioural tests are unaffected.
/// Exact first-contact card copy shared by daemon state and TUI rendering.
///
/// The rotated-key clause interpolates
/// [`crate::domain::services::peer_admission::ROTATED_KEY_DOCTRINE`] (Story
/// 18.4b): AC4's key-mismatch alarm states the same doctrine, and two literals
/// are two doctrines waiting to diverge. The rendered string is unchanged.
///
/// Story 19.18d `P22` — exhaustive over the principal's rail: a pseudonym and
/// a RAP pin keep today's bytes; an operator-bound A2A key's `[a]` line says
/// what `[a]` trusts there, the pin
/// ([`crate::domain::services::peer_admission::BOUND_KEY_CONSENT_LINE`]).
pub(crate) fn consent_card_text(sender: &crate::domain::models::PolicyPrincipal) -> String {
    use crate::domain::models::{PinnedVia, PolicyPrincipal};
    use crate::domain::services::peer_admission::{BOUND_KEY_CONSENT_LINE, ROTATED_KEY_DOCTRINE};

    let peer = sender.peer();
    let key_line = || {
        format!("[a] trusts the key this sender presents, not the person; {ROTATED_KEY_DOCTRINE}.")
    };
    let trust_line = match sender {
        PolicyPrincipal::Pseudonymous(_) => key_line(),
        PolicyPrincipal::Pinned { via, .. } => match via {
            PinnedVia::Rap => key_line(),
            PinnedVia::BoundA2a => format!("[a] {BOUND_KEY_CONSENT_LINE}"),
        },
    };
    format!(
        "Consent required — {peer}\n\
         No standing consent is recorded for this sender.\n\
         [y] Allow once  [a] Always allow  [n] Decline\n\
         {trust_line}\n\
         Awaiting your decision.\n\
         Logged immediately for safety; interruption timing follows your policy."
    )
}

/// The `/team` dispatch arm (rail 3, the DEFAULT rail). `pub` so the Story
/// 19.16f front-door keystones enter exactly where the operator's typed
/// command does (the `artifact_bridge::artifact_command` precedent).
pub async fn team_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
) {
    use crate::adapters::tui::handlers::team_command::{self as handler, TeamCommandArgs};

    let command = match handler::parse_team_command(cmd_arg) {
        Ok(command) => command,
        Err(message) => {
            emit_team_warning(state, conversation_id, app_state, message);
            return;
        }
    };
    match command {
        // Story 19.16b AC3 — the DEFAULT rail, and the only one that holds the
        // A2A egress this verb needs. ⛔ Not a silent `=> {}`: this match is
        // compile-forced over `TeamCommandArgs`, and a swallowed board would
        // leave the operator believing no recipient has acknowledged when in
        // fact nothing was ever asked.
        TeamCommandArgs::Board { selector } => {
            #[cfg(feature = "a2a")]
            {
                let Some(runtime) = app_state.a2a_send.clone() else {
                    // ⛔ Not `emit_team_warning`: Warning is turn-fatal
                    // (`NoticeLevel::is_turn_fatal`), and this arm's own rule
                    // is that a board refusal must never abort an unrelated
                    // streaming turn (19.16b review). The refusal renders in
                    // the same stable block the board itself uses.
                    handler::show_team_board(
                        state,
                        "· no A2A send runtime is configured for this session, so there is \
                         no roster to read the board from."
                            .to_owned(),
                    );
                    return;
                };
                let event_bus = app_state.event_bus.clone();
                let conversation_id = conversation_id.to_owned();
                // Session-scoped like `/team send` (19.16b review): a board
                // issued against unresponsive peers must not keep its task
                // and sockets alive past the session, then emit a notice for
                // a conversation that is gone.
                let cancel = app_state.session_cancel.child_token();
                // Spawned, never awaited inline: the board is N concurrent
                // remote reads and the event loop must not stall on peer
                // timeouts. The result arrives as a `TeamBoardReady` view
                // event, which replaces the stable `team-board` block —
                // ⛔ never a stacked dismissible notice, ⛔ never a
                // turn-fatal Warning.
                tokio::spawn(async move {
                    let message = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        message = team_board_message(&runtime, &selector) => message,
                    };
                    let _ =
                        event_bus.emit_domain(crate::domain::events::AppEvent::TeamBoardReady {
                            conversation_id,
                            message,
                        });
                });
            }
            #[cfg(not(feature = "a2a"))]
            {
                let _ = selector;
                handler::show_team_board(
                    state,
                    "· `/team board` needs the `a2a` feature; this build was compiled without \
                     it."
                    .to_owned(),
                );
            }
        }
        // Story 19.16f `AC3(b)` — rail 3 SERVES the retract: the only rail
        // holding this session's own A2A egress. Spawned, never awaited
        // inline, and never a turn-fatal Warning: the confirm-time read
        // arrives as `TeamRetractPreviewReady`, which raises the card.
        TeamCommandArgs::Retract { peer, item_id } => {
            #[cfg(feature = "a2a")]
            {
                let Some(runtime) = app_state.a2a_send.clone() else {
                    handler::show_team_retract(
                        state,
                        "· no A2A send runtime is configured for this session, so there is no \
                         peer to retract on. Nothing was marked."
                            .to_owned(),
                    );
                    return;
                };
                let event_bus = app_state.event_bus.clone();
                let conversation_id = conversation_id.to_owned();
                let cancel = app_state.session_cancel.child_token();
                tokio::spawn(async move {
                    let preview = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return,
                        preview = team_retract_preview(&runtime, &peer, &item_id) => preview,
                    };
                    let _ = event_bus.emit_domain(
                        crate::domain::events::AppEvent::TeamRetractPreviewReady {
                            conversation_id,
                            preview,
                        },
                    );
                });
            }
            #[cfg(not(feature = "a2a"))]
            {
                let _ = (peer, item_id);
                handler::show_team_retract(
                    state,
                    "· `/team retract` needs the `a2a` feature; this build was compiled \
                     without it. Nothing was marked."
                        .to_owned(),
                );
            }
        }
        TeamCommandArgs::Send {
            recipients,
            text,
            message_type,
            subject,
        } => {
            start_team_send(
                state,
                conversation_id,
                app_state,
                TeamSendPayload::Typed { recipients, text },
                message_type,
                None,
                subject,
                crate::domain::models::ReplyAuthorship::HumanWritten,
            );
        }
        // Story 19.18 `AC7` — a follow-up IS a 19.17 delivery, N = 1, with the
        // reply-to carrier: the same validation, block, settle handler, rows
        // and outcome words. ⛔ No second send path (`M21`).
        TeamCommandArgs::Followup {
            peer,
            turn_id,
            text,
        } => {
            start_team_send(
                state,
                conversation_id,
                app_state,
                TeamSendPayload::Typed {
                    recipients: vec![peer],
                    text,
                },
                None,
                Some(turn_id),
                None,
                crate::domain::models::ReplyAuthorship::HumanWritten,
            );
        }
        // Story 19.18 `AC6` — rail 3 SERVES the thread read: the only rail
        // holding this session's own A2A egress. Spawned, never awaited
        // inline; the answer arrives as `TeamThreadReady` and replaces the
        // stable `team-thread` block in place.
        TeamCommandArgs::Thread { peer, item_id } => {
            use crate::adapters::tui::handlers::team_command::{
                TEAM_THREAD_READING, show_team_thread,
            };
            #[cfg(feature = "a2a")]
            {
                let Some(runtime) = app_state.a2a_send.clone() else {
                    show_team_thread(
                        state,
                        "· no A2A send runtime is configured for this session, so there is no \
                         peer to read a thread from."
                            .to_owned(),
                    );
                    return;
                };
                show_team_thread(state, TEAM_THREAD_READING.to_owned());
                // Detached, like every rail-3 spawn: the session token owns
                // its lifetime; the handle exists for the `K11` keystone.
                drop(spawn_team_thread(
                    runtime,
                    app_state.event_bus.clone(),
                    conversation_id.to_owned(),
                    peer,
                    item_id,
                    app_state.session_cancel.child_token(),
                ));
            }
            #[cfg(not(feature = "a2a"))]
            {
                let _ = (peer, item_id, TEAM_THREAD_READING);
                show_team_thread(
                    state,
                    "· `/team thread` needs the `a2a` feature; this build was compiled without \
                     it."
                    .to_owned(),
                );
            }
        }
        TeamCommandArgs::View { item_id } => {
            if let Err(error) = super::recipient_surface::open_received_surface(
                &app_state.compose_snapshot.workspace_path,
                None,
                &item_id,
                state,
                crate::adapters::tui::state::RecipientSurfaceAccess::LocalOnly,
                super::recipient_view::ReceivedThreadWindow::Newest,
            )
            .await
            {
                let id = "recipient-view".to_owned();
                state.feedback_blocks.insert(
                    id.clone(),
                    crate::domain::models::FeedbackBlock {
                        id: id.clone(),
                        level: crate::domain::models::FeedbackLevel::Info,
                        message: error.to_string(),
                        actions: Vec::new(),
                    },
                );
                state.active_feedback_id = Some(id);
                state.needs_redraw = true;
            }
        }
        // Story 19.18 `AC9(b)` — answering an item needs the daemon that
        // holds it. ⛔ Not a silent `=> {}`: the ack/remove refusal shape.
        TeamCommandArgs::Reply { .. } => {
            emit_team_warning(
                state,
                conversation_id,
                app_state,
                "Replying to a recipient item requires a daemon-attached session.".to_owned(),
            );
        }
        TeamCommandArgs::Defer { .. } => {
            emit_team_warning(
                state,
                conversation_id,
                app_state,
                "/team defer requires a daemon-attached session".to_owned(),
            );
        }
        TeamCommandArgs::Context { .. } => {
            emit_team_warning(
                state,
                conversation_id,
                app_state,
                "/team context requires a daemon-attached session".to_owned(),
            );
        }
        TeamCommandArgs::Acknowledge { item_id: _ } => {
            emit_team_warning(
                state,
                conversation_id,
                app_state,
                "Recipient acknowledgement requires a daemon-attached session.".to_owned(),
            );
        }
        TeamCommandArgs::Remove { item_id: _ } => {
            // ⛔ NOT a silent `=> {}`. This is the non-attached, DEFAULT rail:
            // a swallowed removal would let an operator believe peer-supplied
            // content is disposed of while the projection still holds it and
            // the ledger still lists it. Mirrors the acknowledgement's shape,
            // and mints no new refusal idiom.
            emit_team_warning(
                state,
                conversation_id,
                app_state,
                "Recipient item removal requires a daemon-attached session.".to_owned(),
            );
        }
        TeamCommandArgs::Log(args) => {
            let input = team_log_input(app_state, &args).await;
            for event in handler::team_command(state, conversation_id, &args, input) {
                let _ = app_state.event_bus.emit_domain(event);
            }
        }
        TeamCommandArgs::Trust => match load_team_status(app_state).await {
            Ok(message) => handler::show_team_status(state, message),
            Err(message) => emit_team_warning(state, conversation_id, app_state, message),
        },
        TeamCommandArgs::Untrust(target) => {
            match change_sender_consent(app_state, &target, false).await {
                Ok(message) => handler::show_team_status(state, message),
                Err(message) => emit_team_warning(state, conversation_id, app_state, message),
            }
        }
        TeamCommandArgs::Status => match load_team_status(app_state).await {
            Ok(message) => handler::show_team_status(state, message),
            Err(message) => emit_team_warning(state, conversation_id, app_state, message),
        },
    }
}

/// Keep a reviewed proposal shared while its send is in flight; the Review
/// history may still own the same body, so copying it into a task is unnecessary.
enum TeamSendPayload {
    Typed {
        recipients: Vec<String>,
        text: String,
    },
    #[cfg(feature = "a2a")]
    Proposed(std::sync::Arc<crate::domain::models::TeamProposal>),
}

impl TeamSendPayload {
    fn parts(&self) -> (&[String], &str) {
        match self {
            Self::Typed { recipients, text } => (recipients, text),
            #[cfg(feature = "a2a")]
            Self::Proposed(proposal) => (&proposal.recipients, &proposal.body),
        }
    }
}

#[cfg(feature = "a2a")]
pub(crate) fn start_proposed_team_send(
    state: &mut TuiState,
    conversation_id: &str,
    app_state: &AppState,
    proposal: std::sync::Arc<crate::domain::models::TeamProposal>,
) {
    let message_type = Some(proposal.message_type);
    start_team_send(
        state,
        conversation_id,
        app_state,
        TeamSendPayload::Proposed(proposal),
        message_type,
        None,
        None,
        crate::domain::models::ReplyAuthorship::AgentComposed,
    );
}

/// The rail-3 addressed send (Story 19.17), shared by `/team send` and Story
/// 19.18's `/team followup` (N = 1, `reply_to` set): ⛔ one send path, one
/// block, one settle handler, one journal discipline (`A9`, `M21`). Story
/// 19.21: that one path is [`coordinate_send`], which journals the operator's
/// send action before any recipient I/O and each recipient's typed settlement
/// before its row settles; a follow-up records no action.
#[allow(clippy::too_many_arguments)]
fn start_team_send(
    state: &mut TuiState,
    conversation_id: &str,
    app_state: &AppState,
    payload: TeamSendPayload,
    message_type: Option<crate::domain::models::SemanticMessageType>,
    reply_to: Option<String>,
    subject: Option<String>,
    authorship: crate::domain::models::ReplyAuthorship,
) {
    #[cfg(not(feature = "a2a"))]
    use crate::adapters::tui::handlers::team_command as handler;
    use crate::adapters::tui::handlers::team_command::show_team_send;

    state.team_send_next_id += 1;
    let block_id = format!("team-send-{}", state.team_send_next_id);
    #[cfg(feature = "a2a")]
    {
        use crate::adapters::a2a::send::{SendRequest, coordinate_send, validate_send_request};
        use crate::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
        let (recipients, text) = payload.parts();
        let Some(runtime) = app_state.a2a_send.clone() else {
            show_team_send(
                state,
                &block_id,
                "· A2A send runtime is not configured for this session.".to_owned(),
            );
            return;
        };
        // Whole-action validation before any row is painted, journal write or
        // I/O (19.17 `G4`): a refused action leaves no trace.
        let refusal = validate_send_request(
            &runtime,
            &SendRequest {
                recipients,
                text,
                message_type,
                reply_to: reply_to.as_deref(),
                subject: subject.as_deref(),
                authorship,
            },
        );
        if let Err(refusal) = refusal {
            show_team_send(state, &block_id, refusal.to_string());
            return;
        }
        let rows: Vec<_> = recipients
            .iter()
            .map(|alias| TeamSendRow {
                alias: alias.clone(),
                outcome: None,
            })
            .collect();
        show_team_send(state, &block_id, render_team_send_rows(&rows));
        state.team_send_blocks.insert(block_id.clone(), rows);
        let event_bus = app_state.event_bus.clone();
        let conversation_id = conversation_id.to_owned();
        let cancel = app_state.session_cancel.child_token();
        tokio::spawn(async move {
            let (recipients, text) = payload.parts();
            let _ = coordinate_send(
                &runtime,
                SendRequest {
                    recipients,
                    text,
                    message_type,
                    reply_to: reply_to.as_deref(),
                    subject: subject.as_deref(),
                    authorship,
                },
                cancel.clone(),
                |_| {},
                |index, outcome| {
                    if !cancel.is_cancelled() {
                        let _ = event_bus.emit_domain(
                            crate::domain::events::AppEvent::TeamSendSettled {
                                conversation_id: conversation_id.clone(),
                                block_id: block_id.clone(),
                                index,
                                outcome,
                            },
                        );
                    }
                },
            )
            .await;
        });
    }
    #[cfg(not(feature = "a2a"))]
    {
        let _ = (
            payload.parts(),
            message_type,
            reply_to,
            subject,
            authorship,
            conversation_id,
            app_state,
        );
        show_team_send(
            state,
            &block_id,
            handler::team_send_unavailable().to_owned(),
        );
    }
}

/// Story 19.18 `AC6` — the thread spawn's body: read, settle against the
/// ledger (journaling each peer turn once), render. `pub`: the front-door
/// keystones await exactly what the operator's spawn awaits.
#[cfg(feature = "a2a")]
pub async fn team_thread_message(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    peer: &str,
    item_id: &str,
) -> String {
    let credentialed = runtime
        .peer_binding(peer)
        .is_some_and(|(spec, _)| spec.auth.is_some());
    let read = crate::adapters::a2a::send::read_thread(runtime, peer, item_id).await;
    crate::adapters::tui::handlers::team_command::render_team_thread(
        peer,
        item_id,
        &read,
        credentialed,
    )
}

/// Story 19.18 `AC6(a)` — the rail-3 thread spawn: session-scoped, and a
/// cancelled session publishes nothing further (`K11`). Returns the handle so
/// the in-crate keystone can await it instead of waiting on a timer.
#[cfg(feature = "a2a")]
fn spawn_team_thread(
    runtime: std::sync::Arc<crate::adapters::a2a::driver::A2aDelegationRuntime>,
    event_bus: std::sync::Arc<crate::infrastructure::runtime::event_bus::EventBus>,
    conversation_id: String,
    peer: String,
    item_id: String,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let message = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            message = team_thread_message(&runtime, &peer, &item_id) => message,
        };
        if cancel.is_cancelled() {
            return;
        }
        let _ = event_bus.emit_domain(crate::domain::events::AppEvent::TeamThreadReady {
            conversation_id,
            message,
        });
    })
}

/// Story 19.18 `AC6(a)` — the `TeamThreadReady` arm: replace the issuing
/// tab's `team-thread` block IN PLACE, even when that tab is inactive — the
/// `team_send_settled` cross-tab path. ⛔ Not the board's background path,
/// which stores an `Advisory` notice (`A12`, `M16`).
pub fn team_thread_ready(
    active_conversation_id: &str,
    state: &mut TuiState,
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: &str,
    message: String,
) {
    use crate::adapters::tui::handlers::team_command::{TEAM_THREAD_BLOCK_ID, show_team_thread};

    if conversation_id == active_conversation_id {
        show_team_thread(state, message);
    } else if let Some(tab) = tab_manager.find_by_conversation_mut(conversation_id) {
        tab.feedback_blocks.insert(
            TEAM_THREAD_BLOCK_ID.to_owned(),
            crate::domain::models::FeedbackBlock {
                id: TEAM_THREAD_BLOCK_ID.to_owned(),
                level: crate::domain::models::FeedbackLevel::Info,
                message,
                actions: Vec::new(),
            },
        );
        state.needs_redraw = true;
    }
}

/// Apply the asynchronous board answer to its issuing conversation.
pub fn team_board_ready(
    active_conversation_id: &str,
    state: &mut TuiState,
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: &str,
    message: String,
) {
    if conversation_id == active_conversation_id {
        crate::adapters::tui::handlers::team_command::show_team_board(state, message);
    } else if let Some(tab) = tab_manager.find_by_conversation_mut(conversation_id) {
        crate::adapters::tui::handlers::notice::store_background_notice(
            tab,
            crate::domain::models::NoticeLevel::Advisory,
            message,
        );
        state.needs_redraw = true;
    }
}

/// [`team_board_message`] for a wire request: the daemon's `TeamBoard` handler
/// calls this so it never names a TUI type, and the two rails render one board
/// through one function (`AC4(b)`).
#[cfg(feature = "a2a")]
pub async fn team_board_message_for_request(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    request: crate::adapters::daemon::protocol::TeamBoardRequest,
) -> String {
    team_board_message_for_request_at(runtime, request, std::time::Instant::now()).await
}

/// [`team_board_message_for_request`] at an injected monotonic `now` — the
/// 500 ms refresh floor's input, so a keystone proves the floor with a
/// deterministic step instead of a sleep.
#[cfg(feature = "a2a")]
pub async fn team_board_message_for_request_at(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    request: crate::adapters::daemon::protocol::TeamBoardRequest,
    now: std::time::Instant,
) -> String {
    team_board_message_at(
        runtime,
        &crate::adapters::tui::handlers::team_command::TeamBoardSelector::from(request),
        now,
    )
    .await
}

/// The board's spawn body (Story 19.16f `AC9`; Story 19.21 `AC3`): the peer
/// inventory — every roster peer, or one named peer uncapped — or one recorded
/// send action's board, or the list of recorded actions. `pub`: the front-door
/// keystones await exactly what the operator's spawn awaits.
#[cfg(feature = "a2a")]
pub async fn team_board_message(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    selector: &crate::adapters::tui::handlers::team_command::TeamBoardSelector,
) -> String {
    team_board_message_at(runtime, selector, std::time::Instant::now()).await
}

/// [`team_board_message`] at an injected monotonic `now`.
#[cfg(feature = "a2a")]
pub async fn team_board_message_at(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    selector: &crate::adapters::tui::handlers::team_command::TeamBoardSelector,
    now: std::time::Instant,
) -> String {
    use crate::adapters::a2a::board::{self, ActionSelector};
    use crate::adapters::tui::handlers::team_command::TeamBoardSelector;

    // The injected `now` an action board's age is measured against.
    let wall = runtime.wall_now_unix_secs();
    match selector {
        TeamBoardSelector::Inventory => match board::collect_board(runtime, now).await {
            Ok(view) => board::render_board(&view),
            Err(refusal) => board::render_refresh_refusal(refusal),
        },
        TeamBoardSelector::Peer(peer) => {
            match board::collect_peer_board(runtime, peer, now).await {
                Ok(view) => board::render_board(&view),
                Err(refusal) => board::render_peer_board_refusal(&refusal),
            }
        }
        TeamBoardSelector::Last => {
            action_board_message(runtime, &ActionSelector::Last, now, wall).await
        }
        TeamBoardSelector::Action(id) => {
            action_board_message(runtime, &ActionSelector::Id(id.clone()), now, wall).await
        }
        TeamBoardSelector::Actions => match board::collect_action_list(runtime).await {
            Ok(actions) => board::render_action_list(&actions, wall),
            Err(refusal) => board::render_action_board_refusal(&refusal),
        },
    }
}

#[cfg(feature = "a2a")]
async fn action_board_message(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    selector: &crate::adapters::a2a::board::ActionSelector,
    now: std::time::Instant,
    wall: i64,
) -> String {
    use crate::adapters::a2a::board;

    match board::collect_action_board(runtime, selector, now).await {
        Ok(view) => board::render_action_board(&view, wall),
        Err(refusal) => board::render_action_board_refusal(&refusal),
    }
}

// ── Story 19.16f · `/team retract` on rail 3 ────────────────────────────────

/// The roster alias as rendered in every retract sentence: operator text,
/// bounded and single-line like every other echoed field.
#[cfg(feature = "a2a")]
fn retract_alias(peer: &str) -> String {
    crate::domain::services::transparency::sanitize_disclosable(
        peer,
        crate::domain::services::transparency::MAX_PEER_ID_BYTES,
    )
}

/// The sentence for a retract that sent nothing (`AC10`, gate item 5).
#[cfg(feature = "a2a")]
fn retract_not_sent_sentence(
    alias: &str,
    peer: &str,
    not_sent: &crate::adapters::a2a::send::RetractNotSent,
) -> String {
    use crate::adapters::a2a::send::RetractNotSent;

    match not_sent {
        // `SendError::UnknownPeer`'s shape, in the board's narrowing words.
        RetractNotSent::UnknownPeer { known } => {
            crate::adapters::a2a::board::render_peer_board_refusal(
                &crate::adapters::a2a::board::PeerBoardRefusal::UnknownPeer {
                    peer: peer.to_owned(),
                    known: known.clone(),
                },
            )
        }
        // The ratified anchor sentence, bare (`AD-1823`).
        RetractNotSent::AnchorRefused(error) => error.to_string(),
        // Story 19.21 `A15` — a LOCAL fact: this host could not record the
        // attempt, so nothing was sent. ⛔ Attributes nothing to the peer and
        // ⛔ is neither the unknown-mark nor the unreachable-peer sentence.
        RetractNotSent::JournalDispatch => {
            crate::domain::models::team_send::TEAM_SEND_NOT_SENT_LABEL.to_owned()
        }
        _ => format!("Could not reach {alias}'s host. Nothing was marked."),
    }
}

/// The confirm-time read the rail-3 spawn awaits (Story 19.16f `AC4(c)`),
/// rendered into a card or the one sentence that replaces it. `pub`: the
/// front-door keystone awaits exactly this.
#[cfg(feature = "a2a")]
pub async fn team_retract_preview(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    peer: &str,
    item: &str,
) -> crate::domain::events::TeamRetractPreview {
    use crate::adapters::a2a::board::{BoardItemState, mark_clock};
    use crate::adapters::a2a::send::{RetractPreview, preview_item_retract};
    use crate::domain::events::{TeamRetractCard, TeamRetractPreview};
    use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};

    let alias = retract_alias(peer);
    let shown_item = sanitize_disclosable(item, MAX_PEER_ID_BYTES);
    // Story 19.18d `A13` — the co-holder sentence is omitted only when the
    // host's list answered `principalBound: true`; an unverified read keeps it.
    let (rows, armed, key_line, task, principal_bound) = match preview_item_retract(
        runtime, peer, item,
    )
    .await
    {
        RetractPreview::Found(found) => {
            let principal_bound = found.principal_bound;
            let mark = match (found.state, found.retracted_at_ms) {
                (BoardItemState::Removed, _) => "removed there — nothing to mark".to_owned(),
                (_, Some(ms)) => format!("already retracted {}", mark_clock(ms)),
                (_, None) => "none — not yet retracted".to_owned(),
            };
            let sent_as = found.task.as_deref().map_or_else(
                || "—".to_owned(),
                |task| sanitize_disclosable(task, MAX_PEER_ID_BYTES),
            );
            let rows = format!(
                "item         {shown_item}\ntheir state  {}\nsent as      {sent_as}\nmark         {mark}",
                found.state.word()
            );
            // `F9` (owner-confirmed): `Retract × Removed` is a deterministic
            // refusal, so the card stops the knowable error — `[y]` disarmed.
            if found.state == BoardItemState::Removed {
                (
                    rows,
                    false,
                    format!(
                        "Already removed on {alias}'s host — nothing to mark.  [n] Cancel (Esc)"
                    ),
                    found.task,
                    principal_bound,
                )
            } else {
                (
                    rows,
                    true,
                    "Awaiting your decision.  [y] Retract  [n] Cancel (Esc)".to_owned(),
                    found.task,
                    principal_bound,
                )
            }
        }
        RetractPreview::NotFound => {
            return TeamRetractPreview::Answer(format!(
                "{alias}'s host has no item {shown_item} this host can address. Nothing was marked."
            ));
        }
        RetractPreview::NotSent(not_sent) => {
            return TeamRetractPreview::Answer(retract_not_sent_sentence(&alias, peer, &not_sent));
        }
        // Owner ruling: an unverified state DISARMS `[y]`. The card still
        // renders and names the cause in its own words; ⛔ no retry key, ⛔ no
        // override — the remedy is retyping the command.
        RetractPreview::Unverified(refusal) => (
            format!(
                "item         {shown_item}\ntheir state  not verified — {}",
                retract_read_failure_cause(&alias, &refusal)
            ),
            false,
            format!("Cannot verify on {alias}'s host.  [n] Cancel (Esc)"),
            None,
            false,
        ),
    };
    let credential = if principal_bound {
        String::new()
    } else {
        format!(
            "{}\n\n",
            crate::adapters::tui::handlers::team_command::one_credential_sentence(&alias)
        )
    };
    TeamRetractPreview::Card(TeamRetractCard {
        peer: peer.to_owned(),
        item_id: item.to_owned(),
        task,
        body: format!(
            "Retract on {alias}'s host\n\n{rows}\n\n{credential}\
             This marks the item on their host. It does not delete it, and there is no \
             un-retract. What they already read, they already read.\n\n\
             {key_line}"
        ),
        armed,
    })
}

/// Why a confirm-time read did not verify, in the failure's own words:
/// could not reach · refused · old build · unknown.
#[cfg(feature = "a2a")]
fn retract_read_failure_cause(
    alias: &str,
    refusal: &crate::adapters::a2a::send::RetractRefusal,
) -> String {
    use crate::adapters::a2a::send::RetractRefusal;

    match refusal {
        RetractRefusal::CouldNotReach => format!("could not reach {alias}'s host"),
        RetractRefusal::RefusedByPolicy | RetractRefusal::Malformed => {
            format!("refused by {alias}'s host")
        }
        RetractRefusal::Credential(sentence) => sentence.clone(),
        RetractRefusal::OldBuild => {
            format!("old build — {alias} runs a build without the item verbs")
        }
        _ => format!("unknown — no usable answer from {alias}'s host"),
    }
}

/// One accepted retract's answer, rendered (Story 19.16f `AC10`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRetractAnswer {
    /// The one outcome sentence for the stable `team-retract` block.
    pub message: String,
    /// After a landed retract: the remembered board with the mark applied.
    pub board: Option<String>,
}

/// The dispatch the accepted card's spawn awaits (Story 19.16f `AC10`) —
/// the served verb's first production caller. `pub`: the front-door keystone
/// awaits exactly this, after the real key path resolved the card.
#[cfg(feature = "a2a")]
pub async fn team_retract_dispatch(
    runtime: &crate::adapters::a2a::driver::A2aDelegationRuntime,
    pending: &crate::adapters::tui::state::PendingTeamRetract,
) -> TeamRetractAnswer {
    use crate::adapters::a2a::board::{mark_clock, render_board};
    use crate::adapters::a2a::send::{RetractOutcome, RetractRefusal, retract_item_on_peer};
    use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};

    let alias = retract_alias(&pending.peer);
    let outcome = retract_item_on_peer(
        runtime,
        &pending.peer,
        &pending.item_id,
        pending.task.clone(),
    )
    .await;
    let mut board = None;
    let message = match outcome {
        RetractOutcome::Landed { retracted_at_ms } => {
            if let Some(ms) = retracted_at_ms {
                board = runtime
                    .mark_board_item_retracted(
                        &pending.peer,
                        &pending.item_id,
                        ms,
                        runtime.wall_now_unix_secs(),
                    )
                    .await;
            }
            format!(
                "Retracted on {alias}'s host. Marked there — never deleted.\n\
                 What they already read, they already read."
            )
        }
        RetractOutcome::AlreadyRetracted { retracted_at_ms } => match retracted_at_ms {
            Some(ms) => format!("already retracted {} — nothing changed", mark_clock(ms)),
            None => "already retracted — nothing changed".to_owned(),
        },
        RetractOutcome::NotSent(not_sent) => {
            retract_not_sent_sentence(&alias, &pending.peer, &not_sent)
        }
        RetractOutcome::Refused(refusal) => match refusal {
            RetractRefusal::Tombstone => format!(
                "{alias}'s host already removed that item — its content is no longer shown \
                 there.\nNothing was marked."
            ),
            RetractRefusal::RefusedByPolicy => {
                format!("{alias}'s host refused the retract. Nothing was marked.")
            }
            RetractRefusal::NotFound => format!(
                "{alias}'s host has no item {} this host can address. Nothing was marked.",
                sanitize_disclosable(&pending.item_id, MAX_PEER_ID_BYTES)
            ),
            RetractRefusal::OldBuild => {
                format!("{alias} runs a build without the retract verb. Nothing was marked.")
            }
            RetractRefusal::Malformed => {
                format!("{alias}'s host refused the request as malformed. Nothing was marked.")
            }
            RetractRefusal::CouldNotReach => {
                format!("Could not reach {alias}'s host. Nothing was marked.")
            }
            RetractRefusal::Credential(sentence) => sentence,
            _ => format!(
                "No usable answer from {alias}'s host — whether the item was marked is \
                 unknown. '/team board' shows its current mark."
            ),
        },
    };
    TeamRetractAnswer { message, board }
}

/// The `TeamRetractConfirm`/`TeamRetractDecline` arm (Story 19.16f `AC4`,
/// `AC10(b)`). Accepting takes the slot, restores focus and shows
/// `  sending…` **before** the spawn; a second `y` finds no slot. Declining
/// sends nothing and changes no block.
pub(crate) async fn resolve_team_retract(state: &mut TuiState, accept: bool, app_state: &AppState) {
    use crate::adapters::tui::handlers::team_command as handler;

    let Some(pending) = handler::resolve_team_retract_card(state, accept) else {
        return;
    };
    #[cfg(feature = "a2a")]
    {
        let Some(runtime) = app_state.a2a_send.clone() else {
            return handler::show_team_retract(
                state,
                "· no A2A send runtime is configured for this session, so there is no peer to \
                 retract on. Nothing was marked."
                    .to_owned(),
            );
        };
        handler::show_team_retract(state, handler::TEAM_RETRACT_SENDING.to_owned());
        let event_bus = app_state.event_bus.clone();
        let cancel = app_state.session_cancel.child_token();
        tokio::spawn(async move {
            let answer = tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                answer = team_retract_dispatch(&runtime, &pending) => answer,
            };
            let _ = event_bus.emit_domain(crate::domain::events::AppEvent::TeamRetractAnswered {
                conversation_id: pending.conversation_id,
                message: answer.message,
                board: answer.board,
            });
        });
    }
    #[cfg(not(feature = "a2a"))]
    let _ = (pending, app_state);
}

/// Story 19.17: settle only the addressed action's row on its issuing tab.
pub fn team_send_settled(
    active_conversation_id: &str,
    state: &mut TuiState,
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: &str,
    block_id: &str,
    index: usize,
    outcome: crate::domain::models::RecipientOutcome,
) {
    let redraw = if conversation_id == active_conversation_id {
        settle_team_send_row(
            &mut state.team_send_blocks,
            &mut state.feedback_blocks,
            block_id,
            index,
            outcome,
        )
    } else if let Some(tab) = tab_manager.find_by_conversation_mut(conversation_id) {
        settle_team_send_row(
            &mut tab.team_send_blocks,
            &mut tab.feedback_blocks,
            block_id,
            index,
            outcome,
        )
    } else {
        false
    };
    if redraw {
        state.needs_redraw = true;
    }
}

/// Replace one row and re-render its block. Once every row has settled the
/// block text is final, so its typed rows are dropped rather than carried
/// (and cloned on every tab switch) for the rest of the session.
pub(crate) fn settle_team_send_row(
    blocks: &mut std::collections::BTreeMap<
        String,
        Vec<crate::adapters::tui::handlers::team_command::TeamSendRow>,
    >,
    feedback_blocks: &mut std::collections::BTreeMap<String, crate::domain::models::FeedbackBlock>,
    block_id: &str,
    index: usize,
    outcome: crate::domain::models::RecipientOutcome,
) -> bool {
    use crate::adapters::tui::handlers::team_command::render_team_send_rows;

    let Some(rows) = blocks.get_mut(block_id) else {
        return false;
    };
    let Some(row) = rows.get_mut(index) else {
        return false;
    };
    row.outcome = Some(outcome);
    let redraw = if let Some(block) = feedback_blocks.get_mut(block_id) {
        block.message = render_team_send_rows(rows);
        true
    } else {
        false
    };
    if rows.iter().all(|row| row.outcome.is_some()) {
        blocks.remove(block_id);
    }
    redraw
}

/// The `TeamRetractPreviewReady` arm. The card is raised on the active tab
/// whichever tab issued the command: it is self-describing (host, item) and
/// cancelling it is free. A sentence answer follows the board's tab routing.
pub(crate) fn team_retract_preview_ready(
    active_conversation_id: String,
    state: &mut TuiState,
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: String,
    preview: crate::domain::events::TeamRetractPreview,
) {
    use crate::adapters::tui::handlers::team_command::open_team_retract_card;
    use crate::domain::events::TeamRetractPreview;

    match preview {
        TeamRetractPreview::Answer(message) if conversation_id != active_conversation_id => {
            store_team_retract_in_background(tab_manager, &conversation_id, message);
        }
        TeamRetractPreview::Card(card) => open_team_retract_card(
            state,
            &active_conversation_id,
            TeamRetractPreview::Card(card),
        ),
        preview => open_team_retract_card(state, &conversation_id, preview),
    }
}

/// The `TeamRetractAnswered` arm: replace the `team-retract` block — and,
/// after a landed retract, the `team-board` block — on the issuing tab.
pub(crate) fn team_retract_answered(
    active_conversation_id: String,
    state: &mut TuiState,
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: String,
    message: String,
    board: Option<String>,
) {
    use crate::adapters::tui::handlers::team_command::{show_team_board, show_team_retract};

    if conversation_id == active_conversation_id {
        if let Some(board) = board {
            show_team_board(state, board);
        }
        show_team_retract(state, message);
    } else {
        store_team_retract_in_background(tab_manager, &conversation_id, message);
    }
}

/// A background tab's retract answer must survive the switch — the board's
/// own routing (`TeamBoardReady`).
fn store_team_retract_in_background(
    tab_manager: &mut crate::domain::models::tab::TabManager,
    conversation_id: &str,
    message: String,
) {
    if let Some(tab) = tab_manager.find_by_conversation_mut(conversation_id) {
        crate::adapters::tui::handlers::notice::store_background_notice(
            tab,
            crate::domain::models::NoticeLevel::Advisory,
            message,
        );
    }
}

fn emit_team_warning(
    state: &mut TuiState,
    conversation_id: &str,
    app_state: &AppState,
    message: String,
) {
    state.needs_redraw = true;
    let _ = app_state
        .event_bus
        .emit_domain(crate::domain::events::AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level: crate::domain::models::NoticeLevel::Warning,
            message,
        });
}

async fn change_sender_consent(
    app_state: &AppState,
    target: &str,
    trust: bool,
) -> Result<String, String> {
    let workspace = &app_state.compose_snapshot.workspace_path;
    let peers = &app_state.compose_snapshot.a2a_peers;
    let (message, event) = persist_sender_consent(
        workspace,
        peers,
        target,
        trust,
        chrono::Utc::now().timestamp_millis(),
    )
    .await?;
    if let Some(event) = event {
        let _ = app_state
            .event_bus
            .emit_domain(crate::domain::events::AppEvent::DomainEvent(event.into()));
    }
    Ok(message)
}

async fn persist_sender_consent(
    workspace: &std::path::Path,
    peers: &[crate::domain::models::A2aPeerSpec],
    target: &str,
    trust: bool,
    now: i64,
) -> Result<(String, Option<crate::domain::models::RoomEvent>), String> {
    use crate::domain::ports::peer_consent_state;

    let sender = crate::adapters::tui::handlers::team_command::resolve_peer_target(target, peers)?;
    let projection = crate::adapters::policy::JournalConsentProjection::load_workspace(workspace)
        .await
        .map_err(|error| error.to_string())?;
    let current = peer_consent_state(&projection, &sender);
    if trust && current == crate::domain::ports::ConsentState::Trusted {
        return Ok((
            format!("{target} ({sender}) is already trusted; no new grant was recorded."),
            None,
        ));
    }
    if !trust && current != crate::domain::ports::ConsentState::Trusted {
        return Ok((
            format!("No active grant for {target} ({sender}); nothing changed."),
            None,
        ));
    }

    let event = if trust {
        crate::domain::models::RoomEvent::ConsentGranted {
            sender: Some(sender.clone()),
            granted_at: now,
            principal: None,
        }
    } else {
        crate::domain::models::RoomEvent::ConsentRevoked {
            sender: Some(sender.clone()),
            revoked_at: now,
        }
    };
    let journal =
        crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .map_err(|error| error.to_string())?;
    journal
        .append_room(event.clone())
        .await
        .map_err(|error| error.to_string())?;
    let message = if trust {
        format!("Trusted {target} ({sender}). Future tasks from this sender may proceed.")
    } else {
        format!("Revoked trust for {target} ({sender}). Future tasks require consent.")
    };
    Ok((message, Some(event)))
}

async fn load_team_status(app_state: &AppState) -> Result<String, String> {
    let workspace = &app_state.compose_snapshot.workspace_path;
    let peers = &app_state.compose_snapshot.a2a_peers;
    let projection = crate::adapters::policy::JournalConsentProjection::load_workspace(workspace)
        .await
        .map_err(|error| error.to_string())?;
    let (policy, _) =
        crate::adapters::policy::resolve_workspace_policy(workspace, peers, &projection)
            .map_err(|error| error.to_string())?;
    Ok(
        crate::adapters::tui::handlers::team_command::render_team_status(
            &policy,
            &projection,
            peers,
        ),
    )
}
/// One-call shell for the shipped `/memory consolidate|forget` dispatch paths.
///
/// Returns `false` for `/memory` adapter overrides so the event loop can keep
/// routing those through `port_dimension_from_command_name`.
pub(crate) async fn memory_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_name: &str,
    cmd_arg: Option<&str>,
    is_streaming: bool,
    config: &AppConfig,
    app_state: &AppState,
    provider: &std::sync::Arc<dyn crate::domain::ports::StreamingProvider>,
    domain_tx: &tokio::sync::mpsc::UnboundedSender<crate::domain::events::AppEvent>,
) -> bool {
    use crate::adapters::tui::handlers;
    use crate::domain::events::AppEvent;
    use crate::domain::models::NoticeLevel;

    let consolidate = cmd_name == "memory" && cmd_arg.map(str::trim) == Some("consolidate");
    let forget_query = handlers::forget_command::parse_forget_query(cmd_name, cmd_arg);
    if !consolidate && forget_query.is_none() {
        return false;
    }

    if is_streaming {
        let message = if consolidate {
            "Consolidation unavailable while a turn is in progress — try again after it finishes."
        } else {
            "Memory forget unavailable while a turn is in progress — try again after it finishes."
        };
        let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level: NoticeLevel::Info,
            message: message.to_string(),
        });
        state.needs_redraw = true;
        return true;
    }

    if consolidate {
        let memory = app_state.agent_core.memory.load_full();
        match memory.recent(30).await {
            Ok(entries) if entries.is_empty() => {
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Info,
                    message: "Nothing to consolidate yet — no recent activity recorded."
                        .to_string(),
                });
                state.needs_redraw = true;
            }
            Ok(entries) => {
                let prompt_body =
                    crate::domain::services::consolidation::build_proposal_prompt(&entries);
                let payload = handlers::consolidation::ConsolidationPayload {
                    provider: std::sync::Arc::clone(provider),
                    model: config.model.clone(),
                    prompt_body,
                    conversation_id: conversation_id.to_owned(),
                    domain_tx: domain_tx.clone(),
                };
                tokio::spawn(handlers::consolidation::run_consolidation(payload));
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Info,
                    message: "Reviewing recent activity for durable facts…".to_string(),
                });
                state.needs_redraw = true;
            }
            Err(e) => {
                let _ = app_state.event_bus.emit_domain(AppEvent::SystemNotice {
                    conversation_id: Some(conversation_id.to_owned()),
                    level: NoticeLevel::Warning,
                    message: format!("Consolidation failed: {e}"),
                });
                state.needs_redraw = true;
            }
        }
    } else if let Some(query) = forget_query {
        let result = if query.is_empty() {
            None
        } else {
            Some(
                app_state
                    .agent_core
                    .memory
                    .load_full()
                    .forget_candidates(&query, handlers::forget_command::FORGET_CANDIDATE_LIMIT)
                    .await,
            )
        };
        for event in handlers::forget_command::handle_forget_command(
            state,
            &conversation_id.to_owned(),
            &query,
            result,
        ) {
            let _ = app_state.event_bus.emit_domain(event);
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ports::{ConsentProjectionQuery, ConsentState, RoomJournalReader};

    #[tokio::test]
    async fn trust_and_untrust_are_durable_idempotent_and_visible_in_team_log() {
        let workspace = tempfile::TempDir::new().unwrap();
        let sender = crate::domain::models::PeerId::from_public_key(&[11u8; 32]).unwrap();
        let target = sender.as_str();

        let (_, grant) = persist_sender_consent(workspace.path(), &[], target, true, 10)
            .await
            .unwrap();
        assert!(matches!(
            grant,
            Some(crate::domain::models::RoomEvent::ConsentGranted { .. })
        ));
        let projection =
            crate::adapters::policy::JournalConsentProjection::load_workspace(workspace.path())
                .await
                .unwrap();
        assert_eq!(
            projection.consent_for(&crate::domain::models::PolicyPrincipal::rap(sender.clone())),
            ConsentState::Trusted
        );

        let (_, duplicate) = persist_sender_consent(workspace.path(), &[], target, true, 11)
            .await
            .unwrap();
        assert!(duplicate.is_none());

        let (_, revoke) = persist_sender_consent(workspace.path(), &[], target, false, 12)
            .await
            .unwrap();
        assert!(matches!(
            revoke,
            Some(crate::domain::models::RoomEvent::ConsentRevoked { .. })
        ));
        let (_, duplicate_revoke) =
            persist_sender_consent(workspace.path(), &[], target, false, 13)
                .await
                .unwrap();
        assert!(duplicate_revoke.is_none());

        let projection =
            crate::adapters::policy::JournalConsentProjection::load_workspace(workspace.path())
                .await
                .unwrap();
        assert_eq!(
            projection.consent_for(&crate::domain::models::PolicyPrincipal::rap(sender)),
            ConsentState::Revoked
        );
        let reader =
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                workspace.path(),
            );
        let entries = reader.load_entries().await.unwrap();
        assert_eq!(entries.len(), 2);
        let rows = crate::domain::services::transparency::fold_transparency(&entries);
        assert!(matches!(
            rows.as_slice(),
            [
                crate::domain::services::transparency::TransparencyRow {
                    kind: crate::domain::services::transparency::TransparencyKind::ConsentGranted,
                    ..
                },
                crate::domain::services::transparency::TransparencyRow {
                    kind: crate::domain::services::transparency::TransparencyKind::ConsentRevoked,
                    ..
                }
            ]
        ));
    }

    /// Task 0 goldens (rustain HEAD e70db869; Story 19.18d0 Debug Log References):
    /// `G1` is the `ConsentGranted` line, `G3` the bare `ConsentRevoked` line that
    /// `persist_sender_consent(trust=false, now=1_700_000_000_000)` wrote. Both come
    /// from production writers; the stamps below are the repro run's own.
    const G1_SENDER: &str = "1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569";
    const G1_CONSENT_GRANTED: &str = r##"{"schema_version":1,"seq":1,"recorded_at_ms":1790974662796,"record":{"kind":"room","payload":{"event":"consent_granted","sender":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569","granted_at":1790974662796}}}"##;
    const G3_CONSENT_REVOKED: &str = r##"{"schema_version":1,"seq":2,"recorded_at_ms":1790974663108,"record":{"kind":"room","payload":{"event":"consent_revoked","sender":"1220fd4baad8bbc21b3d13fcc22ab21561129ab39d9c335fd4f7509cf26ba15f7569","revoked_at":1700000000000}}}"##;

    fn golden_line(
        seq: u64,
        recorded_at_ms: i64,
        event: crate::domain::models::RoomEvent,
    ) -> String {
        serde_json::to_string(&crate::domain::models::JournalEntry::new(
            seq,
            crate::domain::models::JournalRecord::Room(event),
            recorded_at_ms,
        ))
        .unwrap()
    }

    /// K03b (untrust): any `Trusted` slot is revocable by `PeerId` — including a
    /// pseudonym-only or legacy-only grant that `consent_for(rap(..))` cannot see —
    /// with the bare `G3` event, every slot ends `Revoked`, a second untrust is a
    /// no-op, and a later typed `Pinned` re-grant re-trusts `rap` only.
    #[tokio::test]
    async fn k03b_sender_revocation_and_legacy_trust_bytes() {
        use crate::domain::models::{PolicyPrincipal, PrincipalKind, RoomEvent};
        use crate::domain::ports::{ConsentSlot, peer_consent_state};

        let sender = crate::domain::models::PeerId::parse(G1_SENDER).unwrap();
        let target = sender.as_str();
        let scenarios: [(&str, Vec<Option<PrincipalKind>>); 3] = [
            ("pseudonym-only", vec![Some(PrincipalKind::Pseudonymous)]),
            ("legacy-only", vec![None]),
            (
                "all-slots",
                vec![
                    Some(PrincipalKind::Pinned),
                    Some(PrincipalKind::Pseudonymous),
                    None,
                ],
            ),
        ];

        for (name, grants) in scenarios {
            let workspace = tempfile::TempDir::new().unwrap();
            let journal =
                crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                    workspace.path(),
                )
                .await
                .unwrap();
            for principal in &grants {
                journal
                    .append_room(RoomEvent::ConsentGranted {
                        sender: Some(sender.clone()),
                        granted_at: 5,
                        principal: *principal,
                    })
                    .await
                    .unwrap();
            }
            let load = || async {
                crate::adapters::policy::JournalConsentProjection::load_workspace(workspace.path())
                    .await
                    .unwrap()
            };
            let before = load().await;
            assert_eq!(
                peer_consent_state(&before, &sender),
                ConsentState::Trusted,
                "{name}"
            );
            assert_eq!(before.known_senders().len(), grants.len(), "{name}");
            // The reason untrust must not ask `consent_for(rap(..))`.
            let rap_view = before.consent_for(&PolicyPrincipal::rap(sender.clone()));
            match name {
                "pseudonym-only" => assert_eq!(rap_view, ConsentState::None),
                _ => assert_eq!(rap_view, ConsentState::Trusted),
            }

            let (message, revoke) =
                persist_sender_consent(workspace.path(), &[], target, false, 1_700_000_000_000)
                    .await
                    .unwrap();
            assert_eq!(
                message,
                format!("Revoked trust for {target} ({sender}). Future tasks require consent."),
                "{name}"
            );
            let revoke = revoke.unwrap_or_else(|| panic!("{name}: untrust must revoke"));
            assert_eq!(
                golden_line(2, 1790974663108, revoke),
                G3_CONSENT_REVOKED,
                "{name}"
            );

            let after = load().await;
            let cells = after.known_senders();
            assert_eq!(cells.len(), grants.len(), "{name}: revoke adds no cells");
            for (slot, peer) in &cells {
                assert_eq!(
                    after.slot_state(*slot, peer),
                    ConsentState::Revoked,
                    "{name}: {slot:?}"
                );
            }
            assert_eq!(
                peer_consent_state(&after, &sender),
                ConsentState::Revoked,
                "{name}"
            );

            let (message, again) =
                persist_sender_consent(workspace.path(), &[], target, false, 1_700_000_000_001)
                    .await
                    .unwrap();
            assert!(again.is_none(), "{name}");
            assert_eq!(
                message,
                format!("No active grant for {target} ({sender}); nothing changed."),
                "{name}"
            );

            // A later typed `Pinned` grant re-trusts the RAP principal only.
            journal
                .append_room(RoomEvent::ConsentGranted {
                    sender: Some(sender.clone()),
                    granted_at: 20,
                    principal: Some(PrincipalKind::Pinned),
                })
                .await
                .unwrap();
            let regranted = load().await;
            assert_eq!(
                regranted.consent_for(&PolicyPrincipal::rap(sender.clone())),
                ConsentState::Trusted,
                "{name}"
            );
            assert_eq!(
                regranted.consent_for(&PolicyPrincipal::Pseudonymous(sender.clone())),
                ConsentState::Revoked,
                "{name}: a pinned re-grant must not re-trust the pseudonym"
            );
            assert_eq!(
                regranted.slot_state(ConsentSlot::Pseudonymous, &sender) == ConsentState::Revoked,
                grants.contains(&Some(PrincipalKind::Pseudonymous)),
                "{name}"
            );
        }
        check_legacy_trust_bytes().await;
    }

    /// K03b (unchanged `trust` branch, D2): no production caller, no provenance
    /// invented — the emitted bytes are `G1` (`principal` absent) for a raw hex
    /// target and for a roster alias.
    async fn check_legacy_trust_bytes() {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        let raw = tempfile::TempDir::new().unwrap();
        let (_, event) = persist_sender_consent(raw.path(), &[], G1_SENDER, true, 1790974662796)
            .await
            .unwrap();
        assert_eq!(
            golden_line(1, 1790974662796, event.unwrap()),
            G1_CONSENT_GRANTED
        );

        let handle = format!(
            "apikey:{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(b"rustain-19-18d0-r1"))
        );
        let key = Sha256::digest(handle.as_bytes());
        let pinned_peer = crate::domain::models::PeerId::from_public_key(&key).unwrap();
        assert_eq!(pinned_peer.as_str(), G1_SENDER);
        let roster = [crate::domain::models::A2aPeerSpec::new(
            "alice",
            crate::domain::models::RedactedUrl::new("https://alice.example/a2a".to_owned()),
            crate::domain::models::A2aPeerSource::Workspace,
        )
        .with_pinned_key(Some(crate::domain::models::PinnedKey::new(
            crate::domain::models::PinnedKeyAlgorithm::EdDsa,
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
            None,
        )))];
        let aliased = tempfile::TempDir::new().unwrap();
        let (_, event) =
            persist_sender_consent(aliased.path(), &roster, "alice", true, 1790974662796)
                .await
                .unwrap();
        let line = golden_line(1, 1790974662796, event.unwrap());
        assert_eq!(line, G1_CONSENT_GRANTED);
        assert!(!line.contains("principal"), "{line}");
    }

    /// A real `AppState` over `workspace`, composed exactly as the startup
    /// root composes it, minus the slots this test does not read. Returns the
    /// domain receiver the event bus feeds.
    pub(super) fn bridge_app_state(
        workspace: &std::path::Path,
    ) -> (
        AppState,
        tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    ) {
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use clap::Parser;

        use crate::infrastructure::runtime::event_bus::EventBus;

        let (event_bus, domain_rx) = EventBus::new(16);
        // `AppState::new` hands the receiver straight back; the bridge's
        // warnings arrive on it, exactly as the event loop sees them.
        let compose_snapshot = Arc::new(crate::infrastructure::composition::ComposeContext {
            workspace_path: workspace.to_path_buf(),
            project_context: crate::domain::models::project_context::ProjectContext::empty(),
            storage: Arc::new(crate::adapters::noop::NoOpStorage)
                as Arc<dyn crate::domain::ports::StoragePort>,
            skill_activator: Arc::new(crate::adapters::skill_activation::SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(crate::infrastructure::skill_cache::SkillCache::new_in_memory()),
            sandbox_adapter: "noop".into(),
            sandbox_startup_policy: crate::domain::models::sandbox::SandboxPolicy::Permissive,
            sandbox_slot: Arc::new(ArcSwap::from_pointee(Arc::new(
                crate::adapters::sandbox::NoOpSandbox,
            )
                as Arc<dyn crate::domain::ports::SandboxManager>)),
            memory_slot: Arc::new(ArcSwap::from_pointee(
                Arc::new(crate::adapters::noop::NoOpMemory)
                    as Arc<dyn crate::domain::ports::MemoryPort>,
            )),
            sandbox_policy: Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(crate::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: crate::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
        });
        let (app_state, domain_rx) = AppState::new(
            Arc::new(event_bus),
            domain_rx,
            crate::domain::services::approval_runtime::ApprovalRuntime::new(
                16,
                Arc::new(crate::adapters::noop::NoOpApprovalPersistence),
            ),
            Arc::new(tokio::sync::RwLock::new(
                crate::domain::models::SandboxPolicy::Permissive,
            )),
            Arc::new(crate::domain::services::plan_manager::PlanManager::new(
                workspace.to_path_buf(),
            )),
            Arc::new(crate::domain::services::plan_mode_injector::DefaultPlanInjector::new()),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(crate::adapters::noop::NoOpProvider)
                    as Arc<dyn crate::domain::ports::StreamingProvider>,
            )),
            Arc::new(crate::adapters::provider::ProviderRegistry::new()),
            Arc::new(crate::adapters::noop::NoOpUsageLedger),
            Arc::new(crate::adapters::budget::BudgetStateStore::new()),
            Arc::new(ArcSwap::from_pointee(
                crate::domain::models::AppConfig::default(),
            )),
            Arc::new(crate::infrastructure::runtime::agent_core::AgentCore::test_noop()),
            None,
            compose_snapshot,
            Arc::new(ArcSwap::from_pointee(Arc::new(
                crate::adapters::profile_resolver::noop::NoopProfileResolver,
            )
                as Arc<dyn crate::domain::ports::ProfileResolver>)),
            crate::adapters::cli::commands::Cli::try_parse_from(["rustain"]).expect("bare cli"),
            None,
            crate::infrastructure::telemetry::ActiveRatioWindow::new_in_memory(),
            #[cfg(feature = "meta-search")]
            None,
        );
        (app_state, domain_rx)
    }

    #[tokio::test]
    async fn untrust_unknown_sender_is_a_noop_without_creating_a_journal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let sender = crate::domain::models::PeerId::from_public_key(&[12u8; 32]).unwrap();

        let (message, event) =
            persist_sender_consent(workspace.path(), &[], sender.as_str(), false, 20)
                .await
                .unwrap();

        assert!(event.is_none());
        assert!(message.contains("nothing changed"));
        assert!(!workspace.path().join(".rustain").exists());
    }

    #[test]
    fn cross_tab_retract_outcome_replaces_the_card_host_blocks_sending_state() {
        use crate::adapters::tui::handlers::team_command::{
            TEAM_RETRACT_BLOCK_ID, TEAM_RETRACT_SENDING, show_team_retract,
        };
        use crate::domain::events::{TeamRetractCard, TeamRetractPreview};

        let mut tabs = crate::domain::models::tab::TabManager::default();
        let origin = tabs.active_tab().conversation.id.clone();
        tabs.create_tab();
        let card_host = tabs.active_tab().conversation.id.clone();
        let mut state = crate::adapters::tui::state::TuiState::new(120, 40);

        super::team_retract_preview_ready(
            card_host.clone(),
            &mut state,
            &mut tabs,
            origin,
            TeamRetractPreview::Card(TeamRetractCard {
                peer: "jun-dev".to_owned(),
                item_id: "ri_x".to_owned(),
                task: Some("task-x".to_owned()),
                body: "card".to_owned(),
                armed: true,
            }),
        );
        let outcome_owner = state
            .pending_team_retract
            .as_ref()
            .expect("the active tab hosts the card")
            .conversation_id
            .clone();
        assert_eq!(outcome_owner, card_host);

        show_team_retract(&mut state, TEAM_RETRACT_SENDING.to_owned());
        super::team_retract_answered(
            card_host,
            &mut state,
            &mut tabs,
            outcome_owner,
            "landed".to_owned(),
            None,
        );
        assert_eq!(
            state.feedback_blocks[TEAM_RETRACT_BLOCK_ID].message,
            "landed"
        );
    }

    /// Story 19.18 `K11` — a thread answer for a background tab replaces THAT
    /// tab's single `team-thread` block and leaves the visible tab's alone.
    ///
    /// `M20` → RED: publish to the active conversation instead.
    #[test]
    fn a_background_thread_answer_replaces_only_its_issuing_tabs_block() {
        use crate::adapters::tui::handlers::team_command::TEAM_THREAD_BLOCK_ID;

        let mut tabs = crate::domain::models::tab::TabManager::default();
        let issuing = tabs.active_tab().conversation.id.clone();
        tabs.create_tab();
        let active = tabs.active_tab().conversation.id.clone();
        let mut state = TuiState::new(120, 40);
        crate::adapters::tui::handlers::team_command::show_team_thread(
            &mut state,
            "visible tab's own thread".to_owned(),
        );
        team_thread_ready(&active, &mut state, &mut tabs, &issuing, "first".to_owned());
        team_thread_ready(
            &active,
            &mut state,
            &mut tabs,
            &issuing,
            "second".to_owned(),
        );
        assert_eq!(
            state.feedback_blocks[TEAM_THREAD_BLOCK_ID].message,
            "visible tab's own thread"
        );
        let tab = tabs.find_by_conversation(&issuing).expect("issuing tab");
        assert_eq!(tab.feedback_blocks[TEAM_THREAD_BLOCK_ID].message, "second");
        assert_eq!(
            tab.feedback_blocks[TEAM_THREAD_BLOCK_ID].level,
            crate::domain::models::FeedbackLevel::Info
        );
        assert_eq!(
            tab.feedback_blocks
                .keys()
                .filter(|id| id.starts_with("team-thread"))
                .count(),
            1,
            "one block per tab, replaced — never stacked"
        );
        team_thread_ready(&active, &mut state, &mut tabs, &active, "mine".to_owned());
        assert_eq!(state.feedback_blocks[TEAM_THREAD_BLOCK_ID].message, "mine");
    }

    /// Story 19.18 `K11` (cancel half) — a cancelled session publishes
    /// nothing further. The test awaits the spawned task's handle, then
    /// `try_recv`s: ⛔ no timing wait. Positive control: an uncancelled spawn
    /// publishes exactly one `TeamThreadReady`.
    #[cfg(feature = "a2a")]
    #[tokio::test]
    async fn a_cancelled_thread_read_publishes_nothing() {
        use crate::infrastructure::runtime::event_bus::EventBus;

        struct Accepting;
        #[async_trait::async_trait]
        impl crate::domain::ports::RoomJournal for Accepting {
            async fn record_event(
                &self,
                _event: crate::domain::models::RoomEvent,
            ) -> Result<(), crate::domain::ports::RoomJournalError> {
                Ok(())
            }
        }
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = std::sync::Arc::new(crate::adapters::a2a::driver::A2aDelegationRuntime::new(
            crate::infrastructure::subagent::NodeTree::new(),
            std::sync::Arc::new(Accepting),
            tx,
        ));
        let (event_bus, mut domain_rx) = EventBus::new(16);
        let event_bus = std::sync::Arc::new(event_bus);
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        spawn_team_thread(
            runtime.clone(),
            event_bus.clone(),
            "conv".to_owned(),
            "nobody".to_owned(),
            "ri_q".to_owned(),
            cancel,
        )
        .await
        .expect("spawned read");
        assert!(
            domain_rx.try_recv().is_err(),
            "a cancelled read publishes nothing"
        );
        spawn_team_thread(
            runtime,
            event_bus,
            "conv".to_owned(),
            "nobody".to_owned(),
            "ri_q".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("spawned read");
        assert!(matches!(
            domain_rx.try_recv(),
            Ok(crate::domain::events::AppEvent::TeamThreadReady { .. })
        ));
    }

    #[test]
    fn a_background_team_send_settlement_updates_only_its_issuing_tab() {
        use crate::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
        use crate::domain::models::RecipientOutcome;
        use crate::domain::models::{FeedbackBlock, FeedbackLevel};

        const BLOCK_ID: &str = "team-send-12";
        let mut tabs = crate::domain::models::tab::TabManager::default();
        let issuing_conversation_id = tabs.active_tab().conversation.id.clone();
        tabs.create_tab();
        let active_conversation_id = tabs.active_tab().conversation.id.clone();
        let mut state = TuiState::new(120, 40);
        state.density_mode = crate::domain::models::visual::DensityMode::Focus;

        let sending_row = TeamSendRow {
            alias: "jun-dev".to_owned(),
            outcome: None,
        };
        let sending_block = FeedbackBlock {
            id: BLOCK_ID.to_owned(),
            level: FeedbackLevel::Info,
            message: render_team_send_rows(std::slice::from_ref(&sending_row)),
            actions: Vec::new(),
        };
        let issuing_tab = tabs
            .find_by_conversation_mut(&issuing_conversation_id)
            .expect("the issuing tab remains open in the background");
        issuing_tab
            .team_send_blocks
            .insert(BLOCK_ID.to_owned(), vec![sending_row.clone()]);
        issuing_tab
            .feedback_blocks
            .insert(BLOCK_ID.to_owned(), sending_block.clone());
        state
            .team_send_blocks
            .insert(BLOCK_ID.to_owned(), vec![sending_row]);
        state
            .feedback_blocks
            .insert(BLOCK_ID.to_owned(), sending_block);

        team_send_settled(
            &active_conversation_id,
            &mut state,
            &mut tabs,
            &issuing_conversation_id,
            BLOCK_ID,
            0,
            RecipientOutcome::Delivered,
        );

        assert_eq!(
            state.team_send_blocks[BLOCK_ID][0].outcome, None,
            "a settlement for a background tab must not overwrite the visible tab's row"
        );
        assert_eq!(
            state.feedback_blocks[BLOCK_ID].message, "    jun-dev  sending…",
            "the visible Info block remains its own in-progress action"
        );
        let issuing_tab = tabs
            .find_by_conversation(&issuing_conversation_id)
            .expect("the issuing tab is retained");
        assert!(
            !issuing_tab.team_send_blocks.contains_key(BLOCK_ID),
            "a fully settled action keeps only its final block text, not its typed rows"
        );
        assert_eq!(
            issuing_tab.feedback_blocks[BLOCK_ID].level,
            FeedbackLevel::Info
        );
        assert_eq!(
            issuing_tab.feedback_blocks[BLOCK_ID].message, "  ● jun-dev  delivered",
            "the background tab's visible Info block is updated in place"
        );
        assert!(
            state.queued_notifications.is_empty(),
            "Focus must retain the Info block rather than queue an advisory notice"
        );
        assert!(state.needs_redraw);
    }
}

/// Story 19.14 `AC2`/`AC4`/`AC5` — the credential, the typed refusals, and the
/// secret's absence, through the `/team send` front door.
///
/// Front door: `A2aEgress::compose` → `egress.runtime()` →
/// `send::deliver_text` → `render_team_send_rows` — the calls `team_command`'s
/// send arm makes, minus its `TuiState`/`AppState` shell (the nearest
/// test-visible production seam).
///
/// ⛔ Forbidden bypasses, none used here: a hand-built
/// `A2aDelegationRuntime::with_peer_bindings` around a hand-constructed adapter,
/// any fake `A2aTaskTransport`, or calling `post_jsonrpc`/`TaskClient` directly.
#[cfg(all(test, feature = "a2a"))]
mod credential_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serial_test::serial;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;
    use tracing_test::traced_test;

    use crate::adapters::a2a::egress::A2aEgress;
    use crate::adapters::a2a::send::deliver_text;
    use crate::adapters::a2a::test_fixtures::{
        PeerFixture, RpcAnswer, TestLeaf, Validity, leaf_issued_by, self_signed_leaf, test_ca,
        write_pem,
    };
    use crate::adapters::tui::handlers::team_command::{TeamSendRow, render_team_send_rows};
    use crate::domain::events::AppEvent;
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, CapabilityId, RecipientOutcome, RedactedUrl, RejectReason,
        RoomEvent,
    };
    use crate::domain::ports::{CapabilityProvider, RoomJournal, RoomJournalError};
    use crate::infrastructure::subagent::NodeTree;

    const SETTLE: Duration = Duration::from_secs(20);

    /// Records every row the send path makes durable.
    ///
    /// `tokio::sync::Mutex`, not `std::sync`: the journal port is async and the
    /// project's async-lock ratchet (`conformance.rs::test_no_std_sync_lock_in_async_module`)
    /// bounds `std::sync` locks under `src/adapters` and `src/infrastructure`.
    #[derive(Default)]
    struct RecordingJournal(Mutex<Vec<RoomEvent>>);

    impl RecordingJournal {
        async fn rows(&self) -> Vec<RoomEvent> {
            self.0.lock().await.clone()
        }
    }

    #[async_trait]
    impl RoomJournal for RecordingJournal {
        async fn record_event(&self, event: RoomEvent) -> Result<(), RoomJournalError> {
            self.0.lock().await.push(event);
            Ok(())
        }
    }

    #[async_trait]
    impl crate::domain::ports::RoomJournalReader for RecordingJournal {
        async fn load_entries(
            &self,
        ) -> Result<Vec<crate::domain::models::JournalEntry>, RoomJournalError> {
            Ok(Vec::new())
        }

        async fn latest_seq(&self) -> Result<u64, RoomJournalError> {
            Ok(0)
        }
    }

    /// ⚠ Edition 2024 makes `set_var` `unsafe` and it is process-global, so every
    /// test here owns a UNIQUE variable name and runs `#[serial]`.
    fn export(name: &str, value: &str) {
        // SAFETY: `#[serial]` serializes these tests, and each owns a variable
        // name no other test touches.
        unsafe { std::env::set_var(name, value) };
    }

    fn unexport(name: &str) {
        // SAFETY: as above.
        unsafe { std::env::remove_var(name) };
    }

    struct Harness {
        egress: A2aEgress,
        journal: Arc<RecordingJournal>,
    }

    impl Harness {
        async fn compose(peers: Vec<A2aPeerSpec>) -> Self {
            let journal = Arc::new(RecordingJournal::default());
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            // Story 19.18c: a production-shaped `Ready` key from a throwaway
            // store — these keystones are about credentials, not capabilities.
            let keys = tempfile::tempdir().expect("item capability key dir");
            let egress = A2aEgress::compose(
                peers,
                NodeTree::new(),
                journal.clone() as Arc<dyn RoomJournal>,
                journal.clone(),
                event_tx,
                crate::adapters::a2a::item_capability::load_item_capability_key(Ok(keys
                    .path()
                    .to_path_buf())),
            )
            .expect("compose must survive an unset variable and an unloadable anchor");
            for (_, client) in egress.provider().peer_bindings().iter() {
                tokio::time::timeout(SETTLE, client.await_settled())
                    .await
                    .expect("boot card fetch settled");
            }
            Self { egress, journal }
        }

        /// The delivery seam production settles independently for each recipient.
        async fn send(&self, alias: &str, text: &str) -> RecipientOutcome {
            deliver_text(self.egress.runtime(), alias, text, None, None).await
        }

        async fn rejected_rows(&self) -> Vec<String> {
            self.journal
                .rows()
                .await
                .into_iter()
                .filter_map(|row| match row {
                    RoomEvent::RemoteEnvelopeRejected {
                        reason: RejectReason::Policy { detail },
                        ..
                    } => Some(detail),
                    _ => None,
                })
                .collect()
        }

        async fn dispatched_rows(&self) -> usize {
            self.journal
                .rows()
                .await
                .into_iter()
                .filter(|row| matches!(row, RoomEvent::RemoteEnvelopeDispatched { .. }))
                .count()
        }
    }

    fn unreachable_cause(alias: &str, outcome: RecipientOutcome) -> String {
        let RecipientOutcome::Unreachable { cause: Some(cause) } = outcome.clone() else {
            panic!("a refusal must retain its typed, operator-safe cause: {outcome:?}");
        };
        assert_eq!(
            render_team_send_rows(&[TeamSendRow {
                alias: alias.to_owned(),
                outcome: Some(outcome),
            }]),
            format!("  ⚠ {alias}  unreachable\n    {}", cause.as_str()),
            "the cause is a separate row line, never a transient notice"
        );
        cause.as_str().to_owned()
    }

    fn peer(alias: &str, origin: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(alias, RedactedUrl::from(origin), A2aPeerSource::Workspace)
    }

    /// `AC2(a)(d)(e)` and the "read the credential in `new`" mutant, in one
    /// fixture: the variable is exported **after** `compose` and its value
    /// **changes between two sends**, and a recording server compares.
    #[tokio::test]
    #[serial]
    async fn the_credential_is_read_per_rpc_so_a_rotated_key_takes_effect_on_the_next_send() {
        const VAR: &str = "RUSTAIN_19_14_PER_RPC_KEY";
        unexport(VAR);
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer("rotator", &fixture.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        // (e) A peer whose variable is unexported still composes AND still
        // discovers its card: the boot GET is unauthenticated.
        assert_eq!(
            harness
                .egress
                .provider()
                .discover()
                .await
                .expect("discover")
                .len(),
            1,
            "an unexported credential must not remove the peer from the catalogue"
        );

        export(VAR, "first-value");
        assert_eq!(
            harness.send("rotator", "one").await,
            RecipientOutcome::AcceptedWithoutItem,
            "the fixture's first answer accepts without an item id"
        );
        export(VAR, "second-value");
        let _ = harness.send("rotator", "two").await;
        unexport(VAR);

        assert_eq!(
            fixture.api_keys_seen().await,
            vec!["first-value".to_owned(), "second-value".to_owned()],
            "the value is read on EVERY call; a constructor read or any cache \
             would send `first-value` twice"
        );
        assert_eq!(harness.dispatched_rows().await, 2);
    }

    /// `AC2(b)` + `AC2(c)`: the card GET carries no credential, and the order
    /// anchor → card producer → credential is asserted **as an order** — a pinned
    /// peer with a bad JWS never reaches the POST, so the credential is never
    /// presented to a card rustain refused to trust.
    #[tokio::test]
    #[serial]
    async fn the_card_get_is_unauthenticated_and_a_failed_card_pin_stops_the_credential() {
        const VAR: &str = "RUSTAIN_19_14_CARD_ORDER_KEY";
        export(VAR, "never-sent");
        let fixture = PeerFixture::plaintext().await;
        // A pin whose key cannot have signed the fixture's unsigned card.
        let pinned = crate::domain::models::PinnedKey::new(
            crate::domain::models::PinnedKeyAlgorithm::EdDsa,
            base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, [3u8; 32]),
            None,
        );
        let harness = Harness::compose(vec![
            peer("pinned", &fixture.origin)
                .with_auth(Some(VAR.to_owned()))
                .with_pinned_key(Some(pinned)),
        ])
        .await;

        let message = unreachable_cause("pinned", harness.send("pinned", "hello").await);
        unexport(VAR);

        let requests = fixture.requests().await;
        assert!(
            requests
                .iter()
                .any(|request| request.path == "/.well-known/agent-card.json"),
            "the boot card GET must have happened for this order to mean anything"
        );
        assert!(
            requests.iter().all(|request| request.api_key.is_none()),
            "⛔ the unauthenticated card GET must carry no credential"
        );
        assert!(
            fixture.posts().await.is_empty(),
            "⛔ zero POSTs: an unverified card producer stops the send before the \
             credential could be presented"
        );
        assert!(
            message.contains("is configured but its AgentCard is not cached"),
            "a JWS pin failure is not an anchor failure and keeps CardNotCached: {message}"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
        assert_eq!(harness.dispatched_rows().await, 0);
    }

    /// `AC2(f)` / `AC4(a′)`: a credentialed peer whose card names **another
    /// origin** — here the same host on a different port — gets no request at
    /// all. This is the leak `A29` closes: without origin binding the key
    /// follows the card.
    #[tokio::test]
    #[serial]
    async fn a_card_naming_another_origin_receives_no_request_and_refuses_by_name() {
        const VAR: &str = "RUSTAIN_19_14_OUT_OF_SCOPE_KEY";
        export(VAR, "must-not-travel");
        let roster = PeerFixture::plaintext().await;
        let elsewhere = PeerFixture::plaintext().await;
        roster
            .serve_endpoint(&format!("{}/", elsewhere.origin))
            .await;
        let harness = Harness::compose(vec![
            peer("wanderer", &roster.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        let message = unreachable_cause("wanderer", harness.send("wanderer", "hello").await);
        unexport(VAR);

        assert_eq!(
            message,
            format!(
                "wanderer's card sends requests to another host; its credential is only \
                 sent to {}",
                roster.origin
            )
        );
        assert_eq!(
            elsewhere.accepted_connections(),
            0,
            "⛔ the other origin must receive NOTHING — not even a connection"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one durable rejection row for the failed send"
        );
    }

    /// `AC2(f)` on the **model rail**: the origin check lives in `post_jsonrpc`,
    /// so `A2aProvider::invoke` inherits it. Placing the check in the
    /// JSON-RPC transport would leave this rail leaking.
    #[tokio::test]
    #[serial]
    async fn the_model_rail_also_refuses_a_card_that_names_another_origin() {
        const VAR: &str = "RUSTAIN_19_14_PROVIDER_SCOPE_KEY";
        export(VAR, "must-not-travel");
        let roster = PeerFixture::plaintext().await;
        let elsewhere = PeerFixture::plaintext().await;
        roster
            .serve_endpoint(&format!("{}/", elsewhere.origin))
            .await;
        let harness = Harness::compose(vec![
            peer("modelpeer", &roster.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        let error = harness
            .egress
            .provider()
            .invoke(
                &CapabilityId {
                    protocol: "a2a".to_owned(),
                    server: "modelpeer".to_owned(),
                    tool: "scan".to_owned(),
                },
                serde_json::json!({ "message": "hello" }),
                CancellationToken::new(),
            )
            .await
            .expect_err("an out-of-scope endpoint must refuse on the model rail too")
            .to_string();
        unexport(VAR);

        assert!(
            error.contains("modelpeer's card sends requests to another host"),
            "the provider rail surfaces the same typed refusal: {error}"
        );
        assert_eq!(
            elsewhere.accepted_connections(),
            0,
            "⛔ zero requests on the other origin, on this rail too"
        );
    }

    /// `AC4(a)` form 1, `AC4(e)`, and `AC4(g)`: a peer whose variable is absent
    /// refuses locally with no I/O and leaves exactly one rejection row.
    #[tokio::test]
    #[serial]
    async fn a_missing_variable_refuses_by_name_without_any_request() {
        const VAR: &str = "RUSTAIN_19_14_MISSING_KEY";
        unexport(VAR);
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer("keyless", &fixture.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        let message = unreachable_cause("keyless", harness.send("keyless", "hello").await);

        assert_eq!(
            message,
            "no credential for keyless: set the env var named in its auth field"
        );
        assert!(
            !message.contains("401") && !message.contains(VAR),
            "⛔ no bare status, and ⛔ never the variable's name: {message}"
        );
        assert!(
            fixture.posts().await.is_empty(),
            "the refusal is local: ⛔ no connection is opened to discover it"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
    }

    /// `AC4(a)` form 2: an **auth-less** peer answered 401 gets the form that
    /// tells the operator to add the field. ⛔ Never `A2A peer returned HTTP 401`
    /// — the exact symptom `DF-18-8-A2A-CLIENT-CREDENTIAL` recorded.
    #[tokio::test]
    #[serial]
    async fn an_auth_less_peer_answered_401_is_told_to_add_an_auth_field() {
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![peer("unconfigured", &fixture.origin)]).await;
        fixture.answer_with(RpcAnswer::Status(401)).await;

        let message =
            unreachable_cause("unconfigured", harness.send("unconfigured", "hello").await);

        assert_eq!(
            message,
            "no credential configured for unconfigured: add an auth field naming the \
             env var that holds its key"
        );
        assert!(
            !message.contains("401") && !message.contains("FIXTURE-SERVER-TEXT"),
            "⛔ no bare status and ⛔ no server-supplied text: {message}"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
        assert_eq!(harness.dispatched_rows().await, 1);
    }

    /// `AC4(b)` form 3 + `AC4(f)`: a present credential the server rejects. The
    /// discarded 401 body carries a sentinel that must not appear anywhere.
    #[tokio::test]
    #[serial]
    async fn a_rejected_credential_says_so_without_the_status_or_the_server_text() {
        const VAR: &str = "RUSTAIN_19_14_REJECTED_KEY";
        export(VAR, "wrong-secret");
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer("picky", &fixture.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;
        fixture.answer_with(RpcAnswer::Status(403)).await;

        let message = unreachable_cause("picky", harness.send("picky", "hello").await);
        unexport(VAR);

        assert_eq!(message, "picky rejected this credential");
        assert!(
            !message.contains("403") && !message.contains("FIXTURE-SERVER-TEXT"),
            "⛔ never the status, ⛔ never the peer's own words: {message}"
        );
        assert!(
            harness
                .rejected_rows()
                .await
                .iter()
                .all(|row| !row.contains("FIXTURE-SERVER-TEXT")),
            "the journal detail must not carry server text either"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
        assert_eq!(harness.dispatched_rows().await, 1);
    }

    /// `AC4(d)` precedence, fixture (i): an anchor failure **and** an unset
    /// variable are both present; the anchor is reported. Reversing the order —
    /// checking the variable before the retained boot cause — renders form 1
    /// instead. `AC4(g)`: the retained-cause path writes exactly one row and
    /// ⛔ no `Dispatched`.
    #[tokio::test]
    #[serial]
    async fn an_anchor_failure_outranks_a_missing_credential_and_writes_one_row() {
        const VAR: &str = "RUSTAIN_19_14_PRECEDENCE_KEY";
        unexport(VAR);
        let ca = test_ca("19-14 precedence CA", Validity::Current);
        let other = test_ca("19-14 precedence other CA", Validity::Current);
        let leaf = leaf_issued_by(
            &other,
            "19-14 precedence leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);
        let harness = Harness::compose(vec![
            peer("both", &fixture.origin)
                .with_auth(Some(VAR.to_owned()))
                .with_ca_cert(Some(anchor)),
        ])
        .await;

        let message = unreachable_cause("both", harness.send("both", "hello").await);

        assert_eq!(
            message, "both's certificate does not match the pinned anchor",
            "the FIRST observed cause wins — the anchor, not the credential"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one RemoteEnvelopeRejected row for the failed send"
        );
        assert_eq!(
            harness.dispatched_rows().await,
            0,
            "⛔ the retained-boot-cause path writes NO Dispatched row: nothing was \
             dispatched"
        );
        assert!(
            harness.rejected_rows().await[0].contains("both's certificate does not match"),
            "the journal detail carries the typed refusal, sanitized and byte-capped: {:?}",
            harness.rejected_rows().await
        );
    }

    /// `AC4(c)` form 6 through the real surface (code review 2026-09-15): the
    /// retained boot cause renders as a typed outcome's cause line and writes
    /// exactly one rejection row with no Dispatched. `egress.rs`'s
    /// characterization module proves the slot and the variant; THIS harness
    /// proves the operator surface and the journal cardinality.
    #[tokio::test]
    #[serial]
    async fn an_expired_leaf_refuses_by_name_with_a_typed_cause_and_one_row() {
        let ca = test_ca("19-14 surface expired CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 surface expired leaf",
            "localhost",
            Validity::Expired,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);
        let harness = Harness::compose(vec![
            peer("stale", &fixture.origin).with_ca_cert(Some(anchor)),
        ])
        .await;

        let message = unreachable_cause("stale", harness.send("stale", "hello").await);

        assert_eq!(
            message,
            "stale's certificate has expired or is not yet valid"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one rejection row"
        );
        assert_eq!(
            harness.dispatched_rows().await,
            0,
            "⛔ the retained-boot-cause path writes NO Dispatched row"
        );
    }

    /// `AC4(c)` form 7 through the real surface.
    #[tokio::test]
    #[serial]
    async fn a_wrong_name_leaf_refuses_by_name_with_a_typed_cause_and_one_row() {
        let ca = test_ca("19-14 surface wrong-name CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 surface wrong-name leaf",
            "elsewhere.invalid",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);
        let harness = Harness::compose(vec![
            peer("misnamed", &fixture.origin).with_ca_cert(Some(anchor)),
        ])
        .await;

        let message = unreachable_cause("misnamed", harness.send("misnamed", "hello").await);

        assert_eq!(
            message,
            "misnamed's certificate is not valid for its roster address"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one rejection row"
        );
        assert_eq!(harness.dispatched_rows().await, 0);
    }

    /// `AC4(c)` form 8 through the real surface — the `CA:TRUE` self-signed
    /// certificate `openssl req -x509` produces by default.
    #[tokio::test]
    #[serial]
    async fn a_ca_true_anchor_refuses_by_name_with_a_typed_cause_and_one_row() {
        let leaf = self_signed_leaf(
            "19-14 surface ca-true leaf",
            "localhost",
            rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained),
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &leaf.cert_pem);
        let harness = Harness::compose(vec![
            peer("ca-true", &fixture.origin).with_ca_cert(Some(anchor)),
        ])
        .await;

        let message = unreachable_cause("ca-true", harness.send("ca-true", "hello").await);

        assert_eq!(
            message,
            "ca-true presents a CA certificate as its server certificate"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one rejection row"
        );
        assert_eq!(harness.dispatched_rows().await, 0);
    }

    /// `AC4(c′)` form 9 through the real surface: an unloadable anchor names
    /// `pem_tls`'s reason on the typed outcome's cause line, writes one rejection
    /// row, and the HTTPS fixture's accepted-connection count stays ZERO — no
    /// fallback client is ever built.
    #[tokio::test]
    #[serial]
    async fn an_unloadable_anchor_refuses_by_name_with_a_typed_cause_without_a_socket() {
        let ca = test_ca("19-14 surface unloadable CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 surface unloadable leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "key-only.pem", &leaf.key_pem);
        let harness = Harness::compose(vec![
            peer("unloadable", &fixture.origin).with_ca_cert(Some(anchor)),
        ])
        .await;

        let message = unreachable_cause("unloadable", harness.send("unloadable", "hello").await);

        assert!(
            message.starts_with("unloadable's pinned anchor could not be loaded: ")
                && message.contains("contains no CERTIFICATE block"),
            "form 9 with pem_tls's own reason: {message}"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one rejection row"
        );
        assert_eq!(harness.dispatched_rows().await, 0);
        assert_eq!(
            fixture.accepted_connections(),
            0,
            "⛔ no client is built when the anchor cannot load"
        );
    }

    /// `AC4(c)`'s rotated-certificate half through the real surface: the boot
    /// GET validates, the fixture swaps to a leaf under a different CA, and the
    /// RPC's own handshake is classified — the refusal arrives through
    /// `DelegationError::Transport` and STILL loses its wrapper prefix. The
    /// journal keeps the driver's existing shape: one Dispatched, one Rejected
    /// (`A28` item 4 — the attempt is real).
    #[tokio::test]
    #[serial]
    async fn a_rotated_certificate_refuses_by_name_on_the_rpc_through_the_front_door() {
        let ca = test_ca("19-14 surface rotation CA", Validity::Current);
        let leaf = leaf_issued_by(
            &ca,
            "19-14 surface rotation leaf",
            "localhost",
            Validity::Current,
        );
        let fixture = PeerFixture::tls(&leaf).await;
        let dir = tempfile::tempdir().expect("anchor dir");
        let anchor = write_pem(dir.path(), "ca.pem", &ca.anchor_pem);
        let harness = Harness::compose(vec![
            peer("rotated", &fixture.origin).with_ca_cert(Some(anchor)),
        ])
        .await;

        let rogue = test_ca("19-14 surface rotation rogue CA", Validity::Current);
        let rotated: TestLeaf = leaf_issued_by(
            &rogue,
            "19-14 surface rotation rogue leaf",
            "localhost",
            Validity::Current,
        );
        fixture.rotate(&rotated).await;
        fixture.answer_with(RpcAnswer::Completed).await;

        let message = unreachable_cause("rotated", harness.send("rotated", "hello").await);

        assert_eq!(
            message, "rotated's certificate does not match the pinned anchor",
            "the RPC handshake is classified and rendered without the transport prefix"
        );
        assert_eq!(
            harness.rejected_rows().await.len(),
            1,
            "exactly one rejection row"
        );
        assert_eq!(
            harness.dispatched_rows().await,
            1,
            "the RPC was genuinely dispatched — the handshake failed mid-flight"
        );
    }

    /// Code review 2026-09-15: an `auth` value that is not a legal variable name
    /// (`=` / NUL — the `KEY=value` paste) must refuse as form 1 instead of
    /// panicking inside the spawned delegation task.
    #[tokio::test]
    #[serial]
    async fn an_auth_value_that_is_not_a_variable_name_refuses_as_form_1() {
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer("badname", &fixture.origin).with_auth(Some("KEY=value".to_owned())),
        ])
        .await;

        let message = unreachable_cause("badname", harness.send("badname", "hello").await);

        assert_eq!(
            message,
            "no credential for badname: set the env var named in its auth field"
        );
        assert!(
            fixture.posts().await.is_empty(),
            "the refusal is local: ⛔ no connection is opened"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
        assert_eq!(harness.dispatched_rows().await, 0);
    }

    /// `AC4(d)` precedence, fixture (ii): an out-of-scope endpoint **and** an
    /// unset variable; form 4 outranks form 1. Checking credential presence
    /// before the origin renders form 1 instead.
    #[tokio::test]
    #[serial]
    async fn an_out_of_scope_origin_outranks_a_missing_credential() {
        const VAR: &str = "RUSTAIN_19_14_SCOPE_PRECEDENCE_KEY";
        unexport(VAR);
        let roster = PeerFixture::plaintext().await;
        let elsewhere = PeerFixture::plaintext().await;
        roster
            .serve_endpoint(&format!("{}/", elsewhere.origin))
            .await;
        let harness = Harness::compose(vec![
            peer("scoped", &roster.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        let message = unreachable_cause("scoped", harness.send("scoped", "hello").await);
        assert_eq!(
            message,
            format!(
                "scoped's card sends requests to another host; its credential is only \
                 sent to {}",
                roster.origin
            ),
            "the origin is checked before the variable is read"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
        assert_eq!(harness.dispatched_rows().await, 0);
    }

    /// `AC2` positive control: an auth-less loopback peer sends no `x-api-key`
    /// and still succeeds — the shipped behaviour is untouched.
    #[tokio::test]
    #[serial]
    async fn an_auth_less_peer_sends_no_credential_and_still_succeeds() {
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![peer("plain", &fixture.origin)]).await;

        let outcome = harness.send("plain", "hello").await;
        assert_eq!(outcome, RecipientOutcome::AcceptedWithoutItem);
        assert_eq!(
            render_team_send_rows(&[TeamSendRow {
                alias: "plain".to_owned(),
                outcome: Some(outcome),
            }]),
            "    plain  accepted — this peer keeps no item id"
        );
        assert!(
            fixture.api_keys_seen().await.is_empty(),
            "⛔ an auth-less peer sends no credential"
        );
        assert_eq!(harness.dispatched_rows().await, 1);
    }

    /// `AC2` positive control: a credentialed peer whose card names an endpoint
    /// **on** the credential origin receives the header and succeeds.
    #[tokio::test]
    #[serial]
    async fn a_credentialed_send_to_the_roster_origin_carries_the_header_and_succeeds() {
        const VAR: &str = "RUSTAIN_19_14_HAPPY_KEY";
        export(VAR, "accepted-secret");
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer("welcome", &fixture.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;

        let outcome = harness.send("welcome", "hello").await;
        unexport(VAR);
        assert_eq!(outcome, RecipientOutcome::AcceptedWithoutItem);
        assert_eq!(
            render_team_send_rows(&[TeamSendRow {
                alias: "welcome".to_owned(),
                outcome: Some(outcome),
            }]),
            "    welcome  accepted — this peer keeps no item id"
        );
        assert_eq!(fixture.api_keys_seen().await, vec!["accepted-secret"]);
        assert_eq!(harness.dispatched_rows().await, 1);
    }

    /// `AC5`: with the secret set to a distinctive sentinel, **every** string
    /// the delivery path emits is scanned for its exact bytes — rendered rows,
    /// journal rows, and the **unfiltered** global log buffer.
    ///
    /// ⚠ The scan must read the buffer unfiltered: `tracing-test`'s
    /// `logs_contain` keeps only lines tagged with the test's own span, and the
    /// credential path runs in spawned tasks. The positive control below proves
    /// the scan can see the thread a mutant would write from.
    #[tokio::test]
    #[serial]
    #[traced_test]
    async fn the_secret_never_reaches_a_delivery_row_a_journal_row_or_a_log_line() {
        const VAR: &str = "RUSTAIN_19_14_SENTINEL_KEY";
        const SENTINEL: &str = "SENTINEL-4d5f6a7b-19-14";
        const ALIAS: &str = "sentinel-peer-19-14";
        export(VAR, SENTINEL);
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![
            peer(ALIAS, &fixture.origin).with_auth(Some(VAR.to_owned())),
        ])
        .await;
        // Drive both a success and a rejection: the secret is on the wire for
        // one and the refusal path runs for the other.
        let success = harness.send(ALIAS, "hello").await;
        fixture.answer_with(RpcAnswer::Status(401)).await;
        let refused = unreachable_cause(ALIAS, harness.send(ALIAS, "hello").await);
        unexport(VAR);

        assert_eq!(success, RecipientOutcome::AcceptedWithoutItem);
        let succeeded = render_team_send_rows(&[TeamSendRow {
            alias: ALIAS.to_owned(),
            outcome: Some(success),
        }]);
        assert_eq!(refused, format!("{ALIAS} rejected this credential"));
        tokio::spawn(async move {
            tracing::info!(peer = ALIAS, "fanout secret scan control");
        })
        .await
        .expect("control logging task");

        let logs = {
            let buffer = tracing_test::internal::global_buf()
                .lock()
                .expect("log buffer");
            String::from_utf8_lossy(&buffer).into_owned()
        };

        // The global log scan must observe a spawned task before checking
        // that no production path logged the credential.
        assert!(
            logs.lines()
                .any(|line| line.contains("fanout secret scan control") && line.contains(ALIAS)),
            "the unfiltered buffer must include a spawned task's log"
        );

        for surface in [succeeded.as_str(), refused.as_str(), logs.as_str()] {
            assert!(
                !surface.contains(SENTINEL),
                "⛔ the secret must not appear on any operator- or file-visible surface"
            );
        }
        // ⛔ EVERY journaled row, not only rejections: `rejected_rows()` filters
        // to `RemoteEnvelopeRejected`/`Policy` details, and a secret in a
        // Dispatched or terminal row would pass that scan (code review
        // 2026-09-15).
        for row in harness.journal.rows().await {
            let rendered = format!("{row:?}");
            assert!(
                !rendered.contains(SENTINEL),
                "⛔ the secret must not reach ANY journal row: {rendered}"
            );
        }
        assert!(
            !refused.contains(&SENTINEL.len().to_string()),
            "⛔ not the secret's length either"
        );
    }

    // ── Story 19.16f · the retract's outcome block, on scripted answers ─────
    //
    // The real axum server cannot be driven to answer `-32601` or `-32602`
    // for the verb it serves, nor a bare 502/401; those answers come from the
    // loopback fixture. Every other outcome is proven against the REAL server
    // in `tests/conformance_19_16f_retract_surface.rs`.

    fn retract_card(peer: &str, armed: bool) -> crate::adapters::tui::state::PendingTeamRetract {
        crate::adapters::tui::state::PendingTeamRetract {
            conversation_id: "conv".to_owned(),
            peer: peer.to_owned(),
            item_id: "ri_x".to_owned(),
            task: Some("task-x".to_owned()),
            card: "Retract on scripted's host".to_owned(),
            armed,
            prior_focus: crate::domain::models::FocusState::Chat,
        }
    }

    async fn scripted_retract(answer: RpcAnswer) -> (super::TeamRetractAnswer, Vec<String>, usize) {
        let fixture = PeerFixture::plaintext().await;
        fixture.answer_with(answer).await;
        let harness = Harness::compose(vec![peer("scripted", &fixture.origin)]).await;
        let answer =
            super::team_retract_dispatch(harness.egress.runtime(), &retract_card("scripted", true))
                .await;
        (
            answer,
            harness.rejected_rows().await,
            harness.dispatched_rows().await,
        )
    }

    /// Story 19.16f `AC10(c)` — every failure keeps its own cause: an old
    /// build is not "unreachable", and a malformed refusal is not "unknown".
    ///
    /// Mutant `M08` → RED: collapse every failure to one cause — the
    /// `-32601` answer stops rendering the old-build sentence.
    #[tokio::test]
    async fn an_old_build_and_a_malformed_refusal_each_render_their_own_sentence() {
        let (old_build, rejected, dispatched) = scripted_retract(RpcAnswer::JsonRpcError(
            crate::adapters::a2a::jsonrpc::CODE_METHOD_NOT_FOUND,
        ))
        .await;
        assert_eq!(
            old_build.message,
            "scripted runs a build without the retract verb. Nothing was marked."
        );
        assert_eq!(dispatched, 1, "the dispatch row is durable before the POST");
        assert_eq!(
            rejected,
            vec!["item retract refused: the peer runs a build without the retract verb"],
            "a -32601 proves nothing was marked, so it is journaled as refused"
        );

        let (malformed, rejected, _) = scripted_retract(RpcAnswer::JsonRpcError(
            crate::adapters::a2a::jsonrpc::CODE_INVALID_PARAMS,
        ))
        .await;
        assert_eq!(
            malformed.message,
            "scripted's host refused the request as malformed. Nothing was marked."
        );
        assert_eq!(rejected.len(), 1);
    }

    /// Story 19.16f `AC5(a)` — ⛔ any non-2xx other than 401/403 is UNKNOWN: a
    /// reverse proxy's 502 can arrive after the peer journaled the mark, so
    /// it renders the unknown sentence and journals NO rejection row. The
    /// auth layer's 401 answers before `dispatch` runs: proven unmarked.
    #[tokio::test]
    async fn a_proxy_502_is_unknown_and_an_auth_layer_401_is_a_proven_refusal() {
        let (bad_gateway, rejected, dispatched) = scripted_retract(RpcAnswer::Status(502)).await;
        assert_eq!(
            bad_gateway.message,
            "No usable answer from scripted's host — whether the item was marked is unknown. \
             '/team board' shows its current mark."
        );
        assert_eq!(dispatched, 1);
        assert!(
            rejected.is_empty(),
            "a rejection for a write that may have landed is a false claim: {rejected:?}"
        );

        let (unauthorized, rejected, _) = scripted_retract(RpcAnswer::Status(401)).await;
        assert_eq!(
            unauthorized.message,
            "no credential configured for scripted: add an auth field naming the env var that \
             holds its key",
            "the ratified credential sentence, bare"
        );
        assert_eq!(rejected.len(), 1, "{rejected:?}");
    }

    /// Story 19.16f `F5` — a connect failure is an unreachable first answer
    /// without a trusted cause line; it still journals the attempted dispatch.
    #[tokio::test]
    async fn a_send_to_a_closed_port_settles_unreachable_without_a_cause_line() {
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![peer("gone", &fixture.origin)]).await;
        drop(fixture);

        let outcome = harness.send("gone", "hello").await;
        assert_eq!(outcome, RecipientOutcome::Unreachable { cause: None });
        assert_eq!(
            render_team_send_rows(&[TeamSendRow {
                alias: "gone".to_owned(),
                outcome: Some(outcome),
            }]),
            "  ⚠ gone  unreachable"
        );
        assert_eq!(harness.dispatched_rows().await, 1);
        assert_eq!(harness.rejected_rows().await.len(), 1);
    }

    /// Story 19.16f `AC10(b)` (`F8`) — accepting the card takes the slot and
    /// shows `  sending…` BEFORE the dispatch spawn; the answer then arrives
    /// as `TeamRetractAnswered`, which replaces the same stable block.
    #[tokio::test]
    async fn accepting_the_card_shows_sending_then_the_answer_replaces_it() {
        use crate::adapters::tui::handlers::team_command::{
            TEAM_RETRACT_BLOCK_ID, TEAM_RETRACT_SENDING,
        };

        let fixture = PeerFixture::plaintext().await;
        fixture
            .answer_with(RpcAnswer::JsonRpcError(
                crate::adapters::a2a::jsonrpc::CODE_METHOD_NOT_FOUND,
            ))
            .await;
        let harness = Harness::compose(vec![peer("scripted", &fixture.origin)]).await;
        let workspace = tempfile::tempdir().expect("workspace");
        let (mut app_state, mut domain_rx) = super::tests::bridge_app_state(workspace.path());
        app_state.a2a_send = Some(harness.egress.runtime().clone());

        let mut state = crate::adapters::tui::state::TuiState::new(120, 40);
        state.pending_team_retract = Some(retract_card("scripted", true));
        super::resolve_team_retract(&mut state, true, &app_state).await;
        assert!(
            state.pending_team_retract.is_none(),
            "the slot is taken: a second `y` finds nothing to confirm"
        );
        assert_eq!(
            state.feedback_blocks[TEAM_RETRACT_BLOCK_ID].message,
            TEAM_RETRACT_SENDING
        );

        let answered = tokio::time::timeout(SETTLE, async {
            loop {
                if let Some(AppEvent::TeamRetractAnswered { message, .. }) = domain_rx.recv().await
                {
                    break message;
                }
            }
        })
        .await
        .expect("the dispatch spawn publishes its answer");
        assert_eq!(
            answered,
            "scripted runs a build without the retract verb. Nothing was marked."
        );
    }

    /// Story 19.16f `AC10(c)` — `M17`'s positive control at the answer the
    /// real server cannot be driven to give for a listed item: `-32001` is the
    /// not-found sentence (≡ not yours), ⛔ never the tombstone's, and it is a
    /// proven refusal.
    #[tokio::test]
    async fn a_task_not_found_answer_renders_not_found_never_the_tombstone() {
        let (answer, rejected, _) = scripted_retract(RpcAnswer::JsonRpcError(
            crate::adapters::a2a::jsonrpc::CODE_TASK_NOT_FOUND,
        ))
        .await;
        assert_eq!(
            answer.message,
            "scripted's host has no item ri_x this host can address. Nothing was marked."
        );
        assert_eq!(
            rejected,
            vec!["item retract refused: the peer has no item this host can address"]
        );
    }
}

/// Story 19.16g keystones: the standalone log visits. Each enters through the
/// production effect shells the event loop dispatches to (`open_panel` for
/// `InputAction::OpenPanel(TransparencyLog)`, `team_command` for `/team`),
/// paints with the real widgets, and completes the frame with the same
/// `TuiState` hand-off `event_loop::render` performs after a successful draw
/// (`restore_unflushed_log_visits` before, `log_visits_presented` after). The
/// observer is the production `log_awareness_observer`.
#[cfg(test)]
mod log_awareness_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;
    use crate::adapters::tui::state::{LogAwareness, LogAwarenessView, TabRenderState};
    use crate::adapters::tui::widgets::chat_pane;
    use crate::domain::models::{Conversation, LogVisitCandidate, StreamingState};
    use crate::infrastructure::transparency_awareness::test_support::append_rows;
    use crate::infrastructure::transparency_awareness::{
        LogAwarenessObserver, SeenBoundary, SeenLoad, SeenStore,
    };

    struct Client {
        app_state: AppState,
        state: TuiState,
        observer: LogAwarenessObserver,
        conversation: Conversation,
        clock: Instant,
        _domain_rx: tokio::sync::mpsc::UnboundedReceiver<crate::domain::events::AppEvent>,
    }

    impl Client {
        fn launch(workspace: &std::path::Path) -> Self {
            let (mut app_state, domain_rx) = super::tests::bridge_app_state(workspace);
            app_state.transparency = Some(Arc::new(
                crate::infrastructure::transparency::TransparencyService::new(
                    Arc::new(
                        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                            workspace,
                        ),
                    ),
                    workspace.to_path_buf(),
                ),
            ));
            let observer = log_awareness_observer(&app_state);
            let mut conversation = Conversation::default();
            // Enough transcript that the chat can be scrolled off the bottom.
            for index in 0..60 {
                conversation
                    .messages
                    .push(crate::domain::models::ChatMessage {
                        id: format!("m{index}"),
                        role: crate::domain::models::MessageRole::System,
                        content: format!("transcript line {index}"),
                        content_blocks: vec![],
                        tool_calls: vec![],
                        created_at: 0,
                        token_count: None,
                        stop_reason: None,
                        synthetic: true,
                        images: vec![],
                        origin: crate::domain::models::ChannelKind::Terminal,
                        authorship: Default::default(),
                        retracted_at_ms: None,
                        history_source: Default::default(),
                    });
            }
            Self {
                app_state,
                state: TuiState::new(120, 40),
                observer,
                conversation,
                clock: Instant::now(),
                _domain_rx: domain_rx,
            }
        }

        /// One scheduled observation, driven to completion.
        async fn poll(&mut self) -> LogAwareness {
            self.clock += Duration::from_secs(1);
            self.observer
                .tick_at(self.clock, &mut self.state.log_awareness);
            self.observer.settle(&mut self.state.log_awareness).await;
            self.state.log_awareness.display
        }

        async fn slash(&mut self, arg: &str) {
            team_command(&mut self.state, "conv", Some(arg), &self.app_state).await;
        }

        /// A successful chat frame: `height` 0 is a zero-height chat pane;
        /// `scroll_offset > 0` reads away from the bottom, where feedback
        /// blocks render.
        fn draw_chat(&mut self, height: u16, scroll_offset: usize) {
            self.state.restore_unflushed_log_visits();
            let mut terminal = Terminal::new(TestBackend::new(100, height.max(1))).unwrap();
            let mut tab = TabRenderState::default();
            let tools = HashMap::new();
            let mut visible = Vec::new();
            let (state, conversation) = (&self.state, &self.conversation);
            terminal
                .draw(|frame| {
                    let area = Rect {
                        height,
                        ..frame.area()
                    };
                    visible = chat_pane::render_attached(
                        frame,
                        area,
                        conversation,
                        &StreamingState::default(),
                        scroll_offset,
                        scroll_offset == 0,
                        &state.theme,
                        &mut tab,
                        &tools,
                        &state.feedback_blocks,
                        state.active_feedback_id.as_deref(),
                    )
                    .visible_feedback_ids;
                })
                .unwrap();
            self.state.log_visits_presented(&visible);
        }

        /// A frame whose chat pane was painted but whose draw failed: the
        /// event loop's `render` returns before the hand-off.
        fn failed_draw(&mut self) {
            self.state.restore_unflushed_log_visits();
        }

        fn draw_panel(&mut self, height: u16) {
            self.paint_panel(height);
            self.state.log_visits_presented(&[]);
        }

        /// The panel painted by a frame whose draw then failed.
        fn paint_panel(&mut self, height: u16) {
            self.state.restore_unflushed_log_visits();
            let area = Rect::new(0, 0, 60, height);
            let mut buffer = Buffer::empty(area);
            crate::adapters::tui::widgets::transparency_panel::render(
                area,
                &mut buffer,
                &mut self.state.transparency_panel,
                self.state.sidebar_selected,
                &self.state.focus,
                &self.state.theme,
            );
        }
    }

    fn stored(workspace: &std::path::Path) -> SeenLoad {
        SeenStore::for_workspace(workspace).load()
    }

    fn valid(seen_seq: u64, reset_revision: u64) -> SeenLoad {
        SeenLoad::Valid(SeenBoundary {
            seen_seq,
            reset_revision,
        })
    }

    /// Story 19.16h — replace the journal with a shorter history of `rows`
    /// rows (the K11 fixture's replacement).
    async fn replace_journal(workspace: &std::path::Path, rows: usize) {
        let rooms = workspace.join(".rustain/rooms");
        for entry in std::fs::read_dir(&rooms).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        append_rows(workspace, rows).await;
    }

    /// Story 19.16h — another local client, with its own production
    /// observer, notices the shorter journal and confirms the reset under the
    /// preference lock. Returns the revision it persisted.
    async fn reset_by_another_client(workspace: &std::path::Path, rows: usize) -> u64 {
        replace_journal(workspace, rows).await;
        let mut view = LogAwarenessView::default();
        let mut other = LogAwarenessObserver::for_workspace(workspace);
        let t0 = Instant::now();
        for second in 0..2 {
            other.tick_at(t0 + Duration::from_secs(second), &mut view);
            other.settle(&mut view).await;
        }
        let SeenLoad::Valid(boundary) = stored(workspace) else {
            panic!("the other client left no valid preference");
        };
        assert_eq!(boundary.seen_seq, 0, "the other client confirmed a reset");
        assert_eq!(view.reset_revision, boundary.reset_revision);
        boundary.reset_revision
    }

    /// Story 19.16h — some fresh client reads and presents the whole current
    /// log after observing it, persisting its head as the boundary.
    async fn visit_elsewhere(workspace: &std::path::Path) {
        let mut other = Client::launch(workspace);
        other.poll().await;
        other.slash("log").await;
        other.draw_chat(20, 0);
        assert_eq!(other.poll().await, LogAwareness::Hidden);
    }

    /// Story 19.16h K03 — a reader whose read is overtaken by a reset: inside
    /// `load_entries` it replaces the journal with two rows and confirms the
    /// reset through the real `SeenStore`, then returns the OLD rows it had
    /// already read — rows whose maximum `seq` fits the new head.
    struct ResetDuringRead {
        workspace: std::path::PathBuf,
        old: Vec<crate::domain::models::JournalEntry>,
    }

    #[async_trait::async_trait]
    impl crate::domain::ports::RoomJournalReader for ResetDuringRead {
        async fn load_entries(
            &self,
        ) -> Result<Vec<crate::domain::models::JournalEntry>, crate::domain::ports::RoomJournalError>
        {
            replace_journal(&self.workspace, 2).await;
            let journal =
                crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                    &self.workspace,
                );
            let confirmed = SeenStore::for_workspace(&self.workspace)
                .confirm_reset(|| {
                    journal
                        .probe_blocking()
                        .map(|probe| probe.head)
                        .map_err(|error| error.to_string())
                })
                .unwrap();
            assert!(
                matches!(
                    confirmed,
                    crate::infrastructure::transparency_awareness::ResetConfirmation::Reset(_)
                ),
                "the reset really confirmed during the read: {confirmed:?}"
            );
            Ok(self.old.clone())
        }
    }

    #[tokio::test]
    async fn only_a_presented_log_visit_clears_awareness() {
        // K02 / M02.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 3).await;
        let mut client = Client::launch(workspace.path());
        assert_eq!(client.poll().await, LogAwareness::Unseen(3));

        // Unrelated input, density/tab-bar state and feedback dismissal do
        // not clear; redraws without a log visit do not clear.
        client.state.density_mode = crate::domain::models::visual::DensityMode::Monitor;
        client.slash("status").await;
        client
            .state
            .feedback_blocks
            .remove(crate::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID);
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(3));

        // `/team log` inserts the block — that alone is not a visit.
        client.slash("log").await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(3), "not at open");
        // Offscreen, zero-height, or a failed draw: still not presented.
        client.draw_chat(10, 200);
        client.draw_chat(0, 0);
        client.failed_draw();
        assert_eq!(client.poll().await, LogAwareness::Unseen(3));
        assert_eq!(stored(workspace.path()), SeenLoad::Missing);
        // Returning to that exact result presents it.
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 3,
                reset_revision: 0
            })
        );

        // The standalone chord: open, then a painted body, then the frame.
        append_rows(workspace.path(), 2).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));
        open_panel(&client.app_state, &mut client.state).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(2), "not at open");
        client.draw_panel(2); // borders only: no body space
        client.paint_panel(20); // painted, but the frame's draw failed
        client.failed_draw();
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));
        client.draw_panel(20);
        assert_eq!(client.poll().await, LogAwareness::Hidden);
    }

    #[tokio::test]
    async fn visit_cannot_consume_a_later_or_hidden_snapshot() {
        // K06 / M06.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 10).await;
        let mut client = Client::launch(workspace.path());
        client.poll().await;

        // Report A ends at 10; row 11 arrives — and is observed — before A
        // is presented.
        client.slash("log").await;
        append_rows(workspace.path(), 1).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(11));
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 10,
                reset_revision: 0
            }),
            "A's own boundary, never the observer's later head"
        );
        // Rerendering the still-open view cannot acknowledge row 11.
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));

        // Another client already visited through 12; A' (ending at 11)
        // presented later must not lower it.
        client.slash("log").await;
        append_rows(workspace.path(), 1).await;
        SeenStore::for_workspace(workspace.path())
            .commit_visits(
                &[crate::domain::models::LogVisitCandidate {
                    seen_through: 12,
                    reset_revision: 0,
                }],
                || Ok(12),
            )
            .unwrap();
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 12,
                reset_revision: 0
            })
        );

        append_rows(workspace.path(), 2).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));
        // A filtered command, and an invalid one, never clear.
        client.slash("log --filter=direction=inbound").await;
        client.draw_chat(20, 0);
        client.slash("log --filter=bogus=term").await;
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));
        // A replaced stable block discards the replaced view's boundary.
        client.slash("log").await;
        client.slash("log --filter=direction=inbound").await;
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));

        // An inactive tab keeps its pending visit with its own block
        // (`event_loop::save_active_tab` / `load_active_tab` move exactly
        // these two fields) and commits only once that tab presents it.
        client.slash("log").await;
        let parked_blocks = std::mem::take(&mut client.state.feedback_blocks);
        let parked_visit = client.state.pending_log_visit.take();
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Unseen(2));
        client.state.feedback_blocks = parked_blocks;
        client.state.pending_log_visit = parked_visit;
        client.draw_chat(20, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);

        // A search applied before the panel's first paint makes it a
        // filtered view: its candidate is discarded, even once cleared.
        append_rows(workspace.path(), 1).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));
        open_panel(&client.app_state, &mut client.state).await;
        client.state.transparency_panel.search = Some("zzz".to_owned());
        client.draw_panel(20);
        client.state.transparency_panel.search = None;
        client.draw_panel(20);
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));
        // Removing the filter and explicitly reopening is a visit.
        open_panel(&client.app_state, &mut client.state).await;
        client.draw_panel(20);
        assert_eq!(client.poll().await, LogAwareness::Hidden);
    }

    #[tokio::test]
    async fn log_options_do_not_forge_a_visit() {
        // K10 / M10.
        let workspace = tempfile::tempdir().unwrap();
        append_rows(workspace.path(), 25).await;
        let mut client = Client::launch(workspace.path());
        assert_eq!(client.poll().await, LogAwareness::Unseen(25));

        // Panel export alone never clears (nothing was presented).
        open_panel(&client.app_state, &mut client.state).await;
        export_command(&client.app_state, &mut client.state, "conv").await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(25));
        assert!(
            crate::infrastructure::paths::transparency_export_path(workspace.path())
                .unwrap()
                .exists()
        );
        client.state.transparency_panel.pending_visit = None;

        // A capped in-chat visit keeps its explicit truncation and clears
        // the whole presented snapshot's reminder.
        client.slash("log").await;
        let block = &client.state.feedback_blocks
            [crate::adapters::tui::handlers::team_command::TEAM_LOG_BLOCK_ID];
        assert!(
            block.message.contains("most recent of 25 rows"),
            "{}",
            block.message
        );
        assert!(block.message.contains("Ctrl+X, L"), "{}", block.message);
        client.draw_chat(40, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);

        // `--json`, unfiltered and presented: the same boundary.
        append_rows(workspace.path(), 1).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));
        client.slash("log --json").await;
        client.draw_chat(40, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);

        // `--export` whose export FAILS still presented its rows.
        append_rows(workspace.path(), 1).await;
        assert_eq!(client.poll().await, LogAwareness::Unseen(1));
        let export =
            crate::infrastructure::paths::transparency_export_path(workspace.path()).unwrap();
        std::fs::remove_file(&export).unwrap();
        std::fs::create_dir(&export).unwrap();
        client.slash("log --export").await;
        client.draw_chat(40, 0);
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(
            stored(workspace.path()),
            SeenLoad::Valid(SeenBoundary {
                seen_seq: 27,
                reset_revision: 0
            })
        );
    }

    #[tokio::test]
    async fn a_stale_revision_visit_from_the_current_journal_still_clears() {
        // Story 19.16h K01 / M01: the durable revision, loaded before the
        // read, binds the visit — never the observer's lagging projection.
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        append_rows(ws, 5).await;
        visit_elsewhere(ws).await;
        assert_eq!(stored(ws), valid(5, 0));
        assert_eq!(reset_by_another_client(ws, 2).await, 1);

        // W1 — startup: the sidecar is at revision 1 and a fresh client
        // presents a visit before its observer completed any observation.
        // (a) standalone `/team log`.
        let mut client = Client::launch(ws);
        client.slash("log").await;
        client.draw_chat(20, 0);
        assert_eq!(client.observer.jobs_started(), 0, "not yet observed");
        assert_eq!(client.state.log_awareness.reset_revision, 0, "stale");
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(2, 1));
        // (b) the panel.
        append_rows(ws, 1).await;
        let mut client = Client::launch(ws);
        open_panel(&client.app_state, &mut client.state).await;
        client.draw_panel(20);
        assert_eq!(client.observer.jobs_started(), 0, "not yet observed");
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(3, 1));
        // (c) the attached client's local `/team log`.
        append_rows(ws, 1).await;
        let mut client = Client::launch(ws);
        let service = client.app_state.transparency.clone().unwrap();
        let notices = attached_team_log(&service, &mut client.state, &TeamLogArgs::default()).await;
        assert!(notices.is_empty(), "{notices:?}");
        client.draw_chat(20, 0);
        assert_eq!(client.observer.jobs_started(), 0, "not yet observed");
        assert_eq!(client.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(4, 1));

        // W2 — cross-client: A observes revision 1 and idles; B replaces the
        // journal and confirms revision 2; A reads and presents before its
        // next observation.
        let mut a = Client::launch(ws);
        assert_eq!(a.poll().await, LogAwareness::Hidden);
        append_rows(ws, 3).await;
        assert_eq!(a.poll().await, LogAwareness::Unseen(3));
        a.slash("log").await;
        a.draw_chat(20, 0);
        assert_eq!(a.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(7, 1));
        assert_eq!(reset_by_another_client(ws, 2).await, 2);
        assert_eq!(a.state.log_awareness.reset_revision, 1, "A lags B");
        a.slash("log").await;
        a.draw_chat(20, 0);
        assert_eq!(a.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(2, 2));
        assert_eq!(a.state.log_awareness.reset_revision, 2, "A adopted it");
        // Only genuinely newer rows count afterwards.
        append_rows(ws, 1).await;
        assert_eq!(a.poll().await, LogAwareness::Unseen(1));
        // The same window on the panel.
        assert_eq!(reset_by_another_client(ws, 1).await, 3);
        assert_eq!(a.state.log_awareness.reset_revision, 2, "A lags B");
        open_panel(&a.app_state, &mut a.state).await;
        a.draw_panel(20);
        assert_eq!(a.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(1, 3));
    }

    #[tokio::test]
    async fn a_candidate_bound_before_a_reset_never_clears_after_it() {
        // Story 19.16h K02 / M03 — the K11 counterexample: candidates read
        // against the OLD journal at head 2, the boundary later raised to
        // 5, the journal replaced by 2 rows and the reset confirmed. Each
        // candidate's `seen_through` fits the new head; none may clear.
        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        append_rows(ws, 2).await;
        let mut a = Client::launch(ws);
        assert_eq!(a.poll().await, LogAwareness::Unseen(2));
        let old = LogVisitCandidate {
            seen_through: 2,
            reset_revision: 0,
        };

        // (1) A `team-log` visit parked with its tab (the two fields
        // `event_loop::save_active_tab` moves).
        a.slash("log").await;
        let parked_blocks = std::mem::take(&mut a.state.feedback_blocks);
        let parked_visit = a.state.pending_log_visit.take();
        assert_eq!(parked_visit, Some(old));
        // (2) A panel visit parked because the panel had no body space.
        open_panel(&a.app_state, &mut a.state).await;
        a.draw_panel(2);
        assert_eq!(a.state.transparency_panel.pending_visit, Some(old));
        // (3) An observer visit retained across a failed save: the durable
        // head cannot be read under the preference lock.
        a.slash("log").await;
        let journal = ws.join(".rustain/rooms").join(format!(
            "room-{}.jsonl",
            crate::infrastructure::paths::workspace_hash(ws)
        ));
        let moved = journal.with_extension("moved");
        std::fs::rename(&journal, &moved).unwrap();
        std::fs::create_dir(&journal).unwrap();
        a.draw_chat(20, 0);
        assert_eq!(a.poll().await, LogAwareness::Unavailable);
        assert_eq!(stored(ws), SeenLoad::Missing, "the save failed");
        std::fs::remove_dir(&journal).unwrap();
        std::fs::rename(&moved, &journal).unwrap();

        // The journal grows to 5, another client's visit stores 5, then the
        // journal is replaced by 2 rows and another client confirms.
        append_rows(ws, 3).await;
        visit_elsewhere(ws).await;
        assert_eq!(stored(ws), valid(5, 0));
        assert_eq!(reset_by_another_client(ws, 2).await, 1);

        // (3) is retried first: `Stale`, drained, nothing persisted.
        assert_eq!(a.poll().await, LogAwareness::Unseen(2));
        assert_eq!(stored(ws), valid(0, 1));
        // (1) The tab comes back and presents its block.
        a.state.feedback_blocks = parked_blocks;
        a.state.pending_log_visit = parked_visit;
        a.draw_chat(20, 0);
        assert_eq!(a.poll().await, LogAwareness::Unseen(2));
        assert_eq!(stored(ws), valid(0, 1));
        // (2) The panel finally gets body space.
        a.draw_panel(20);
        assert_eq!(a.poll().await, LogAwareness::Unseen(2));
        assert_eq!(stored(ws), valid(0, 1));

        // Positive control: a read made now matches the stored revision and
        // commits.
        a.slash("log").await;
        a.draw_chat(20, 0);
        assert_eq!(a.poll().await, LogAwareness::Hidden);
        assert_eq!(stored(ws), valid(2, 1));
    }

    #[tokio::test]
    async fn a_reset_confirmed_during_the_read_leaves_the_candidate_stale() {
        // Story 19.16h K03 / M02: the revision is loaded BEFORE the read, so
        // a reset confirmed while the read runs leaves the candidate on the
        // pre-read revision — never old rows labelled with the new one.
        use crate::domain::ports::RoomJournalReader as _;

        let workspace = tempfile::tempdir().unwrap();
        let ws = workspace.path();
        append_rows(ws, 2).await;
        let old =
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                ws,
            )
            .load_entries()
            .await
            .unwrap();
        let reader = Arc::new(ResetDuringRead {
            workspace: ws.to_path_buf(),
            old,
        });

        let mut revision = 0;
        for surface in ["team log", "panel"] {
            append_rows(ws, 3).await;
            visit_elsewhere(ws).await;
            let SeenLoad::Valid(before) = stored(ws) else {
                panic!("{surface}: no stored boundary");
            };
            assert_eq!(before.reset_revision, revision, "{surface}");
            assert!(before.seen_seq > 2, "{surface}: a reset can confirm");

            let mut client = Client::launch(ws);
            client.app_state.transparency = Some(Arc::new(
                crate::infrastructure::transparency::TransparencyService::new(
                    reader.clone(),
                    ws.to_path_buf(),
                ),
            ));
            assert_eq!(client.poll().await, LogAwareness::Hidden, "{surface}");
            if surface == "panel" {
                open_panel(&client.app_state, &mut client.state).await;
                client.draw_panel(20);
            } else {
                client.slash("log").await;
                client.draw_chat(20, 0);
            }
            revision += 1;
            assert_eq!(client.poll().await, LogAwareness::Unseen(2), "{surface}");
            assert_eq!(
                stored(ws),
                valid(0, revision),
                "{surface}: nothing persisted"
            );
        }
    }
}
