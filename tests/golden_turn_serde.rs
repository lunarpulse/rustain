//! AC5: Round-trip serde golden test for `Turn`.

use rustain::domain::models::turn::*;

#[test]
fn golden_turn_serde_round_trip() {
    let mut turn = Turn::new("claude-3-5-sonnet".into(), 1_704_000_000_000);

    turn.push_part(|id| TurnPart::Prose {
        id,
        text: "Reading the file.".into(),
    });

    let _read_id = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: "read_file".into(),
        args: serde_json::json!({"path": "src/main.rs"}),
        status: InvocationStatus::Success,
        started_at: 1_704_000_001_000,
        ended_at: Some(1_704_000_002_000),
    });

    turn.push_part(|id| TurnPart::Prose {
        id,
        text: "Looks like the bug is on line 42.".into(),
    });

    let edit_id = turn.push_part(|id| TurnPart::ToolInvocation {
        id,
        tool: "edit_file".into(),
        args: serde_json::json!({"path": "src/main.rs", "old": "foo", "new": "bar"}),
        status: InvocationStatus::Success,
        started_at: 1_704_000_003_000,
        ended_at: Some(1_704_000_004_000),
    });

    turn.push_part(|id| TurnPart::Prose {
        id,
        text: "Done.".into(),
    });

    turn.push_part(|id| TurnPart::Reasoning {
        id,
        text: "The edit_file tool will fix the off-by-one.".into(),
    });

    turn.push_part(|id| TurnPart::ToolResult {
        id,
        refs: edit_id,
        output: ToolOutput {
            content: "Applied edit.".into(),
            is_error: false,
        },
    });

    let json = serde_json::to_string(&turn).unwrap();

    // Tag verification (camelCase)
    assert!(
        json.contains(r#""kind":"prose""#),
        "JSON should contain prose kind tag"
    );
    assert!(
        json.contains(r#""kind":"toolInvocation""#),
        "JSON should contain toolInvocation kind tag"
    );
    assert!(
        json.contains(r#""kind":"toolResult""#),
        "JSON should contain toolResult kind tag"
    );

    let restored: Turn = serde_json::from_str(&json).unwrap();

    // Because `next_part_id` is `#[serde(skip)]`, the deserialized turn has
    // `next_part_id: 0`. We compare structurally instead of using `==`.
    assert_eq!(restored.id, turn.id);
    assert_eq!(restored.started_at, turn.started_at);
    assert_eq!(restored.model, turn.model);
    assert_eq!(restored.parts.len(), turn.parts.len());

    for (orig, new) in turn.parts.iter().zip(restored.parts.iter()) {
        assert_parts_eq(orig, new);
    }
}

fn assert_parts_eq(a: &TurnPart, b: &TurnPart) {
    match (a, b) {
        (
            TurnPart::Prose {
                id: id_a,
                text: t_a,
            },
            TurnPart::Prose {
                id: id_b,
                text: t_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(t_a, t_b);
        }
        (
            TurnPart::ToolInvocation {
                id: id_a,
                tool: tool_a,
                args: args_a,
                status: status_a,
                started_at: sa_a,
                ended_at: ea_a,
            },
            TurnPart::ToolInvocation {
                id: id_b,
                tool: tool_b,
                args: args_b,
                status: status_b,
                started_at: sa_b,
                ended_at: ea_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(tool_a, tool_b);
            assert_eq!(args_a, args_b);
            assert_invocation_status_eq(status_a, status_b);
            assert_eq!(sa_a, sa_b);
            assert_eq!(ea_a, ea_b);
        }
        (
            TurnPart::ToolResult {
                id: id_a,
                refs: refs_a,
                output: out_a,
            },
            TurnPart::ToolResult {
                id: id_b,
                refs: refs_b,
                output: out_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(refs_a, refs_b);
            assert_eq!(out_a.content, out_b.content);
            assert_eq!(out_a.is_error, out_b.is_error);
        }
        (
            TurnPart::Reasoning {
                id: id_a,
                text: t_a,
            },
            TurnPart::Reasoning {
                id: id_b,
                text: t_b,
            },
        ) => {
            assert_eq!(id_a, id_b);
            assert_eq!(t_a, t_b);
        }
        _ => panic!("Mismatched TurnPart variants: {:?} vs {:?}", a, b),
    }
}

fn assert_invocation_status_eq(a: &InvocationStatus, b: &InvocationStatus) {
    match (a, b) {
        (InvocationStatus::Pending, InvocationStatus::Pending) => {}
        (InvocationStatus::Running, InvocationStatus::Running) => {}
        (InvocationStatus::Success, InvocationStatus::Success) => {}
        (InvocationStatus::Error, InvocationStatus::Error) => {}
        (InvocationStatus::Cancelled, InvocationStatus::Cancelled) => {}
        _ => panic!("Mismatched InvocationStatus: {:?} vs {:?}", a, b),
    }
}
