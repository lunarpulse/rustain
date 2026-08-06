//! Effect shell for `/artifacts`, `/artifact` and `Ctrl+X, E` (Story 18.3a-c,
//! AC3 / AC4 / AC5).
//!
//! A sibling of `room_bridge` for the same two reasons that module exists: keep
//! `event_loop.rs` inside its line budget, and keep `adapters/tui/handlers/`
//! free of `crate::infrastructure::*` imports. The handler parses and renders;
//! this performs the I/O.
//!
//! # The write authority is the journal, not a process
//!
//! `/artifact review` appends through [`PatchReviewRecorder`] — the narrow
//! domain port whose sole implementation wraps `PatchMergeBack::review`. That
//! is **durable-first, bus-second**: a journal failure returns `Err` and
//! nothing reaches the bus.
//!
//! ⛔ **Nothing here applies a patch.** No `git apply`, no working-tree
//! mutation, no file written outside `.rustain/`. Ruling A1: the verdict verb
//! records a review; the apply front door is `18-3a-d`.
//!
//! # Addressing goes through the projection
//!
//! Ruling P3. `PatchMergeBack::review` validates against the artifact *store*
//! only, and the room's `PatchReviewed` fold silently no-ops for an artifact it
//! has never seen — so a raw-id front door lets a verdict be journaled and then
//! vanish. Every id typed by the operator is resolved against
//! `room.artifacts()` and the **resolved** `ArtifactRef` is what reaches the
//! port.

use crate::adapters::tui::handlers::artifact_command::{
    self as handler, ArtifactCommandArgs, ResolveError,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{
    AgentId, ArtifactRef, OrchestrationRoom, PermissionMode, ReviewVerdict, RoomEditDecision,
    RoomEditKind, RoomEvent,
};
use crate::domain::ports::{PatchReviewError, PatchReviewRecorder, RoomJournalReader};
use crate::domain::services::patch_review::MergeBackPolicy;
use crate::domain::services::room_role::{local_room_role, room_edit_decision};
use crate::infrastructure::runtime::app_state::AppState;

/// Message shown when the workspace has no orchestration journal at all.
const NO_JOURNAL: &str =
    "this session has no orchestration journal (the subagent subsystem is not composed)";

/// Message shown when the journal exists but no verdict recorder is composed.
const NO_RECORDER: &str =
    "this session composed no merge-back service, so no verdict can be recorded";

/// The principal acting on `/artifact review`.
///
/// 🔴 **This is a SEAM, not enforcement, and saying so is load bearing (ruling
/// P2).** The function returns [`AgentId::local_operator`] unconditionally, so
/// [`local_room_role`] always answers `Owner` and
/// [`room_edit_decision`] is a **constant function** in this build. It ships
/// anyway, for the reason 18.3a shipped the identical `RoleAssignment` call:
/// it is the seam 18.4's role-parameterised projection needs, and adding it now
/// costs one line where finding every durable-write path later costs a survey.
///
/// ⛔ **Do not describe this as enforcement anywhere.** No behavioural test can
/// show a `Viewer` being refused on the production path, because no `Viewer`
/// principal can be constructed here; such a test would be a Rule-0 vacuous
/// keystone. The evidence is the structural routing ratchet in
/// `tests/conformance_18_3a_c_artifacts.rs`, plus a domain unit test that
/// drives the pure `room_edit_decision(Viewer, DurableContent)` case and is
/// labelled as covering the **core**, not the production path.
fn acting_principal() -> AgentId {
    AgentId::local_operator()
}

/// One-call shell for the `/artifacts` dispatch arm.
///
/// `pub` so the front-door keystone drives the exact callee the
/// `InputAction::ExecuteCommand` dispatch arm calls (event_loop.rs), with a
/// real `AppState` — the arm line itself stays pinned by the structural
/// ratchet.
pub async fn artifacts_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
    permission_mode: PermissionMode,
) {
    dispatch(
        state,
        conversation_id,
        true,
        cmd_arg,
        app_state,
        permission_mode,
    )
    .await;
}

/// One-call shell for the `/artifact` dispatch arm. `pub` for the same
/// front-door keystone reason as [`artifacts_command`].
pub async fn artifact_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
    permission_mode: PermissionMode,
) {
    dispatch(
        state,
        conversation_id,
        false,
        cmd_arg,
        app_state,
        permission_mode,
    )
    .await;
}

async fn dispatch(
    state: &mut TuiState,
    conversation_id: &str,
    bare_list: bool,
    cmd_arg: Option<&str>,
    app_state: &AppState,
    permission_mode: PermissionMode,
) {
    let command = match handler::parse_artifact_command(bare_list, cmd_arg) {
        Ok(command) => command,
        Err(message) => return warn(state, conversation_id, app_state, message),
    };
    match command {
        ArtifactCommandArgs::List => {
            match open_artifacts_view(app_state, state, permission_mode).await {
                Ok(()) => {}
                Err(message) => warn(state, conversation_id, app_state, message),
            }
        }
        ArtifactCommandArgs::Show { id } => {
            match show_artifact(app_state, &id, permission_mode).await {
                Ok(message) => handler::show_artifact_message(state, message),
                Err(message) => warn(state, conversation_id, app_state, message),
            }
        }
        ArtifactCommandArgs::Review { id, verdict } => {
            match review_artifact(app_state, &id, verdict, permission_mode).await {
                Ok((message, _event)) => {
                    handler::show_artifact_message(state, message);
                    // ⛔ NO bus emit here. `PatchMergeBack::persist` already
                    // emitted the journaled `PatchReviewed` — durable-first,
                    // bus-second — so re-emitting the returned event puts TWO
                    // copies of every verdict on the bus: daemon, wire-log and
                    // transcript subscribers would each see the review twice.
                    // `_event` is returned as evidence of what was appended,
                    // for the round-trip keystone, not for re-emission.
                    // Refold so the row's decision moves on the next paint
                    // without waiting for a reopen.
                    refresh_panel(app_state, state, permission_mode).await;
                }
                Err(message) => warn(state, conversation_id, app_state, message),
            }
        }
    }
}

fn activate_artifacts_sidebar(state: &mut TuiState) -> Result<(), String> {
    if state.terminal_width < crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH {
        return Err(format!(
            "Panel requires terminal width >= {} cols.",
            crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH
        ));
    }
    let panel = crate::domain::models::visual::PanelType::Artifacts;
    state.sidebar_visible = true;
    state.sidebar_panel = Some(panel);
    state.focus = crate::domain::models::FocusState::Sidebar {
        panel,
        selected: state.sidebar_selected,
    };
    state.needs_redraw = true;
    Ok(())
}

async fn open_artifacts_view(
    app_state: &AppState,
    state: &mut TuiState,
    permission_mode: PermissionMode,
) -> Result<(), String> {
    activate_artifacts_sidebar(state)?;
    refresh_panel(app_state, state, permission_mode).await;
    finalize_artifacts_panel(state);
    Ok(())
}

fn warn(state: &mut TuiState, conversation_id: &str, app_state: &AppState, message: String) {
    state.needs_redraw = true;
    let _ = app_state
        .event_bus
        .emit_domain(crate::domain::events::AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level: crate::domain::models::NoticeLevel::Warning,
            message,
        });
}

/// Open the artifact panel: fold the journal for THIS host, park the cursor,
/// and size the sidebar.
pub(crate) async fn open_panel(
    app_state: &AppState,
    state: &mut TuiState,
    permission_mode: PermissionMode,
) {
    refresh_panel(app_state, state, permission_mode).await;
    finalize_artifacts_panel(state);
}

fn finalize_artifacts_panel(state: &mut TuiState) {
    state.sidebar_entry_count = state.artifacts_panel.visible_len();
    state
        .artifacts_panel
        .synchronize_selection(&mut state.sidebar_selected);
    if matches!(
        state.focus,
        crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Artifacts,
            ..
        }
    ) {
        state.focus = crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Artifacts,
            selected: state.sidebar_selected,
        };
    }
}

/// Refresh the panel's fold from the durable journal.
///
/// **Never live.** One consistent read under a shared `flock`; the panel
/// renders "as of <time>" and takes no head poll
/// (`DF-18-3a-c-ARTIFACT-HEAD-POLL`).
pub(crate) async fn refresh_panel(
    app_state: &AppState,
    state: &mut TuiState,
    permission_mode: PermissionMode,
) {
    let workspace = &app_state.compose_snapshot.workspace_path;
    let host_id = crate::infrastructure::subagent::current_host_id(workspace);
    let policy = effective_policy(app_state);
    let Some(reader) = journal_reader(app_state) else {
        state.artifacts_panel.not_attached = true;
        state.artifacts_panel.error = None;
        state.artifacts_panel.host_id = host_id;
        return;
    };
    let entries = match reader.load_entries().await {
        Ok(entries) => entries,
        Err(error) => {
            state.artifacts_panel.error = Some(error.to_string());
            return;
        }
    };
    let (room, max_seq, unknown_records) =
        fold_artifacts_read(entries, room_id(workspace), &host_id);
    let now = chrono::Utc::now().timestamp_millis();
    state.artifacts_panel.apply_read(
        room,
        host_id,
        max_seq,
        unknown_records,
        now,
        policy,
        permission_mode,
        &mut state.sidebar_selected,
    );
    state.sidebar_entry_count = state.artifacts_panel.visible_len();
}

/// Fold one durable read into the panel's inputs: the host-honest room, the
/// high-water sequence, and the count of records this build cannot recognise.
///
/// `pub` so a keystone can enter here — this is the nearest test-visible seam
/// that is still **on** the production path ([`refresh_panel`] is its only
/// production caller), not a bypass beneath it.
#[must_use]
pub fn fold_artifacts_read(
    entries: Vec<crate::domain::models::JournalEntry>,
    room_id: crate::domain::models::OrchestrationRoomId,
    host_id: &str,
) -> (OrchestrationRoom, u64, usize) {
    let max_seq = entries.last().map(|entry| entry.seq).unwrap_or(0);
    let mut unknown_records = 0usize;
    let events: Vec<RoomEvent> = entries
        .into_iter()
        .filter_map(|entry| match entry.record {
            crate::domain::models::JournalRecord::Room(event) => {
                if event == RoomEvent::Unrecognized {
                    unknown_records += 1;
                }
                Some(event)
            }
            _ => None,
        })
        .collect();
    // `project_for_host`, never bare `project()`: the host-bound derivation is
    // what keeps a foreign-host artifact honest about where it lives.
    let room = OrchestrationRoom::project_for_host(room_id, events, host_id);
    (room, max_seq, unknown_records)
}

fn room_id(workspace: &std::path::Path) -> crate::domain::models::OrchestrationRoomId {
    crate::domain::models::OrchestrationRoomId::parse(format!(
        "room-{}",
        crate::infrastructure::paths::workspace_hash(workspace)
    ))
    .expect("workspace hash produces a valid room id")
}

/// The read side of the one journal, or `None` when none is composed.
fn journal_reader(app_state: &AppState) -> Option<std::sync::Arc<dyn RoomJournalReader>> {
    app_state.transparency.as_ref()?;
    Some(std::sync::Arc::new(
        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            &app_state.compose_snapshot.workspace_path,
        ),
    ))
}

/// The merge-back policy the apply path uses, or the fail-closed default when
/// no merge-back service is composed (in which case there are no patches to
/// describe either).
fn effective_policy(app_state: &AppState) -> MergeBackPolicy {
    app_state
        .patch_review
        .as_ref()
        .map(|recorder| recorder.effective_policy())
        .unwrap_or_default()
}

async fn load_room(app_state: &AppState) -> Result<OrchestrationRoom, String> {
    let workspace = &app_state.compose_snapshot.workspace_path;
    let host_id = crate::infrastructure::subagent::current_host_id(workspace);
    let reader = journal_reader(app_state).ok_or_else(|| NO_JOURNAL.to_owned())?;
    let entries = reader.load_entries().await.map_err(|e| e.to_string())?;
    Ok(fold_artifacts_read(entries, room_id(workspace), &host_id).0)
}

async fn show_artifact(
    app_state: &AppState,
    typed: &str,
    permission_mode: PermissionMode,
) -> Result<String, String> {
    let room = load_room(app_state).await?;
    let artifact = handler::resolve_artifact(&room, typed).map_err(|e| e.to_string())?;
    let policy = effective_policy(app_state);
    let body = match app_state.patch_review.as_ref() {
        Some(recorder) => recorder
            .body(&artifact.id)
            .await
            .map_err(|error| error.to_string()),
        None => Err(NO_RECORDER.to_owned()),
    };
    Ok(handler::render_show(
        artifact,
        &room,
        permission_mode,
        &policy,
        body.as_deref().map_err(String::clone),
    ))
}

/// Record one operator verdict durably.
///
/// Takes the [`AppState`] shell above it and its collaborators below, mirroring
/// `room_bridge::persist_room_role`: [`record_verdict`] is the seam the
/// round-trip keystone enters, and it is on the production path.
async fn review_artifact(
    app_state: &AppState,
    typed: &str,
    verdict: ReviewVerdict,
    permission_mode: PermissionMode,
) -> Result<(String, Option<RoomEvent>), String> {
    let room = load_room(app_state).await?;
    let recorder = app_state
        .patch_review
        .as_ref()
        .ok_or_else(|| NO_RECORDER.to_owned())?;
    record_verdict(
        &room,
        recorder.as_ref(),
        &acting_principal(),
        typed,
        verdict,
        permission_mode,
    )
    .await
}

/// The gated durable write, and the **first production caller** of
/// [`RoomEditKind::DurableContent`] in the codebase's history.
///
/// Returns the operator message plus the event that was actually appended.
/// The event is **evidence, not a re-emission source**: `PatchMergeBack::persist`
/// already emitted it to the bus after the journal accepted it (durable-first,
/// bus-second), and a caller that emits it again double-counts every verdict
/// for every bus subscriber.
///
/// `pub` so a keystone can enter here — this is the nearest test-visible seam
/// that is still **on** the production path ([`review_artifact`] is its only
/// production caller), not a bypass beneath it. ⛔ The forbidden bypass is
/// calling `PatchMergeBack::review` directly: that proves the service works,
/// not that the operator can reach it.
pub async fn record_verdict(
    room: &OrchestrationRoom,
    recorder: &dyn PatchReviewRecorder,
    acting: &AgentId,
    typed: &str,
    verdict: ReviewVerdict,
    _permission_mode: PermissionMode,
) -> Result<(String, Option<RoomEvent>), String> {
    // Ruling P3: resolve against the ROOM, never build an id from operator
    // text. `Unknown` covers both "no such artifact" and the silent-no-op case
    // where the artifact is in the store but absent from this room's fold.
    let artifact: &ArtifactRef = match handler::resolve_artifact(room, typed) {
        Ok(artifact) => artifact,
        Err(error @ (ResolveError::Unknown(_) | ResolveError::Ambiguous { .. })) => {
            return Err(error.to_string());
        }
    };

    // Ruling P2 — the room-content seam. A constant function in this build; the
    // structural ratchet, not a behavioural test, is its evidence.
    if room_edit_decision(local_room_role(acting), RoomEditKind::DurableContent)
        != RoomEditDecision::Allow
    {
        return Err(format!(
            "{} may not make durable room-content edits",
            acting.as_str()
        ));
    }

    let artifact = artifact.clone();
    let id = artifact.id.clone();
    match recorder
        .record_verdict(artifact, acting.clone(), verdict)
        .await
    {
        Ok(_) => Ok((
            handler::render_verdict_recorded(&id, verdict),
            Some(RoomEvent::PatchReviewed {
                artifact: id,
                reviewer: acting.clone(),
                verdict,
            }),
        )),
        Err(PatchReviewError::Refused(message)) => Err(message),
    }
}
