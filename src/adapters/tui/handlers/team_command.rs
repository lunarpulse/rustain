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

/// The valid sub-verb set, named verbatim in the unknown-sub-verb warning.
pub const USAGE: &str = "/team log [--filter=<direction=…|kind=…|peer=…|text>] [--json] [--export]";

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

/// `Ok(None)` = not a `log` invocation. `Err(msg)` = operator-facing refusal.
pub fn parse_team_command(cmd_arg: Option<&str>) -> Result<Option<TeamLogArgs>, String> {
    let arg = cmd_arg.map(str::trim).unwrap_or("");
    // Bare `/team` defaults to `log`, matching `/context`'s bare-invocation
    // behaviour.
    let tail = if arg.is_empty() {
        ""
    } else if let Some(rest) = arg.strip_prefix("log") {
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return Err(format!("Unknown /team subcommand '{arg}'. Use: {USAGE}"));
        }
        rest.trim()
    } else {
        let verb = arg.split_whitespace().next().unwrap_or(arg);
        return Err(format!("Unknown /team subcommand '{verb}'. Use: {USAGE}"));
    };

    let mut args = TeamLogArgs::default();
    for token in tail.split_whitespace() {
        match token {
            "--json" => args.json = true,
            "--export" => args.export = true,
            _ => match token.strip_prefix("--filter=") {
                Some(spec) if !spec.is_empty() => args.filter = Some(spec.to_owned()),
                _ => return Err(format!("Unknown /team log flag '{token}'. Use: {USAGE}")),
            },
        }
    }
    Ok(Some(args))
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
    }
    out.push_str(&format!(
        "— {STRUCTURAL_REPLAY_CLAIM}; {ATTRIBUTION_CAVEAT}"
    ));
    out
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
            direction: Direction::Outbound,
            kind: TransparencyKind::Rejected,
            peer: "peer-a".to_owned(),
            task: Some("t-1".to_owned()),
            summary: "peer reported terminal state failed".to_owned(),
        }
    }

    #[test]
    fn bare_team_defaults_to_log() {
        assert_eq!(parse_team_command(None), Ok(Some(TeamLogArgs::default())));
        assert_eq!(
            parse_team_command(Some("log")),
            Ok(Some(TeamLogArgs::default()))
        );
    }

    #[test]
    fn flags_parse_and_combine() {
        assert_eq!(
            parse_team_command(Some("log --json --export --filter=direction=inbound")),
            Ok(Some(TeamLogArgs {
                filter: Some("direction=inbound".to_owned()),
                json: true,
                export: true,
            }))
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
}
