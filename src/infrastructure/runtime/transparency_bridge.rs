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

use crate::adapters::tui::handlers::team_command::{TeamLogArgs, TeamLogInput};
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
        return;
    };
    match service.report().await {
        Ok(report) => {
            let now = chrono::Utc::now().timestamp_millis();
            state.transparency_panel.apply_report(report, now);
        }
        Err(error) => state.transparency_panel.error = Some(error.to_string()),
    }
    state.sidebar_entry_count = state.transparency_panel.visible_rows().len();
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
    let Some(service) = app_state.transparency.as_ref() else {
        return TeamLogInput {
            rows: Err(NO_JOURNAL.to_owned()),
            divergence: None,
            export: None,
        };
    };
    let report = match service.report().await {
        Ok(report) => report,
        Err(error) => {
            return TeamLogInput {
                rows: Err(error.to_string()),
                divergence: None,
                export: None,
            };
        }
    };
    let divergence = report.structural_divergence_report();
    let rows = match filter_rows(&report.rows, args.filter.as_deref()) {
        Ok(rows) => rows,
        Err(error) => {
            return TeamLogInput {
                rows: Err(error),
                divergence,
                export: None,
            };
        }
    };
    // The export renders the WHOLE supplied report, never the filtered view:
    // a filtered export would be a different file every time and would stop
    // being byte-identically regenerable.
    let export = if args.export {
        Some(export_report(app_state, &report).await)
    } else {
        None
    };
    TeamLogInput {
        rows: Ok(rows),
        divergence,
        export,
    }
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
pub(crate) fn consent_card_text(sender: &crate::domain::models::PeerId) -> String {
    let doctrine = crate::domain::services::peer_admission::ROTATED_KEY_DOCTRINE;
    format!(
        "Consent required — {sender}\n\
         No standing consent is recorded for this sender.\n\
         [y] Allow once  [a] Always allow  [n] Decline\n\
         [a] trusts the key this sender presents, not the person; {doctrine}.\n\
         Awaiting your decision.\n\
         Logged immediately for safety; interruption timing follows your policy."
    )
}

pub(crate) async fn team_command(
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
        TeamCommandArgs::Send { peer, text } => {
            #[cfg(feature = "a2a")]
            {
                let Some(runtime) = app_state.a2a_send.clone() else {
                    emit_team_warning(
                        state,
                        conversation_id,
                        app_state,
                        "A2A send runtime is not configured for this session.".to_owned(),
                    );
                    return;
                };
                let event_bus = app_state.event_bus.clone();
                let conversation_id = conversation_id.to_owned();
                let cancel = app_state.session_cancel.child_token();
                tokio::spawn(async move {
                    let result =
                        crate::adapters::a2a::send::send_text(&runtime, &peer, &text, cancel).await;
                    let event = team_send_event(&conversation_id, result);
                    let _ = event_bus.emit_domain(event);
                });
            }
            #[cfg(not(feature = "a2a"))]
            {
                let _ = (peer, text);
                emit_team_warning(
                    state,
                    conversation_id,
                    app_state,
                    handler::team_send_unavailable().to_owned(),
                );
            }
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

#[cfg(feature = "a2a")]
fn team_send_event(
    conversation_id: &str,
    result: Result<crate::adapters::a2a::send::SendOutcome, crate::adapters::a2a::send::SendError>,
) -> crate::domain::events::AppEvent {
    match result {
        Ok(outcome) => crate::adapters::tui::handlers::team_command::team_send(
            conversation_id,
            &outcome.peer,
            &outcome.task_id,
            &outcome.state,
            outcome.reply_text.as_deref(),
        ),
        Err(error) => crate::domain::events::AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            // Advisory, not Warning: refusals arrive from a background task
            // and must not abort an unrelated streaming turn (turn-fatal).
            level: crate::domain::models::NoticeLevel::Advisory,
            message: team_send_refusal(&error),
        },
    }
}

/// Render one send refusal.
///
/// Story 19.14 (`A5`): a credential or anchor refusal renders **exactly** its
/// ratified sentence. Matched on the **variant** — ⛔ never by string-matching a
/// `Display`, and ⛔ never with `SendError::Delegation`'s
/// `A2A send to peer …: A2A transport failure:` prefix in front of it, which is
/// what `FR166` forbids and what `A2A peer returned HTTP 401` used to be.
///
/// Story 19.15 shares this arm and owns the single peer-text sanitize point
/// (`AD-1824`); every string below is locally minted, so ⛔ no second sanitize
/// point is added here.
#[cfg(feature = "a2a")]
pub(crate) fn team_send_refusal(error: &crate::adapters::a2a::send::SendError) -> String {
    use crate::adapters::a2a::driver::DelegationError;
    use crate::adapters::a2a::error::A2aError;
    use crate::adapters::a2a::send::SendError;

    match error {
        SendError::AnchorRefused { .. } => error.to_string(),
        SendError::Delegation {
            source: DelegationError::Transport(transport),
            ..
        } => match transport {
            A2aError::CredentialMissing { .. }
            | A2aError::CredentialRejected { .. }
            | A2aError::CredentialOutOfScope { .. }
            | A2aError::AnchorValidationFailed { .. }
            | A2aError::CaCertUnloadable { .. } => transport.to_string(),
            _ => error.to_string(),
        },
        _ => error.to_string(),
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
    use crate::domain::ports::ConsentProjectionQuery;

    let sender = crate::adapters::tui::handlers::team_command::resolve_peer_target(target, peers)?;
    let projection = crate::adapters::policy::JournalConsentProjection::load_workspace(workspace)
        .await
        .map_err(|error| error.to_string())?;
    let current = projection.consent_for(&sender);
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
        assert_eq!(projection.consent_for(&sender), ConsentState::Trusted);

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
        assert_eq!(projection.consent_for(&sender), ConsentState::Revoked);
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
    #[cfg(feature = "a2a")]
    #[test]
    fn send_completion_and_input_required_use_tainted_feedback_events() {
        let success = team_send_event(
            "conv",
            Ok(crate::adapters::a2a::send::SendOutcome {
                peer: "moon".to_owned(),
                task_id: "peer-task-42".to_owned(),
                state: "completed".to_owned(),
                reply_text: Some("peer answer".to_owned()),
            }),
        );
        let crate::domain::events::AppEvent::SystemNotice { level, message, .. } = success else {
            panic!("send completion must use the feedback event path");
        };
        // Advisory: a late peer reply must not abort an unrelated turn.
        assert!(matches!(
            level,
            crate::domain::models::NoticeLevel::Advisory
        ));
        assert!(!level.is_turn_fatal());
        assert_eq!(
            message,
            "[peer: moon] task peer-task-42 — completed\npeer answer"
        );

        let input_required = team_send_event(
            "conv",
            Err(crate::adapters::a2a::send::SendError::InputRequired {
                peer: "moon".to_owned(),
                task_id: "peer-task-43".to_owned(),
            }),
        );
        let crate::domain::events::AppEvent::SystemNotice { message, .. } = input_required else {
            panic!("input-required must use the feedback event path");
        };
        assert_eq!(
            message,
            "peer `moon` asked a question this verb cannot answer (task `peer-task-43` \
             cancelled) — multi-turn arrives with 19.18"
        );
    }
}

/// Story 19.14 `AC2`/`AC4`/`AC5` — the credential, the typed refusals, and the
/// secret's absence, through the `/team send` front door.
///
/// Front door: `A2aEgress::compose` → `egress.runtime()` → `send::send_text` →
/// `team_send_event` — the calls `team_command`'s send arm makes, minus its
/// `TuiState`/`AppState` shell (the nearest test-visible production seam;
/// `team_command` is `pub(crate)` and `team_send_event` private).
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
    use crate::adapters::a2a::send::send_text;
    use crate::adapters::a2a::test_fixtures::{
        PeerFixture, RpcAnswer, TestLeaf, Validity, leaf_issued_by, self_signed_leaf, test_ca,
        write_pem,
    };
    use crate::domain::events::AppEvent;
    use crate::domain::models::{
        A2aPeerSource, A2aPeerSpec, CapabilityId, RedactedUrl, RejectReason, RoomEvent,
    };
    use crate::domain::ports::{CapabilityProvider, RoomJournal, RoomJournalError};
    use crate::infrastructure::subagent::NodeTree;

    use super::{team_send_event, team_send_refusal};

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
            let egress = A2aEgress::compose(
                peers,
                NodeTree::new(),
                journal.clone() as Arc<dyn RoomJournal>,
                event_tx,
            )
            .expect("compose must survive an unset variable and an unloadable anchor");
            for (_, client) in egress.provider().peer_bindings().iter() {
                tokio::time::timeout(SETTLE, client.await_settled())
                    .await
                    .expect("boot card fetch settled");
            }
            Self { egress, journal }
        }

        /// The whole front door: send, then render the way production renders.
        async fn send(&self, alias: &str, text: &str) -> AppEvent {
            let result =
                send_text(self.egress.runtime(), alias, text, CancellationToken::new()).await;
            team_send_event("conv", result)
        }

        async fn advisory(&self, alias: &str) -> String {
            let AppEvent::SystemNotice { level, message, .. } = self.send(alias, "hello").await
            else {
                panic!("a send outcome always arrives as a SystemNotice");
            };
            assert!(
                matches!(level, crate::domain::models::NoticeLevel::Advisory),
                "a refusal is an Advisory, ⛔ never a turn-fatal Warning"
            );
            message
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
        assert!(
            matches!(
                harness.send("rotator", "one").await,
                AppEvent::SystemNotice { .. }
            ),
            "the first send completes"
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

        let message = harness.advisory("pinned").await;
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

        let message = harness.advisory("wanderer").await;
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
    /// so `A2aProvider::invoke` inherits it. Placing the check in `send_text`
    /// would leave this rail leaking.
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

        let message = harness.advisory("keyless").await;

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

        let message = harness.advisory("unconfigured").await;

        assert_eq!(
            message,
            "no credential configured for unconfigured: add an auth field naming the \
             env var that holds its key"
        );
        assert!(
            !message.contains("401") && !message.contains("FIXTURE-SERVER-TEXT"),
            "⛔ no bare status and ⛔ no server-supplied text: {message}"
        );
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

        let message = harness.advisory("picky").await;
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
    }

    /// `AC4(d)` precedence, fixture (i): an anchor failure **and** an unset
    /// variable are both present; the anchor is reported. Reversing the order —
    /// checking the variable in `send_text` — renders form 1 instead.
    /// `AC4(g)`: the retained-cause path writes exactly one row and ⛔ no
    /// `Dispatched`.
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

        let message = harness.advisory("both").await;

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
    /// retained boot cause renders the ratified string on the Advisory **and**
    /// writes exactly one rejection row with no Dispatched. `egress.rs`'s
    /// characterization module proves the slot and the variant; THIS harness
    /// proves the operator surface and the journal cardinality.
    #[tokio::test]
    #[serial]
    async fn an_expired_leaf_refuses_by_name_on_the_advisory_with_one_row() {
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

        let message = harness.advisory("stale").await;

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
    async fn a_wrong_name_leaf_refuses_by_name_on_the_advisory_with_one_row() {
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

        let message = harness.advisory("misnamed").await;

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
    async fn a_ca_true_anchor_refuses_by_name_on_the_advisory_with_one_row() {
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

        let message = harness.advisory("ca-true").await;

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
    /// `pem_tls`'s reason on the Advisory, writes one rejection row, and the
    /// HTTPS fixture's accepted-connection count stays ZERO — no fallback client
    /// is ever built.
    #[tokio::test]
    #[serial]
    async fn an_unloadable_anchor_refuses_by_name_on_the_advisory_without_a_socket() {
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

        let message = harness.advisory("unloadable").await;

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

        let message = harness.advisory("rotated").await;

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

        let message = harness.advisory("badname").await;

        assert_eq!(
            message,
            "no credential for badname: set the env var named in its auth field"
        );
        assert!(
            fixture.posts().await.is_empty(),
            "the refusal is local: ⛔ no connection is opened"
        );
        assert_eq!(harness.rejected_rows().await.len(), 1);
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

        assert_eq!(
            harness.advisory("scoped").await,
            format!(
                "scoped's card sends requests to another host; its credential is only \
                 sent to {}",
                roster.origin
            ),
            "the origin is checked before the variable is read"
        );
    }

    /// `AC2` positive control: an auth-less loopback peer sends no `x-api-key`
    /// and still succeeds — the shipped behaviour is untouched.
    #[tokio::test]
    #[serial]
    async fn an_auth_less_peer_sends_no_credential_and_still_succeeds() {
        let fixture = PeerFixture::plaintext().await;
        let harness = Harness::compose(vec![peer("plain", &fixture.origin)]).await;

        let AppEvent::SystemNotice { message, .. } = harness.send("plain", "hello").await else {
            panic!("a send outcome always arrives as a SystemNotice");
        };

        assert!(
            message.contains("[peer: plain]") && message.contains("completed"),
            "the normal success row still renders: {message}"
        );
        assert!(
            fixture.api_keys_seen().await.is_empty(),
            "⛔ an auth-less peer sends no credential"
        );
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

        let AppEvent::SystemNotice { message, .. } = harness.send("welcome", "hello").await else {
            panic!("a send outcome always arrives as a SystemNotice");
        };
        unexport(VAR);

        assert!(
            message.contains("[peer: welcome]") && message.contains("completed"),
            "a credentialed send still renders its normal success row: {message}"
        );
        assert_eq!(fixture.api_keys_seen().await, vec!["accepted-secret"]);
    }

    /// `AC5`: with the secret set to a distinctive sentinel, **every** string the
    /// story's paths emit is scanned for its exact bytes — the Advisory, the
    /// journal rows, and the **unfiltered** global log buffer.
    ///
    /// ⚠ The scan must read the buffer unfiltered: `tracing-test`'s
    /// `logs_contain` keeps only lines tagged with the test's own span, and the
    /// credential path runs in spawned tasks. The positive control below proves
    /// the scan can see the thread a mutant would write from.
    #[tokio::test]
    #[serial]
    #[traced_test]
    async fn the_secret_never_reaches_an_advisory_a_journal_row_or_a_log_line() {
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
        // one and the refusal path runs for the other. ⛔ BOTH events are kept:
        // a leak into the SUCCESS notice must be caught too (code review
        // 2026-09-15 — the success event used to be discarded unscanned).
        let success = harness.send(ALIAS, "hello").await;
        fixture.answer_with(RpcAnswer::Status(401)).await;
        let refused = harness.advisory(ALIAS).await;
        unexport(VAR);

        let AppEvent::SystemNotice {
            message: succeeded, ..
        } = success
        else {
            panic!("the credentialed send to the roster origin must succeed");
        };
        assert_eq!(refused, format!("{ALIAS} rejected this credential"));

        let logs = {
            let buffer = tracing_test::internal::global_buf()
                .lock()
                .expect("log buffer");
            String::from_utf8_lossy(&buffer).into_owned()
        };

        // Positive control FIRST: if the scan cannot see the spawned delegation
        // task, the invariant below is green from birth.
        assert!(
            logs.lines()
                .any(|line| line.contains("dispatching A2A delegation") && line.contains(ALIAS)),
            "the unfiltered buffer must contain the SPAWNED task's own line, carrying \
             this test's unique alias — otherwise the sentinel scan proves nothing"
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

    /// `AC5` positive control on the refusal's usefulness: the form names the
    /// alias and the fix. Also `AC4(e)`'s exact-string guard against form 1
    /// being rendered for an auth-less peer.
    #[test]
    fn the_two_credential_missing_forms_are_selected_by_the_option_not_a_string() {
        use crate::adapters::a2a::error::A2aError;

        let named = A2aError::CredentialMissing {
            alias: "alpha".to_owned(),
            env_var: Some("ALPHA_KEY".to_owned()),
        };
        let unconfigured = A2aError::CredentialMissing {
            alias: "alpha".to_owned(),
            env_var: None,
        };
        assert_eq!(
            named.to_string(),
            "no credential for alpha: set the env var named in its auth field"
        );
        assert_eq!(
            unconfigured.to_string(),
            "no credential configured for alpha: add an auth field naming the env var \
             that holds its key"
        );
        assert!(
            !named.to_string().contains("ALPHA_KEY"),
            "⛔ no form interpolates the variable's name"
        );
    }

    /// `AC4(e)`: the render boundary matches on the **variant**. A refusal that
    /// travels inside `DelegationError::Transport` must lose that wrapper's
    /// prefix entirely.
    #[test]
    fn a_typed_refusal_renders_without_the_delegation_wrapper_prefix() {
        use crate::adapters::a2a::driver::DelegationError;
        use crate::adapters::a2a::error::A2aError;
        use crate::adapters::a2a::send::SendError;

        let rendered = team_send_refusal(&SendError::Delegation {
            peer: "beta".to_owned(),
            source: DelegationError::Transport(A2aError::CredentialRejected {
                alias: "beta".to_owned(),
            }),
        });
        assert_eq!(rendered, "beta rejected this credential");

        // A non-19.14 transport failure keeps today's rendering untouched.
        let untouched = team_send_refusal(&SendError::Delegation {
            peer: "beta".to_owned(),
            source: DelegationError::Transport(A2aError::HttpStatus { status: 503 }),
        });
        assert_eq!(
            untouched,
            "A2A send to peer `beta` failed: A2A transport failure: A2A peer returned HTTP 503"
        );
    }
}
