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
use crate::adapters::cli::peer::{add, invite, list, ping, revoke, show};
use crate::adapters::p2p_config::{
    load_workspace_p2p_config, pin_peer_in_workspace_config, remove_peer_from_workspace_config,
};
use crate::adapters::p2p_reach::record_peer_reach;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{
    P2pConfigState, PeerAdmissionOutcome, PeerId, PeerTicket, PinnedKey, RoomEvent,
};
use crate::domain::ports::PeerAddress;
use crate::domain::services::peer_admission::{
    PeerImportVerdict, PeerRevokeVerdict, peer_import_verdict, peer_revoke_verdict, peer_roster,
};
use crate::domain::services::peer_reach_filter::{ReachRefusal, imported_reach};
use crate::infrastructure::paths::{workspace_p2p_config_path, workspace_p2p_reach_path};
use crate::infrastructure::runtime::app_state::AppState;

/// The relay mode this workspace composed, as every peer surface must state it.
///
/// ⚑ One read, one value, handed to every renderer. Two reads could disagree if
/// the file changed between them, and two surfaces contradicting each other
/// about which relay this host uses is exactly the asymmetric knowledge the
/// disclosure exists to prevent.
fn relay_state(workspace: &std::path::Path) -> crate::domain::models::RelayConfigState {
    crate::adapters::relay_config::load_workspace_relay_config(
        &crate::infrastructure::paths::workspace_relay_config_path(workspace),
    )
}

/// Run one `peer` verb from the CLI.
///
/// Intercepted in `startup.rs` before provider construction: reading and writing
/// a local allowlist is offline-safe and non-billable, exactly like `team log`.
pub(crate) async fn run_cli(action: &PeerAction) -> anyhow::Result<()> {
    let workspace = crate::infrastructure::paths::workspace_dir()?;
    let config_path = workspace_p2p_config_path(&workspace);
    // ⚑ Read once, per invocation, and handed to every renderer: the reach
    // copy, the `--json` `reach` value and the relay disclosure must all
    // describe the **same** composed mode, or one surface contradicts another.
    let relay = relay_state(&workspace);
    let mut stdout = std::io::stdout();
    match action {
        PeerAction::Invite { ttl, qr, name } => {
            let ttl_seconds = match ttl.as_deref() {
                Some(spec) => invite::parse_ttl(spec).map_err(|error| anyhow::anyhow!(error))?,
                None => invite::DEFAULT_TTL_SECONDS,
            };
            let ticket = mint_local_ticket(&workspace, ttl_seconds, name.clone())?;
            invite::render_invite(
                &ticket,
                crossterm::terminal::size().ok(),
                *qr,
                &relay,
                &mut stdout,
            )
        }
        PeerAction::Add {
            alias,
            ticket,
            allow_local_addresses,
        } => {
            run_cli_add(
                &workspace,
                &config_path,
                alias,
                ticket,
                *allow_local_addresses,
                &mut stdout,
            )
            .await
        }
        PeerAction::Ping {
            alias,
            count,
            interval,
        } => {
            let count = ping::validate_count(*count).map_err(|error| anyhow::anyhow!(error))?;
            let interval = match interval.as_deref() {
                Some(spec) => ping::parse_interval(spec).map_err(|error| anyhow::anyhow!(error))?,
                None => std::time::Duration::ZERO,
            };
            run_cli_ping(
                &workspace,
                &config_path,
                alias,
                count,
                interval,
                &mut stdout,
            )
            .await
        }
        PeerAction::Share {
            alias,
            artifact,
            topic,
            summary,
        } => {
            run_cli_share(
                &workspace,
                &config_path,
                alias,
                artifact,
                topic,
                summary.as_deref(),
                &mut stdout,
            )
            .await
        }
        PeerAction::List { json } => {
            let config = load_workspace_p2p_config(&config_path);
            list::render_peer_list(
                &peer_roster(&config),
                list::listen_flag(&config_path),
                &relay,
                *json,
                &mut stdout,
            )
        }
        PeerAction::Show { alias } => {
            let config = load_workspace_p2p_config(&config_path);
            show::render_peer_show(alias, &config, &relay, &mut stdout)
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
    allow_local_addresses: bool,
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
    // D15 — the claimed reach is untrusted remote input and is filtered **before**
    // anything is persisted or dialed. A refusal here refuses the whole import:
    // silently dropping the address while keeping the pin would leave the operator
    // believing they can reach a peer they cannot.
    let offered_reach = match imported_reach(&decoded.addresses, &offered_id, allow_local_addresses)
    {
        Ok(reach) => reach,
        Err(refusal) => {
            writeln!(out, "{}", refusal.message())?;
            anyhow::bail!("peer add refused the ticket's network address");
        }
    };
    match peer_import_verdict(alias, &decoded.offered_key, &config) {
        // Ruling P7 — the shipped flow early-returned here, and every daemon
        // restart changes the ephemeral port, so a restarted peer would have been
        // permanently undialable unless the operator revoked its trust identity
        // first: destroying admission to fix an address. This is the refresh path
        // that avoids that: same alias, same key, consented, expiry-rechecked,
        // and ⛔ no first-contact trust decision is re-run.
        PeerImportVerdict::AlreadyPinned => {
            run_cli_reach_refresh(
                workspace,
                config_path,
                alias,
                &decoded,
                &offered_id,
                offered_reach.as_ref(),
                out,
            )
            .await
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
            // AC3 — reach lands **after** the confirm, the expiry re-check and the
            // pin. Every refusal above wrote nothing, and this write revalidates
            // the alias→key binding inside its own transaction.
            if let Some(problem) = write_imported_reach(
                workspace,
                config_path,
                alias,
                &offered_id,
                offered_reach.as_ref(),
            ) {
                writeln!(out, "{problem}")?;
                anyhow::bail!("peer add recorded the pin but not the address");
            }
            Ok(())
        }
    }
}

/// Persist an already-filtered bundle beside a pin, or say what went wrong.
///
/// Returns `None` when there was nothing to write (the honest empty case) or the
/// write succeeded. A failure here is reported explicitly rather than swallowed:
/// a pin whose address did not land means `peer ping` will refuse, and the
/// operator has to be told which half happened.
fn write_imported_reach(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
    peer_id: &PeerId,
    address: Option<&PeerAddress>,
) -> Option<String> {
    let reach_path = workspace_p2p_reach_path(workspace);
    let Some(address) = address else {
        // The honest empty case **must still write** when an entry already
        // exists: whatever is filed under this alias was recorded against a
        // previous key's ticket, and a stale address must not outlive the
        // trust decision that justified it.
        return match crate::adapters::p2p_reach::clear_peer_reach(&reach_path, alias) {
            Ok(()) => None,
            Err(error) => Some(format!(
                "The key is pinned, but a stale network address under {alias} could not be \
                 cleared ({error}). `rustain peer ping {alias}` will say this peer is not \
                 dialable rather than dial the old address."
            )),
        };
    };
    match record_peer_reach(
        &reach_path,
        config_path,
        alias,
        peer_id,
        address,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "The key is pinned, but the network address could not be recorded ({error}). This \
             peer is admitted and NOT dialable; `rustain peer ping {alias}` will say so. Nothing \
             else was changed."
        )),
    }
}

/// The reach-only refresh for a peer whose key is already pinned (ruling P7).
///
/// ⚠ It changes **only** where this host dials them. The pinned key is untouched,
/// no admission record is appended, and no first-contact trust decision is
/// re-run — because none is being made. What it does need is the same consent
/// gate as an import: this host is about to dial addresses a ticket named, and
/// that is a decision.
async fn run_cli_reach_refresh(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
    ticket: &PeerTicket,
    peer_id: &PeerId,
    address: Option<&PeerAddress>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let Some(address) = address else {
        writeln!(
            out,
            "{}",
            rows::nothing_changed_text("This key is already pinned", alias)
        )?;
        return Ok(());
    };
    let on_file =
        crate::adapters::p2p_reach::load_workspace_p2p_reach(&workspace_p2p_reach_path(workspace))
            .store()
            .and_then(|store| store.peer(alias))
            .map(|reach| reach.address.clone());
    if on_file.as_ref() == Some(address) {
        writeln!(
            out,
            "{}",
            rows::nothing_changed_text(
                "This key is already pinned and its address is already on file",
                alias
            )
        )?;
        return Ok(());
    }
    // ⛔ No terminal is not permission to proceed here either.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        writeln!(out, "{}", add::AddRefusal::NoTerminal.message())?;
        anyhow::bail!("peer add needs an interactive terminal");
    }
    let card = rows::reach_refresh_card_text(alias, ticket, peer_id);
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    if !add::confirm_on_terminal(&card, &mut stdin, out)? {
        writeln!(out, "{}", add::AddRefusal::Declined.message())?;
        anyhow::bail!("peer add was cancelled at the confirm");
    }
    if let Err(error) = ticket.check_expiry(chrono::Utc::now().timestamp()) {
        writeln!(out, "{}", add::AddRefusal::Ticket(error).message())?;
        anyhow::bail!("peer add refused: the ticket expired before confirmation");
    }
    match record_peer_reach(
        &workspace_p2p_reach_path(workspace),
        config_path,
        alias,
        peer_id,
        address,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(()) => {
            writeln!(out, "{}", rows::reach_refreshed_text(alias, peer_id))?;
            Ok(())
        }
        Err(error) => {
            writeln!(
                out,
                "The network address could not be recorded ({error}). Nothing was changed — the \
                 pinned key and the address already on file both stand."
            )?;
            anyhow::bail!("peer add did not update the address");
        }
    }
}

/// Resolve an alias to the identity and address `peer ping` would use.
///
/// ⚠ Every refusal happens **here**, before anything is bound or dialed: an
/// unknown or unpinned alias must never produce a socket.
pub(crate) fn resolve_ping_target(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
) -> Result<(PeerId, PeerAddress, crate::domain::models::RelayMode), ping::PingRefusal> {
    let peers = match load_workspace_p2p_config(config_path) {
        P2pConfigState::Present(peers) => peers,
        P2pConfigState::Absent => return Err(ping::PingRefusal::UnknownAlias),
        P2pConfigState::Malformed { reason } => {
            return Err(ping::PingRefusal::LocalFault { reason });
        }
    };
    let entry = peers
        .iter()
        .find(|peer| peer.id == alias)
        .ok_or(ping::PingRefusal::UnknownAlias)?;
    let peer_id = entry.pinned_identity().ok_or(ping::PingRefusal::Unpinned)?;
    // The one builder (AC4). ⛔ Never an inline map: the daemon listener and this
    // verb must read reach through the same symbol or they can disagree about who
    // is dialable.
    //
    // ⚑ And it is filtered by the same relay set the client endpoint below is
    // composed with (18.4c, D13) — ⛔ a `peer ping` that dialed what the daemon
    // refuses would be the mode-honoured-in-one-bind defect wearing a verb.
    let relay = crate::adapters::relay_config::load_workspace_relay_config(
        &crate::infrastructure::paths::workspace_relay_config_path(workspace),
    )
    .mode();
    let relay_set = crate::adapters::relay_config::relay_url_set(&relay);
    let mut dial_map =
        crate::adapters::p2p_reach::peer_dial_map_from_workspace(workspace, &relay_set);
    let address = match dial_map.remove(&peer_id) {
        Some(address) => address,
        None if dial_map.excluded_by_relay_set(alias) => {
            return Err(ping::PingRefusal::RelayNotConfigured);
        }
        None => return Err(ping::PingRefusal::NoReach),
    };
    Ok((peer_id, address, relay))
}

/// `peer ping` on the CLI: resolve, dial, send, report what the peer said.
async fn run_cli_ping(
    workspace: &std::path::Path,
    config_path: &std::path::Path,
    alias: &str,
    count: u32,
    interval: std::time::Duration,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    let (peer_id, address, relay) = match resolve_ping_target(workspace, config_path, alias) {
        Ok(target) => target,
        Err(refusal) => {
            writeln!(out, "{}", rows::ping_refusal_text(alias, &refusal))?;
            anyhow::bail!("peer ping refused before anything was sent");
        }
    };
    #[cfg(not(feature = "p2p"))]
    {
        let _ = (&peer_id, &address, &relay, count, interval, workspace);
        writeln!(
            out,
            "{}",
            rows::ping_refusal_text(alias, &ping::PingRefusal::FeatureDisabled)
        )?;
        anyhow::bail!("this build was compiled without the peer transport");
    }
    #[cfg(feature = "p2p")]
    {
        send_ping_frames(
            workspace, alias, &peer_id, address, &relay, count, interval, out,
        )
        .await
    }
}

/// `peer share` on the CLI: forward the operator's share act to the running
/// daemon (Story 18.4a, FR150; code-review D3).
///
/// ⚑ **The daemon is the producer, not this process.** The daemon holds the
/// one Topic store the context provider reads and the one bound transport the
/// host identity listens on. A CLI-side endpoint would sign with the same
/// identity key over a second connection — forking the receiver's feed against
/// the daemon's — and its fresh store would be dropped at exit, so this host
/// would never retain its own log (the pre-review shape, which also computed
/// the advertised head over a fabricated header and false-fired
/// `PeerEquivocated` on every receiver: P1/P2).
///
/// Every refusal still happens before anything is dialed: the daemon resolves
/// the alias, the artifact and the listener before any peer socket exists.
async fn run_cli_share(
    workspace: &std::path::Path,
    _config_path: &std::path::Path,
    alias: &str,
    artifact_id: &str,
    topic: &str,
    summary: Option<&str>,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    use crate::adapters::cli::peer::share::{
        ShareRefusal, share_refusal_text, share_sent_text, validate_summary,
    };
    use crate::adapters::daemon::protocol::{
        ClientFrame, ConnectionTier, DaemonFrame, PeerShareOutcome, answer_attach_challenge,
        read_frame, write_frame,
    };

    let refuse = |out: &mut dyn std::io::Write, refusal: ShareRefusal| -> anyhow::Result<()> {
        writeln!(out, "{}", share_refusal_text(alias, &refusal))?;
        anyhow::bail!("peer share refused before anything was sent")
    };

    // The one client-side validation: the summary. Everything else resolves
    // daemon-side, against the one p2p.json and the one room the daemon reads.
    if let Some(text) = summary
        && let Err(refusal) = validate_summary(text)
    {
        return refuse(out, refusal);
    }

    let socket = crate::infrastructure::paths::daemon_socket_path(workspace)?;
    let stream = match tokio::net::UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(_) => return refuse(out, ShareRefusal::DaemonUnavailable),
    };
    let (mut reader, mut writer) = stream.into_split();
    let signer =
        crate::adapters::rap::IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
            .load_or_generate()
            .map_err(|error| anyhow::anyhow!("this host's identity key did not load: {error}"))?;
    answer_attach_challenge(
        &mut reader,
        &mut writer,
        false,
        ConnectionTier::TrustedLocal,
        &signer,
    )
    .await?;
    match read_frame::<_, DaemonFrame>(&mut reader).await? {
        Some(DaemonFrame::AttachAck { granted_mode, .. }) => {
            if granted_mode != crate::adapters::daemon::protocol::AttachMode::ReadWrite {
                return refuse(
                    out,
                    ShareRefusal::LocalFault {
                        reason: "another client holds this daemon's writer slot".to_owned(),
                    },
                );
            }
        }
        other => anyhow::bail!("the daemon did not accept the attach: {other:?}"),
    }
    write_frame(
        &mut writer,
        &ClientFrame::PeerShare {
            alias: alias.to_owned(),
            artifact: artifact_id.to_owned(),
            topic: topic.to_owned(),
            summary: summary.map(str::to_owned),
        },
    )
    .await?;
    // Event frames may stream while the share resolves; the answer is the
    // first PeerShareResult (or a protocol Error).
    loop {
        match read_frame::<_, DaemonFrame>(&mut reader).await? {
            Some(DaemonFrame::PeerShareResult { outcome }) => match outcome {
                PeerShareOutcome::Advertised => {
                    writeln!(out, "{}", share_sent_text(alias, topic, artifact_id))?;
                    return Ok(());
                }
                PeerShareOutcome::Refused { reason } => {
                    writeln!(out, "{reason}")?;
                    anyhow::bail!("peer share refused before anything was sent");
                }
            },
            Some(DaemonFrame::Event(_)) => continue,
            Some(DaemonFrame::Error(error)) => {
                return refuse(
                    out,
                    ShareRefusal::LocalFault {
                        reason: error.to_string(),
                    },
                );
            }
            Some(other) => anyhow::bail!("unexpected daemon frame while sharing: {other:?}"),
            None => anyhow::bail!("the daemon closed the connection mid-share"),
        }
    }
}

/// Look one artifact up in this workspace's room projection.
pub(crate) async fn resolve_shared_artifact(
    workspace: &std::path::Path,
    artifact_id: &str,
) -> anyhow::Result<Option<crate::domain::models::EvidenceArtifact>> {
    let journal =
        crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(workspace)
            .await?;
    // The **same** host-id derivation every other room read uses. ⛔ Not a
    // second one: `project_for_host` derives host-bound availability from it,
    // and two derivations would disagree about which artifacts live here.
    let host_id = crate::infrastructure::subagent::current_host_id(workspace);
    let room = journal.project_room(&host_id).await?;
    Ok(room
        .artifacts()
        .iter()
        .find(|(id, _)| id.as_str() == artifact_id)
        .map(|(_, artifact)| artifact.clone()))
}

/// Send `count` signed frames to one peer on **one** connection.
///
/// # Durable-first, on every path
///
/// Each attempt appends its own frame-attempt event carrying the **real** verdict
/// before any operator output. A journal-append failure prints no success line:
/// a claim with no record is the failure mode this whole surface exists to avoid.
///
/// # The feed position is learned, never remembered (D9)
///
/// The position lives in this function for the life of this process. It starts
/// optimistically at sequence 1 with an empty predecessor — which is exactly what
/// a receiver with no feed for this sender demands — and on a feed-position
/// refusal it retries **once** at the position the receiver named, after
/// validating it. ⛔ There is no cursor file, no cross-process lock and no second
/// feed authority to fork against, and a second guided retry per destination is a
/// refusal rather than a loop: a hostile or buggy receiver must not be able to
/// spin the sender.
#[cfg(feature = "p2p")]
#[allow(clippy::too_many_arguments)]
async fn send_ping_frames(
    workspace: &std::path::Path,
    alias: &str,
    peer_id: &PeerId,
    address: PeerAddress,
    relay: &crate::domain::models::RelayMode,
    count: u32,
    interval: std::time::Duration,
    out: &mut impl std::io::Write,
) -> anyhow::Result<()> {
    use crate::adapters::rap::{IdentityKeyStore, entry_hash};
    use crate::domain::models::{
        AgentId, CorrelationId, FeedPosition, FrameOutcome, MessageKind, PeerFrameAttemptOutcome,
    };
    use crate::domain::ports::PeerTransport;

    // D6 — the **host** identity key, not a fresh one. The receiver pins that
    // key, so a new key is refused as a stranger. A second endpoint may bind the
    // same key on its own ephemeral socket, which is why this works while the
    // daemon's own transport stays owned by the listener task.
    let signer =
        match IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?).load_or_generate() {
            Ok(signer) => signer,
            Err(error) => {
                writeln!(
                    out,
                    "{}",
                    rows::ping_refusal_text(
                        alias,
                        &ping::PingRefusal::LocalFault {
                            reason: error.to_string()
                        }
                    )
                )?;
                anyhow::bail!("peer ping could not load this host's identity key");
            }
        };
    let local = signer.identity().peer_id.clone();
    let sender = AgentId::from_peer_path(&ping::ping_sender_path(&local))
        .map_err(|error| anyhow::anyhow!("could not derive the ping sender: {error}"))?;
    let recipient = AgentId::from_peer_path(&ping::ping_recipient_path(&local))
        .map_err(|error| anyhow::anyhow!("could not derive the ping recipient: {error}"))?;

    // ⚑ The **same** relay mode the daemon listener composes. A mode honoured
    // in one production bind and not the other is a host whose ping takes a
    // path its own listener would refuse.
    let transport = match crate::adapters::iroh::IrohPeerTransport::bind(
        signer.transport_secret_key_bytes(),
        std::collections::HashMap::from([(peer_id.clone(), address)]),
        relay,
    )
    .await
    {
        Ok(transport) => transport,
        Err(error) => {
            writeln!(
                out,
                "{}",
                rows::ping_refusal_text(
                    alias,
                    &ping::PingRefusal::LocalFault {
                        reason: error.to_string()
                    }
                )
            )?;
            anyhow::bail!("peer ping could not bind a client endpoint");
        }
    };

    if let Err(error) = transport.dial(peer_id).await {
        let _ = transport.shutdown().await;
        // Durable even here: an attempt that never reached the wire is still an
        // attempt, and the journal is what an operator reads afterwards. The
        // correlation is minted per attempt, as it is for frames — one shared
        // id would make distinct dial failures indistinguishable.
        let dial_correlation = format!(
            "peer-ping-dial-{}-{}",
            std::process::id(),
            crate::domain::clock::Clock::wall_now_ms(&crate::domain::clock::SystemClock::default(),)
        );
        journal_frame_attempt(
            workspace,
            peer_id,
            &dial_correlation,
            0,
            PeerFrameAttemptOutcome::SendFailed,
            None,
        )
        .await?;
        writeln!(
            out,
            "{}",
            rows::ping_refusal_text(
                alias,
                &ping::PingRefusal::DialFailed {
                    reason: error.to_string()
                }
            )
        )?;
        anyhow::bail!("peer ping could not dial");
    }

    let mut position = FeedPosition::start();
    let mut guided_retry_spent = false;
    let mut accepted = 0u32;
    let mut transmitted = 0u32;
    let mut stop: Option<String> = None;
    let mut observed_paths: Vec<crate::domain::models::PathObservation> = Vec::new();

    'frames: for frame_index in 1..=count {
        if frame_index > 1 && !interval.is_zero() {
            tokio::time::sleep(interval).await;
        }
        for attempt in 1..=2u32 {
            let now_ms = crate::domain::clock::Clock::wall_now_ms(
                &crate::domain::clock::SystemClock::default(),
            );
            // Unique per frame **and** per attempt: reusing one nonce across a
            // `--count` run would have the receiver refuse the second frame as
            // its own nonce replay.
            let correlation = format!(
                "peer-ping-{}-{now_ms}-{frame_index}-{attempt}",
                std::process::id()
            );
            let envelope = signer
                .sign(
                    sender.clone(),
                    recipient.clone(),
                    CorrelationId::new(&correlation),
                    MessageKind::PeerMessage,
                    position.next_sequence,
                    now_ms.saturating_add(ping::PING_TTL_MS),
                    correlation.clone(),
                    position.prev_hash.clone(),
                    serde_json::Value::String(ping::PING_BODY.to_owned()),
                )
                .map_err(|error| anyhow::anyhow!("could not sign the ping frame: {error}"))?;
            let header_hash = entry_hash(&envelope.header)
                .map_err(|error| anyhow::anyhow!("could not hash the ping frame: {error}"))?;
            let sequence = envelope.header.sequence;
            let bytes = serde_json::to_vec(&envelope)
                .map(|body| body.len())
                .unwrap_or(0);

            let result = transport.send_to(peer_id, envelope).await;
            transmitted += 1;
            let (outcome, refusal) = match &result {
                Ok(verdict) => match verdict.outcome() {
                    FrameOutcome::Accepted => (PeerFrameAttemptOutcome::Accepted, None),
                    FrameOutcome::Refused(class) => (PeerFrameAttemptOutcome::Refused, Some(class)),
                    FrameOutcome::Unanswered => (PeerFrameAttemptOutcome::OutcomeUnknown, None),
                },
                Err(_) => (PeerFrameAttemptOutcome::SendFailed, None),
            };
            // ⚑ One claim per verdict, recorded as the verdict arrives (AC5).
            // `path()` is `None` on every unanswered frame and cannot be made
            // anything else, so nothing here can put a path sentence under
            // *"the peer did not answer"*.
            if let Ok(verdict) = &result
                && let Some(path) = verdict.path()
            {
                observed_paths.push(path.clone());
            }
            if let Err(error) =
                journal_frame_attempt(workspace, peer_id, &correlation, bytes, outcome, refusal)
                    .await
            {
                let _ = transport.shutdown().await;
                return Err(error);
            }

            match result {
                Ok(verdict) if verdict.outcome().is_accepted() => {
                    accepted += 1;
                    position = FeedPosition::advanced(sequence, header_hash);
                    continue 'frames;
                }
                Ok(verdict) => {
                    if !guided_retry_spent && attempt == 1 {
                        if let Some(next) = verdict.guided_retry(&position) {
                            guided_retry_spent = true;
                            position = next;
                            continue;
                        }
                    }
                    stop = Some(if count == 1 {
                        rows::ping_single_text(alias, peer_id, verdict.outcome())
                    } else {
                        let reason = match verdict.outcome() {
                            FrameOutcome::Refused(class) => {
                                crate::domain::services::transparency::frame_refusal_label(class)
                            }
                            _ => "the peer did not answer, so the outcome is unknown",
                        };
                        rows::ping_partial_text(
                            alias,
                            peer_id,
                            accepted,
                            count,
                            frame_index,
                            reason,
                        )
                    });
                    break 'frames;
                }
                Err(error) => {
                    stop = Some(if count == 1 {
                        rows::ping_refusal_text(
                            alias,
                            &ping::PingRefusal::DialFailed {
                                reason: error.to_string(),
                            },
                        )
                    } else {
                        rows::ping_partial_text(
                            alias,
                            peer_id,
                            accepted,
                            count,
                            frame_index,
                            &error.to_string(),
                        )
                    });
                    break 'frames;
                }
            }
        }
    }

    let _ = transport.shutdown().await;
    let retry_clause = rows::ping_retry_clause(count, transmitted);
    let (line, complete) = match stop {
        Some(line) => (line, false),
        None if count == 1 => (
            rows::ping_single_text(alias, peer_id, FrameOutcome::Accepted),
            true,
        ),
        None => (rows::ping_multi_text(alias, peer_id, count), true),
    };
    writeln!(out, "{line}{retry_clause}")?;
    // ⚑ A separate line per verdict, ⛔ never folded into the pinned summary
    // sentence above. `--count 3` yields three verdicts and therefore up to
    // three claims: iroh holepunches **after** connecting, so one run can
    // genuinely migrate relay→direct mid-flight and one claim would describe
    // whichever side of that migration it happened to be asked on.
    for path in &observed_paths {
        writeln!(out, "{}", rows::ping_path_text(path))?;
    }
    if !complete {
        anyhow::bail!("peer ping did not complete every frame");
    }
    Ok(())
}

/// Append the outbound frame-attempt record (D10).
///
/// ⛔ Never emitted before the outcome is known, and never with `Accepted` unless
/// a verdict said so.
async fn journal_frame_attempt(
    workspace: &std::path::Path,
    peer: &PeerId,
    correlation: &str,
    bytes: usize,
    outcome: crate::domain::models::PeerFrameAttemptOutcome,
    refusal: Option<crate::domain::models::FrameRefusal>,
) -> anyhow::Result<()> {
    let event = RoomEvent::PeerFrameAttempted {
        peer: peer.clone(),
        correlation: correlation.to_owned(),
        bytes,
        outcome,
        refusal,
    };
    append_room_event(workspace, &event).await.map_err(|error| {
        anyhow::anyhow!(
            "the frame was sent but its durable record could not be written ({error}); refusing \
             to report an outcome that is not recorded"
        )
    })
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

/// Mint a ticket for this host's local identity, carrying its claimed reach.
///
/// The address comes from the reach store's `self` record, which the daemon
/// writes at listener bind (AC1). With no record the addresses list stays empty
/// and the copy says so — ⛔ it is never filled with a guess, and ⛔ `peer invite`
/// never binds its own endpoint to learn one: a fresh ephemeral bind would name a
/// port nobody is listening on, which is exactly the fabricated fact
/// `UX-DR-PT-04` and `UX-DR-PT-07` forbid.
///
/// ⚠ **Claimed**, and nothing stronger. The ticket is self-signed: the signature
/// proves possession of the key the ticket itself carries and says nothing about
/// who owns those addresses. No surface may describe them as anything more.
fn mint_local_ticket(
    workspace: &std::path::Path,
    ttl_seconds: i64,
    name: Option<String>,
) -> anyhow::Result<PeerTicket> {
    let signer =
        crate::adapters::rap::IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?)
            .load_or_generate()?;
    let not_after = chrono::Utc::now()
        .timestamp()
        .checked_add(ttl_seconds)
        .ok_or_else(|| anyhow::anyhow!("--ttl is longer than this format carries"))?;
    let public_key = signer.identity().public_key.clone();
    // Exactly one bundle, or none (D4). One `EndpointAddr` is one endpoint; two
    // elements would be two endpoints, which this format does not mean.
    let addresses = crate::adapters::p2p_reach::load_workspace_p2p_reach(
        &crate::infrastructure::paths::workspace_p2p_reach_path(workspace),
    )
    .own()
    .map(|reach| vec![reach.address.as_bytes().to_vec()])
    .unwrap_or_default();
    PeerTicket::mint_with(&public_key, addresses, not_after, name, |message| {
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
    let relay = relay_state(&workspace);
    match command {
        PeerCommandArgs::List { json } => {
            let mut buffer = Vec::new();
            let rendered = list::render_peer_list(
                &peer_roster(&load_workspace_p2p_config(&config_path)),
                list::listen_flag(&config_path),
                &relay,
                json,
                &mut buffer,
            );
            handler::show_peer_message(state, render_or_error(rendered, buffer));
        }
        PeerCommandArgs::Show { alias } => {
            let mut buffer = Vec::new();
            let config = load_workspace_p2p_config(&config_path);
            let rendered = show::render_peer_show(&alias, &config, &relay, &mut buffer);
            handler::show_peer_message(state, render_or_error(rendered, buffer));
        }
        PeerCommandArgs::Invite { ttl, name } => {
            let ttl_seconds = match ttl.as_deref().map(invite::parse_ttl) {
                Some(Ok(seconds)) => seconds,
                Some(Err(message)) => return handler::show_peer_message(state, message),
                None => invite::DEFAULT_TTL_SECONDS,
            };
            match mint_local_ticket(&workspace, ttl_seconds, name) {
                Ok(ticket) => {
                    let mut buffer = Vec::new();
                    // ⛔ No QR in the chat transcript: the block is not a
                    // terminal-sized canvas and a partial code would not scan.
                    // `rustain peer invite --qr` is where a code can be measured
                    // against the real terminal.
                    let rendered = invite::render_invite(&ticket, None, false, &relay, &mut buffer);
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
        PeerCommandArgs::Add {
            alias,
            ticket,
            allow_local_addresses,
        } => {
            raise_peer_add_confirm(
                state,
                conversation_id,
                app_state,
                &alias,
                &ticket,
                allow_local_addresses,
            )
            .await;
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
    allow_local_addresses: bool,
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
    // D15 — filter before the card is raised: the operator must be shown what
    // this host would actually dial, and a hostile bundle must never get as far
    // as a keypress.
    let reach = match imported_reach(&ticket.addresses, &offered_id, allow_local_addresses) {
        Ok(reach) => reach,
        Err(refusal) => return handler::show_peer_message(state, refusal.message()),
    };
    match peer_import_verdict(alias, &ticket.offered_key, &config) {
        // Ruling P7 — an already-pinned key whose address changed gets a
        // reach-only refresh card. ⛔ Not a re-pin: the key is not up for
        // decision, and the card says so.
        PeerImportVerdict::AlreadyPinned => {
            let on_file = crate::adapters::p2p_reach::load_workspace_p2p_reach(
                &workspace_p2p_reach_path(&workspace),
            )
            .store()
            .and_then(|store| store.peer(alias))
            .map(|entry| entry.address.clone());
            if reach.is_none() || on_file == reach {
                return handler::show_peer_message(
                    state,
                    rows::nothing_changed_text("This key is already pinned", alias),
                );
            }
            let card = rows::reach_refresh_card_text(alias, &ticket, &offered_id);
            raise_peer_add_card(
                state,
                conversation_id,
                alias,
                ticket,
                offered_id,
                card,
                reach,
                true,
            );
        }
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
            raise_peer_add_card(
                state,
                conversation_id,
                alias,
                ticket,
                offered_id,
                card,
                reach,
                false,
            );
        }
    }
}

/// Raise one pending peer card. One place sets the overlay focus, so the import
/// card and the reach-refresh card cannot drift in how they are dismissed.
#[allow(clippy::too_many_arguments)]
fn raise_peer_add_card(
    state: &mut TuiState,
    conversation_id: &str,
    alias: &str,
    ticket: PeerTicket,
    peer_id: PeerId,
    card: String,
    reach: Option<PeerAddress>,
    reach_refresh: bool,
) {
    state.pending_peer_add = Some(crate::adapters::tui::state::PendingPeerAdd {
        conversation_id: conversation_id.to_owned(),
        alias: alias.to_owned(),
        ticket,
        peer_id,
        card,
        reach,
        reach_refresh,
        prior_focus: state.focus.clone(),
    });
    state.focus = crate::domain::models::FocusState::Overlay(
        crate::domain::models::visual::OverlayType::Confirmation(
            crate::domain::models::visual::ConfirmationType::PeerAdd,
        ),
    );
    state.needs_redraw = true;
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
    if pending.reach_refresh {
        // ⛔ No pin, no admission record: this card only changes where this host
        // dials an already-pinned peer.
        let Some(address) = pending.reach.as_ref() else {
            return handler::show_peer_message(
                state,
                rows::nothing_changed_text("This key is already pinned", &pending.alias),
            );
        };
        match record_peer_reach(
            &workspace_p2p_reach_path(&workspace),
            &config_path,
            &pending.alias,
            &pending.peer_id,
            address,
            chrono::Utc::now().timestamp(),
        ) {
            Ok(()) => handler::show_peer_message(
                state,
                rows::reach_refreshed_text(&pending.alias, &pending.peer_id),
            ),
            Err(error) => handler::show_peer_alarm(
                state,
                format!(
                    "The network address could not be recorded ({error}). Nothing was changed — \
                     the pinned key and the address already on file both stand."
                ),
            ),
        }
        return;
    }
    let outcome = record_pin(
        &workspace,
        &config_path,
        &pending.alias,
        &pending.ticket.offered_key,
        &pending.peer_id,
    )
    .await;
    let pin_failed = outcome.failed;
    emit_and_show(state, app_state, outcome);
    if pin_failed {
        return;
    }
    // AC3 — reach lands only after the pin it belongs to.
    if let Some(problem) = write_imported_reach(
        &workspace,
        &config_path,
        &pending.alias,
        &pending.peer_id,
        pending.reach.as_ref(),
    ) {
        handler::show_peer_alarm(state, problem);
    }
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
