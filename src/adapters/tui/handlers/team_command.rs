//! `/team log [--filter=<spec>] [--export] [--json]` — Story 18.2, AC6 / FR95.
//!
//! The sub-120-column path to the same rows the `Ctrl+X, L` panel shows and
//! `rustain team log` prints. All three render the output of **one** fold
//! ([`crate::domain::services::transparency::fold_transparency`]); divergence
//! between the faces is the defect this shape exists to prevent.
//!
//! Named `team_command`, not `handle_team_command`, on purpose:
//! `tests/conformance.rs` pins `EXPECTED_HANDLE_COUNT` with an exact
//! `assert_eq!` and bumping it requires a `RATCHET-SIGNOFF` trailer. Nothing
//! here needs a `HandlerOutcome`, so the counter stays untouched — the same
//! choice `handlers/notice.rs` made.
//!
//! Effectful work (the journal read, the export write) happens in the dispatch
//! arm's shell and arrives here as data.

use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::NoticeLevel;
use crate::domain::services::transparency::{
    ATTRIBUTION_CAVEAT, STRUCTURAL_REPLAY_CLAIM, TransparencyExport, TransparencyRow,
};

/// The valid sub-verb set, named verbatim in every parser refusal.
pub const USAGE: &str = "/team log [--filter=<direction=…|kind=…|peer=…|text>] [--json] [--export] | /team board [<peer-id>] | /team ack <item-id> | /team remove <item-id> | /team retract <peer-id> <item-id> | /team send <peer-id>[,<peer-id>…] <text…> | /team reply <item-id> <text…> | /team thread <peer-id> <item-id> | /team followup <peer-id> <turn-id> <text…> | /team status | /team trust | /team untrust <alias-or-peer-id>; `rustain team send` (the CLI twin) is not in this cut — `18-9b-cli-team-send`";

/// Three verbs retract three different objects (`…addendum-team-messaging.md:116`),
/// so every refusal of one names the others **by object** (Story 19.16f
/// `AC3(d)`).
const THREE_RETRACTS: &str = "'/team retract' marks an item on a peer's host; '/team remove' \
     removes your own received item on this host; Ctrl+X retracts this host's auto-sent message.";

/// What the dispatch arm already did on the caller's behalf.
pub struct TeamLogInput {
    /// Rows from the shared fold, or the read error.
    pub rows: Result<Vec<TransparencyRow>, String>,
    /// Structural replay divergence, if any (AC7).
    pub divergence: Option<String>,
    /// Export result for the exact unfiltered report snapshot.
    pub export: Option<Result<TransparencyExport, String>>,
    /// Story 19.16g — the maximum row `seq` of the **unfiltered** report
    /// these rows came from (0 when nothing was read). An unfiltered,
    /// presented block contributes exactly this boundary — never a later
    /// observed head.
    pub snapshot_max_seq: u64,
    /// Story 19.16h — the durable seen preference's reset revision, loaded
    /// **before** these rows were read; `None` when it was untrusted or could
    /// not be loaded (the projection's revision is used instead).
    pub reset_revision: Option<u64>,
    /// Story 19.16g — the client presenting the block.
    pub rail: LogRail,
}

/// Story 19.16g — which client presents the in-chat log. Only the access
/// guidance differs: the attached client has no `Ctrl+X, L` (its `Ctrl+X`
/// already retracts an auto-sent message), so it names the full CLI reader
/// alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogRail {
    Standalone,
    Attached,
}

impl LogRail {
    fn full_readers(self) -> &'static str {
        match self {
            Self::Standalone => "`rustain team log` or Ctrl+X, L for all",
            Self::Attached => "`rustain team log` for all",
        }
    }
}

/// Parse the `log` tail. Mirrors `forget_command::parse_forget_query`'s
/// prefix-strip idiom: flat, no tokenizer, no clap.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TeamLogArgs {
    pub filter: Option<String>,
    pub json: bool,
    pub export: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TeamCommandArgs {
    Log(TeamLogArgs),
    /// Act 1's distribution board (Story 19.16b, `UX-DR-TM-02`): one
    /// `x-rustain-items/list` read per configured A2A roster peer — or, with a
    /// `<peer-id>`, that one roster peer with its item rows uncapped (Story
    /// 19.16f `AC9`).
    Board {
        peer: Option<String>,
    },
    Send {
        recipients: Vec<String>,
        text: String,
    },
    Acknowledge {
        item_id: String,
    },
    Remove {
        item_id: String,
    },
    /// Story 19.16f: mark one item on ONE peer's host as retracted by its
    /// sender. Two tokens, like `send`: the id is only meaningful on the host
    /// that minted it, so the peer is never inferred and ⛔ never fanned out.
    Retract {
        peer: String,
        item_id: String,
    },
    /// Story 19.18 `G3` — answer an item that arrived on THIS host (rail 4
    /// only). The body is the raw remainder after the id, verbatim.
    Reply {
        item_id: String,
        text: String,
    },
    /// Story 19.18 — read one thread on a peer's host (rail 3 only).
    Thread {
        peer: String,
        item_id: String,
    },
    /// Story 19.18 — add a turn to a thread on a peer's host (rail 3 only):
    /// a 19.17 delivery carrying the id it answers.
    Followup {
        peer: String,
        turn_id: String,
        text: String,
    },
    Trust,
    Untrust(String),
    Status,
}

/// Parse one `/team` subcommand. Bare `/team` remains the log view.
pub fn parse_team_command(cmd_arg: Option<&str>) -> Result<TeamCommandArgs, String> {
    let arg = cmd_arg.map(str::trim).unwrap_or("");
    let mut tokens = arg.split_whitespace();
    let verb = tokens.next().unwrap_or("log");
    match verb {
        "board" => {
            let peer = tokens.next().map(str::to_owned);
            if tokens.next().is_some() {
                return Err(format!(
                    "'/team board' reads every configured A2A peer, or one named peer: it takes \
                     at most one peer id. Use: {USAGE}"
                ));
            }
            Ok(TeamCommandArgs::Board { peer })
        }
        "retract" => {
            let (Some(peer), Some(item_id), None) = (tokens.next(), tokens.next(), tokens.next())
            else {
                return Err(format!(
                    "'/team retract' takes exactly a peer id and an item id. {THREE_RETRACTS} \
                     Use: {USAGE}"
                ));
            };
            Ok(TeamCommandArgs::Retract {
                peer: peer.to_owned(),
                item_id: item_id.to_owned(),
            })
        }
        "send" => {
            let recipient_token = tokens
                .next()
                .ok_or_else(|| format!("Missing peer id after '/team send'. Use: {USAGE}"))?;
            let mut recipients = Vec::new();
            for id in recipient_token.split(',') {
                if id.is_empty() || recipients.iter().any(|existing| existing == id) {
                    return Err(format!(
                        "Invalid or duplicate peer id in '/team send'. Use: {USAGE}"
                    ));
                }
                recipients.push(id.to_owned());
            }
            // FR54-a: only the verb and the recipient-list token delimit the
            // body; preserve its internal whitespace exactly as typed.
            let text = raw_remainder_after_tokens(arg, 1).ok_or_else(|| {
                format!("Missing message text after peers `{recipient_token}`. Use: {USAGE}")
            })?;
            Ok(TeamCommandArgs::Send { recipients, text })
        }
        // Story 19.18 `AC1` — bodies are located by TOKEN POSITION, ⛔ never
        // by searching for the id (an id may recur inside the body). An empty
        // or whitespace-only body is refused on purpose.
        "reply" => {
            let item_id = tokens
                .next()
                .ok_or_else(|| format!("Missing item id after '/team reply'. Use: {USAGE}"))?;
            let text = raw_remainder_after_tokens(arg, 1).ok_or_else(|| {
                format!("Missing reply text after item `{item_id}`. Use: {USAGE}")
            })?;
            Ok(TeamCommandArgs::Reply {
                item_id: item_id.to_owned(),
                text,
            })
        }
        "thread" => {
            let (Some(peer), Some(item_id), None) = (tokens.next(), tokens.next(), tokens.next())
            else {
                return Err(format!(
                    "'/team thread' takes exactly a peer id and an item id. Use: {USAGE}"
                ));
            };
            Ok(TeamCommandArgs::Thread {
                peer: peer.to_owned(),
                item_id: item_id.to_owned(),
            })
        }
        "followup" => {
            let (Some(peer), Some(turn_id)) = (tokens.next(), tokens.next()) else {
                return Err(format!(
                    "'/team followup' takes a peer id, the id it answers and a message. Use: \
                     {USAGE}"
                ));
            };
            let text = raw_remainder_after_tokens(arg, 2).ok_or_else(|| {
                format!("Missing message text after `{peer} {turn_id}`. Use: {USAGE}")
            })?;
            Ok(TeamCommandArgs::Followup {
                peer: peer.to_owned(),
                turn_id: turn_id.to_owned(),
                text,
            })
        }
        "ack" => {
            let item_id = tokens
                .next()
                .ok_or_else(|| format!("Missing item id after '/team ack'. Use: {USAGE}"))?;
            if tokens.next().is_some() {
                return Err(format!(
                    "Expected exactly one item id after '/team ack'. Use: {USAGE}"
                ));
            }
            Ok(TeamCommandArgs::Acknowledge {
                item_id: item_id.to_owned(),
            })
        }
        "remove" => {
            let item_id = tokens.next().ok_or_else(|| {
                format!("Missing item id after '/team remove'. {THREE_RETRACTS} Use: {USAGE}")
            })?;
            if tokens.next().is_some() {
                return Err(format!(
                    "Expected exactly one item id after '/team remove'. {THREE_RETRACTS} \
                     Use: {USAGE}"
                ));
            }
            Ok(TeamCommandArgs::Remove {
                item_id: item_id.to_owned(),
            })
        }
        "trust" => {
            if tokens.next().is_some() {
                return Err(format!(
                    "'/team trust' lists effective grants and takes no arguments. Use: {USAGE}"
                ));
            }
            Ok(TeamCommandArgs::Trust)
        }
        "untrust" => {
            let target = tokens
                .next()
                .ok_or_else(|| format!("Missing peer target. Use: {USAGE}"))?;
            if tokens.next().is_some() {
                return Err(format!(
                    "Expected one alias or PeerId after 'untrust'. Use: {USAGE}"
                ));
            }
            Ok(TeamCommandArgs::Untrust(target.to_owned()))
        }
        "status" => {
            if tokens.next().is_some() {
                return Err(format!("'/team status' takes no arguments. Use: {USAGE}"));
            }
            Ok(TeamCommandArgs::Status)
        }
        "log" => {
            let mut args = TeamLogArgs::default();
            for token in tokens {
                match token {
                    "--json" => args.json = true,
                    "--export" => args.export = true,
                    _ => match token.strip_prefix("--filter=") {
                        Some(spec) if !spec.is_empty() => args.filter = Some(spec.to_owned()),
                        _ => {
                            return Err(format!("Unknown /team log flag '{token}'. Use: {USAGE}"));
                        }
                    },
                }
            }
            Ok(TeamCommandArgs::Log(args))
        }
        _ => Err(format!("Unknown /team subcommand '{verb}'. Use: {USAGE}")),
    }
}

/// The message body after the verb and `tokens` more whitespace-delimited
/// tokens, verbatim (FR54-a). Tokens are located by their POSITION, never by
/// their contents, which may recur inside the body (the 19.17 `M01` defect).
/// `None` when the body is empty — an empty body is refused, not sent.
fn raw_remainder_after_tokens(arg: &str, tokens: usize) -> Option<String> {
    let mut rest = arg;
    // The verb, then each positional token: skip leading whitespace, then the
    // token's non-whitespace run.
    for _ in 0..=tokens {
        rest = rest.trim_start();
        let token_end = rest.find(char::is_whitespace)?;
        rest = &rest[token_end..];
    }
    let body = rest.trim_start();
    (!body.is_empty()).then(|| body.to_owned())
}

#[cfg(not(feature = "a2a"))]
pub(crate) const fn team_send_unavailable() -> &'static str {
    "`/team send` needs the `a2a` feature; this build was compiled without it."
}

/// Stable id for the in-chat rows. `/team log` is a **view**, not an event
/// stream: re-running it replaces the row rather than stacking a new copy of
/// the same log under the old one.
pub const TEAM_LOG_BLOCK_ID: &str = "team-log";

/// Most rows rendered in-chat. The panel and the export are unbounded; the
/// transcript is not, and a thousand-line block would bury the conversation.
/// The cut is stated in the output — silent truncation is a lie with good
/// intentions (ADR R23).
pub const MAX_INCHAT_ROWS: usize = 20;

/// Render the rows in-chat. Returns the `AppEvent`s the caller should emit.
///
/// The rows go into a `FeedbackBlock` directly, **not** through
/// `AppEvent::SystemNotice { level: Info }`: an `Info` notice becomes a
/// transient status-bar flash and never reaches the transcript, so the rows
/// would blink and vanish. Only Warning/Error notices are routed through the
/// bus, because those levels DO produce a chat block.
pub(crate) fn team_command(
    state: &mut TuiState,
    conversation_id: &str,
    args: &TeamLogArgs,
    input: TeamLogInput,
) -> Vec<AppEvent> {
    state.needs_redraw = true;
    let notice = |message: String, level: NoticeLevel| AppEvent::SystemNotice {
        conversation_id: Some(conversation_id.to_string()),
        level,
        message,
    };

    let rows = match input.rows {
        Ok(rows) => rows,
        Err(error) => {
            return vec![notice(
                format!("Could not read the room journal: {error}"),
                NoticeLevel::Error,
            )];
        }
    };

    let mut out = Vec::new();
    if let Some(divergence) = input.divergence {
        // AC7: surface structural divergence without claiming authenticity.
        out.push(notice(format!("⚠ {divergence}"), NoticeLevel::Warning));
    }
    match input.export {
        Some(Ok(export)) => out.push(notice(
            format!(
                "Transparency export written to {} ({} rows).",
                export.path.display(),
                export.rows
            ),
            NoticeLevel::Warning,
        )),
        Some(Err(error)) => out.push(notice(
            format!("Transparency export failed: {error}"),
            NoticeLevel::Error,
        )),
        None => {}
    }

    state.feedback_blocks.insert(
        TEAM_LOG_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: TEAM_LOG_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message: render_rows(&rows, args.json, input.rail),
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_LOG_BLOCK_ID.to_owned());
    // Story 19.16g: the visit travels with the block it replaced — a newer
    // command discards the older, unpresented one, and a filtered view is
    // never a full visit. `TuiState::log_visits_presented` consumes it only
    // once the block intersects a successfully drawn chat viewport.
    state.pending_log_visit =
        args.filter
            .is_none()
            .then_some(crate::domain::models::LogVisitCandidate {
                seen_through: input.snapshot_max_seq,
                reset_revision: input
                    .reset_revision
                    .unwrap_or(state.log_awareness.reset_revision),
            });
    out
}

/// The shared rendering. `--json` emits one machine-readable object per line —
/// byte-identical to the export body, because it *is* the export body.
pub fn render_rows(rows: &[TransparencyRow], json: bool, rail: LogRail) -> String {
    if json {
        return crate::domain::services::transparency::render_export(rows);
    }
    if rows.is_empty() {
        return "· no A2A interactions recorded (or none match this filter).".to_owned();
    }
    let mut out = String::new();
    let skipped = rows.len().saturating_sub(MAX_INCHAT_ROWS);
    if skipped > 0 {
        out.push_str(&format!(
            "· showing the {MAX_INCHAT_ROWS} most recent of {} rows — {}\n",
            rows.len(),
            rail.full_readers()
        ));
    }
    for row in rows.iter().skip(skipped) {
        out.push_str(&format!("{} {}\n", row.timestamp_label(), row.one_line()));
        if let Some(provenance) = &row.provenance {
            out.push_str(&format!("  {}\n", provenance.response_clause()));
            out.push_str(&format!("  {}\n", provenance.notification_clause()));
        }
    }
    out.push_str(&format!(
        "— {STRUCTURAL_REPLAY_CLAIM}; {ATTRIBUTION_CAVEAT}"
    ));
    out
}

/// Resolve an operator-facing peer alias or a full stable `PeerId`.
pub fn resolve_peer_target(
    target: &str,
    peers: &[crate::domain::models::A2aPeerSpec],
) -> Result<crate::domain::models::PeerId, String> {
    if let Ok(peer_id) = crate::domain::models::PeerId::parse(target.to_owned()) {
        return Ok(peer_id);
    }
    peers
        .iter()
        .find(|peer| peer.id == target)
        .and_then(crate::domain::models::A2aPeerSpec::pinned_identity)
        .ok_or_else(|| {
            format!(
                "'{target}' has no usable pinned identity. Standing consent must key on a \
                 transport-authenticated PeerId, never a rename-unstable alias. Pin the peer \
                 (pinned_key) or supply a full PeerId."
            )
        })
}

/// Persistent in-chat policy/consent summary for `/team status`.
pub fn render_team_status(
    policy: &crate::domain::models::EffectivePolicy,
    projection: &dyn crate::domain::ports::ConsentProjectionQuery,
    peers: &[crate::domain::models::A2aPeerSpec],
) -> String {
    use crate::domain::ports::{ConsentState, peer_consent_state};

    let mut identities: Vec<(String, crate::domain::models::PeerId)> = peers
        .iter()
        .map(|peer| (peer.id.clone(), peer.resolved_identity()))
        .collect();
    for (_, sender) in projection.known_senders() {
        if !identities.iter().any(|(_, known)| known == &sender) {
            identities.push((sender.as_str().to_owned(), sender));
        }
    }
    identities.sort_by(|left, right| left.0.cmp(&right.0));

    let mut status = format!(
        "Team interaction policy\nResponse mode: {}\nNotification urgency: {}",
        policy.automation.value.as_str(),
        policy.urgency.value.as_str()
    );
    if identities.is_empty() {
        status.push_str("\nPeers: no known peers.");
        return status;
    }
    status.push_str("\nPeers:");
    for (label, sender) in identities {
        let consent = peer_consent_state(projection, &sender);
        let source = match consent {
            ConsentState::Trusted => "trusted (journaled)".to_owned(),
            ConsentState::Revoked => "revoked".to_owned(),
            ConsentState::None => {
                if crate::domain::services::team_policy::sender_policy_for(
                    policy,
                    &crate::domain::models::PolicyPrincipal::rap(sender.clone()),
                    crate::domain::models::SemanticMessageType::Unknown,
                )
                .is_some()
                {
                    "consent implied by TOML override".to_owned()
                } else {
                    "not granted".to_owned()
                }
            }
        };
        status.push_str(&format!("\n- {label} ({sender}): {source}"));
    }
    status
}

pub(crate) fn show_team_status(state: &mut TuiState, message: String) {
    const TEAM_STATUS_BLOCK_ID: &str = "team-status";
    state.feedback_blocks.insert(
        TEAM_STATUS_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: TEAM_STATUS_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_STATUS_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

/// Stable id for the board block. `/team board` is a **view** with its own
/// refresh verb and a stated refresh floor (19.16b): re-running it replaces
/// the block — `team-log` and `team-status` semantics — ⛔ never a fresh
/// dismissible `wfb-N` warning stacked under the previous board.
pub const TEAM_BOARD_BLOCK_ID: &str = "team-board";

/// Render the board (or its refusal) into the stable in-chat block, at Info
/// level. The board is not a warning about anything: it is the answer to a
/// question the operator asked.
pub(crate) fn show_team_board(state: &mut TuiState, message: String) {
    state.feedback_blocks.insert(
        TEAM_BOARD_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: TEAM_BOARD_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_BOARD_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

/// Typed, action-local row state. The block text is always a projection of
/// these rows, never mutated by line number or completion order.
#[cfg(feature = "a2a")]
#[derive(Clone)]
pub struct TeamSendRow {
    pub alias: String,
    pub outcome: Option<crate::adapters::a2a::send::RecipientOutcome>,
}

#[cfg(feature = "a2a")]
pub fn render_team_send_rows(rows: &[TeamSendRow]) -> String {
    use std::borrow::Cow;
    use std::fmt::Write;

    use crate::adapters::a2a::board::BoardOutcome;
    use crate::adapters::a2a::send::{RecipientOutcome, SendCause};
    use crate::domain::services::transparency::{
        MAX_PEER_ID_BYTES, MAX_SUMMARY_BYTES, sanitize_disclosable,
    };

    let width = rows
        .iter()
        .map(|row| row.alias.chars().count())
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for row in rows {
        let (token, label, cause): (&str, Cow<'_, str>, Option<&SendCause>) = match row
            .outcome
            .as_ref()
        {
            None => (" ", Cow::Borrowed("sending…"), None),
            Some(RecipientOutcome::Delivered) => (
                BoardOutcome::Delivered.token(),
                Cow::Borrowed(BoardOutcome::Delivered.label()),
                None,
            ),
            Some(RecipientOutcome::Declined) => (
                BoardOutcome::Declined.token(),
                Cow::Borrowed(BoardOutcome::Declined.label()),
                None,
            ),
            Some(RecipientOutcome::Unreachable { cause }) => (
                BoardOutcome::Unreachable.token(),
                Cow::Borrowed(BoardOutcome::Unreachable.label()),
                cause.as_ref(),
            ),
            Some(RecipientOutcome::AwaitingApproval) => {
                (" ", Cow::Borrowed("awaiting their approval"), None)
            }
            Some(RecipientOutcome::AcceptedWithoutItem) => (
                " ",
                Cow::Borrowed("accepted — this peer keeps no item id"),
                None,
            ),
            Some(RecipientOutcome::NoUsableAnswer) => (
                " ",
                Cow::Borrowed("no usable answer — '/team board' shows whether it arrived"),
                None,
            ),
            Some(RecipientOutcome::NotSent) => (
                " ",
                Cow::Borrowed("not sent — this host could not record the attempt"),
                None,
            ),
            Some(RecipientOutcome::AskedQuestion { task_id }) => (
                " ",
                Cow::Owned(format!(
                    "asked a question this verb cannot answer (task `{}` cancelled) — this peer uses A2A input-required, which rustain does not continue yet",
                    sanitize_disclosable(task_id, MAX_PEER_ID_BYTES)
                )),
                None,
            ),
        };
        let _ = writeln!(text, "  {token} {:width$}  {label}", row.alias);
        if let Some(cause) = cause {
            text.push_str("    ");
            let limit = match cause {
                SendCause::Ratified(_) => MAX_SUMMARY_BYTES,
                SendCause::PeerInfluenced(_) => MAX_PEER_ID_BYTES,
            };
            text.push_str(&sanitize_disclosable(cause.as_str(), limit));
            text.push('\n');
        }
    }
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

/// Story 19.16f's unconditional credential sentence, shared by the retract
/// card and (Story 19.18 `P5(h)`) the thread block: this host addresses a
/// peer with one credential, so the peer cannot separate this host's
/// operators. ⛔ A legibility statement, never a claim about who acted.
pub fn one_credential_sentence(alias: &str) -> String {
    format!(
        "This host addresses {alias} with one credential and cannot tell which operator sent \
         this item."
    )
}

/// Stable id for the thread block (Story 19.18 `AC6(a)`): one per tab,
/// replaced on every read. ⛔ Never a stacked `wfb-N`, never a Warning, never
/// a `SystemNotice` (`A12`).
pub const TEAM_THREAD_BLOCK_ID: &str = "team-thread";

/// The thread block's in-flight line, in the board's pre-outcome shape.
pub const TEAM_THREAD_READING: &str = "  reading…";

/// The thread block (`G5(a)`, ratified with `P5`). Every peer-influenced
/// field is sanitized ONCE here (`A11`), and every body line is indented under
/// its turn header, so a newline in a body can never forge a header (`M15`).
/// Line breaks are kept (19.15 P2).
#[cfg(feature = "a2a")]
pub fn render_team_thread(
    alias: &str,
    item_id: &str,
    read: &crate::adapters::a2a::send::ThreadRead,
    credentialed: bool,
) -> String {
    use std::fmt::Write;

    use crate::adapters::a2a::send::ThreadRead;
    use crate::domain::models::{MAX_REPLY_BYTES, ReplyAuthorship};
    use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};

    let alias = sanitize_disclosable(alias, MAX_PEER_ID_BYTES);
    let (thread, mine) = match read {
        ThreadRead::Read { thread, mine } => (thread, mine),
        ThreadRead::NotFound => return format!("thread not found on {alias}'s host"),
        ThreadRead::AccessOff => {
            return format!("thread access is off on {alias}'s host — its operator can enable it");
        }
        ThreadRead::UnknownPeer { known } => {
            return crate::adapters::a2a::send::SendError::UnknownPeer {
                peer: alias,
                known: known.clone(),
            }
            .to_string();
        }
        ThreadRead::Failed(outcome) => {
            return format!(
                "thread · {alias} · {}\n{}",
                sanitize_disclosable(item_id, MAX_PEER_ID_BYTES),
                render_team_send_rows(&[TeamSendRow {
                    alias: alias.clone(),
                    outcome: Some(outcome.clone()),
                }])
            );
        }
    };
    let labels: Vec<&str> = thread
        .turns
        .iter()
        .zip(mine.iter().copied().chain(std::iter::repeat(false)))
        .map(|(turn, mine)| {
            if turn.reply {
                alias.as_str()
            } else if mine {
                "you"
            } else {
                "another sender"
            }
        })
        .collect();
    let width = labels
        .iter()
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    let mut text = format!(
        "thread · {alias} · {}",
        sanitize_disclosable(&thread.root, MAX_PEER_ID_BYTES)
    );
    for (turn, label) in thread.turns.iter().zip(labels) {
        let _ = write!(
            text,
            "\n  {label:width$}  {}",
            sanitize_disclosable(&turn.turn_id, MAX_PEER_ID_BYTES)
        );
        if turn.reply && turn.authorship == ReplyAuthorship::AgentComposed {
            text.push_str("  [agent-composed]");
        }
        if turn.removed {
            text.push_str("  removed");
        }
        if let Some(ms) = turn.retracted_at_ms {
            let _ = write!(
                text,
                "  [retracted {}]",
                crate::adapters::a2a::board::mark_clock(ms)
            );
        }
        if !turn.removed {
            match &turn.content {
                Some(content) => {
                    for line in content.split('\n') {
                        text.push_str("\n      ");
                        text.push_str(&sanitize_disclosable(line, MAX_REPLY_BYTES));
                    }
                }
                None => {
                    text.push_str("\n      ");
                    text.push_str(crate::adapters::a2a::projection::HOST_BOUND_UNAVAILABLE_TEXT);
                }
            }
        }
        if turn.turn_id == thread.root && thread.omitted_before > 0 {
            let _ = write!(
                text,
                "\n  {} earlier turns not shown — the full thread is on {alias}'s host",
                thread.omitted_before
            );
        }
    }
    if credentialed {
        text.push('\n');
        text.push_str(&one_credential_sentence(&alias));
    }
    if thread.principal_collapsed {
        text.push('\n');
        text.push_str(crate::adapters::a2a::board::COLLAPSED_PRINCIPAL_DISCLOSURE);
    }
    text
}

/// Replace the stable `team-thread` block, at Info level, keyless.
pub fn show_team_thread(state: &mut TuiState, message: String) {
    state.feedback_blocks.insert(
        TEAM_THREAD_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: TEAM_THREAD_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_THREAD_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

pub fn show_team_send(state: &mut TuiState, block_id: &str, message: String) {
    state.feedback_blocks.insert(
        block_id.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: block_id.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(block_id.to_owned());
    state.needs_redraw = true;
}

/// Stable id for the retract's outcome block (Story 19.16f `AC10`): one block
/// for EVERY rail-3 retract answer — `sending…`, not found, not sent, and each
/// dispatch outcome — replaced on every update. ⛔ Never a stacked `wfb-N`,
/// ⛔ never `Warning`, ⛔ never a `SystemNotice`.
pub const TEAM_RETRACT_BLOCK_ID: &str = "team-retract";

/// The outcome block's in-flight form (`F8`): the board's ratified pre-outcome
/// shape — two leading spaces, no token.
pub const TEAM_RETRACT_SENDING: &str = "  sending…";

/// Replace the stable `team-retract` block, at Info level, keyless.
pub fn show_team_retract(state: &mut TuiState, message: String) {
    state.feedback_blocks.insert(
        TEAM_RETRACT_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: TEAM_RETRACT_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_RETRACT_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

/// The preview-ready handler (Story 19.16f `AC4(c)`): raise the decision
/// card from the confirm-time read, or render the sentence that replaces it.
///
/// The only production constructor of [`PendingTeamRetract`], and the only
/// writer of `ConfirmationType::TeamRetract` focus. A card already awaiting an
/// answer is never replaced by a newer read: the operator answers the card on
/// screen, and the newer command's result says so.
pub fn open_team_retract_card(
    state: &mut TuiState,
    conversation_id: &str,
    preview: crate::domain::events::TeamRetractPreview,
) {
    use crate::domain::events::TeamRetractPreview;
    use crate::domain::models::visual::{ConfirmationType, OverlayType};

    let card = match preview {
        TeamRetractPreview::Answer(message) => return show_team_retract(state, message),
        TeamRetractPreview::Card(card) => card,
    };
    if state.pending_team_retract.is_some() {
        return show_team_retract(
            state,
            "A retract is already awaiting your answer. Nothing was sent for this one.".to_owned(),
        );
    }
    if matches!(
        state.focus,
        crate::domain::models::FocusState::Overlay(OverlayType::Confirmation(_))
    ) {
        return show_team_retract(
            state,
            "Another confirmation is already awaiting your answer. Nothing was sent for this one."
                .to_owned(),
        );
    }
    state.pending_team_retract = Some(crate::adapters::tui::state::PendingTeamRetract {
        conversation_id: conversation_id.to_owned(),
        peer: card.peer,
        item_id: card.item_id,
        task: card.task,
        card: card.body,
        armed: card.armed,
        prior_focus: state.focus.clone(),
    });
    state.focus = crate::domain::models::FocusState::Overlay(OverlayType::Confirmation(
        ConfirmationType::TeamRetract,
    ));
    state.needs_redraw = true;
}

/// Resolve the retract card: take the slot, restore focus, and return it only
/// when accepted **and armed** (Story 19.16f `AC4(b)`, `AC10(b)`).
///
/// Taking the slot is the double-press guard: a second `y` finds no slot, so
/// it can never reach a second dispatch. The key path already refuses `y` on a
/// disarmed card; this refuses it again at the one place a dispatch is
/// decided.
pub fn resolve_team_retract_card(
    state: &mut TuiState,
    accept: bool,
) -> Option<crate::adapters::tui::state::PendingTeamRetract> {
    let pending = state.pending_team_retract.take()?;
    state.focus = pending.prior_focus.clone();
    state.needs_redraw = true;
    (accept && pending.armed).then_some(pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::Direction;
    use crate::domain::services::transparency::TransparencyKind;

    fn row(seq: u64) -> TransparencyRow {
        TransparencyRow {
            seq,
            recorded_at_ms: Some(1_700_000_000_000),
            retracted_at_ms: None,
            direction: Direction::Outbound,
            kind: TransparencyKind::Rejected,
            peer: "peer-a".to_owned(),
            task: Some("t-1".to_owned()),
            summary: "peer reported terminal state failed".to_owned(),
            provenance: None,
            principal_collapsed: false,
        }
    }

    #[test]
    fn bare_team_defaults_to_log() {
        assert_eq!(
            parse_team_command(None),
            Ok(TeamCommandArgs::Log(TeamLogArgs::default()))
        );
        assert_eq!(
            parse_team_command(Some("log")),
            Ok(TeamCommandArgs::Log(TeamLogArgs::default()))
        );
    }

    /// Story 19.16b AC3 — `/team board` is a named verb on the shared parser,
    /// so every rail classifies it identically.
    ///
    /// Mutant → RED: drop the `board` arm — the verb falls into
    /// `Unknown /team subcommand`, and on the attached rail that difference is
    /// the gap between a refusal and an LLM prompt.
    #[test]
    ///
    /// Story 19.16f `AC9` (owner gate item 3 = C) REWRITES the shipped
    /// "takes no arguments" refusal: one `<peer-id>` narrows the board to that
    /// roster peer — the escape hatch the capped picker's overflow line names.
    /// Two tokens still refuse.
    fn board_is_a_named_verb_that_takes_at_most_one_peer_id() {
        assert_eq!(
            parse_team_command(Some("board")),
            Ok(TeamCommandArgs::Board { peer: None })
        );
        assert_eq!(
            parse_team_command(Some("board jun-dev")),
            Ok(TeamCommandArgs::Board {
                peer: Some("jun-dev".to_owned())
            })
        );
        let error = parse_team_command(Some("board jun-dev tom-dev"))
            .expect_err("one narrowed peer, never a peer list");
        assert!(error.contains("/team board"), "{error}");
        assert!(
            USAGE.contains("/team board [<peer-id>]"),
            "every parser refusal names the verb set verbatim: {USAGE}"
        );
    }

    /// Story 19.16f `AC3(b)(d)` — two tokens exactly; every refusal reprints
    /// `USAGE`, and names the other two retracts by object.
    ///
    /// Mutant `M04`(i) → RED: omit `retract` from `USAGE`.
    #[test]
    fn retract_takes_exactly_a_peer_and_an_item_and_refusals_name_the_three_retracts() {
        assert_eq!(
            parse_team_command(Some("retract jun-dev ri_x")),
            Ok(TeamCommandArgs::Retract {
                peer: "jun-dev".to_owned(),
                item_id: "ri_x".to_owned(),
            })
        );
        for malformed in ["retract", "retract jun-dev", "retract jun-dev ri_x extra"] {
            let error = parse_team_command(Some(malformed)).expect_err(malformed);
            assert!(error.contains(USAGE), "{malformed}: {error}");
            assert!(
                error.contains("'/team remove'") && error.contains("Ctrl+X"),
                "{malformed}: {error}"
            );
        }
        assert!(
            USAGE.contains("/team retract <peer-id> <item-id>"),
            "the verb set names the retract: {USAGE}"
        );
        let remove = parse_team_command(Some("remove")).expect_err("missing id");
        assert!(remove.contains("'/team retract'"), "{remove}");
    }

    #[test]
    fn flags_parse_and_combine() {
        assert_eq!(
            parse_team_command(Some("log --json --export --filter=direction=inbound")),
            Ok(TeamCommandArgs::Log(TeamLogArgs {
                filter: Some("direction=inbound".to_owned()),
                json: true,
                export: true,
            }))
        );
    }

    #[test]
    fn send_parses_one_peer_and_preserves_the_body_verbatim() {
        // FR54-a: exactly what the operator typed leaves the host. Internal
        // whitespace — the repeated spaces and the tab here — must survive;
        // only the verb/peer delimiters and the delimiter run after the peer
        // are consumed.
        assert_eq!(
            parse_team_command(Some("send moon   Καλημέρα 🌕\tsecond  line")),
            Ok(TeamCommandArgs::Send {
                recipients: vec!["moon".to_owned()],
                text: "Καλημέρα 🌕\tsecond  line".to_owned(),
            })
        );
        assert!(parse_team_command(Some("send")).is_err());
        assert!(parse_team_command(Some("send moon")).is_err());
        assert!(parse_team_command(Some("send moon   ")).is_err());
        assert!(USAGE.contains("/team send <peer-id>[,<peer-id>…] <text…>"));
        assert!(USAGE.contains(
            "`rustain team send` (the CLI twin) is not in this cut — \
             `18-9b-cli-team-send`"
        ));
    }

    #[test]
    fn send_list_refuses_malformed_addresses_and_keeps_body_by_position() {
        assert_eq!(
            parse_team_command(Some("send a,b,c body")),
            Ok(TeamCommandArgs::Send {
                recipients: vec!["a".into(), "b".into(), "c".into()],
                text: "body".into(),
            })
        );
        assert_eq!(
            parse_team_command(Some("send e hi")),
            Ok(TeamCommandArgs::Send {
                recipients: vec!["e".into()],
                text: "hi".into(),
            })
        );
        for input in [
            "send a,,b hi",
            "send a, hi",
            "send ,a hi",
            "send a,b,a hi",
            "send a",
        ] {
            let error = parse_team_command(Some(input)).expect_err(input);
            assert!(error.contains(USAGE), "{input}: {error}");
        }
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn send_rows_keep_operator_order_and_strip_peer_line_injection() {
        use crate::adapters::a2a::send::{RecipientOutcome, SendCause};

        let rows = [
            TeamSendRow {
                alias: "long-peer".into(),
                outcome: None,
            },
            TeamSendRow {
                alias: "b".into(),
                outcome: Some(RecipientOutcome::Unreachable {
                    cause: Some(SendCause::PeerInfluenced(
                        "unavailable\n  ● forged  delivered".into(),
                    )),
                }),
            },
        ];
        assert_eq!(
            render_team_send_rows(&rows),
            "    long-peer  sending…\n  ⚠ b          unreachable\n    unavailable  ● forged  delivered"
        );
        let mut settled = rows.to_vec();
        settled[0].outcome = Some(RecipientOutcome::Delivered);
        assert_eq!(
            render_team_send_rows(&settled),
            "  ● long-peer  delivered\n  ⚠ b          unreachable\n    unavailable  ● forged  delivered"
        );
    }

    #[test]
    fn acknowledge_parses_one_item_without_an_alias_parameter() {
        assert_eq!(
            parse_team_command(Some("ack ri_123")),
            Ok(TeamCommandArgs::Acknowledge {
                item_id: "ri_123".to_owned(),
            })
        );
        assert!(parse_team_command(Some("ack")).is_err());
        assert!(parse_team_command(Some("ack ri_123 alias")).is_err());
        assert!(USAGE.contains("/team ack <item-id>"));
    }

    #[cfg(not(feature = "a2a"))]
    #[test]
    fn send_has_a_named_refusal_when_the_a2a_feature_is_absent() {
        assert_eq!(
            team_send_unavailable(),
            "`/team send` needs the `a2a` feature; this build was compiled without it."
        );
    }

    #[test]
    fn an_unknown_sub_verb_names_the_valid_set() {
        let error = parse_team_command(Some("logs")).unwrap_err();
        assert!(error.contains("Unknown /team subcommand 'logs'"), "{error}");
        assert!(error.contains("/team log"), "{error}");

        let error = parse_team_command(Some("roster")).unwrap_err();
        assert!(error.contains("roster"), "{error}");
        assert!(error.contains("--export"), "{error}");
    }

    #[test]
    fn an_unknown_flag_refuses_rather_than_being_ignored() {
        // Silently ignoring a flag would tell the operator their filter
        // applied when it did not.
        let error = parse_team_command(Some("log --colour=red")).unwrap_err();
        assert!(error.contains("--colour=red"), "{error}");
        assert!(parse_team_command(Some("log --filter=")).is_err());
    }

    #[test]
    fn json_output_is_the_export_body_byte_for_byte() {
        let rows = vec![row(1), row(2)];
        assert_eq!(
            render_rows(&rows, true, LogRail::Standalone),
            crate::domain::services::transparency::render_export(&rows),
            "the two faces must not have two renderers"
        );
    }

    #[test]
    fn text_output_states_the_scoped_integrity_claim() {
        let text = render_rows(&[row(1)], false, LogRail::Standalone);
        // Bind to the CONSTANTS, not to a copy of their prose. Story 18.2's
        // review replaced the old "not cryptographically tamper-evident"
        // wording with the scoped structural-replay claim but left this test
        // asserting the retired copy, so it shipped RED (repaired in 18.3
        // Task 0.5). Asserting the constant cannot drift out of sync again.
        assert!(text.contains(STRUCTURAL_REPLAY_CLAIM), "{text}");
        assert!(text.contains(ATTRIBUTION_CAVEAT), "{text}");
        assert!(text.contains("append-only"), "{text}");
        // …and the claim must stay SCOPED. Nothing hash-chains a `JournalEntry`
        // today (DF-18-2-AUTHENTICATED-JOURNAL), so the surface must never
        // assert the log itself is tamper-evident.
        assert!(
            !text.to_lowercase().contains("tamper-evident log"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_log_says_so_rather_than_rendering_a_blank() {
        assert!(render_rows(&[], false, LogRail::Standalone).contains("no A2A interactions"));
        assert_eq!(render_rows(&[], true, LogRail::Standalone), "");
    }

    #[test]
    fn a_read_failure_is_an_error_notice_not_an_empty_log() {
        let mut state = TuiState::new(80, 24);
        let events = team_command(
            &mut state,
            "conv",
            &TeamLogArgs::default(),
            TeamLogInput {
                rows: Err("disk on fire".to_owned()),
                divergence: None,
                export: None,
                snapshot_max_seq: 0,
                reset_revision: None,
                rail: LogRail::Standalone,
            },
        );
        assert_eq!(events.len(), 1);
        let AppEvent::SystemNotice { level, message, .. } = &events[0] else {
            panic!("expected a notice");
        };
        assert!(matches!(level, NoticeLevel::Error));
        assert!(message.contains("disk on fire"));
    }

    #[test]
    fn divergence_is_reported_alongside_the_rows_never_instead_of_them() {
        let mut state = TuiState::new(80, 24);
        let events = team_command(
            &mut state,
            "conv",
            &TeamLogArgs::default(),
            TeamLogInput {
                rows: Ok(vec![row(1)]),
                divergence: Some("sequence gap: expected 2, found 3".to_owned()),
                export: None,
                snapshot_max_seq: 1,
                reset_revision: None,
                rail: LogRail::Standalone,
            },
        );
        // The divergence warning rides the bus (Warning DOES reach the
        // transcript); the rows go straight into a FeedbackBlock, because an
        // Info notice would be a transient status flash instead.
        assert_eq!(events.len(), 1, "one divergence warning");
        assert!(format!("{events:?}").contains("sequence gap"));
        let block = &state.feedback_blocks[TEAM_LOG_BLOCK_ID];
        assert!(block.message.contains("peer-a"), "{}", block.message);
        assert_eq!(state.active_feedback_id.as_deref(), Some(TEAM_LOG_BLOCK_ID));
    }

    #[test]
    fn rerunning_the_command_replaces_the_row_rather_than_stacking_copies() {
        let mut state = TuiState::new(80, 24);
        for _ in 0..3 {
            team_command(
                &mut state,
                "conv",
                &TeamLogArgs::default(),
                TeamLogInput {
                    rows: Ok(vec![row(1)]),
                    divergence: None,
                    export: None,
                    snapshot_max_seq: 1,
                    reset_revision: None,
                    rail: LogRail::Standalone,
                },
            );
        }
        assert_eq!(
            state.feedback_blocks.len(),
            1,
            "a log view, not an event stream"
        );
    }

    #[test]
    fn an_over_long_log_states_its_own_truncation_and_a_rail_true_full_reader() {
        let rows: Vec<TransparencyRow> = (1..=MAX_INCHAT_ROWS as u64 + 5).map(row).collect();
        let text = render_rows(&rows, false, LogRail::Standalone);
        assert!(
            text.contains(&format!("most recent of {} rows", rows.len())),
            "silent truncation is a lie with good intentions: {text}"
        );
        assert!(text.contains("Ctrl+X, L"), "the full surface must be named");
        // Story 19.16g: the attached client has no `Ctrl+X, L` — its Ctrl+X
        // retracts an auto-sent message — so it names only the full reader,
        // and keeps the same explicit truncation disclosure.
        let attached = render_rows(&rows, false, LogRail::Attached);
        assert!(
            attached.contains(&format!("most recent of {} rows", rows.len())),
            "{attached}"
        );
        assert!(
            attached.contains("`rustain team log` for all"),
            "{attached}"
        );
        assert!(!attached.contains("Ctrl+X"), "{attached}");
    }

    #[test]
    fn trust_lists_takes_no_argument_untrust_takes_a_target() {
        // `/team trust` is a read-only listing (review D2): it takes no
        // argument and never grants. Only `/team untrust <sender>` mutates.
        assert_eq!(
            parse_team_command(Some("trust")),
            Ok(TeamCommandArgs::Trust)
        );
        assert!(parse_team_command(Some("trust alice")).is_err());
        assert_eq!(
            parse_team_command(Some("untrust 1220abcd")),
            Ok(TeamCommandArgs::Untrust("1220abcd".to_owned()))
        );
        assert_eq!(
            parse_team_command(Some("status")),
            Ok(TeamCommandArgs::Status)
        );
        assert!(parse_team_command(Some("trust alice extra")).is_err());
        assert!(parse_team_command(Some("status extra")).is_err());
    }

    #[test]
    fn pinned_alias_resolves_unpinned_rejected_and_status_annotates_source() {
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let pinned = crate::domain::models::A2aPeerSpec::new(
            "alice",
            crate::domain::models::RedactedUrl::new("https://alice.example/a2a".to_owned()),
            crate::domain::models::A2aPeerSource::Workspace,
        )
        .with_pinned_key(Some(crate::domain::models::PinnedKey::new(
            crate::domain::models::PinnedKeyAlgorithm::EdDsa,
            URL_SAFE_NO_PAD.encode([7u8; 32]),
            None,
        )));
        let identity = pinned.pinned_identity().unwrap();
        // F3: a pinned alias resolves to its transport-authenticated PeerId.
        assert_eq!(
            resolve_peer_target("alice", std::slice::from_ref(&pinned)).unwrap(),
            identity
        );
        // A raw PeerId string resolves even with no configured peers.
        assert_eq!(
            resolve_peer_target(identity.as_str(), &[]).unwrap(),
            identity
        );
        assert!(resolve_peer_target("unknown", std::slice::from_ref(&pinned)).is_err());
        // F3: an unpinned alias is rejected — standing consent must key on a pin,
        // never a rename-unstable alias pseudonym.
        let unpinned = crate::domain::models::A2aPeerSpec::new(
            "bob",
            crate::domain::models::RedactedUrl::new("https://bob.example/a2a".to_owned()),
            crate::domain::models::A2aPeerSource::Workspace,
        );
        assert!(resolve_peer_target("bob", std::slice::from_ref(&unpinned)).is_err());

        let entries = vec![crate::domain::models::JournalEntry::new(
            1,
            crate::domain::models::JournalRecord::Room(
                crate::domain::models::RoomEvent::ConsentGranted {
                    sender: Some(identity),
                    granted_at: 10,
                    principal: None,
                },
            ),
            10,
        )];
        let projection = crate::adapters::policy::JournalConsentProjection::from_entries(&entries);
        let policy = crate::domain::services::team_policy::resolve_effective_policy(
            &crate::domain::models::IndividualPolicy::default(),
            None,
            std::slice::from_ref(&pinned),
        );

        let status = render_team_status(&policy, &projection, std::slice::from_ref(&pinned));
        assert!(
            status.contains("Response mode: notify-and-wait"),
            "{status}"
        );
        assert!(status.contains("Notification urgency: queue"), "{status}");
        assert!(status.contains("alice"), "{status}");
        // D2: journaled grants are source-annotated.
        assert!(status.contains("trusted (journaled)"), "{status}");
    }

    /// K03a: `/team status` over a real folded projection and a written roster. One
    /// row per roster alias (two aliases of one pin keep their own labels) plus one per
    /// distinct journal-only `PeerId`; typed-only senders appear; `Trusted` wins across
    /// slots; the stable sort is by label, not by peer string.
    #[test]
    fn team_status_aggregates_slots_per_peer_and_keeps_every_alias_row() {
        use crate::domain::models::{
            JournalEntry, JournalRecord, PeerId, PrincipalKind, RoomEvent,
        };
        use crate::domain::ports::{ConsentProjectionQuery, ConsentSlot};
        use base64::Engine;
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;

        let (key_p, key_q, key_r, key_s, key_t) =
            ([17u8; 32], [12u8; 32], [19u8; 32], [14u8; 32], [7u8; 32]);
        let id = |key: &[u8; 32]| PeerId::from_public_key(key).unwrap();
        let (p, q, r, s, t) = (id(&key_p), id(&key_q), id(&key_r), id(&key_s), id(&key_t));
        assert!(
            p.as_str() < q.as_str(),
            "fixture precondition: label order must differ from peer order"
        );

        let dir = tempfile::tempdir().unwrap();
        let roster_path = crate::infrastructure::paths::workspace_a2a_config_path(dir.path());
        std::fs::create_dir_all(roster_path.parent().unwrap()).unwrap();
        let pin = |key: &[u8; 32]| URL_SAFE_NO_PAD.encode(key);
        let agent = |url: &str, key: &[u8; 32]| {
            format!(
                r#"{{"url":"https://{url}.example/a2a","pinnedKey":{{"alg":"EdDSA","x":"{}"}}}}"#,
                pin(key)
            )
        };
        std::fs::write(
            &roster_path,
            format!(
                r#"{{"agents":{{"alpha":{},"mid":{},"zulu":{}}}}}"#,
                agent("alpha", &key_q),
                agent("mid", &key_p),
                agent("zulu", &key_p)
            ),
        )
        .unwrap();
        let peers = crate::adapters::a2a::config::parse_workspace_a2a_config(&roster_path).unwrap();
        assert_eq!(peers.len(), 3);

        let grant = |seq: u64, sender: &PeerId, principal: Option<PrincipalKind>| {
            JournalEntry::new(
                seq,
                JournalRecord::Room(RoomEvent::ConsentGranted {
                    sender: Some(sender.clone()),
                    granted_at: seq as i64,
                    principal,
                }),
                seq as i64,
            )
        };
        let revoke = |seq: u64, sender: &PeerId| {
            JournalEntry::new(
                seq,
                JournalRecord::Room(RoomEvent::ConsentRevoked {
                    sender: Some(sender.clone()),
                    revoked_at: seq as i64,
                }),
                seq as i64,
            )
        };
        let entries = vec![
            grant(1, &p, None),
            grant(2, &p, Some(PrincipalKind::Pseudonymous)),
            revoke(3, &p),
            grant(4, &p, Some(PrincipalKind::Pinned)),
            grant(5, &q, None),
            revoke(6, &q),
            grant(7, &r, Some(PrincipalKind::Pinned)),
            grant(8, &s, Some(PrincipalKind::Pseudonymous)),
            grant(9, &t, None),
        ];
        let projection = crate::adapters::policy::JournalConsentProjection::from_entries(&entries);

        let mut raw = vec![
            (ConsentSlot::Pinned, p.clone()),
            (ConsentSlot::Pseudonymous, p.clone()),
            (ConsentSlot::Legacy, p.clone()),
            (ConsentSlot::Legacy, q.clone()),
            (ConsentSlot::Pinned, r.clone()),
            (ConsentSlot::Pseudonymous, s.clone()),
            (ConsentSlot::Legacy, t.clone()),
        ];
        raw.sort_by(|a, b| a.1.as_str().cmp(b.1.as_str()).then(a.0.cmp(&b.0)));
        assert_eq!(projection.known_senders(), raw);

        let policy = crate::domain::services::team_policy::resolve_effective_policy(
            &crate::domain::models::IndividualPolicy::default(),
            None,
            &peers,
        );
        let status = render_team_status(&policy, &projection, &peers);

        // Journal-only labels are the hex peer strings (`1220…`), which sort before the
        // alphabetic aliases; ties keep roster-then-journal insertion order.
        let expected = format!(
            "Team interaction policy\nResponse mode: notify-and-wait\nNotification urgency: queue\nPeers:\n\
             - {r} ({r}): trusted (journaled)\n\
             - {s} ({s}): trusted (journaled)\n\
             - {t} ({t}): trusted (journaled)\n\
             - alpha ({q}): revoked\n\
             - mid ({p}): trusted (journaled)\n\
             - zulu ({p}): trusted (journaled)",
            r = r.as_str(),
            s = s.as_str(),
            t = t.as_str(),
            q = q.as_str(),
            p = p.as_str(),
        );
        assert_eq!(status, expected);
    }

    #[test]
    fn an_async_retract_preview_does_not_steal_an_existing_confirmation() {
        use crate::domain::events::{TeamRetractCard, TeamRetractPreview};
        use crate::domain::models::FocusState;
        use crate::domain::models::visual::{ConfirmationType, OverlayType};

        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::PeerAdd));

        open_team_retract_card(
            &mut state,
            "conv",
            TeamRetractPreview::Card(TeamRetractCard {
                peer: "jun-dev".to_owned(),
                item_id: "ri_x".to_owned(),
                task: Some("task-x".to_owned()),
                body: "card".to_owned(),
                armed: true,
            }),
        );

        assert!(state.pending_team_retract.is_none());
        assert!(matches!(
            state.focus,
            FocusState::Overlay(OverlayType::Confirmation(ConfirmationType::PeerAdd))
        ));
        assert_eq!(
            state.feedback_blocks[TEAM_RETRACT_BLOCK_ID].message,
            "Another confirmation is already awaiting your answer. Nothing was sent for this one."
        );
    }

    /// Story 19.18 `K01` — the three verbs on the shared parser, bodies
    /// located by TOKEN POSITION and kept verbatim.
    ///
    /// `M01` → RED: locate the reply body with `find(item_id)` — `"reply r r
    /// hi"` then yields `"r hi"`. `M02` → RED: accept an empty body as `""`.
    /// `M28` → RED: slice the followup body with `find(turn_id)` — `"followup
    /// u rr_u hi"` then yields the wrong body.
    #[test]
    fn reply_thread_and_followup_parse_by_token_position() {
        assert_eq!(
            parse_team_command(Some("reply ri_x  two  words")),
            Ok(TeamCommandArgs::Reply {
                item_id: "ri_x".to_owned(),
                text: "two  words".to_owned(),
            })
        );
        assert_eq!(
            parse_team_command(Some("reply r r hi")),
            Ok(TeamCommandArgs::Reply {
                item_id: "r".to_owned(),
                text: "r hi".to_owned(),
            })
        );
        assert_eq!(
            parse_team_command(Some("thread b ri_x")),
            Ok(TeamCommandArgs::Thread {
                peer: "b".to_owned(),
                item_id: "ri_x".to_owned(),
            })
        );
        assert_eq!(
            parse_team_command(Some("followup b rr_y hi\tthere")),
            Ok(TeamCommandArgs::Followup {
                peer: "b".to_owned(),
                turn_id: "rr_y".to_owned(),
                text: "hi\tthere".to_owned(),
            })
        );
        assert_eq!(
            parse_team_command(Some("followup u rr_u hi")),
            Ok(TeamCommandArgs::Followup {
                peer: "u".to_owned(),
                turn_id: "rr_u".to_owned(),
                text: "hi".to_owned(),
            })
        );
        // The turn id recurs inside the verb itself (`follow`-`u`-`p`).
        assert_eq!(
            parse_team_command(Some("followup b u  two")),
            Ok(TeamCommandArgs::Followup {
                peer: "b".to_owned(),
                turn_id: "u".to_owned(),
                text: "two".to_owned(),
            })
        );
        for refused in [
            "reply ri_x",
            "reply ri_x   ",
            "reply",
            "followup b rr_y",
            "followup b",
            "thread b",
            "thread b ri_x extra",
        ] {
            let error = parse_team_command(Some(refused)).expect_err(refused);
            assert!(error.contains(USAGE), "{refused}: {error}");
        }
        // Positive control: a multi-line, non-ASCII body is byte-identical.
        assert_eq!(
            parse_team_command(Some("reply ri_x Καλημέρα 🌕\n second")),
            Ok(TeamCommandArgs::Reply {
                item_id: "ri_x".to_owned(),
                text: "Καλημέρα 🌕\n second".to_owned(),
            })
        );
        // The shared helper still serves `/team send` (N = 1).
        assert_eq!(
            parse_team_command(Some("send a,a2 a2  hello")),
            Ok(TeamCommandArgs::Send {
                recipients: vec!["a".to_owned(), "a2".to_owned()],
                text: "a2  hello".to_owned(),
            })
        );
    }

    #[cfg(feature = "a2a")]
    fn served_thread(
        turns: Vec<crate::adapters::a2a::send::ServedTurn>,
    ) -> crate::adapters::a2a::send::ServedThread {
        crate::adapters::a2a::send::ServedThread {
            root: "ri_q".to_owned(),
            turns,
            omitted_before: 0,
            principal_collapsed: false,
        }
    }

    #[cfg(feature = "a2a")]
    fn served_turn(id: &str, reply: bool, content: &str) -> crate::adapters::a2a::send::ServedTurn {
        crate::adapters::a2a::send::ServedTurn {
            turn_id: id.to_owned(),
            reply,
            task: (!reply).then(|| "t-1".to_owned()),
            in_reply_to: None,
            removed: false,
            content: Some(content.to_owned()),
            content_hash: None,
            authorship: crate::domain::models::ReplyAuthorship::HumanWritten,
            retracted_at_ms: None,
        }
    }

    /// Story 19.18 `K09` (render units) — the `G5(a)` block: every body line
    /// indented under its header and sanitized once; `[agent-composed]` only
    /// for that recorded value.
    ///
    /// `M15` → RED: render a body line unindented — `"  you       ri_fake"`
    /// forges a header. `M29` → RED: skip `sanitize_disclosable` — `\x1b[2K`
    /// reaches the block. `M30` → RED: render `[agent-composed]` for an
    /// `Unknown` authorship (a newer build's `"future-value"`).
    #[cfg(feature = "a2a")]
    #[test]
    fn the_thread_block_indents_sanitizes_and_marks_only_recorded_authorship() {
        use crate::adapters::a2a::send::ThreadRead;
        use crate::domain::models::ReplyAuthorship;

        let mut agent = served_turn("rr_1", true, "use the outbox\n  you       ri_fake");
        agent.authorship = ReplyAuthorship::AgentComposed;
        let mut future = served_turn("rr_2", true, "later\u{1b}[2K");
        future.authorship = ReplyAuthorship::from_wire("future-value");
        let read = ThreadRead::Read {
            thread: served_thread(vec![served_turn("ri_q", false, "question"), agent, future]),
            mine: vec![true, false, false],
        };
        assert_eq!(
            render_team_thread("marcus", "ri_q", &read, false),
            concat!(
                "thread · marcus · ri_q\n",
                "  you       ri_q\n",
                "      question\n",
                "  marcus    rr_1  [agent-composed]\n",
                "      use the outbox\n",
                "        you       ri_fake\n",
                "  marcus    rr_2\n",
                "      later[2K"
            )
        );
    }

    /// Story 19.18 `P5(a)`/`P9`/`P5(h)` — a turn this host cannot prove it
    /// sent renders `another sender`; the truncation line follows the root;
    /// the credential sentence and the collapse disclosure close the block.
    #[cfg(feature = "a2a")]
    #[test]
    fn the_thread_block_states_truncation_attribution_and_its_caveats() {
        use crate::adapters::a2a::send::ThreadRead;

        let mut thread = served_thread(vec![
            served_turn("ri_q", false, "question"),
            served_turn("rr_9", true, "newest"),
        ]);
        thread.omitted_before = 3;
        thread.principal_collapsed = true;
        let rendered = render_team_thread(
            "marcus",
            "ri_q",
            &ThreadRead::Read {
                thread,
                mine: vec![false, false],
            },
            true,
        );
        assert!(
            rendered.starts_with(concat!(
                "thread · marcus · ri_q\n",
                "  another sender  ri_q\n",
                "      question\n",
                "  3 earlier turns not shown — the full thread is on marcus's host\n",
                "  marcus          rr_9\n",
                "      newest\n",
            )),
            "{rendered}"
        );
        assert!(rendered.contains(&one_credential_sentence("marcus")));
        assert!(rendered.ends_with(crate::adapters::a2a::board::COLLAPSED_PRINCIPAL_DISCLOSURE));
        assert_eq!(
            render_team_thread("marcus", "ri_nope", &ThreadRead::NotFound, false),
            "thread not found on marcus's host"
        );
        assert_eq!(
            render_team_thread("marcus", "ri_q", &ThreadRead::AccessOff, false),
            "thread access is off on marcus's host — its operator can enable it"
        );
    }
}
