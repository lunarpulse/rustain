//! Effect shell for the `peer` family (Story 18.4b).
//!
//! Everything here is I/O: reading `.rustain/p2p.json`, appending to the durable
//! room journal, writing the allowlist, prompting a terminal, and pushing blocks
//! into `TuiState`. Every *decision* is delegated to
//! [`crate::domain::services::peer_admission`] and every *string* to
//! [`crate::adapters::cli::peer::rows`], so the CLI face and the `/peer` face
//! cannot diverge. ⛔ Two cores is the defect this module exists to prevent.
//!
//! # Durable-first, following the room-role seam verbatim (ruling A9)
//!
//! Every state change appends to the journal **before** anything else runs. If
//! the append fails, the allowlist is untouched, no bus event is emitted and no
//! success line prints. The order is: gate → resolve target → durable append →
//! config write → message. A config write that fails *after* a successful append
//! reports that explicitly rather than printing success: a record of a pin that
//! does not admit is a lie the operator must be told about.
//!
//! # Why the journal append precedes the config write
//!
//! Both orders can fail halfway. Appending first can leave a record of a pin
//! that is not in effect; writing first can leave a peer admitted with no record
//! of when or by whom. The second is worse — silent admission is the failure this
//! whole surface exists to prevent — so the journal goes first, and the partial
//! state that remains possible is the one that is loud.

use std::io::Write as _;

use crate::adapters::cli::peer::PeerAction;
use crate::adapters::cli::peer::rows;
use crate::adapters::cli::peer::{add, invite, list, revoke, show};
use crate::adapters::p2p_config::{
    load_workspace_p2p_config, pin_peer_in_workspace_config, remove_peer_from_workspace_config,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{PeerAdmissionOutcome, PeerId, PeerTicket, PinnedKey, RoomEvent};
use crate::domain::services::peer_admission::{
    PeerImportVerdict, PeerRevokeVerdict, peer_import_verdict, peer_revoke_verdict, peer_roster,
};
use crate::infrastructure::paths::workspace_p2p_config_path;
use crate::infrastructure::runtime::app_state::AppState;

/// Run one `peer` verb from the CLI.
///
/// Intercepted in `startup.rs` before provider construction: reading and writing
/// a local allowlist is offline-safe and non-billable, exactly like `team log`.
pub(crate) async fn run_cli(action: &PeerAction) -> anyhow::Result<()> {
    let workspace = crate::infrastructure::paths::workspace_dir()?;
    let config_path = workspace_p2p_config_path(&workspace);
    let mut stdout = std::io::stdout();
    match action {
        PeerAction::Invite { ttl, qr, name } => {
            let ttl_seconds = match ttl.as_deref() {
                Some(spec) => invite::parse_ttl(spec).map_err(|error| anyhow::anyhow!(error))?,
                None => invite::DEFAULT_TTL_SECONDS,
            };
            let ticket = mint_local_ticket(ttl_seconds, name.clone())?;
            invite::render_invite(&ticket, crossterm::terminal::size().ok(), *qr, &mut stdout)
        }
        PeerAction::Add { alias, ticket } => {
            run_cli_add(&workspace, &config_path, alias, ticket, &mut stdout).await
        }
        PeerAction::List { json } => {
            let config = load_workspace_p2p_config(&config_path);
            list::render_peer_list(
                &peer_roster(&config),
                list::listen_flag(&config_path),
                *json,
                &mut stdout,
            )
        }
        PeerAction::Show { alias } => {
            let config = load_workspace_p2p_config(&config_path);
            show::render_peer_show(alias, &config, &mut stdout)
        }
        PeerAction::Revoke { target, now } => {
            if *now {
                writeln!(stdout, "{}", revoke::NOW_IS_A_NO_OP)?;
            }
            let message = revoke_peer(&workspace, &config_path, target).await;
            writeln!(stdout, "{}", message.text)?;
            if message.failed {
                anyhow::bail!("peer revoke did not complete");
            }
            Ok(())
        }
    }
}

/// `peer add` on the CLI: decode, decide, confirm on a terminal, then record.
async fn run_cli_add(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
    blob: &str,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    let decoded = match PeerTicket::decode(blob, now.timestamp()) {
        Ok(ticket) => ticket,
        Err(error) => {
            writeln!(out, "{}", add::AddRefusal::Ticket(error).message())?;
            anyhow::bail!("peer add refused the ticket");
        }
    };
    let offered_id = decoded
        .peer_id()
        .map_err(|error| anyhow::anyhow!("could not derive the offered peer id: {error}"))?;
    let config = load_workspace_p2p_config(config_path);
    match peer_import_verdict(alias, &decoded.offered_key, &config) {
        PeerImportVerdict::AlreadyPinned => {
            writeln!(
                out,
                "{}",
                rows::nothing_changed_text("This key is already pinned", alias)
            )?;
            Ok(())
        }
        PeerImportVerdict::AlreadyPinnedAs {
            alias: on_file_alias,
        } => {
            writeln!(
                out,
                "{}",
                add::AddRefusal::AlreadyPinnedAs {
                    alias: on_file_alias,
                }
                .message()
            )?;
            anyhow::bail!("peer add refused a second alias for one identity");
        }
        PeerImportVerdict::InvalidAlias => {
            writeln!(out, "{}", add::AddRefusal::InvalidAlias.message())?;
            anyhow::bail!("peer add refused an invalid alias");
        }
        PeerImportVerdict::RefuseUnreadable { reason } => {
            writeln!(out, "{}", add::AddRefusal::Unreadable { reason }.message())?;
            anyhow::bail!("peer add refused to overwrite an unreadable allowlist");
        }
        PeerImportVerdict::KeyMismatch { on_file } => {
            let outcome = record_key_mismatch(workspace, alias, &on_file, &offered_id).await;
            writeln!(out, "{}", outcome.text)?;
            anyhow::bail!("peer add refused: key mismatch");
        }
        PeerImportVerdict::Pin => {
            // ⛔ No terminal is not permission to proceed.
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                writeln!(out, "{}", add::AddRefusal::NoTerminal.message())?;
                anyhow::bail!("peer add needs an interactive terminal");
            }
            let card = rows::confirm_card_text(alias, &decoded, &offered_id);
            let mut stdin = std::io::BufReader::new(std::io::stdin());
            if !add::confirm_on_terminal(&card, &mut stdin, out)? {
                writeln!(out, "{}", add::AddRefusal::Declined.message())?;
                anyhow::bail!("peer add was cancelled at the confirm");
            }
            if let Err(error) = decoded.check_expiry(chrono::Utc::now().timestamp()) {
                writeln!(out, "{}", add::AddRefusal::Ticket(error).message())?;
                anyhow::bail!("peer add refused: the ticket expired before confirmation");
            }
            let outcome = record_pin(
                workspace,
                config_path,
                alias,
                &decoded.offered_key,
                &offered_id,
            )
            .await;
            writeln!(out, "{}", outcome.text)?;
            if outcome.failed {
                anyhow::bail!("peer add did not complete");
            }
            Ok(())
        }
    }
}

/// What an effectful `peer` step produced: one operator-facing block, and
/// whether the step failed.
pub(crate) struct PeerOutcome {
    pub(crate) text: String,
    pub(crate) failed: bool,
    /// Present only when a durable record was appended, so the caller can emit
    /// it on the bus. ⛔ Never emitted before the append succeeds.
    pub(crate) event: Option<RoomEvent>,
}

/// Journal a pin, then write the allowlist. Durable-first.
pub(crate) async fn record_pin(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
    key: &PinnedKey,
    peer_id: &PeerId,
) -> PeerOutcome {
    let current = load_workspace_p2p_config(config_path);
    match peer_import_verdict(alias, key, &current) {
        PeerImportVerdict::Pin => {}
        PeerImportVerdict::AlreadyPinned => {
            return PeerOutcome {
                text: rows::nothing_changed_text("This key is already pinned", alias),
                failed: false,
                event: None,
            };
        }
        PeerImportVerdict::AlreadyPinnedAs { alias } => {
            return PeerOutcome {
                text: add::AddRefusal::AlreadyPinnedAs { alias }.message(),
                failed: true,
                event: None,
            };
        }
        PeerImportVerdict::InvalidAlias => {
            return PeerOutcome {
                text: add::AddRefusal::InvalidAlias.message(),
                failed: true,
                event: None,
            };
        }
        PeerImportVerdict::KeyMismatch { on_file } => {
            return record_key_mismatch(workspace, alias, &on_file, peer_id).await;
        }
        PeerImportVerdict::RefuseUnreadable { reason } => {
            return PeerOutcome {
                text: add::AddRefusal::Unreadable { reason }.message(),
                failed: true,
                event: None,
            };
        }
    }
    let event = RoomEvent::PeerAdmissionRecorded {
        alias: alias.to_owned(),
        peer: Some(peer_id.clone()),
        outcome: PeerAdmissionOutcome::Pinned,
    };
    if let Err(error) = append_room_event(workspace, &event).await {
        return PeerOutcome {
            text: format!(
                "Refusing to pin: the durable record could not be written ({error}). Nothing \
                 was changed — the allowlist is untouched."
            ),
            failed: true,
            event: None,
        };
    }
    if let Err(error) = pin_peer_in_workspace_config(config_path, alias, key) {
        return PeerOutcome {
            text: format!(
                "The pin was recorded but .rustain/p2p.json could not be updated ({error}). \
                 This peer is NOT admitted. Nothing else was changed; re-run once the file is \
                 writable."
            ),
            failed: true,
            event: Some(event),
        };
    }
    PeerOutcome {
        text: add::pinned_text(alias, peer_id),
        failed: false,
        event: Some(event),
    }
}

/// Journal a refused import and render the alarm. ⛔ Writes no allowlist entry
/// and binds no key.
pub(crate) async fn record_key_mismatch(
    workspace: &std::path::Path,
    alias: &str,
    on_file: &PinnedKey,
    offered_id: &PeerId,
) -> PeerOutcome {
    let on_file_id = on_file.peer_id().ok();
    let block = match on_file_id.as_ref() {
        Some(on_file_id) => rows::key_mismatch_text(alias, on_file_id, offered_id),
        // The pinned key is present but does not derive an identity, which means
        // the file is not in a state this surface may compare against.
        None => format!(
            "Key mismatch — {alias}\n\nThe key pinned for this peer could not be read as a \
             key, so it cannot be compared with the one this ticket offers. Nothing was \
             changed. Fix .rustain/p2p.json first.",
            alias = rows::sanitize_for_terminal(alias)
        ),
    };
    let event = RoomEvent::PeerAdmissionRecorded {
        alias: alias.to_owned(),
        peer: on_file_id,
        outcome: PeerAdmissionOutcome::ImportRefused,
    };
    if let Err(error) = append_room_event(workspace, &event).await {
        return PeerOutcome {
            text: format!(
                "{block}\n\n(The refusal itself could not be recorded durably: {error}. The \
                 prior pin still stands and nothing was changed.)"
            ),
            failed: true,
            event: None,
        };
    }
    PeerOutcome {
        text: block,
        failed: true,
        event: Some(event),
    }
}

/// Journal a revocation, then remove the entry. Durable-first.
pub(crate) async fn revoke_peer(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    target: &str,
) -> PeerOutcome {
    let config = load_workspace_p2p_config(config_path);
    let (alias, peer_id) = match peer_revoke_verdict(target, &config) {
        PeerRevokeVerdict::Remove { alias, peer_id } => (alias, peer_id),
        PeerRevokeVerdict::NothingRecorded => {
            return PeerOutcome {
                text: revoke::nothing_recorded_text(target),
                failed: false,
                event: None,
            };
        }
        PeerRevokeVerdict::RefuseUnreadable { reason } => {
            return PeerOutcome {
                text: revoke::unreadable_text(&reason),
                failed: true,
                event: None,
            };
        }
    };
    let event = RoomEvent::PeerAdmissionRecorded {
        alias: alias.clone(),
        peer: peer_id.clone(),
        outcome: PeerAdmissionOutcome::Revoked,
    };
    if let Err(error) = append_room_event(workspace, &event).await {
        return PeerOutcome {
            text: format!(
                "Refusing to revoke: the durable record could not be written ({error}). \
                 Nothing was changed — the peer is still admitted."
            ),
            failed: true,
            event: None,
        };
    }
    if let Err(error) = remove_peer_from_workspace_config(config_path, &alias) {
        return PeerOutcome {
            text: format!(
                "The revocation was recorded but .rustain/p2p.json could not be updated \
                 ({error}). This peer is STILL admitted. Re-run once the file is writable."
            ),
            failed: true,
            event: Some(event),
        };
    }
    PeerOutcome {
        text: rows::revocation_text(&alias, peer_id.as_ref()),
        failed: false,
        event: Some(event),
    }
}

/// Mint a ticket for this host's local identity.
///
/// The addresses list is empty in this cut and the copy says so: the transport
/// binds with no configured address map, so there is no address to publish. ⛔ It
/// is never filled with a guess — an invented address is exactly the fabricated
/// fact `UX-DR-PT-04` and `UX-DR-PT-07` forbid.
fn mint_local_ticket(ttl_seconds: i64, name: Option<String>) -> anyhow::Result<PeerTicket> {
    let signer =
        crate::adapters::rap::IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
            .load_or_generate()?;
    let not_after = chrono::Utc::now()
        .timestamp()
        .checked_add(ttl_seconds)
        .ok_or_else(|| anyhow::anyhow!("--ttl is longer than this format carries"))?;
    let public_key = signer.identity().public_key.clone();
    PeerTicket::mint_with(&public_key, Vec::new(), not_after, name, |message| {
        signer.sign_detached(message).0
    })
    .map_err(|error| anyhow::anyhow!("could not mint a ticket: {error}"))
}

async fn append_room_event(workspace: &std::path::Path, event: &RoomEvent) -> Result<(), String> {
    let journal =
        crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await
            .map_err(|error| error.to_string())?;
    journal
        .append_room(event.clone())
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// Run one `/peer` sub-verb in the TUI.
///
/// Same cores, same strings, same journal as [`run_cli`]. The only differences
/// are that output lands in `TuiState` and that the confirm is a card rather
/// than a stdin prompt.
pub(crate) async fn peer_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
) {
    use crate::adapters::tui::handlers::peer_command::{self as handler, PeerCommandArgs};

    let command = match handler::parse_peer_command(cmd_arg) {
        Ok(command) => command,
        Err(message) => return handler::show_peer_message(state, message),
    };
    let workspace = app_state.compose_snapshot.workspace_path.clone();
    let config_path = workspace_p2p_config_path(&workspace);
    match command {
        PeerCommandArgs::List { json } => {
            let mut buffer = Vec::new();
            let rendered = list::render_peer_list(
                &peer_roster(&load_workspace_p2p_config(&config_path)),
                list::listen_flag(&config_path),
                json,
                &mut buffer,
            );
            handler::show_peer_message(state, render_or_error(rendered, buffer));
        }
        PeerCommandArgs::Show { alias } => {
            let mut buffer = Vec::new();
            let config = load_workspace_p2p_config(&config_path);
            let rendered = show::render_peer_show(&alias, &config, &mut buffer);
            handler::show_peer_message(state, render_or_error(rendered, buffer));
        }
        PeerCommandArgs::Invite { ttl, name } => {
            let ttl_seconds = match ttl.as_deref().map(invite::parse_ttl) {
                Some(Ok(seconds)) => seconds,
                Some(Err(message)) => return handler::show_peer_message(state, message),
                None => invite::DEFAULT_TTL_SECONDS,
            };
            match mint_local_ticket(ttl_seconds, name) {
                Ok(ticket) => {
                    let mut buffer = Vec::new();
                    // ⛔ No QR in the chat transcript: the block is not a
                    // terminal-sized canvas and a partial code would not scan.
                    // `rustain peer invite --qr` is where a code can be measured
                    // against the real terminal.
                    let rendered = invite::render_invite(&ticket, None, false, &mut buffer);
                    handler::show_peer_message(state, render_or_error(rendered, buffer));
                }
                Err(error) => {
                    handler::show_peer_message(state, format!("Could not mint a ticket: {error}"))
                }
            }
        }
        PeerCommandArgs::Revoke { target } => {
            let outcome = revoke_peer(&workspace, &config_path, &target).await;
            emit_and_show(state, app_state, outcome);
        }
        PeerCommandArgs::Add { alias, ticket } => {
            raise_peer_add_confirm(state, conversation_id, app_state, &alias, &ticket).await;
        }
    }
}

/// Decode, decide, and either raise the confirm card or refuse outright.
async fn raise_peer_add_confirm(
    state: &mut TuiState,
    conversation_id: &str,
    app_state: &AppState,
    alias: &str,
    blob: &str,
) {
    use crate::adapters::tui::handlers::peer_command as handler;

    if state.pending_peer_add.is_some() {
        return handler::show_peer_message(
            state,
            "A peer import is already awaiting your answer.".to_owned(),
        );
    }
    let workspace = app_state.compose_snapshot.workspace_path.clone();
    let config_path = workspace_p2p_config_path(&workspace);
    let now = chrono::Utc::now();
    let ticket = match PeerTicket::decode(blob, now.timestamp()) {
        Ok(ticket) => ticket,
        Err(error) => {
            return handler::show_peer_message(
                state,
                crate::adapters::cli::peer::add::AddRefusal::Ticket(error).message(),
            );
        }
    };
    let offered_id = match ticket.peer_id() {
        Ok(peer_id) => peer_id,
        Err(error) => {
            return handler::show_peer_message(
                state,
                format!("Could not derive the offered peer id: {error}"),
            );
        }
    };
    let config = load_workspace_p2p_config(&config_path);
    match peer_import_verdict(alias, &ticket.offered_key, &config) {
        PeerImportVerdict::AlreadyPinned => handler::show_peer_message(
            state,
            rows::nothing_changed_text("This key is already pinned", alias),
        ),
        PeerImportVerdict::AlreadyPinnedAs { alias } => handler::show_peer_message(
            state,
            crate::adapters::cli::peer::add::AddRefusal::AlreadyPinnedAs { alias }.message(),
        ),
        PeerImportVerdict::InvalidAlias => handler::show_peer_message(
            state,
            crate::adapters::cli::peer::add::AddRefusal::InvalidAlias.message(),
        ),
        PeerImportVerdict::RefuseUnreadable { reason } => handler::show_peer_message(
            state,
            crate::adapters::cli::peer::add::AddRefusal::Unreadable { reason }.message(),
        ),
        PeerImportVerdict::KeyMismatch { on_file } => {
            // AC4. ⛔ Never a card: a mismatch presents no choice, so it renders
            // as a never-truncated error block that binds no key at all.
            let outcome = record_key_mismatch(&workspace, alias, &on_file, &offered_id).await;
            if let Some(event) = outcome.event.as_ref() {
                emit_event(app_state, event.clone());
            }
            handler::show_peer_alarm(state, outcome.text);
        }
        PeerImportVerdict::Pin => {
            let card = rows::confirm_card_text(alias, &ticket, &offered_id);
            state.pending_peer_add = Some(crate::adapters::tui::state::PendingPeerAdd {
                conversation_id: conversation_id.to_owned(),
                alias: alias.to_owned(),
                ticket,
                peer_id: offered_id,
                card,
                prior_focus: state.focus.clone(),
            });
            state.focus = crate::domain::models::FocusState::Overlay(
                crate::domain::models::visual::OverlayType::Confirmation(
                    crate::domain::models::visual::ConfirmationType::PeerAdd,
                ),
            );
            state.needs_redraw = true;
        }
    }
}

/// Resolve the pending `/peer add` card. `confirm == false` writes nothing.
pub(crate) async fn resolve_peer_add(state: &mut TuiState, confirm: bool, app_state: &AppState) {
    use crate::adapters::tui::handlers::peer_command as handler;

    let Some(pending) = handler::resolve_peer_add_card(state, confirm) else {
        if !confirm {
            handler::show_peer_message(
                state,
                crate::adapters::cli::peer::add::AddRefusal::Declined.message(),
            );
        }
        return;
    };
    if let Err(error) = pending.ticket.check_expiry(chrono::Utc::now().timestamp()) {
        handler::show_peer_message(
            state,
            crate::adapters::cli::peer::add::AddRefusal::Ticket(error).message(),
        );
        return;
    }
    let workspace = app_state.compose_snapshot.workspace_path.clone();
    let config_path = workspace_p2p_config_path(&workspace);
    let outcome = record_pin(
        &workspace,
        &config_path,
        &pending.alias,
        &pending.ticket.offered_key,
        &pending.peer_id,
    )
    .await;
    emit_and_show(state, app_state, outcome);
}

fn emit_and_show(state: &mut TuiState, app_state: &AppState, outcome: PeerOutcome) {
    use crate::adapters::tui::handlers::peer_command as handler;

    if let Some(event) = outcome.event.as_ref() {
        emit_event(app_state, event.clone());
    }
    if outcome.failed {
        handler::show_peer_alarm(state, outcome.text);
    } else {
        handler::show_peer_message(state, outcome.text);
    }
}

/// Bus emit, always AFTER a successful durable append (ruling A9).
fn emit_event(app_state: &AppState, event: RoomEvent) {
    let _ = app_state
        .event_bus
        .emit_domain(crate::domain::events::AppEvent::DomainEvent(event.into()));
}

fn render_or_error(rendered: anyhow::Result<()>, buffer: Vec<u8>) -> String {
    match rendered {
        Ok(()) => String::from_utf8_lossy(&buffer).into_owned(),
        Err(error) => format!("Could not render the peer roster: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key(seed: u8) -> PinnedKey {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        PeerTicket::mint(&signer, Vec::new(), i64::MAX, None)
            .expect("ticket")
            .offered_key
    }

    #[tokio::test]
    async fn record_pin_revalidates_the_alias_after_confirmation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let config_path = workspace_p2p_config_path(workspace.path());
        let on_file = test_key(241);
        let offered = test_key(242);
        pin_peer_in_workspace_config(&config_path, "alice", &on_file).expect("existing pin");

        let outcome = record_pin(
            workspace.path(),
            &config_path,
            "alice",
            &offered,
            &offered.peer_id().expect("peer id"),
        )
        .await;

        assert!(outcome.failed, "a changed pin must not be overwritten");
        assert!(matches!(
            outcome.event,
            Some(RoomEvent::PeerAdmissionRecorded {
                outcome: PeerAdmissionOutcome::ImportRefused,
                ..
            })
        ));
        match load_workspace_p2p_config(&config_path) {
            crate::domain::models::P2pConfigState::Present(peers) => {
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].pinned_identity(), on_file.peer_id().ok());
            }
            other => panic!("existing config must remain readable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn record_pin_revalidates_identity_alias_uniqueness() {
        let workspace = tempfile::tempdir().expect("workspace");
        let config_path = workspace_p2p_config_path(workspace.path());
        let key = test_key(243);
        pin_peer_in_workspace_config(&config_path, "alice", &key).expect("existing pin");

        let outcome = record_pin(
            workspace.path(),
            &config_path,
            "bob",
            &key,
            &key.peer_id().expect("peer id"),
        )
        .await;

        assert!(outcome.failed, "one identity must not gain a second alias");
        assert!(
            outcome.event.is_none(),
            "an alias-policy refusal is not journaled"
        );
        assert!(
            outcome.text.contains("already pinned as alice"),
            "{}",
            outcome.text
        );
    }
}
