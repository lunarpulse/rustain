//! Per-item status selection and policy-labelled rendering (Story 19.24).

use super::{ActionBoardRow, ActionRowState, bounded};
use crate::adapters::a2a::send::ServedThread;
use crate::domain::models::{Authorship, ReplyKind, StatusDisclosure, StatusStatement};
use ratatui::{style::Style, text::Span};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StatusReport {
    Awaiting {
        defer_minutes: Option<u16>,
    },
    Shared {
        statement: StatusStatement,
        authorship: Authorship,
        note: Option<String>,
    },
    Nothing,
    Note(String),
    /// Story 19.25 D8 — no served Status turn answers the item and the host
    /// left turns out: the status may be among them.
    Omitted,
    Unreadable,
}

/// Thread order is served journal order. Only direct replies to this item count;
/// a linked follow-up must never answer its parent's status request.
pub(super) fn report_for_item(thread: &ServedThread, item_id: &str) -> StatusReport {
    let mut replies = thread
        .turns
        .iter()
        .rev()
        .filter(|turn| turn.reply && turn.in_reply_to.as_deref() == Some(item_id))
        .peekable();
    if replies
        .peek()
        .is_some_and(|turn| turn.kind == ReplyKind::Unknown)
    {
        return StatusReport::Unreadable;
    }
    let mut status = None;
    let mut answer = None;
    let mut defer_minutes = None;
    for turn in replies {
        match turn.kind {
            ReplyKind::Status if status.is_none() => {
                // Structured status and its content share the wire's disclosure gate.
                if turn.content.is_none() || turn.status.is_none() {
                    return StatusReport::Unreadable;
                }
                status = Some(turn);
            }
            ReplyKind::Answer if answer.is_none() => answer = Some(turn),
            ReplyKind::Defer { minutes } if defer_minutes.is_none() => {
                defer_minutes = Some(minutes);
            }
            _ => {}
        }
    }
    if status.is_none() && thread.omitted_before > 0 {
        return StatusReport::Omitted;
    }
    let note = match answer {
        Some(turn) => match &turn.content {
            Some(content) => Some(content.clone()),
            None => return StatusReport::Unreadable,
        },
        None => None,
    };
    if let Some(turn) = status {
        let statement = turn.status.as_ref().expect("readable status checked above");
        if !statement.is_nothing() {
            return StatusReport::Shared {
                statement: statement.clone(),
                authorship: turn.authorship,
                note,
            };
        }
        return note.map_or(StatusReport::Nothing, StatusReport::Note);
    }
    note.map_or(StatusReport::Awaiting { defer_minutes }, StatusReport::Note)
}

/// `width` is the parent's alias-column width, not the terminal width. Status
/// rows have a 60-cell floor layout; long aliases wrap rather than consuming
/// the complete detail column. Only a delivered, acknowledged or send-time
/// declined row is a report row; every other outcome (failed reads, notes and
/// rows whose send never reached an item) keeps the legacy renderer.
pub(super) fn render_status_row(out: &mut String, row: &ActionBoardRow, width: usize) {
    if row.read_failed
        || row.note.is_some()
        || !matches!(
            row.state,
            ActionRowState::Delivered | ActionRowState::Acknowledged | ActionRowState::Declined
        )
    {
        super::render_action_row(out, row, width);
        return;
    }
    let alias = bounded(&row.alias);
    let column = width.max(alias.width()).min(20);
    let indent = " ".repeat(column + 5);
    let fallback = if row.state == ActionRowState::Declined {
        StatusReport::Nothing
    } else {
        StatusReport::Awaiting {
            defer_minutes: None,
        }
    };
    let report = if row.state == ActionRowState::Declined {
        &fallback
    } else {
        row.status_report.as_ref().unwrap_or(&fallback)
    };
    let mut details = Vec::new();
    match report {
        StatusReport::Awaiting { defer_minutes } => {
            let mut label = "awaiting reply".to_owned();
            if let Some(minutes) = defer_minutes {
                label.push_str(&format!(" [defer {minutes}m — reported expectation]"));
            }
            details.push(label);
        }
        StatusReport::Nothing => {
            details.push("no status shared".to_owned());
            details.push("Source: —".to_owned());
        }
        StatusReport::Unreadable => {
            details.push("status not readable by this build".to_owned());
            details.push("Source: —".to_owned());
        }
        StatusReport::Omitted => {
            details.push("status not among the turns this peer served".to_owned());
            details.push("Source: —".to_owned());
        }
        StatusReport::Note(note) => append_note(&mut details, note),
        StatusReport::Shared {
            statement,
            authorship,
            note,
        } => {
            let level = statement.level.unwrap_or(StatusDisclosure::None).as_str();
            details.push(
                statement
                    .stories
                    .iter()
                    .map(|story| match story.state {
                        Some(state) => {
                            format!("{} {}", bounded(story.key.as_str()), state.as_str())
                        }
                        None => bounded(story.key.as_str()),
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            if statement.level == Some(StatusDisclosure::Minimal) {
                details.push(format!("(limited detail — {alias} shares minimal)"));
            }
            let marker = if *authorship == Authorship::AgentComposed {
                " [agent-composed]"
            } else {
                ""
            };
            details.push(format!(
                "Source: manually shared ({alias}'s policy: {level}){marker}"
            ));
            if let Some(note) = note {
                append_note(&mut details, note);
            }
        }
    }
    if alias.width() <= column {
        let padding = " ".repeat(column - alias.width());
        let first = details.remove(0);
        append_wrapped(out, &format!("  {alias}{padding}   "), &indent, &first);
    } else {
        append_wrapped(out, "  ", "  ", &alias);
    }
    for detail in details {
        append_wrapped(out, &indent, &indent, &detail);
    }
}

fn append_note(details: &mut Vec<String>, note: &str) {
    details.push(format!("\"{}\"", bounded(note)));
    details.push("Source: manually shared".to_owned());
}

/// Preserve every grapheme and space, wrapping in terminal cells rather than
/// bytes or scalar values. A row breaks after its last space, so a story key
/// or word splits only when it alone is wider than the row (the card's rule).
/// Prefixes are host-authored and bounded to 25 cells.
fn append_wrapped(out: &mut String, first: &str, continuation: &str, text: &str) {
    let mut line = first.to_owned();
    let mut cells = first.width();
    // Byte offset and cell count just past the row's last space.
    let mut space: Option<(usize, usize)> = None;
    let span = Span::raw(text);
    for styled in span.styled_graphemes(Style::default()) {
        let grapheme = styled.symbol;
        let size = grapheme.width();
        if cells + size > 60 {
            let carry = match space.take() {
                Some((at, at_cells)) => {
                    cells -= at_cells;
                    line.split_off(at)
                }
                None => {
                    cells = 0;
                    String::new()
                }
            };
            out.push_str(&line);
            out.push('\n');
            line.clear();
            line.push_str(continuation);
            cells += continuation.width();
            // `cells` now counts the continuation and the carried word.
            if !carry.is_empty() && cells + size > 60 {
                // The carried word cannot share a row with this grapheme.
                line.push_str(&carry);
                out.push_str(&line);
                out.push('\n');
                line.clear();
                line.push_str(continuation);
                cells = continuation.width();
            } else {
                line.push_str(&carry);
            }
        }
        line.push_str(grapheme);
        cells += size;
        if grapheme == " " {
            space = Some((line.len(), cells));
        }
    }
    out.push_str(&line);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::a2a::send::ServedTurn;
    use crate::domain::models::{ReceiptState, SharedStory, StoryKey};

    fn statement(level: StatusDisclosure) -> StatusStatement {
        StatusStatement {
            level: Some(level),
            stories: vec![SharedStory {
                key: StoryKey::parse("S7-03").unwrap(),
                state: (level == StatusDisclosure::Status).then_some(ReceiptState::Acknowledged),
            }],
        }
    }

    fn reply(kind: ReplyKind, content: &str) -> ServedTurn {
        ServedTurn {
            retro: None,
            turn_id: "reply".to_owned(),
            reply: true,
            task: None,
            in_reply_to: Some("item".to_owned()),
            removed: false,
            content: Some(content.to_owned()),
            content_hash: None,
            kind,
            status: None,
            authorship: Authorship::HumanWritten,
            retracted_at_ms: None,
        }
    }

    fn status(statement: StatusStatement) -> ServedTurn {
        ServedTurn {
            retro: None,
            status: Some(statement),
            authorship: Authorship::AgentComposed,
            ..reply(ReplyKind::Status, "never render this status body")
        }
    }

    fn report(turns: Vec<ServedTurn>) -> StatusReport {
        report_for_item(
            &ServedThread {
                root: "item".to_owned(),
                turns,
                omitted_before: 0,
                principal_collapsed: false,
                principal_bound: true,
            },
            "item",
        )
    }

    fn row(report: StatusReport) -> ActionBoardRow {
        ActionBoardRow {
            alias: "jun".to_owned(),
            bound_alias: Some("renamed-jun".to_owned()),
            state: ActionRowState::Delivered,
            read_failed: false,
            note: None,
            items: Vec::new(),
            status_report: Some(report),
        }
    }

    fn render(row: &ActionBoardRow) -> String {
        let mut out = String::new();
        render_status_row(&mut out, row, row.alias.width());
        out
    }

    #[test]
    fn story19_25_omitted_status_turn_is_not_definite() {
        let report = report_for_item(
            &ServedThread {
                root: "item".to_owned(),
                turns: vec![reply(ReplyKind::Answer, "own words")],
                omitted_before: 1,
                principal_collapsed: false,
                principal_bound: true,
            },
            "item",
        );
        let text = render(&row(report));
        assert!(
            text.contains("status not among the turns this peer served"),
            "{text}"
        );
        assert!(text.contains("Source: —"), "{text}");
    }

    /// D8 control: a served Status turn (or an Unknown newest reply) keeps
    /// its precedence over the omission row.
    #[test]
    fn story19_25_served_status_wins_over_omitted_turns() {
        let omitted = |turns: Vec<ServedTurn>| {
            report_for_item(
                &ServedThread {
                    root: "item".to_owned(),
                    turns,
                    omitted_before: 4,
                    principal_collapsed: false,
                    principal_bound: true,
                },
                "item",
            )
        };
        let shared = statement(StatusDisclosure::Status);
        assert_eq!(
            omitted(vec![status(shared.clone())]),
            report(vec![status(shared)])
        );
        assert_eq!(
            omitted(vec![reply(ReplyKind::Unknown, "x")]),
            StatusReport::Unreadable
        );
        assert_eq!(omitted(vec![]), StatusReport::Omitted);
        assert_eq!(
            report(vec![]),
            StatusReport::Awaiting {
                defer_minutes: None
            }
        );
    }

    #[test]
    fn status_report_d6_precedence() {
        let shared = statement(StatusDisclosure::Minimal);
        let cases = [
            (
                vec![],
                StatusReport::Awaiting {
                    defer_minutes: None,
                },
            ),
            (
                vec![
                    reply(ReplyKind::Defer { minutes: 10 }, "old"),
                    reply(ReplyKind::Defer { minutes: 30 }, "new"),
                ],
                StatusReport::Awaiting {
                    defer_minutes: Some(30),
                },
            ),
            (
                vec![status(StatusStatement::nothing())],
                StatusReport::Nothing,
            ),
            (
                vec![reply(ReplyKind::Answer, "own words")],
                StatusReport::Note("own words".to_owned()),
            ),
            (
                vec![
                    status(StatusStatement::nothing()),
                    reply(ReplyKind::Answer, "note"),
                ],
                StatusReport::Note("note".to_owned()),
            ),
            (
                vec![
                    reply(ReplyKind::Answer, "old"),
                    status(shared.clone()),
                    reply(ReplyKind::Answer, "new"),
                    reply(ReplyKind::Defer { minutes: 30 }, "busy"),
                ],
                StatusReport::Shared {
                    statement: shared.clone(),
                    authorship: Authorship::AgentComposed,
                    note: Some("new".to_owned()),
                },
            ),
            (
                vec![
                    status(shared.clone()),
                    reply(ReplyKind::Unknown, "private raw"),
                ],
                StatusReport::Unreadable,
            ),
            (
                vec![reply(ReplyKind::Status, "legacy raw")],
                StatusReport::Unreadable,
            ),
        ];
        for (turns, expected) in cases {
            assert_eq!(report(turns), expected);
        }
        let mut withheld = status(shared.clone());
        withheld.content = None;
        assert_eq!(
            report(vec![withheld, reply(ReplyKind::Answer, "later")]),
            StatusReport::Unreadable
        );
        assert_eq!(
            report(vec![
                reply(ReplyKind::Unknown, "older unknown"),
                status(shared.clone())
            ]),
            StatusReport::Shared {
                statement: shared,
                authorship: Authorship::AgentComposed,
                note: None
            }
        );
    }

    #[test]
    fn status_report_ignores_sibling_followup_and_item_turns() {
        let mut followup = reply(ReplyKind::Answer, "follow-up secret");
        followup.in_reply_to = Some("followup".to_owned());
        let mut sibling = reply(ReplyKind::Unknown, "sibling");
        sibling.in_reply_to = Some("sibling".to_owned());
        let mut item = reply(ReplyKind::Answer, "item body");
        item.reply = false;
        assert_eq!(
            report(vec![
                status(StatusStatement::nothing()),
                followup,
                sibling,
                item
            ]),
            StatusReport::Nothing
        );
    }

    #[test]
    fn status_report_minimal_and_status_grammar_and_served_authorship() {
        let output = render(&row(StatusReport::Shared {
            statement: statement(StatusDisclosure::Minimal),
            authorship: Authorship::AgentComposed,
            note: Some("my note".to_owned()),
        }));
        assert!(output.starts_with("  jun   S7-03\n"));
        let logical = output.replace("\n        ", "").trim_end().to_owned();
        assert_eq!(
            logical,
            concat!(
                "  jun   S7-03",
                "(limited detail — jun shares minimal)",
                "Source: manually shared (jun's policy: minimal) [agent-composed]",
                "\"my note\"",
                "Source: manually shared"
            )
        );
        for authorship in [
            Authorship::HumanWritten,
            Authorship::Unknown,
            Authorship::AgentComposed,
        ] {
            let output = render(&row(StatusReport::Shared {
                statement: statement(StatusDisclosure::Status),
                authorship,
                note: None,
            }));
            assert!(output.contains("S7-03 acknowledged"));
            assert!(!output.contains("limited detail"));
            assert_eq!(
                output
                    .replace('\n', "")
                    .replace("        ", "")
                    .contains("[agent-composed]"),
                authorship == Authorship::AgentComposed
            );
            assert!(!output.contains("never render this status body"));
        }
    }

    #[test]
    fn status_report_nothing_declined_unreadable_and_awaiting_labels() {
        let mut declined = row(StatusReport::Note("must not show".to_owned()));
        declined.state = ActionRowState::Declined;
        assert_eq!(
            render(&declined),
            "  jun   no status shared\n        Source: —\n"
        );
        assert_eq!(render(&declined), render(&row(StatusReport::Nothing)));
        let unreadable = render(&row(StatusReport::Unreadable));
        assert!(unreadable.contains("status not readable by this build\n        Source: —"));
        assert_eq!(
            render(&row(StatusReport::Awaiting {
                defer_minutes: None
            })),
            "  jun   awaiting reply\n"
        );
        assert!(
            render(&row(StatusReport::Awaiting {
                defer_minutes: Some(30)
            }))
            .contains("awaiting reply [defer 30m — reported expectation]")
        );
    }

    #[test]
    fn status_report_wraps_long_cjk_aliases_and_notes_at_sixty_cells() {
        for alias in ["long-alias-".repeat(8), "界".repeat(35), "界jun".to_owned()] {
            let mut row = row(StatusReport::Shared {
                statement: statement(StatusDisclosure::Minimal),
                authorship: Authorship::AgentComposed,
                note: Some("界e\u{301} long note ".repeat(8)),
            });
            row.alias = alias.clone();
            let output = render(&row);
            assert!(output.lines().all(|line| line.width() <= 60), "{output}");
            let compact = output.split_whitespace().collect::<String>();
            assert!(compact.contains(&alias));
            assert!(compact.contains("S7-03"));
            assert!(compact.contains("[agent-composed]"));
            assert!(compact.contains("界e\u{301}longnote"));
            assert!(!output.contains("[truncated]"));
        }
    }

    #[test]
    fn status_report_bounds_peer_text_before_rendering() {
        let mut row = row(StatusReport::Note("hello\nforged\u{1b}[31m".to_owned()));
        row.alias = "jun\nforged\u{1b}".to_owned();
        let output = render(&row);
        assert!(!output.contains('\u{1b}'));
        assert!(!output.lines().any(|line| line.starts_with("forged")));
        assert!(output.contains(&bounded("hello\nforged\u{1b}[31m")));
    }

    #[test]
    fn status_report_preserves_legacy_failure_and_unsent_rows_byte_for_byte() {
        let mut rows = vec![row(StatusReport::Nothing); 4];
        rows[0].state = ActionRowState::Unreachable { cause: None };
        rows[1].state = ActionRowState::NotSent;
        rows[2].read_failed = true;
        rows[3].note = Some(super::super::RowNote::NoEligibleBinding);
        // A send that never reached an item keeps its TM-02 outcome label.
        for state in [
            ActionRowState::AwaitingApproval,
            ActionRowState::AcceptedWithoutItem,
            ActionRowState::AskedQuestion {
                task_id: "task-1".to_owned(),
            },
            ActionRowState::NoUsableAnswer,
            ActionRowState::Sending,
            ActionRowState::NoOutcomeRecorded,
            ActionRowState::Unrecognised,
        ] {
            let mut unsettled = row(StatusReport::Awaiting {
                defer_minutes: None,
            });
            unsettled.state = state;
            rows.push(unsettled);
        }
        for row in rows {
            let mut legacy = String::new();
            super::super::render_action_row(&mut legacy, &row, 3);
            assert_eq!(render(&row), legacy);
            assert!(!render(&row).contains("awaiting reply"));
        }
        for state in [ActionRowState::Delivered, ActionRowState::Acknowledged] {
            let mut delivered = row(StatusReport::Awaiting {
                defer_minutes: None,
            });
            delivered.state = state;
            assert_eq!(render(&delivered), "  jun   awaiting reply\n");
        }
    }

    #[test]
    fn status_report_wraps_story_lists_between_keys() {
        let stories = (1..=30)
            .map(|index| SharedStory {
                key: StoryKey::parse(&format!("S7-{index:02}")).unwrap(),
                state: None,
            })
            .collect::<Vec<_>>();
        let output = render(&row(StatusReport::Shared {
            statement: StatusStatement {
                level: Some(StatusDisclosure::Minimal),
                stories,
            },
            authorship: Authorship::AgentComposed,
            note: None,
        }));
        assert!(output.lines().count() > 3, "{output}");
        assert!(output.lines().all(|line| line.width() <= 60), "{output}");
        for index in 1..=30 {
            let key = format!("S7-{index:02}");
            assert!(
                output
                    .lines()
                    .any(|line| line.split(' ').any(|word| word == key)),
                "{key} split across rows: {output}"
            );
        }
        assert!(output.contains("(jun's policy: minimal) "), "{output}");
        let logical = output.replace("\n        ", "");
        assert!(
            logical.contains(
                &(1..=30)
                    .map(|index| format!("S7-{index:02}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            )
        );
        assert!(
            logical.contains("Source: manually shared (jun's policy: minimal) [agent-composed]")
        );
    }
}
