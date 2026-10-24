//! Offline, read-only view of one locally received recipient thread.

use std::path::Path;

use crate::adapters::policy::recipient_item::{JournalRecipientItemProjection, ThreadTurnRef};
use crate::domain::models::provenance::provenance_tags;
use crate::domain::models::{ItemScope, RecipientItemState, ReplyKind, SemanticMessageType};
use crate::domain::ports::RoomJournalReader;
use crate::domain::services::{
    recipient_thread_bounds as bounds, transparency::item_principal_label,
};
use anyhow::{Context, anyhow};

const BODY_LIMIT: usize = bounds::MAX_REPLY_BYTES;
const WITHHELD_TURN: &str = "      turn content withheld · exceeds 448 KiB\n";
const WITHHELD_PAGE: &str = "      content unavailable\n";

#[derive(Clone, Copy)]
struct TurnCost {
    full: usize,
    metadata: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceivedThreadWindow {
    Newest,
    Earlier { before: usize },
    Newer { after: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedThreadPage {
    pub opened_address: crate::domain::models::ItemAddress,
    pub root_id: String,
    pub message_type: SemanticMessageType,
    pub item_state: &'static str,
    pub context_available: bool,
    /// Complete, bounded text ready for the native Info reader.
    pub content: String,
    pub first_non_pinned_ordinal: Option<usize>,
    pub last_non_pinned_ordinal: Option<usize>,
    pub omitted_before: usize,
    pub omitted_after: usize,
}

pub(crate) fn recipient_context_available(item: &crate::domain::models::RecipientItemView) -> bool {
    matches!(
        &item.state,
        RecipientItemState::Received { content } | RecipientItemState::Acknowledged { content }
            if content.len() <= BODY_LIMIT
                && turn_cost(&ThreadTurnRef::Item(item), true).full.saturating_add(4096)
                    <= bounds::MAX_THREAD_TURN_BYTES
    )
}

/// Read one exact-principal thread from the workspace journal. This path is
/// offline and read-only: it neither composes a daemon nor contacts a peer.
pub(crate) async fn load_received_thread(
    workspace: &Path,
    item_id: &str,
    page: ReceivedThreadWindow,
) -> anyhow::Result<ReceivedThreadPage> {
    let reader =
        crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
            workspace,
        );
    let entries = reader
        .load_entries()
        .await
        .context("loading workspace room journal")?;
    let projection = JournalRecipientItemProjection::from_entries(&entries);
    let snapshot = projection.snapshot();
    let opened_header = snapshot
        .find_by_id(item_id)
        .ok_or_else(|| anyhow!("recipient item '{item_id}' not found"))?;
    let principal = opened_header.address.principal().clone();
    let scope = ItemScope::exact(principal);
    let (root_id, turns) =
        JournalRecipientItemProjection::thread_in_snapshot(&snapshot, &scope, item_id)
            .ok_or_else(|| anyhow!("received thread not found"))?;
    let opened_index = turns
        .iter()
        .position(|turn| turn.id() == item_id)
        .ok_or_else(|| anyhow!("opened item is not in its received thread"))?;
    let opened = match turns[opened_index] {
        ThreadTurnRef::Item(item) => item,
        ThreadTurnRef::Reply(_) => return Err(anyhow!("opened address is not a received item")),
    };
    let root_index = turns.iter().position(|turn| turn.id() == root_id);
    drop(projection);
    drop(entries);
    let pinned = |index: usize| Some(index) == root_index || index == opened_index;

    let opened_turn_cost = turn_cost(&turns[opened_index], true);
    let pins = [opened_index, root_index.unwrap_or(turns.len())];
    let next_candidate = match page {
        ReceivedThreadWindow::Newest => (0..turns.len()).rev().find(|index| !pinned(*index)),
        ReceivedThreadWindow::Earlier { before } => (0..before.min(turns.len()))
            .rev()
            .find(|index| !pinned(*index)),
        ReceivedThreadWindow::Newer { after } => {
            (after.saturating_add(1)..turns.len()).find(|index| !pinned(*index))
        }
    };
    let candidate_cost = next_candidate.map(|index| turn_cost(&turns[index], false));
    let root_cost = root_index
        .filter(|index| *index != opened_index)
        .map(|index| turn_cost(&turns[index], false));
    let bounded_cost = |turn: TurnCost, reserve: usize| {
        if turn.full.saturating_add(reserve) > bounds::MAX_THREAD_TURN_BYTES {
            turn.metadata + 1 + WITHHELD_TURN.len() + reserve
        } else {
            turn.full + reserve
        }
    };
    let opened_cost = bounded_cost(opened_turn_cost, 4096);
    let withhold_root_body = root_cost.zip(candidate_cost).is_some_and(|(root, next)| {
        opened_cost
            .saturating_add(bounded_cost(root, 0))
            .saturating_add(bounded_cost(next, 0))
            > bounds::MAX_THREAD_RESPONSE_BYTES
    });
    let root_render_cost = root_cost.map_or(0, |root| {
        if withhold_root_body && root.full <= bounds::MAX_THREAD_TURN_BYTES {
            root.metadata + 1 + WITHHELD_PAGE.len()
        } else {
            bounded_cost(root, 0)
        }
    });
    let non_pinned_budget = bounds::MAX_THREAD_RESPONSE_BYTES
        .saturating_sub(opened_cost.saturating_add(root_render_cost));
    let cached_turn_cost = |index: usize| {
        if index == opened_index {
            opened_turn_cost
        } else if Some(index) == root_index {
            root_cost.expect("distinct pinned root")
        } else if Some(index) == next_candidate {
            candidate_cost.expect("next page candidate")
        } else {
            turn_cost(&turns[index], false)
        }
    };
    let page_withheld = |index: usize, turn: TurnCost| {
        (Some(index) == root_index && withhold_root_body)
            || (!pinned(index) && bounded_cost(turn, 0) > non_pinned_budget)
    };
    let cost = |index: usize| {
        let turn = cached_turn_cost(index);
        if page_withheld(index, turn) && turn.full <= bounds::MAX_THREAD_TURN_BYTES {
            turn.metadata + 1 + WITHHELD_PAGE.len()
        } else {
            bounded_cost(turn, if index == opened_index { 4096 } else { 0 })
        }
    };
    let selected = match page {
        ReceivedThreadWindow::Newest => {
            bounds::window_indices(turns.len(), &pins, (0..turns.len()).rev(), cost)
        }
        ReceivedThreadWindow::Earlier { before } => {
            bounds::window_indices(turns.len(), &pins, (0..before.min(turns.len())).rev(), cost)
        }
        ReceivedThreadWindow::Newer { after } => bounds::window_indices(
            turns.len(),
            &pins,
            after.saturating_add(1)..turns.len(),
            cost,
        ),
    };
    let first = selected.iter().copied().find(|i| !pinned(*i));
    let last = selected.iter().rev().copied().find(|i| !pinned(*i));
    let (omitted_before, omitted_after) = match (first, last, page) {
        (Some(first), Some(last), _) => (
            (0..first).filter(|i| !pinned(*i)).count(),
            ((last + 1)..turns.len()).filter(|i| !pinned(*i)).count(),
        ),
        (None, None, ReceivedThreadWindow::Earlier { .. }) => {
            (0, (0..turns.len()).filter(|i| !pinned(*i)).count())
        }
        (None, None, ReceivedThreadWindow::Newer { .. }) => {
            ((0..turns.len()).filter(|i| !pinned(*i)).count(), 0)
        }
        (None, None, ReceivedThreadWindow::Newest) => {
            ((0..turns.len()).filter(|i| !pinned(*i)).count(), 0)
        }
        _ => unreachable!("first and last are both derived from the same set"),
    };

    let label = opened.alias.as_deref().map_or_else(
        || item_principal_label(opened.address.principal()),
        |alias| {
            crate::domain::services::transparency::sanitize_disclosable(
                alias,
                crate::domain::services::transparency::MAX_PEER_ID_BYTES,
            )
        },
    );
    let mut content = String::new();
    use std::fmt::Write as _;
    let canonical = semantic_type_token(opened.message_type);
    if opened.message_type == SemanticMessageType::Unknown {
        let _ = writeln!(content, "  unknown type · {label}");
    } else {
        let _ = writeln!(content, "  {canonical} · {label}");
    }
    let _ = writeln!(content, "  received thread · {label} · {root_id}");
    if root_id != item_id {
        let _ = writeln!(content, "  thread root · {root_id}");
    }
    match &opened.state {
        RecipientItemState::Received { content: body }
        | RecipientItemState::Acknowledged { content: body } => {
            let _ = writeln!(
                content,
                "  Context received: 1 text body · {} bytes",
                body.len()
            );
            if body.len() <= BODY_LIMIT
                && opened_turn_cost.full.saturating_add(4096) <= bounds::MAX_THREAD_TURN_BYTES
            {
                append_safe_body(&mut content, body);
                content.push('\n');
            } else {
                content.push_str(if body.len() > BODY_LIMIT {
                    "      content withheld · body exceeds 64 KiB read ceiling\n"
                } else {
                    WITHHELD_TURN
                });
            }
        }
        RecipientItemState::Removed { .. } => {
            content.push_str("  Context received: content unavailable · removed\n")
        }
    }
    if omitted_before > 0 {
        let _ = writeln!(
            content,
            "  {omitted_before} earlier turns not shown — [p] loads earlier turns from this host"
        );
    }
    for &index in &selected {
        let cost = cached_turn_cost(index);
        let reserve = if index == opened_index { 4096 } else { 0 };
        if cost.full + reserve > bounds::MAX_THREAD_TURN_BYTES || page_withheld(index, cost) {
            append_turn(&mut content, &turns[index], false);
            content.push('\n');
            content.push_str(if cost.full + reserve > bounds::MAX_THREAD_TURN_BYTES {
                WITHHELD_TURN
            } else {
                WITHHELD_PAGE
            });
        } else {
            append_turn(&mut content, &turns[index], index != opened_index);
            content.push('\n');
        }
    }
    if content.len() > bounds::MAX_THREAD_RESPONSE_BYTES {
        return Err(anyhow!(
            "received thread metadata exceeds the bounded page size"
        ));
    }
    Ok(ReceivedThreadPage {
        opened_address: opened.address.clone(),
        root_id,
        message_type: opened.message_type,
        item_state: opened.state.wire_name(),
        context_available: recipient_context_available(opened),
        content,
        first_non_pinned_ordinal: first,
        last_non_pinned_ordinal: last,
        omitted_before,
        omitted_after,
    })
}

fn semantic_type_token(kind: SemanticMessageType) -> &'static str {
    crate::domain::models::semantic_message_type_metadata(kind).token
}

fn turn_cost(turn: &ThreadTurnRef<'_>, opened: bool) -> TurnCost {
    match turn {
        ThreadTurnRef::Item(item) => {
            let base = item.address.item().as_str().len()
                + " · ".len()
                + match &item.state {
                    RecipientItemState::Received { .. } => "received".len(),
                    RecipientItemState::Acknowledged { .. } => "acknowledged".len(),
                    RecipientItemState::Removed {
                        acknowledged_before: true,
                    } => "removed · acknowledged before removal".len(),
                    RecipientItemState::Removed {
                        acknowledged_before: false,
                    } => "removed".len(),
                }
                + provenance_tags(item.authorship, item.send_authorization).len()
                + if item.retracted_at_ms.is_some() {
                    " · retracted by sender".len()
                } else {
                    0
                }
                + item
                    .in_reply_to
                    .as_ref()
                    .map_or(0, |parent| " · follow-up to ".len() + parent.len());
            let body = match &item.state {
                RecipientItemState::Received { content }
                | RecipientItemState::Acknowledged { content }
                    if content.len() <= BODY_LIMIT =>
                {
                    safe_display_len(content)
                }
                RecipientItemState::Received { .. } | RecipientItemState::Acknowledged { .. } => {
                    "      content withheld · body exceeds 64 KiB read ceiling".len()
                }
                RecipientItemState::Removed { .. } => "      content unavailable · removed".len(),
            };
            TurnCost {
                full: 2 + base + 2 + body,
                metadata: 2 + base,
            }
        }
        ThreadTurnRef::Reply(reply) => {
            let kind = match reply.kind {
                ReplyKind::Answer => "answer".len(),
                ReplyKind::Defer { minutes } => {
                    "[defer ".len() + decimal_digits(minutes) + "m — reported expectation]".len()
                }
                ReplyKind::Unknown => "[reply kind not recognised]".len(),
            };
            let metadata = 2
                + reply.address.turn().as_str().len()
                + " · you · ".len()
                + reply.authorship.wire_name().len()
                + " · ".len()
                + kind
                + " · ".len()
                + reply.answers.as_str().len();
            let body = if opened {
                0
            } else {
                1 + if reply.content.len() > BODY_LIMIT {
                    "      content withheld · body exceeds 64 KiB read ceiling".len()
                } else {
                    safe_display_len(&reply.content)
                }
            };
            TurnCost {
                full: metadata + body + 1,
                metadata,
            }
        }
    }
}

fn append_turn(output: &mut String, turn: &ThreadTurnRef<'_>, include_body: bool) {
    use std::fmt::Write as _;
    match turn {
        ThreadTurnRef::Item(item) => {
            let mark = match &item.state {
                RecipientItemState::Received { .. } => "received",
                RecipientItemState::Acknowledged { .. } => "acknowledged",
                RecipientItemState::Removed {
                    acknowledged_before: true,
                } => "removed · acknowledged before removal",
                RecipientItemState::Removed {
                    acknowledged_before: false,
                } => "removed",
            };
            let _ = write!(output, "  {} · {mark}", item.address.item().as_str());
            output.push_str(provenance_tags(item.authorship, item.send_authorization));
            if item.retracted_at_ms.is_some() {
                output.push_str(" · retracted by sender");
            }
            if let Some(parent) = &item.in_reply_to {
                let parent = crate::domain::services::transparency::sanitize_disclosable(
                    parent,
                    crate::domain::services::transparency::MAX_PEER_ID_BYTES,
                );
                let _ = write!(output, " · follow-up to {parent}");
            }
            if include_body {
                match &item.state {
                    RecipientItemState::Received { content }
                    | RecipientItemState::Acknowledged { content }
                        if content.len() <= BODY_LIMIT =>
                    {
                        output.push('\n');
                        append_safe_body(output, content);
                    }
                    RecipientItemState::Received { .. }
                    | RecipientItemState::Acknowledged { .. } => output
                        .push_str("\n      content withheld · body exceeds 64 KiB read ceiling"),
                    RecipientItemState::Removed { .. } => {
                        output.push_str("\n      content unavailable · removed")
                    }
                }
            }
        }
        ThreadTurnRef::Reply(reply) => {
            let _ = write!(
                output,
                "  {} · you · {} · ",
                reply.address.turn().as_str(),
                reply.authorship.wire_name()
            );
            match reply.kind {
                ReplyKind::Answer => output.push_str("answer"),
                ReplyKind::Defer { minutes } => {
                    let _ = write!(output, "[defer {minutes}m — reported expectation]");
                }
                ReplyKind::Unknown => output.push_str("[reply kind not recognised]"),
            }
            let _ = write!(output, " · {}", reply.answers.as_str());
            if include_body {
                output.push('\n');
                if reply.content.len() <= BODY_LIMIT {
                    append_safe_body(output, &reply.content);
                } else {
                    output.push_str("      content withheld · body exceeds 64 KiB read ceiling");
                }
            }
        }
    }
}

pub(crate) fn safe_display_len(text: &str) -> usize {
    6 + text
        .chars()
        .map(|ch| match ch {
            '\n' => 7,
            '\r' | '\t' => 2,
            ch if ch.is_control() => 4 + hex_digits(ch as u32),
            ch => ch.len_utf8(),
        })
        .sum::<usize>()
}

fn hex_digits(mut value: u32) -> usize {
    let mut digits = 1;
    while value >= 16 {
        value >>= 4;
        digits += 1;
    }
    digits
}

fn decimal_digits(mut value: u16) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

pub(crate) fn append_safe_body(output: &mut String, text: &str) {
    use std::fmt::Write as _;
    output.push_str("      ");
    for ch in text.chars() {
        match ch {
            '\n' => output.push_str("\n      "),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(output, "\\u{{{:x}}}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn story19_20_recipient_view_renders_sender_alias_and_independent_tags() {
        use crate::domain::models::{
            Authorship, ItemAddress, ItemId, PeerId, RoomEvent, SendAuthorization,
        };
        let workspace = tempfile::tempdir().unwrap();
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .unwrap();
        let address = ItemAddress::from_a2a_ingress(
            PeerId::from_public_key(&[74; 32]).unwrap(),
            ItemId::from_replay("ri_marks"),
        );
        journal
            .append_room(RoomEvent::RecipientItemReceived {
                address,
                task: "task-marks".into(),
                alias: Some("arrival-alias".into()),
                message_type: SemanticMessageType::BugReport,
                authorship: Authorship::AgentComposed,
                send_authorization: SendAuthorization::Automatic,
                content: "bug body".into(),
                in_reply_to: None,
                capability_hash: None,
                reply_capability_matched: false,
            })
            .await
            .unwrap();
        let page = load_received_thread(workspace.path(), "ri_marks", ReceivedThreadWindow::Newest)
            .await
            .unwrap();
        assert!(
            page.content.contains("bug_report · arrival-alias"),
            "{}",
            page.content
        );
        assert!(
            page.content
                .contains("ri_marks · received [agent-composed] [auto-sent]"),
            "{}",
            page.content
        );
    }

    #[tokio::test]
    async fn story19_20_view_fact_matrix_alias_bounds_and_cost_parity() {
        use crate::domain::models::{
            Authorship, ItemAddress, ItemId, PeerId, RoomEvent, SendAuthorization,
        };
        use crate::domain::services::transparency::{MAX_PEER_ID_BYTES, sanitize_disclosable};
        let workspace = tempfile::tempdir().expect("workspace");
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .expect("journal");
        let aliases = [
            None,
            Some("arrival-alias".to_string()),
            Some("arrival\n\r\t\u{1b}alias".to_string()),
            Some("界\n".repeat(MAX_PEER_ID_BYTES * 2)),
            Some(String::new()),
        ];
        for authorship in [
            Authorship::Unknown,
            Authorship::HumanWritten,
            Authorship::AgentComposed,
        ] {
            for authorization in [
                SendAuthorization::Unknown,
                SendAuthorization::HumanDecided,
                SendAuthorization::Automatic,
            ] {
                for (index, alias) in aliases.iter().enumerate() {
                    let id = format!(
                        "marks-{}-{}-{index}",
                        authorship.wire_name(),
                        authorization.wire_name(),
                    );
                    let address = ItemAddress::from_a2a_ingress(
                        PeerId::from_public_key(&[75; 32]).expect("peer"),
                        ItemId::from_replay(&id),
                    );
                    let label = alias.as_deref().map_or_else(
                        || item_principal_label(address.principal()),
                        |alias| sanitize_disclosable(alias, MAX_PEER_ID_BYTES),
                    );
                    journal
                        .append_room(RoomEvent::RecipientItemReceived {
                            address,
                            task: id.clone(),
                            alias: alias.clone(),
                            message_type: SemanticMessageType::BugReport,
                            authorship,
                            send_authorization: authorization,
                            content: "body\nwith\tcontrols\u{1b}".into(),
                            in_reply_to: None,
                            capability_hash: None,
                            reply_capability_matched: false,
                        })
                        .await
                        .expect("append item");
                    let page =
                        load_received_thread(workspace.path(), &id, ReceivedThreadWindow::Newest)
                            .await
                            .expect("bounded view opens");
                    assert!(page.content.starts_with(&format!(
                        "  bug_report · {label}\n  received thread · {label} · {id}\n",
                    )));
                    assert!(
                        label.len()
                            <= MAX_PEER_ID_BYTES
                                + crate::domain::services::transparency::TRUNCATION_MARKER.len()
                    );
                    assert!(!label.chars().any(char::is_control));
                    assert!(page.content.len() <= bounds::MAX_THREAD_RESPONSE_BYTES);
                    let tags = match (
                        authorship == Authorship::AgentComposed,
                        authorization == SendAuthorization::Automatic,
                    ) {
                        (false, false) => "",
                        (true, false) => " [agent-composed]",
                        (false, true) => " [auto-sent]",
                        (true, true) => " [agent-composed] [auto-sent]",
                    };
                    assert!(page.content.contains(&format!("{id} · received{tags}\n")));
                    let reader = crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(workspace.path());
                    let entries = reader.load_entries().await.expect("entries");
                    let projection = JournalRecipientItemProjection::from_entries(&entries);
                    let snapshot = projection.snapshot();
                    let mut item = snapshot.find_by_id(&id).expect("item").clone();
                    item.in_reply_to = Some("parent".into());
                    item.retracted_at_ms = Some(1);
                    for state in [
                        RecipientItemState::Received {
                            content: "body\n\t\u{1b}".into(),
                        },
                        RecipientItemState::Acknowledged {
                            content: "body\n\t\u{1b}".into(),
                        },
                        RecipientItemState::Removed {
                            acknowledged_before: false,
                        },
                        RecipientItemState::Removed {
                            acknowledged_before: true,
                        },
                    ] {
                        item.state = state;
                        let turn = ThreadTurnRef::Item(&item);
                        let cost = turn_cost(&turn, false);
                        let mut metadata = String::new();
                        append_turn(&mut metadata, &turn, false);
                        assert_eq!(metadata.len(), cost.metadata);
                        assert!(metadata.contains(&format!(
                            "{tags} · retracted by sender · follow-up to parent",
                        )));
                        let mut rendered = String::new();
                        append_turn(&mut rendered, &turn, true);
                        rendered.push('\n');
                        assert_eq!(rendered.len(), cost.full);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn k07_earlier_page_respects_requested_boundary_instead_of_reopening_newest() {
        use crate::domain::models::{ItemAddress, ItemId, PeerId, RoomEvent, SemanticMessageType};
        let workspace = tempfile::tempdir().expect("workspace");
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .expect("workspace journal");
        let peer = PeerId::from_public_key(&[73; 32]).expect("peer id");
        let mut previous = None;
        for index in 0..70 {
            let id = format!("k07-page-{index}");
            journal
                .append_room(RoomEvent::RecipientItemReceived {
                    address: ItemAddress::from_a2a_ingress(peer.clone(), ItemId::from_replay(&id)),
                    task: format!("task-{index}"),
                    alias: None,
                    message_type: SemanticMessageType::Unknown,
                    content: format!("page-body-{index}"),
                    in_reply_to: previous.clone(),
                    capability_hash: None,
                    reply_capability_matched: false,
                    authorship: crate::domain::models::Authorship::Unknown,
                    send_authorization: crate::domain::models::SendAuthorization::Unknown,
                })
                .await
                .expect("append linked received item");
            previous = Some(id);
        }
        let page = load_received_thread(
            workspace.path(),
            previous.as_deref().expect("opened item"),
            ReceivedThreadWindow::Earlier { before: 20 },
        )
        .await
        .expect("earlier page");
        assert!(
            page.content.contains("page-body-19"),
            "K07: earlier page contains its requested older interval"
        );
        assert!(
            !page.content.contains("page-body-68"),
            "K07: earlier page does not silently return the newest interval"
        );
    }
    #[tokio::test]
    async fn story19_19_review_pinned_bodies_leave_truthful_thread_paging() {
        use crate::adapters::tui::state::RecipientSurfaceAccess;
        use crate::domain::models::{
            FocusState, ItemAddress, ItemId, PeerId, RoomEvent, SemanticMessageType,
        };
        let workspace = tempfile::tempdir().expect("workspace");
        let journal = crate::infrastructure::subagent::node_journal::NodeJournal::open_workspace(
            workspace.path(),
        )
        .await
        .expect("workspace journal");
        let peer = PeerId::from_public_key(&[91; 32]).expect("peer id");
        let root_body = format!("r{}r", "\n".repeat(65_498));
        let opened_body = format!("o{}o", "\n".repeat(64_798));
        assert_eq!(root_body.len(), 65_500);
        assert_eq!(opened_body.len(), 64_800);
        let pinned_cost = safe_display_len(&root_body) + safe_display_len(&opened_body) + 4_096;
        let middle_cost = safe_display_len(&"m".repeat(8 * 1024));
        assert!(pinned_cost < bounds::MAX_THREAD_RESPONSE_BYTES);
        assert!(
            pinned_cost + middle_cost > bounds::MAX_THREAD_RESPONSE_BYTES,
            "pin reserve should leave less room than the ordinary middle turn needs"
        );
        let append =
            |id: &str, content: String, parent: Option<String>| {
                let journal = &journal;
                let peer = peer.clone();
                let id = id.to_string();
                async move {
                    journal
                        .append_room(RoomEvent::RecipientItemReceived {
                            address: ItemAddress::from_a2a_ingress(peer, ItemId::from_replay(&id)),
                            task: id.clone(),
                            alias: Some("界\n".repeat(
                                crate::domain::services::transparency::MAX_PEER_ID_BYTES * 2,
                            )),
                            message_type: SemanticMessageType::Unknown,
                            content,
                            in_reply_to: parent,
                            capability_hash: None,
                            reply_capability_matched: false,
                            authorship: crate::domain::models::Authorship::AgentComposed,
                            send_authorization: crate::domain::models::SendAuthorization::Automatic,
                        })
                        .await
                        .expect("append received item");
                }
            };
        append("review-root", root_body.clone(), None).await;
        append(
            "review-middle",
            "m".repeat(8 * 1024),
            Some("review-root".into()),
        )
        .await;
        let mut previous = "review-middle".to_string();
        for index in 0..60 {
            let id = format!("review-reply-{index}");
            append(&id, "x".repeat(8 * 1024), Some(previous)).await;
            previous = id;
        }
        append("review-opened", opened_body.clone(), Some(previous)).await;
        let reader =
            crate::infrastructure::subagent::node_journal::WorkspaceJournalReader::open_workspace(
                workspace.path(),
            );
        let entries = reader.load_entries().await.expect("entries");
        let projection = JournalRecipientItemProjection::from_entries(&entries);
        let snapshot = projection.snapshot();
        let opened = snapshot.find_by_id("review-opened").expect("opened");
        let scope = ItemScope::exact(opened.address.principal().clone());
        let (_, turns) =
            JournalRecipientItemProjection::thread_in_snapshot(&snapshot, &scope, "review-opened")
                .expect("thread");
        for turn in turns {
            let mut rendered = String::new();
            append_turn(&mut rendered, &turn, true);
            rendered.push('\n');
            assert_eq!(rendered.len(), turn_cost(&turn, false).full);
            assert!(rendered.contains(" [agent-composed] [auto-sent]"));
        }

        let initial = load_received_thread(
            workspace.path(),
            "review-opened",
            ReceivedThreadWindow::Newest,
        )
        .await
        .expect("initial page");
        assert_eq!(initial.root_id, "review-root");
        assert_eq!(initial.opened_address.item().as_str(), "review-opened");
        assert!(initial.content.len() <= bounds::MAX_THREAD_RESPONSE_BYTES);
        let rendered_body = |body: &str| {
            let mut rendered = String::new();
            append_safe_body(&mut rendered, body);
            rendered.push('\n');
            rendered
        };
        assert!(initial.content.contains("review-root"));
        let rendered_opened = rendered_body(&opened_body);
        assert!(initial.content.contains(&rendered_opened));
        assert_eq!(initial.content.matches(&rendered_opened).count(), 1);
        assert!(
            !initial
                .content
                .contains(&rendered_body(&"m".repeat(8 * 1024)))
        );
        assert!(initial.omitted_before > 0);

        let mut state = crate::adapters::tui::state::TuiState::new(80, 24);
        state.focus = FocusState::Input;
        crate::infrastructure::runtime::recipient_surface::open_received_surface(
            workspace.path(),
            None,
            "review-opened",
            &mut state,
            RecipientSurfaceAccess::ReadOnly,
            ReceivedThreadWindow::Newest,
        )
        .await
        .expect("open surface");
        let (first_omitted, first_ordinal) = {
            let first = state.recipient_surface.as_ref().expect("surface");
            (
                first.omitted_before,
                first
                    .first_non_pinned_ordinal
                    .expect("bounded newest page includes a linked turn"),
            )
        };
        state
            .recipient_surface
            .as_mut()
            .expect("surface")
            .scroll_offset = 9;
        crate::infrastructure::runtime::recipient_surface::navigate_received_surface(
            workspace.path(),
            None,
            &mut state,
            true,
        )
        .await
        .expect("earlier navigation");
        let earlier = state.recipient_surface.as_ref().expect("earlier surface");
        assert_eq!(earlier.root_id.as_deref(), Some("review-root"));
        assert!(earlier.content.len() <= bounds::MAX_THREAD_RESPONSE_BYTES);
        assert!(
            earlier
                .first_non_pinned_ordinal
                .is_some_and(|ordinal| ordinal < first_ordinal)
                && earlier.omitted_before < first_omitted,
            "K07: earlier navigation must advance a real linked turn, not only omission counters"
        );
        assert!(
            earlier.content.contains(&rendered_opened)
                && earlier.content.matches(&rendered_opened).count() == 1
        );
        assert_eq!(state.focus, FocusState::Chat);
    }
}
