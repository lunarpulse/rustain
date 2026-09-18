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
pub const USAGE: &str = "/team log [--filter=<direction=…|kind=…|peer=…|text>] [--json] [--export] | /team ack <item-id> | /team send <peer-id> <text…> | /team trust | /team untrust <alias-or-peer-id>; `rustain team send` (the CLI twin) is not in this cut — `18-9b-cli-team-send`";

/// What the dispatch arm already did on the caller's behalf.
pub struct TeamLogInput {
    /// Rows from the shared fold, or the read error.
    pub rows: Result<Vec<TransparencyRow>, String>,
    /// Structural replay divergence, if any (AC7).
    pub divergence: Option<String>,
    /// Export result for the exact unfiltered report snapshot.
    pub export: Option<Result<TransparencyExport, String>>,
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
    Send { peer: String, text: String },
    Acknowledge { item_id: String },
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
            message: render_rows(&rows, args.json),
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(TEAM_LOG_BLOCK_ID.to_owned());
    out
}

/// The shared rendering. `--json` emits one machine-readable object per line —
/// byte-identical to the export body, because it *is* the export body.
pub fn render_rows(rows: &[TransparencyRow], json: bool) -> String {
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
            "· showing the {MAX_INCHAT_ROWS} most recent of {} rows — \
             `rustain team log` or Ctrl+X, L for all\n",
            rows.len()
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
            render_rows(&rows, true),
            crate::domain::services::transparency::render_export(&rows),
            "the two faces must not have two renderers"
        );
    }

    #[test]
    fn text_output_states_the_scoped_integrity_claim() {
        let text = render_rows(&[row(1)], false);
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
        assert!(render_rows(&[], false).contains("no A2A interactions"));
        assert_eq!(render_rows(&[], true), "");
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
    fn an_over_long_log_states_its_own_truncation() {
        let rows: Vec<TransparencyRow> = (1..=MAX_INCHAT_ROWS as u64 + 5).map(row).collect();
        let text = render_rows(&rows, false);
        assert!(
            text.contains(&format!("most recent of {} rows", rows.len())),
            "silent truncation is a lie with good intentions: {text}"
        );
        assert!(text.contains("Ctrl+X, L"), "the full surface must be named");
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
}
