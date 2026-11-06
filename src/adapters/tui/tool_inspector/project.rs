//! Pure projection from canonical turn parts, without copying result bodies.

use crate::domain::models::{Turn, TurnPart, turn::tool_call_id_matches};
use crate::domain::services::reducer::ReducerState;

use super::model::{ToolTarget, ToolView};

/// Resolve an exact call id in the supplied conversation's committed/open turns.
///
/// The host must first match `target.conversation_id` to the conversation it
/// supplies. Part ids are turn-local: only an invocation resolved in `open_turn`
/// may read the reducer's per-call tail. Prefer the open turn when a caller's
/// snapshot also includes it among committed turns.
pub fn project<'a>(
    turns: &'a [Turn],
    open_turn: Option<&'a Turn>,
    tails: &'a ReducerState,
    target: &'a ToolTarget,
) -> Option<ToolView<'a>> {
    let mut view = without_tail(turns, open_turn, target)?;
    if view.is_running()
        && view.result.is_none()
        && open_turn.is_some_and(|turn| std::ptr::eq(&turn.id, view.turn_id))
    {
        if let Some((tail, revision)) = tails.tail_source_for(view.part_id) {
            view.tail = Some(tail);
            view.tail_revision = revision;
        }
    }
    Some(view)
}

/// The same borrowed projection when the host has no reducer at open time.
/// Refresh can subsequently attach the matching call's tail via [`project`].
pub fn without_tail<'a>(
    turns: &'a [Turn],
    open_turn: Option<&'a Turn>,
    target: &'a ToolTarget,
) -> Option<ToolView<'a>> {
    if let Some(turn) = open_turn {
        if let Some(view) = in_turn(turn, target) {
            return Some(view);
        }
    }
    turns.iter().find_map(|turn| in_turn(turn, target))
}

/// Presence-only discovery never allocates a formatted id for each candidate.
pub fn contains_call(turns: &[Turn], open_turn: Option<&Turn>, call_id: &str) -> bool {
    open_turn.into_iter().chain(turns).any(|turn| {
        turn.parts.iter().any(|part| {
            matches!(part, TurnPart::ToolInvocation { id, .. }
                if tool_call_id_matches(&turn.id, *id, call_id))
        })
    })
}

fn in_turn<'a>(turn: &'a Turn, target: &'a ToolTarget) -> Option<ToolView<'a>> {
    let mut ordinal = 0;
    for part in &turn.parts {
        let TurnPart::ToolInvocation {
            id,
            tool,
            args,
            status,
            started_at,
            ended_at,
        } = part
        else {
            continue;
        };
        ordinal += 1;
        if !tool_call_id_matches(&turn.id, *id, &target.tool_call_id) {
            continue;
        }
        // The Stage's result map is last-wins; retain that rule while borrowing.
        let result = turn.parts.iter().rev().find_map(|part| match part {
            TurnPart::ToolResult { refs, output, .. } if refs == id => Some(output),
            _ => None,
        });
        let view = ToolView {
            target,
            turn_id: &turn.id,
            part_id: *id,
            name: tool,
            status,
            ordinal,
            args,
            result,
            tail: None,
            started_at: *started_at,
            ended_at: *ended_at,
            source_revision: turn.parts.revision(),
            tail_revision: 0,
        };
        return Some(view);
    }
    None
}

#[derive(Clone, Copy)]
pub struct Neighbour<'a> {
    pub turn_id: &'a crate::domain::models::TurnId,
    pub part_id: crate::domain::models::PartId,
}

pub struct Neighbours<'a> {
    pub previous: Option<Neighbour<'a>>,
    pub next: Option<Neighbour<'a>>,
}

/// Borrow adjacent invocations from exactly the resolved invocation's turn.
pub fn neighbours<'a>(
    turns: &'a [Turn],
    open_turn: Option<&'a Turn>,
    target: &'a ToolTarget,
) -> Option<Neighbours<'a>> {
    let view = without_tail(turns, open_turn, target)?;
    let turn = open_turn
        .into_iter()
        .chain(turns)
        .find(|turn| std::ptr::eq(&turn.id, view.turn_id))?;
    let mut invocations = turn.parts.iter().filter_map(|part| match part {
        TurnPart::ToolInvocation { id, .. } => Some(Neighbour {
            turn_id: &turn.id,
            part_id: *id,
        }),
        _ => None,
    });
    let mut previous = None;
    while let Some(current) = invocations.next() {
        if current.part_id == view.part_id {
            return Some(Neighbours {
                previous,
                next: invocations.next(),
            });
        }
        previous = Some(current);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        InvocationStatus, PartId, ToolOutput, TurnId, WriteDiffState, tool_call_id_for,
    };
    use serde_json::json;
    use std::time::Instant;

    fn invocation(turn: &mut Turn, name: &str) -> PartId {
        turn.push_part(|id| TurnPart::ToolInvocation {
            id,
            tool: name.to_owned(),
            args: json!({"command": name}),
            status: InvocationStatus::Running,
            started_at: 1_000,
            ended_at: None,
        })
    }

    fn target(turn: &Turn, part: PartId) -> ToolTarget {
        ToolTarget {
            conversation_id: "conversation".into(),
            tool_call_id: tool_call_id_for(&turn.id, part),
        }
    }

    #[test]
    fn story21_5_projection_joins_refs_not_result_order_and_borrows() {
        let mut turn = Turn::new("model".into(), 0);
        turn.id = TurnId("turn_with_underscores".into());
        turn.push_part(|id| TurnPart::Prose {
            id,
            text: "before".into(),
        });
        let first = invocation(&mut turn, "Bash");
        let second = invocation(&mut turn, "Edit");
        for (refs, content) in [(second, "second result"), (first, "first result")] {
            turn.push_part(|id| TurnPart::ToolResult {
                id,
                refs,
                output: ToolOutput {
                    content: content.into(),
                    is_error: false,
                    diff: WriteDiffState::NotAWrite,
                    images: Vec::new(),
                },
            });
        }
        let target = target(&turn, second);
        let reducer = ReducerState::new(0, Instant::now());
        let turns = [turn];
        let view = project(&turns, None, &reducer, &target).unwrap();
        assert_eq!(view.ordinal, 2);
        assert_eq!(view.name, "Edit");
        assert_eq!(view.part_id, second);
        assert_eq!(view.result.unwrap().content, "second result");
        let TurnPart::ToolResult { output, .. } = &turns[0].parts[3] else {
            panic!("second result fixture");
        };
        assert!(std::ptr::eq(view.result.unwrap(), output));
        let TurnPart::ToolInvocation { args, .. } = &turns[0].parts[2] else {
            panic!("second invocation fixture");
        };
        assert!(std::ptr::eq(view.args, args));
    }

    #[test]
    fn story21_5_projection_uses_only_its_own_open_turn_tail() {
        let mut open = Turn::new("model".into(), 0);
        let first = invocation(&mut open, "Bash");
        let second = invocation(&mut open, "Bash");
        let unknown = invocation(&mut open, "Bash");
        let mut reducer = ReducerState::new(0, Instant::now());
        reducer.pending_invocations.insert("wire-a".into(), first);
        reducer.pending_invocations.insert("wire-b".into(), second);
        reducer.set_tail("wire-a", "A1".into());
        reducer.set_tail("wire-b", "B1".into());
        for (part, expected) in [(first, Some("A1")), (second, Some("B1")), (unknown, None)] {
            let target = target(&open, part);
            assert_eq!(
                project(&[], Some(&open), &reducer, &target).unwrap().tail,
                expected
            );
        }
        // A different turn's PartId(0) must not borrow the running call's A1.
        let mut committed = Turn::new("model".into(), 0);
        let part = invocation(&mut committed, "Bash");
        assert_eq!(part, first);
        let target = target(&committed, part);
        assert_eq!(
            project(&[committed], Some(&open), &reducer, &target)
                .unwrap()
                .tail,
            None
        );
    }

    #[test]
    fn story21_5_projection_rejects_stale_and_prefix_ids() {
        let mut turn = Turn::new("model".into(), 0);
        let part = invocation(&mut turn, "Bash");
        let mut target = target(&turn, part);
        let reducer = ReducerState::new(0, Instant::now());
        let canonical = target.tool_call_id.clone();
        assert!(project(std::slice::from_ref(&turn), None, &reducer, &target).is_some());
        for invalid in [
            format!("{canonical}_stale"),
            format!("{canonical}0"),
            canonical.trim_end_matches('0').to_owned(),
            format!("tc_wrong_turn_{}", part.0),
        ] {
            target.tool_call_id = invalid;
            assert!(project(std::slice::from_ref(&turn), None, &reducer, &target).is_none());
        }
    }
}
