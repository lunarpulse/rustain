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
use crate::domain::services::peer_text::sanitize_peer_text_line;
use crate::domain::services::transparency::{
    ATTRIBUTION_CAVEAT, STRUCTURAL_REPLAY_CLAIM, TransparencyExport, TransparencyRow,
};

/// The valid sub-verb set, named verbatim in every parser refusal.
pub const USAGE: &str = "/team log [--filter=<direction=…|kind=…|peer=…|text>] [--json] [--export] | /team board [<peer-id>] | /team ack <item-id> | /team remove <item-id> | /team retract <peer-id> <item-id> | /team send <peer-id> <text…> | /team status | /team trust | /team untrust <alias-or-peer-id>; `rustain team send` (the CLI twin) is not in this cut — `18-9b-cli-team-send`";

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
        peer: String,
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
            let peer = tokens
                .next()
                .ok_or_else(|| format!("Missing peer id after '/team send'. Use: {USAGE}"))?;
            // FR54-a: exactly the text the operator typed leaves the host.
            // Only the verb and the peer token are delimiters — the remainder
            // keeps its internal whitespace verbatim (indentation, repeated
            // spaces, tabs), so pasted snippets are not rewritten on the wire.
            let text = raw_remainder_after_peer(arg, peer)
                .ok_or_else(|| format!("Missing message text after peer `{peer}`. Use: {USAGE}"))?;
            Ok(TeamCommandArgs::Send {
                peer: peer.to_owned(),
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

pub(crate) fn team_send(
    conversation_id: &str,
    peer: &str,
    task_id: &str,
    state: &str,
    reply_text: Option<&str>,
) -> AppEvent {
    let peer = sanitize_peer_text_line(peer);
    let task_id = sanitize_peer_text_line(task_id);
    let state = sanitize_peer_text_line(state);
    let mut message = format!("[peer] {peer} task {task_id} — {state}");
    if let Some(reply_text) = reply_text {
        message.push('\n');
        message.push_str(&sanitize_peer_text_line(reply_text));
    }
    // Advisory, not Warning: a peer reply can land minutes after dispatch,
    // and Warning is turn-fatal (`NoticeLevel::is_turn_fatal`) — it would
    // abort whatever unrelated model turn is streaming when the peer answers.
    // Advisory renders through the same Warning→FeedbackBlock path.
    AppEvent::SystemNotice {
        conversation_id: Some(conversation_id.to_owned()),
        level: NoticeLevel::Advisory,
        message,
    }
}

/// The message body: the raw argument text after the `send` verb and peer
/// token, with only the delimiter whitespace between peer and body skipped.
/// Internal whitespace is preserved verbatim (FR54-a).
fn raw_remainder_after_peer(arg: &str, peer: &str) -> Option<String> {
    let peer_start = arg.find(peer)?;
    let after_peer = peer_start + peer.len();
    let body_start = arg[after_peer..]
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| after_peer + idx)?;
    let text = &arg[body_start..];
    (!text.is_empty()).then(|| text.to_owned())
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
                reset_revision: state.log_awareness.reset_revision,
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
    use crate::domain::ports::ConsentState;

    let mut identities: Vec<(String, crate::domain::models::PeerId)> = peers
        .iter()
        .map(|peer| (peer.id.clone(), peer.resolved_identity()))
        .collect();
    for sender in projection.known_senders() {
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
        let consent = projection.consent_for(&sender);
        let source = match consent {
            ConsentState::Trusted => "trusted (journaled)".to_owned(),
            ConsentState::Revoked => "revoked".to_owned(),
            ConsentState::None => {
                if crate::domain::services::team_policy::sender_policy_for(
                    policy,
                    &sender,
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
                peer: "moon".to_owned(),
                text: "Καλημέρα 🌕\tsecond  line".to_owned(),
            })
        );
        assert!(parse_team_command(Some("send")).is_err());
        assert!(parse_team_command(Some("send moon")).is_err());
        assert!(parse_team_command(Some("send moon   ")).is_err());
        assert!(USAGE.contains("/team send <peer-id> <text…>"));
        assert!(USAGE.contains(
            "`rustain team send` (the CLI twin) is not in this cut — \
             `18-9b-cli-team-send`"
        ));
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

    #[test]
    fn send_result_routes_peer_task_state_and_optional_reply_as_tainted_feedback() {
        let event = team_send(
            "conv",
            "moon",
            "peer-task-42",
            "completed",
            Some("peer answer"),
        );

        let AppEvent::SystemNotice {
            conversation_id,
            level,
            message,
        } = event
        else {
            panic!("peer send result must use the existing feedback event path");
        };
        assert_eq!(conversation_id.as_deref(), Some("conv"));
        // A late peer reply must never abort an unrelated streaming turn.
        assert!(matches!(level, NoticeLevel::Advisory));
        assert!(!level.is_turn_fatal());
        assert_eq!(
            message,
            "[peer] moon task peer-task-42 — completed\npeer answer"
        );
    }

    #[test]
    fn send_result_sanitizes_each_peer_derived_one_line_slot() {
        let AppEvent::SystemNotice { message, .. } = team_send(
            "conv",
            "mo\non",
            "peer-\x1b[2Jtask-42",
            "completed\x1b[2J\r\n[urgent]",
            Some("answer\x1b]0;forged\x07\nsecond"),
        ) else {
            panic!("peer send result must use the existing feedback event path");
        };
        assert_eq!(
            message,
            "[peer] moon task peer-task-42 — completed[urgent]\nanswersecond"
        );
        assert!(
            message
                .lines()
                .all(|line| !line.chars().any(char::is_control)),
            "{message:?}"
        );
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
}
