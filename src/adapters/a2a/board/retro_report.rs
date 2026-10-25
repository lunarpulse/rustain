//! Story 19.25 D6/D8 — the aggregate retrospective report that replaces the
//! action board for a `retro_request` action. ⛔ No rows, no aliases, no
//! addressed count, no per-member state.

use futures::future::join_all;

use super::{ActionBoardRow, ActionBoardView, ActionRowState, attempted_age, bounded};
use crate::adapters::a2a::driver::A2aDelegationRuntime;
use crate::adapters::a2a::send::{ServedThread, ThreadRead, read_thread};
use crate::domain::models::{ReplyKind, RetroContribution, RetroReport};

/// What one row's served thread contributed: at most one contribution and
/// the generic flags, never an identity.
#[derive(Debug, Default, PartialEq, Eq)]
struct RowRetro {
    contribution: Option<RetroContribution>,
    omitted: bool,
    unreadable: bool,
}

/// The ONE direct Retro reply to `item_id` (newest served first). Item turns,
/// linked follow-ups and replies to other items never count.
fn retro_for_item(thread: &ServedThread, item_id: &str) -> RowRetro {
    let mut found = RowRetro {
        omitted: thread.omitted_before > 0,
        ..RowRetro::default()
    };
    let direct = thread
        .turns
        .iter()
        .rev()
        .filter(|turn| turn.reply && turn.in_reply_to.as_deref() == Some(item_id));
    for turn in direct {
        match turn.kind {
            ReplyKind::Unknown => found.unreadable = true,
            ReplyKind::Retro if found.contribution.is_none() => match &turn.retro {
                Some(contribution) => found.contribution = Some(contribution.clone()),
                None => found.unreadable = true,
            },
            _ => {}
        }
    }
    found
}

/// The one served thread a row is read through: its maximum-ordinal
/// action-correlated item via the bound alias. `Err` = the row cannot be read
/// from this host (a failed read, or no eligible binding whatever its send
/// outcome): noted like a failed read, never silently dropped. `Ok(None)` =
/// nothing to read (no correlated item).
fn read_target(row: &ActionBoardRow) -> Result<Option<(&str, &str)>, ()> {
    let Some(alias) = row.bound_alias.as_deref().filter(|_| !row.read_failed) else {
        return Err(());
    };
    Ok(row
        .items
        .iter()
        .max_by_key(|item| item.ordinal)
        .map(|item| (alias, item.item_id.as_str())))
}

/// Per row: the maximum-ordinal action-correlated item, one `read_thread`
/// through the bound alias, one Retro contribution. Returns the summed report
/// and whether any read thread reported a collapsed principal.
pub(super) async fn collect_retro_report(
    runtime: &A2aDelegationRuntime,
    rows: &[ActionBoardRow],
    unnameable: usize,
) -> (RetroReport, bool) {
    let reads = join_all(rows.iter().map(|row| async move {
        let (alias, item_id) = match read_target(row) {
            Ok(Some(target)) => target,
            Ok(None) => return Ok(None),
            Err(()) => return Err(()),
        };
        match read_thread(runtime, alias, item_id).await {
            ThreadRead::Read { thread, .. } => Ok(Some((
                retro_for_item(&thread, item_id),
                thread.principal_collapsed,
            ))),
            _ => Err(()),
        }
    }))
    .await;
    let mut report_rows = Vec::with_capacity(reads.len());
    let mut read_failed = false;
    let mut collapsed = false;
    for read in reads {
        match read {
            Ok(Some((row, row_collapsed))) => {
                collapsed |= row_collapsed;
                report_rows.push(row);
            }
            Ok(None) => {}
            Err(()) => read_failed = true,
        }
    }
    let unknown_outcomes = unnameable > 0
        || rows
            .iter()
            .any(|row| row.state == ActionRowState::Unrecognised);
    (
        aggregate(&report_rows, read_failed, unknown_outcomes),
        collapsed,
    )
}

fn aggregate(rows: &[RowRetro], read_failed: bool, unknown_outcomes: bool) -> RetroReport {
    let mut report =
        RetroReport::from_contributions(rows.iter().filter_map(|row| row.contribution.as_ref()));
    report.read_failed = read_failed;
    report.omitted_turns = rows.iter().any(|row| row.omitted);
    report.unreadable = rows.iter().any(|row| row.unreadable);
    report.unknown_outcomes = unknown_outcomes;
    report
}

/// The canonical D6 text. The subject is sanitized here, once.
pub(super) fn render_retro_report(view: &ActionBoardView, now_unix_secs: i64) -> String {
    let subject = view.subject.as_deref().map(bounded);
    let attempted = attempted_age(view.recorded_at_ms, now_unix_secs);
    let empty;
    let report = match &view.retro_report {
        Some(report) => report,
        None => {
            empty = RetroReport::default();
            &empty
        }
    };
    report.render(subject.as_deref(), &attempted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::a2a::board::{BoardItem, BoardItemState, render_action_board};
    use crate::adapters::a2a::send::ServedTurn;
    use crate::domain::models::{
        Authorship, RETRO_HONESTY_LINE, SemanticMessageType, SendActionId,
    };
    use std::collections::BTreeMap;

    fn share() -> RetroContribution {
        RetroContribution {
            received: BTreeMap::from([("consultation".to_owned(), 3)]),
            reply_minutes: vec![25],
            ack_minutes: vec![70],
        }
    }

    fn turn(kind: ReplyKind, in_reply_to: &str, retro: Option<RetroContribution>) -> ServedTurn {
        ServedTurn {
            turn_id: "t".to_owned(),
            reply: true,
            task: None,
            in_reply_to: Some(in_reply_to.to_owned()),
            removed: false,
            content: None,
            content_hash: None,
            kind,
            status: None,
            retro,
            authorship: Authorship::HumanWritten,
            retracted_at_ms: None,
        }
    }

    fn thread(turns: Vec<ServedTurn>, omitted_before: usize) -> ServedThread {
        ServedThread {
            root: "item".to_owned(),
            turns,
            omitted_before,
            principal_collapsed: false,
            principal_bound: true,
        }
    }

    fn row(alias: &str, state: ActionRowState) -> ActionBoardRow {
        ActionBoardRow {
            alias: alias.to_owned(),
            bound_alias: Some(alias.to_owned()),
            state,
            read_failed: false,
            note: None,
            items: vec![BoardItem {
                item_id: format!("{alias}-item"),
                task: None,
                state: BoardItemState::Received,
                ordinal: 1,
                retracted_at_ms: None,
                reply_count: None,
            }],
            status_report: None,
        }
    }

    fn view(report: RetroReport) -> ActionBoardView {
        ActionBoardView {
            action: SendActionId::from_replay("sa_retro"),
            message_type: SemanticMessageType::RetroRequest,
            subject: Some("sprint-7".to_owned()),
            recorded_at_ms: 1_700_000_000_000,
            addressed: 4,
            rows: vec![
                row("jun-dev", ActionRowState::Delivered),
                row("mia-qa", ActionRowState::Declined),
            ],
            unnameable: 1,
            principal_collapsed: true,
            retro_report: Some(report),
        }
    }

    fn shared(contribution: Option<RetroContribution>) -> RowRetro {
        RowRetro {
            contribution,
            ..RowRetro::default()
        }
    }

    /// D5 — (share + decline) ≡ (share + nothing) ≡ (share + unanswered) ≡
    /// (share alone), byte for byte through the production renderer.
    #[test]
    fn retro_report_is_identical_across_the_four_configurations() {
        let alone = aggregate(&[shared(Some(share()))], false, false);
        let configurations = [
            vec![shared(Some(share())), shared(None)],
            vec![
                shared(Some(share())),
                shared(Some(RetroContribution::nothing())),
            ],
            vec![shared(None), shared(Some(share()))],
        ];
        let expected = render_action_board(&view(alone), 1_700_000_060);
        for rows in configurations {
            let text = render_action_board(&view(aggregate(&rows, false, false)), 1_700_000_060);
            assert_eq!(text, expected);
        }
        assert!(expected.starts_with("Retrospective report · retro_request\nsprint-7\nattempted "));
        assert!(expected.contains("median 25m · 1 replies"), "{expected}");
        assert!(expected.ends_with(&format!("{RETRO_HONESTY_LINE}\n")));
    }

    /// A row this host cannot read — a failed read, or no eligible binding
    /// whatever its send outcome — is noted, never silently dropped; a bound
    /// row with no correlated item has nothing to read.
    #[test]
    fn retro_report_notes_rows_it_cannot_read_regardless_of_outcome() {
        let bound = row("jun-dev", ActionRowState::Delivered);
        assert_eq!(read_target(&bound), Ok(Some(("jun-dev", "jun-dev-item"))));
        let mut no_item = bound.clone();
        no_item.items.clear();
        assert_eq!(read_target(&no_item), Ok(None));
        let mut failed = bound.clone();
        failed.read_failed = true;
        assert_eq!(read_target(&failed), Err(()));
        for state in [
            ActionRowState::Delivered,
            ActionRowState::Declined,
            ActionRowState::AwaitingApproval,
        ] {
            let mut unbound = row("mia-qa", state.clone());
            unbound.bound_alias = None;
            assert_eq!(read_target(&unbound), Err(()), "{state:?}");
            unbound.items.clear();
            assert_eq!(read_target(&unbound), Err(()), "{state:?}");
        }
    }

    /// D6 — no rows, no aliases, no addressed count, no per-member footers.
    #[test]
    fn retro_report_renders_no_alias_count_or_member_rows() {
        let text = render_action_board(
            &view(aggregate(&[shared(Some(share()))], false, false)),
            1_700_000_060,
        );
        for leaked in [
            "jun-dev",
            "mia-qa",
            "addressed",
            "Distribution board",
            "roster peer",
            "●",
            "✗",
            "collapsed",
        ] {
            assert!(!text.contains(leaked), "{leaked} leaked: {text}");
        }
        let nothing = render_action_board(&view(RetroReport::default()), 1_700_000_060);
        assert!(
            nothing.contains("Nothing has been shared so far.\n"),
            "{nothing}"
        );
        let mut absent = view(RetroReport::default());
        absent.retro_report = None;
        assert_eq!(render_action_board(&absent, 1_700_000_060), nothing);
    }

    /// D6/D8 notes: each at most once, never an alias or count.
    #[test]
    fn retro_report_notes_render_once_each() {
        let rows = [
            RowRetro {
                omitted: true,
                ..RowRetro::default()
            },
            RowRetro {
                omitted: true,
                unreadable: true,
                ..RowRetro::default()
            },
            RowRetro {
                unreadable: true,
                ..RowRetro::default()
            },
        ];
        let text = render_action_board(&view(aggregate(&rows, true, true)), 1_700_000_060);
        for note in [
            "Some replies could not be read now; read the report again later.\n",
            "Some replies may be in thread turns a peer did not serve.\n",
            "Some replies are not readable by this build.\n",
            "Some send outcomes are not recognised by this build.\n",
        ] {
            assert_eq!(text.matches(note).count(), 1, "{note}: {text}");
        }
        let clean = render_action_board(&view(aggregate(&[], false, false)), 1_700_000_060);
        assert!(!clean.contains("Some "), "{clean}");
    }

    /// D6 — only the ONE direct Retro reply to the selected item counts.
    #[test]
    fn retro_report_selects_the_direct_retro_turn_only() {
        let other = RetroContribution {
            received: BTreeMap::from([("other".to_owned(), 9)]),
            reply_minutes: vec![1],
            ack_minutes: vec![],
        };
        let mut followup = turn(ReplyKind::Retro, "followup", Some(other.clone()));
        followup.in_reply_to = Some("followup".to_owned());
        let mut item_turn = turn(ReplyKind::Retro, "item", Some(other.clone()));
        item_turn.reply = false;
        let picked = retro_for_item(
            &thread(
                vec![
                    turn(ReplyKind::Retro, "item", Some(share())),
                    followup,
                    item_turn,
                    turn(ReplyKind::Answer, "item", None),
                ],
                3,
            ),
            "item",
        );
        assert_eq!(
            picked,
            RowRetro {
                omitted: true,
                ..shared(Some(share()))
            }
        );
        let unrelated = retro_for_item(
            &thread(vec![turn(ReplyKind::Retro, "sibling", Some(other))], 0),
            "item",
        );
        assert_eq!(unrelated, RowRetro::default());
        let omitted = retro_for_item(&thread(vec![], 2), "item");
        assert!(omitted.omitted && omitted.contribution.is_none());
        assert!(
            retro_for_item(
                &thread(vec![turn(ReplyKind::Unknown, "item", None)], 0),
                "item"
            )
            .unreadable
        );
        assert!(
            retro_for_item(
                &thread(vec![turn(ReplyKind::Retro, "item", None)], 0),
                "item"
            )
            .unreadable
        );
        let older_unknown = retro_for_item(
            &thread(
                vec![
                    turn(ReplyKind::Unknown, "item", None),
                    turn(ReplyKind::Retro, "item", Some(share())),
                ],
                0,
            ),
            "item",
        );
        assert!(older_unknown.unreadable);
        assert_eq!(older_unknown.contribution, Some(share()));
    }

    /// Non-retro views keep the distribution board.
    #[test]
    fn retro_report_does_not_touch_other_types() {
        let mut other = view(RetroReport::default());
        other.message_type = SemanticMessageType::StoryAssignment;
        other.retro_report = None;
        assert!(render_action_board(&other, 1_700_000_060).starts_with("Distribution board · "));
    }
}
