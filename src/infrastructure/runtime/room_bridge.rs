//! Effect shell for `/room` and `Ctrl+X, R` (Story 18.3a, AC1 / AC4).
//!
//! A sibling of `transparency_bridge` rather than more of it: that module is
//! already past the ~300-line split guidance, and the two surfaces share no
//! state. Same reason for existing, though — keep `event_loop.rs` inside
//! `EVENT_LOOP_HARD_BUDGET` and keep `adapters/tui/handlers/` free of
//! `crate::infrastructure::*` imports. The handler parses; this performs the
//! I/O.
//!
//! # The write authority is the journal, not a process
//!
//! `/room role grant|revoke` appends from the TUI side, exactly as
//! `transparency_bridge::persist_sender_consent` does for `/team trust`, and
//! the projection holder refolds on a high-water sequence compare. Both halves
//! ship together: an appending writer without a refolding holder is Story
//! 18.3d's shipped defect.
//!
//! Ordering is **durable-first, bus-second**. A journal failure returns `Err`
//! and nothing reaches the bus or the cached projection.

use crate::adapters::tui::handlers::room_command::{self as handler, RoomCommandArgs};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{
    A2aPeerSpec, AgentId, PeerId, RoomEditDecision, RoomEditKind, RoomEvent, RoomRole,
};
use crate::domain::ports::{RoomJournalReader, RoomRoleProjectionQuery, RoomRoleState};
use crate::domain::services::room_role::{local_room_role, room_edit_decision};
use crate::infrastructure::runtime::app_state::AppState;

/// Message shown when the workspace has no orchestration journal at all.
const NO_JOURNAL: &str =
    "this session has no orchestration journal (the subagent subsystem is not composed)";

/// Check the durable head once per second while the Room panel is open. This
/// is deliberately slower than the render tick and does not refold the
/// anchored viewport.
pub(crate) const ROOM_HEAD_POLL_INTERVAL_MS: u64 = 1_000;

/// The principal acting on `/room role`.
///
/// **Story 18.3a-b (AC2) retired 18.3a's placeholder role constant here** —
/// its name is deliberately not repeated, because a structural ratchet in
/// `tests/conformance_18_3a_b_addressing.rs` asserts this file no longer
/// mentions it at all. 18.3a asserted `RoomRole::Owner` about an actor it
/// could not name, because no local operator identity existed in the tree.
/// One now does:
/// [`AgentId::local_operator`] is a reserved, unforgeable address, and the
/// role is **derived** from it by
/// [`crate::domain::services::room_role::local_room_role`] rather than
/// asserted. The answer is still `Owner` — the human at this keyboard owns the
/// workspace, the journal file and the process — but it is now an answer about
/// a named principal, and a different principal reaching this path gets
/// `Viewer` and is refused.
///
/// [`room_edit_decision`] remains the only thing that turns a role into a
/// verdict.
fn acting_principal() -> AgentId {
    AgentId::local_operator()
}

/// One-call shell for the `/room` dispatch arm.
pub(crate) async fn room_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
) {
    let command = match handler::parse_room_command(cmd_arg) {
        Ok(command) => command,
        Err(message) => return warn(state, conversation_id, app_state, message),
    };
    match command {
        RoomCommandArgs::View => match open_room_view(app_state, state).await {
            Ok(()) => {}
            Err(message) => warn(state, conversation_id, app_state, message),
        },
        RoomCommandArgs::RoleList => match role_list(app_state).await {
            Ok(message) | Err(message) => handler::show_room_message(state, message),
        },
        RoomCommandArgs::RoleGrant { target, role } => {
            match change_room_role(app_state, &target, Some(role)).await {
                Ok(message) => handler::show_room_message(state, message),
                Err(message) => warn(state, conversation_id, app_state, message),
            }
        }
        RoomCommandArgs::RoleRevoke { target } => {
            match change_room_role(app_state, &target, None).await {
                Ok(message) => handler::show_room_message(state, message),
                Err(message) => warn(state, conversation_id, app_state, message),
            }
        }
    }
}

fn activate_room_sidebar(state: &mut TuiState) -> Result<(), String> {
    if state.terminal_width < crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH {
        return Err(format!(
            "Panel requires terminal width >= {} cols.",
            crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH
        ));
    }
    let panel = crate::domain::models::visual::PanelType::Room;
    state.sidebar_visible = true;
    state.sidebar_panel = Some(panel);
    state.focus = crate::domain::models::FocusState::Sidebar {
        panel,
        selected: state.sidebar_selected,
    };
    state.needs_redraw = true;
    Ok(())
}

async fn open_room_view(app_state: &AppState, state: &mut TuiState) -> Result<(), String> {
    open_room_view_with_source(
        journal_reader(app_state),
        &app_state.compose_snapshot.workspace_path,
        crate::infrastructure::subagent::current_host_id(
            &app_state.compose_snapshot.workspace_path,
        ),
        state,
    )
    .await
}

async fn open_room_view_with_source(
    reader: Option<std::sync::Arc<dyn crate::domain::ports::RoomJournalReader>>,
    workspace: &std::path::Path,
    host_id: String,
    state: &mut TuiState,
) -> Result<(), String> {
    activate_room_sidebar(state)?;
    refresh_panel_from_source(reader, workspace, host_id, state).await;
    finalize_room_panel(state);
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

/// Open the durable-room panel: fold the journal for THIS host, park the
/// cursor, and size the sidebar.
pub(crate) async fn open_panel(app_state: &AppState, state: &mut TuiState) {
    refresh_panel(app_state, state).await;
    finalize_room_panel(state);
}

fn finalize_room_panel(state: &mut TuiState) {
    state.sidebar_entry_count = state.room_panel.visible_len();
    state
        .room_panel
        .synchronize_selection(&mut state.sidebar_selected);
    if matches!(
        state.focus,
        crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Room,
            ..
        }
    ) {
        state.focus = crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Room,
            selected: state.sidebar_selected,
        };
    }
}

/// Refresh the panel's fold from the durable journal.
///
/// **Never live.** One consistent read under a shared `flock`; the panel
/// renders "as of <time>" and counts entries it has not folded.
pub(crate) async fn refresh_panel(app_state: &AppState, state: &mut TuiState) {
    let workspace = &app_state.compose_snapshot.workspace_path;
    refresh_panel_from_source(
        journal_reader(app_state),
        workspace,
        crate::infrastructure::subagent::current_host_id(workspace),
        state,
    )
    .await;
}

async fn refresh_panel_from_source(
    reader: Option<std::sync::Arc<dyn crate::domain::ports::RoomJournalReader>>,
    workspace: &std::path::Path,
    host_id: String,
    state: &mut TuiState,
) {
    let Some(reader) = reader else {
        state.room_panel.not_attached = true;
        state.room_panel.error = None;
        state.room_panel.host_id = host_id;
        return;
    };
    let entries = match reader.load_entries().await {
        Ok(entries) => entries,
        Err(error) => {
            state.room_panel.error = Some(error.to_string());
            return;
        }
    };
    let (room, max_seq, unknown_records) = fold_room_read(entries, room_id(workspace), &host_id);
    let now = chrono::Utc::now().timestamp_millis();
    state.room_panel.apply_read(
        room,
        host_id,
        max_seq,
        unknown_records,
        now,
        &mut state.sidebar_selected,
    );
    state.sidebar_entry_count = state.room_panel.visible_len();
}

/// Observe a newer durable head without replacing the reader's anchored fold.
pub(crate) async fn refresh_head(app_state: &AppState, state: &mut TuiState) {
    let Some(reader) = journal_reader(app_state) else {
        return;
    };
    let max_seq = match reader.latest_seq().await {
        Ok(max_seq) => max_seq,
        Err(error) => {
            state.room_panel.error = Some(error.to_string());
            state.needs_redraw = true;
            return;
        }
    };
    if state.room_panel.observe_head(max_seq) {
        state.needs_redraw = true;
    }
}

/// Fold one durable read into the panel's inputs: the host-honest room, the
/// high-water sequence, and the count of records this build cannot recognise.
///
/// `pub` so a keystone can enter here — this is the nearest test-visible seam
/// that is still **on** the production path ([`refresh_panel`] is its only
/// production caller), not a bypass beneath it.
#[must_use]
pub fn fold_room_read(
    entries: Vec<crate::domain::models::JournalEntry>,
    room_id: crate::domain::models::OrchestrationRoomId,
    host_id: &str,
) -> (crate::domain::models::OrchestrationRoom, u64, usize) {
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
    // what makes AC2's honesty possible.
    let room = crate::domain::models::OrchestrationRoom::project_for_host(room_id, events, host_id);
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

/// `/room role list` — refold first, so a grant appended by any writer shows.
async fn role_list(app_state: &AppState) -> Result<String, String> {
    render_role_list(
        &app_state.room_roles,
        &app_state.compose_snapshot.a2a_peers,
        app_state.transparency.is_some(),
    )
    .await
}

/// Refold, then render. Split from the `AppState` shell so the round-trip
/// keystone drives the real function against a real journal.
async fn render_role_list(
    projection: &crate::adapters::policy::JournalRoomRoleProjection,
    peers: &[A2aPeerSpec],
    journal_attached: bool,
) -> Result<String, String> {
    if !journal_attached {
        return Err(format!("Cannot list room roles: {NO_JOURNAL}."));
    }
    projection.refresh().await;
    let roles: Vec<(String, PeerId, RoomRoleState)> = projection
        .journaled_roles()
        .into_iter()
        .map(|(peer, state)| (peer_label(&peer, peers), peer, state))
        .collect();
    Ok(handler::render_role_list(
        &roles,
        projection.read_error().as_deref(),
    ))
}

/// Prefer the operator-facing alias; fall back to the stable identity. Never
/// fabricates a name for a peer that is not configured.
fn peer_label(peer: &PeerId, peers: &[A2aPeerSpec]) -> String {
    peers
        .iter()
        .find(|spec| spec.resolved_identity() == *peer)
        .map(|spec| spec.id.clone())
        .unwrap_or_else(|| peer.as_str().to_owned())
}

fn resolve_configured_peer_target(target: &str, peers: &[A2aPeerSpec]) -> Result<PeerId, String> {
    let parsed = PeerId::parse(target.to_owned()).ok();
    peers
        .iter()
        .find(|spec| {
            spec.id == target
                || parsed
                    .as_ref()
                    .is_some_and(|peer| spec.resolved_identity() == *peer)
        })
        .map(A2aPeerSpec::resolved_identity)
        .ok_or_else(|| {
            format!(
                "'{target}' is not a configured A2A peer. Add it to a2a.json before assigning \
                 a room role."
            )
        })
}

/// `/room role grant|revoke` through the `AppState` shell.
async fn change_room_role(
    app_state: &AppState,
    target: &str,
    role: Option<RoomRole>,
) -> Result<String, String> {
    if app_state.transparency.is_none() {
        return Err(format!("Cannot record a room role: {NO_JOURNAL}."));
    }
    let (message, event) = persist_room_role(
        &app_state.compose_snapshot.workspace_path,
        &app_state.compose_snapshot.a2a_peers,
        &app_state.room_roles,
        &acting_principal(),
        target,
        role,
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

/// Record one room-role change durably, and reflect it in the live projection.
///
/// The gated edit, and the **first and only production caller** of
/// [`room_edit_decision`] and of
/// [`crate::domain::services::room_role::local_room_role`]. `role: Some(_)`
/// grants, `None` revokes. Returns the operator message plus the event that was
/// actually appended — `None` when the act was a no-op, so the caller emits
/// nothing to the bus.
///
/// `acting` is a **principal**, not a role: the role is derived here so the
/// keystone that enters this seam exercises the derivation on the production
/// path (18.3a-b AC2).
///
/// Takes its collaborators explicitly rather than reaching into `AppState`,
/// mirroring `transparency_bridge::persist_sender_consent`: it is the seam the
/// round-trip keystone enters, and it is on the production path.
#[allow(clippy::too_many_arguments)]
async fn persist_room_role(
    workspace: &std::path::Path,
    peers: &[A2aPeerSpec],
    projection: &crate::adapters::policy::JournalRoomRoleProjection,
    acting: &AgentId,
    target: &str,
    role: Option<RoomRole>,
    now: i64,
) -> Result<(String, Option<RoomEvent>), String> {
    // The gate. Effect-free decision core, one call, before anything durable.
    // Two cores, one each: `local_room_role` answers *which role*,
    // `room_edit_decision` answers *may it*.
    if room_edit_decision(local_room_role(acting), RoomEditKind::RoleAssignment)
        != RoomEditDecision::Allow
    {
        return Err(
            "Room roles may only be changed by a room owner. This is a room-edit permission \
             and grants no execution authority."
                .to_owned(),
        );
    }
    // A role target must be an existing `A2aPeerSpec`: accept its configured
    // alias or its resolved PeerId, never a syntactically valid phantom id.
    let peer = resolve_configured_peer_target(target, peers)?;

    projection.refresh().await;
    if let Some(error) = projection.read_error() {
        return Err(format!(
            "Refusing to record a room role: the journal could not be read ({error}). \
             Nothing was written."
        ));
    }
    let journaled = projection
        .journaled_roles()
        .into_iter()
        .find(|(known, _)| known == &peer)
        .map(|(_, state)| state);
    let event = match role {
        Some(role) => {
            if journaled == Some(RoomRoleState::Granted(role)) {
                return Ok((
                    format!(
                        "{target} ({peer}) already holds room role '{}'; no new grant was \
                         recorded.",
                        role.label()
                    ),
                    None,
                ));
            }
            RoomEvent::RoomRoleGranted {
                peer: Some(peer.clone()),
                role,
                granted_at: now,
            }
        }
        None => match journaled {
            Some(RoomRoleState::Granted(_)) => RoomEvent::RoomRoleRevoked {
                peer: Some(peer.clone()),
                revoked_at: now,
            },
            Some(RoomRoleState::Revoked) => {
                return Ok((
                    format!("Room role for {target} ({peer}) is already revoked; nothing changed."),
                    None,
                ));
            }
            None => {
                // A revocation never manufactures identity: nothing is written.
                return Ok((
                    format!("No room role recorded for {target} ({peer}); nothing changed."),
                    None,
                ));
            }
        },
    };

    // DURABLE FIRST. Nothing below runs if the append fails.
    let journal =
        crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .map_err(|error| error.to_string())?;
    let appended = journal
        .append_room(event.clone())
        .await
        .map_err(|error| error.to_string())?;

    // Cached snapshot second, so this process sees its own append before the
    // next high-water refresh; the bus emit is the caller's.
    projection.apply(&event, appended.seq);

    let message = match role {
        // Journaled fact, not enforcement claim.
        Some(role) => format!(
            "Recorded room role '{}' for {target} ({peer}).",
            role.label()
        ),
        None => format!("Recorded revocation of {target}'s ({peer}) room role."),
    };
    Ok((message, Some(event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::policy::JournalRoomRoleProjection;
    use crate::domain::ports::RoomRoleProjectionQuery;
    use crate::infrastructure::subagent::node_journal::WorkspaceJournalReader;

    fn projection_for(workspace: &std::path::Path) -> JournalRoomRoleProjection {
        JournalRoomRoleProjection::with_reader(std::sync::Arc::new(
            WorkspaceJournalReader::open_workspace(workspace),
        ))
    }

    fn configured_peer(alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            crate::domain::models::RedactedUrl::new(format!("https://{alias}.example/a2a")),
            crate::domain::models::A2aPeerSource::Workspace,
        )
    }

    #[test]
    fn slash_room_activates_the_room_sidebar() {
        let mut state = TuiState::new(crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH, 24);
        state.needs_redraw = false;

        activate_room_sidebar(&mut state).expect("wide terminal opens the room panel");

        let panel = crate::domain::models::visual::PanelType::Room;
        assert!(state.sidebar_visible);
        assert_eq!(state.sidebar_panel, Some(panel));
        assert!(matches!(
            state.focus,
            crate::domain::models::FocusState::Sidebar {
                panel: crate::domain::models::visual::PanelType::Room,
                ..
            }
        ));
        assert!(state.needs_redraw);
    }

    #[derive(Clone)]
    struct StaticRoomReader {
        entries: Vec<crate::domain::models::JournalEntry>,
    }

    #[async_trait::async_trait]
    impl crate::domain::ports::RoomJournalReader for StaticRoomReader {
        async fn load_entries(
            &self,
        ) -> Result<Vec<crate::domain::models::JournalEntry>, crate::domain::ports::RoomJournalError>
        {
            Ok(self.entries.clone())
        }
    }

    #[tokio::test]
    async fn production_room_view_bridge_activates_refreshes_and_focuses() {
        let workspace = tempfile::TempDir::new().unwrap();
        let node = crate::domain::models::AgentId::parse("room-keystone").unwrap();
        let reader = StaticRoomReader {
            entries: vec![crate::domain::models::JournalEntry::new(
                1,
                crate::domain::models::JournalRecord::Room(
                    crate::domain::models::RoomEvent::NodeRegistered {
                        node: node.clone(),
                        origin: crate::domain::models::NodeOrigin::Interactive,
                        host: crate::domain::models::HostBinding::new("host-A", "ws"),
                    },
                ),
                10,
            )],
        };
        let mut state = TuiState::new(crate::adapters::tui::layout::SIDEBAR_MIN_WIDTH, 24);

        open_room_view_with_source(
            Some(std::sync::Arc::new(reader)),
            workspace.path(),
            "host-A".to_owned(),
            &mut state,
        )
        .await
        .expect("the production View bridge opens");

        let panel = crate::domain::models::visual::PanelType::Room;
        assert!(state.sidebar_visible);
        assert_eq!(state.sidebar_panel, Some(panel));
        assert_eq!(state.sidebar_entry_count, 1);
        assert_eq!(
            state
                .room_panel
                .room
                .as_ref()
                .and_then(|room| room.nodes().get(&node))
                .map(|view| view.id.clone()),
            Some(node)
        );
        assert!(matches!(
            state.focus,
            crate::domain::models::FocusState::Sidebar {
                panel: crate::domain::models::visual::PanelType::Room,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn role_list_reports_when_no_journal_is_attached() {
        let projection = JournalRoomRoleProjection::inert();
        let error = render_role_list(&projection, &[], false)
            .await
            .expect_err("an inert unattached projection is not an empty journal");
        assert!(error.contains(NO_JOURNAL), "{error}");
    }

    /// **The AC4 round-trip keystone.** grant → visible → revoke → gone →
    /// re-grant, all through the real `/room role` production function, against
    /// a real on-disk journal, in ONE live process with no restart and no
    /// reconstruction of the projection.
    ///
    /// First mutant: hold the startup projection and never refold → the revoke
    /// is inert until restart (18.3d's exact shipped defect).
    /// Second mutant: emit to the bus before the append succeeds → a role
    /// change observable with no durable record; the append here is
    /// unconditional and the emit is the caller's, after `Ok`.
    #[tokio::test]
    async fn room_role_grant_and_revoke_round_trip_in_one_live_process() {
        let workspace = tempfile::TempDir::new().unwrap();
        let configured = configured_peer("peer-21");
        let peer = configured.resolved_identity();
        let target = configured.id.clone();
        let peers = [configured];
        // ONE holder for the whole test. Nothing below rebuilds it.
        let projection = projection_for(workspace.path());

        assert!(
            render_role_list(&projection, &peers, true)
                .await
                .unwrap()
                .contains("no room roles recorded")
        );

        let (message, event) = persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            &target,
            Some(RoomRole::Editor),
            10,
        )
        .await
        .unwrap();
        assert!(message.contains("Recorded room role 'editor'"), "{message}");
        assert!(matches!(event, Some(RoomEvent::RoomRoleGranted { .. })));
        assert_eq!(projection.role_for(&peer), RoomRole::Editor);
        let listed = render_role_list(&projection, &peers, true).await.unwrap();
        assert!(listed.contains("granted role 'editor'"), "{listed}");

        // Duplicate grant — idempotent, and nothing is appended.
        let (duplicate, no_event) = persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            &target,
            Some(RoomRole::Editor),
            11,
        )
        .await
        .unwrap();
        assert!(duplicate.contains("already holds"), "{duplicate}");
        assert!(no_event.is_none());

        // Revoke — the answer must change in the SAME process.
        let (revoked, revoke_event) = persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            &target,
            None,
            12,
        )
        .await
        .unwrap();
        assert!(revoked.contains("Recorded revocation"), "{revoked}");
        assert!(matches!(
            revoke_event,
            Some(RoomEvent::RoomRoleRevoked { .. })
        ));
        assert_eq!(projection.role_for(&peer), RoomRole::Viewer);
        let listed = render_role_list(&projection, &peers, true).await.unwrap();
        assert!(listed.contains("role revoked"), "{listed}");

        // Duplicate revoke — a no-op that writes nothing.
        let (duplicate_revoke, none) = persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            &target,
            None,
            13,
        )
        .await
        .unwrap();
        assert!(
            duplicate_revoke.contains("already revoked"),
            "{duplicate_revoke}"
        );
        assert!(none.is_none());

        // Re-grant — present again.
        persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            &target,
            Some(RoomRole::Owner),
            14,
        )
        .await
        .unwrap();
        assert_eq!(projection.role_for(&peer), RoomRole::Owner);

        // ── The live-refold half, and it needs a SECOND writer ──────────────
        //
        // Everything above would still pass with refolding disabled, because
        // `persist_room_role` updates the holder's own cache after its own
        // append. The defect 18.3d shipped is about *another* writer: a
        // `/room role revoke` issued from a second client, a CLI, or the
        // daemon. So append one directly and assert the production `/room role
        // list` path reflects it — same process, same projection instance, no
        // restart.
        crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap()
        .append_room(RoomEvent::RoomRoleRevoked {
            peer: Some(peer.clone()),
            revoked_at: 15,
        })
        .await
        .unwrap();
        let listed = render_role_list(&projection, &peers, true).await.unwrap();
        assert!(
            listed.contains("role revoked"),
            "another writer's revocation must take effect WITHOUT a restart: {listed}"
        );
        assert_eq!(projection.role_for(&peer), RoomRole::Viewer);

        // Durable-first: exactly the four non-no-op acts reached the journal,
        // in order, and they render as room-role rows in `/team log`.
        let entries = crate::domain::ports::RoomJournalReader::load_entries(
            &WorkspaceJournalReader::open_workspace(workspace.path()),
        )
        .await
        .unwrap();
        assert_eq!(entries.len(), 4);
        let rows = crate::domain::services::transparency::fold_transparency(&entries);
        let kinds: Vec<_> = rows.iter().map(|row| row.kind).collect();
        assert_eq!(
            kinds,
            vec![
                crate::domain::services::transparency::TransparencyKind::RoomRoleGranted,
                crate::domain::services::transparency::TransparencyKind::RoomRoleRevoked,
                crate::domain::services::transparency::TransparencyKind::RoomRoleGranted,
                crate::domain::services::transparency::TransparencyKind::RoomRoleRevoked,
            ]
        );
    }

    #[tokio::test]
    async fn a_missing_journal_clears_cached_authority() {
        let workspace = tempfile::TempDir::new().unwrap();
        let configured = configured_peer("peer-regression");
        let peer = configured.resolved_identity();
        let peers = [configured];
        let projection = projection_for(workspace.path());

        persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            "peer-regression",
            Some(RoomRole::Owner),
            10,
        )
        .await
        .unwrap();
        assert_eq!(projection.role_for(&peer), RoomRole::Owner);

        let journal_path =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                workspace.path(),
            )
            .await
            .unwrap()
            .path()
            .to_path_buf();
        std::fs::remove_file(journal_path).unwrap();

        projection.refresh().await;
        assert_eq!(projection.role_for(&peer), RoomRole::Viewer);
        assert!(
            projection.journaled_roles().is_empty(),
            "a successful empty read must replace, not preserve, cached roles"
        );
    }

    /// AC3's gate, reached through the production caller: a principal that is
    /// **not** the local operator is refused, and **nothing durable is
    /// written**.
    ///
    /// Story 18.3a-b AC2 rewrote this from a loop over non-owner *roles* to a
    /// loop over non-operator *principals* — the placeholder it used to assert
    /// about is gone, and the derivation now runs on this exact production
    /// path. Mutant: make `local_room_role` return `Owner` unconditionally →
    /// every principal below is admitted and this test fires.
    #[tokio::test]
    async fn a_non_operator_principal_cannot_assign_roles_and_creates_no_journal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let peer = crate::domain::models::PeerId::from_public_key(&[22u8; 32]).unwrap();
        let projection = projection_for(workspace.path());

        for acting in [
            AgentId::root(),
            AgentId::new(),
            AgentId::from_peer_path("mcp/s-srv").expect("valid peer path"),
        ] {
            let error = persist_room_role(
                workspace.path(),
                &[],
                &projection,
                &acting,
                peer.as_str(),
                Some(RoomRole::Owner),
                10,
            )
            .await
            .unwrap_err();
            assert!(error.contains("only be changed by a room owner"), "{error}");
            assert!(
                error.contains("grants no execution authority"),
                "the refusal must not imply the role IS execution authority: {error}"
            );
        }
        assert!(
            !workspace.path().join(".rustain").exists(),
            "a refused role edit must not create a journal"
        );
    }

    /// A syntactically valid peer id that is absent from `a2a.json` is refused.
    /// Role assignment never creates a phantom principal.
    #[tokio::test]
    async fn an_unconfigured_peer_is_refused_without_fabricating_a_principal() {
        let workspace = tempfile::TempDir::new().unwrap();
        let projection = projection_for(workspace.path());
        let peer = PeerId::from_public_key(&[22u8; 32]).unwrap();
        let error = persist_room_role(
            workspace.path(),
            &[],
            &projection,
            &acting_principal(),
            peer.as_str(),
            Some(RoomRole::Editor),
            10,
        )
        .await
        .unwrap_err();
        assert!(error.contains("not a configured A2A peer"), "{error}");
        assert!(!workspace.path().join(".rustain").exists());
    }
    /// **Durable-first, bus-second.** The read succeeds and only the *append*
    /// fails (the journal file is read-only), so this exercises the ordering
    /// rather than the fail-closed refresh guard. `persist_room_role` returns
    /// `Err` and hands back **no event**, so the caller emits nothing: a role
    /// change is never observable without a durable record. The cached
    /// projection must be untouched too.
    ///
    /// Second mutant: update the cached snapshot (or emit) before the append
    /// succeeds. This goes RED the moment the change escapes a failed write.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_append_yields_no_event_and_leaves_the_projection_untouched() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::TempDir::new().unwrap();
        let configured = configured_peer("peer-23");
        let peer = configured.resolved_identity();
        let peers = [configured];
        let journal_path =
            crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
                workspace.path(),
            )
            .await
            .unwrap()
            .path()
            .to_path_buf();
        let projection = projection_for(workspace.path());
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o444)).unwrap();

        projection.refresh().await;
        assert!(
            projection.read_error().is_none(),
            "the READ must still work, or this would test the fail-closed guard instead"
        );

        let result = persist_room_role(
            workspace.path(),
            &peers,
            &projection,
            &acting_principal(),
            peer.as_str(),
            Some(RoomRole::Editor),
            10,
        )
        .await;
        assert!(result.is_err(), "a failed durable append must not succeed");
        assert_eq!(
            projection.role_for(&peer),
            RoomRole::Viewer,
            "the cached projection must not record a role that was never written"
        );

        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
