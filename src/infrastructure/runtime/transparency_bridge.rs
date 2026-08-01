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
        TeamCommandArgs::Log(args) => {
            let input = team_log_input(app_state, &args).await;
            for event in handler::team_command(state, conversation_id, &args, input) {
                let _ = app_state.event_bus.emit_domain(event);
            }
        }
        TeamCommandArgs::Trust(target) => {
            match change_sender_consent(app_state, &target, true).await {
                Ok(message) => handler::show_team_status(state, message),
                Err(message) => emit_team_warning(state, conversation_id, app_state, message),
            }
        }
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
}
