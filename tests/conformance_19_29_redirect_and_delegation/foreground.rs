//! Story 19.29 AC5 — both origins trigger the redirect step, the text names
//! them, and no approval card is raised.
//!
//! Front door: the production turn (`run_turn`, through
//! `skill_restriction_notice::run_turn_front_door`) with the real
//! `SecurityAdapter`; `RecordingTools` records what executes. The harness has no
//! approval responder, so a Normal-mode approval request would park the turn
//! until the harness deadline and fail it — the turn FINISHING with the deny
//! text as the call's result is the observable.

use std::path::PathBuf;

use rustain::domain::models::{PermissionMode, SkillActivationSet};

use crate::skill_restriction_notice::{
    activation_from_skill_md, active_agent, drive_agent_tool_calls, error_for, write_skill,
};

struct Ground {
    _tmp: tempfile::TempDir,
    workspace: PathBuf,
    outside: PathBuf,
}

fn ground() -> Ground {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    let outside = base.join("out");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Ground {
        _tmp: tmp,
        workspace,
        outside,
    }
}

fn write_restricted_skill(ground: &Ground, name: &str, allowed: &str) {
    write_skill(
        &ground.workspace,
        name,
        &format!(
            "---\nname: {name}\ndescription: Redirect probe\nallowed-tools: {allowed}\n---\n# Body\n"
        ),
    );
}

async fn activation(ground: &Ground, names: &[&str]) -> SkillActivationSet {
    let mut merged = SkillActivationSet::new();
    for name in names {
        let set = activation_from_skill_md(&ground.workspace, name).await;
        for skill in set.active_skills() {
            merged.push(skill.clone());
        }
    }
    merged
}

fn expected(origin: &str, redir: &str, operator: &str) -> String {
    format!(
        "Tool 'Bash' redirect refused by {origin}: '{redir}' writes to a path the workspace check refuses (Workspace violation: Parent directory resolves outside workspace). Redirect to a literal path inside the workspace, or drop the redirect.{operator}"
    )
}

/// Drive one refused redirect through the turn door in `mode` and assert the
/// exact text and that `Bash` never executed.
async fn assert_refusal(
    ground: &Ground,
    activation: Option<SkillActivationSet>,
    agent: rustain::domain::models::ActiveAgent,
    mode: PermissionMode,
    expected_text: &str,
) {
    let probe = ground.outside.join("probe-19-29");
    let command = format!("echo PROBE > {}", probe.display());
    let (executed, results, _) = drive_agent_tool_calls(
        &ground.workspace,
        activation,
        agent,
        mode,
        None,
        vec![(
            "redirect-1",
            "Bash",
            serde_json::json!({ "command": command }),
        )],
    )
    .await;
    let result = error_for(&results, "redirect-1");
    assert!(result.is_error, "mode={mode:?}: {}", result.content);
    assert_eq!(result.content, expected_text, "mode={mode:?}");
    assert!(
        !executed.contains(&"Bash".to_string()),
        "mode={mode:?}: Bash must not have executed: {executed:?}"
    );
}

fn redir_for(ground: &Ground) -> String {
    format!("> {}", ground.outside.join("probe-19-29").display())
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_skill_axis_refuses_and_names_the_skill_in_every_mode() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let ground = ground();
        write_restricted_skill(&ground, "redirect-a", "Bash(echo:*)");
        let activation = activation(&ground, &["redirect-a"]).await;
        let plain = active_agent(&ground.workspace, "plain", None, None);
        assert_refusal(
            &ground,
            Some(activation),
            plain,
            mode,
            &expected(
                "skill 'redirect-a'",
                &redir_for(&ground),
                " To run it unrestricted, the operator can deactivate skill 'redirect-a'.",
            ),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_two_constrained_skills_are_named_in_activation_order() {
    let ground = ground();
    write_restricted_skill(&ground, "redirect-a", "Bash(echo:*) Read");
    write_restricted_skill(&ground, "redirect-b", "Bash(echo:*)");
    let activation = activation(&ground, &["redirect-a", "redirect-b"]).await;
    let plain = active_agent(&ground.workspace, "plain", None, None);
    assert_refusal(
        &ground,
        Some(activation),
        plain,
        PermissionMode::Normal,
        &expected(
            "skills 'redirect-a, redirect-b'",
            &redir_for(&ground),
            " To run it unrestricted, the operator can deactivate skills 'redirect-a, redirect-b'.",
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_agent_axis_refuses_and_names_the_agent_in_every_mode() {
    for mode in [PermissionMode::Normal, PermissionMode::Yolo] {
        let ground = ground();
        let agent = active_agent(&ground.workspace, "ops", Some(&["Bash(echo:*)"]), None);
        assert_refusal(
            &ground,
            None,
            agent,
            mode,
            &expected(
                "agent 'ops'",
                &redir_for(&ground),
                " To run it unrestricted, the operator can run it without agent 'ops'.",
            ),
        )
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ac5_both_axes_are_named_together() {
    let ground = ground();
    write_restricted_skill(&ground, "redirect-a", "Bash(echo:*)");
    let activation = activation(&ground, &["redirect-a"]).await;
    let agent = active_agent(&ground.workspace, "ops", Some(&["Bash(echo:*)"]), None);
    assert_refusal(
        &ground,
        Some(activation),
        agent,
        PermissionMode::Normal,
        &expected(
            "agent 'ops' and skill 'redirect-a'",
            &redir_for(&ground),
            " To run it unrestricted, the operator can deactivate skill 'redirect-a'.",
        ),
    )
    .await;
}

/// Positive control: the same restriction with an in-workspace target runs
/// `Bash` (YOLO, so no approval is needed).
#[tokio::test(flavor = "multi_thread")]
async fn ac5_an_in_workspace_target_executes_in_yolo() {
    let ground = ground();
    write_restricted_skill(&ground, "redirect-a", "Bash(echo:*)");
    let activation = activation(&ground, &["redirect-a"]).await;
    let plain = active_agent(&ground.workspace, "plain", None, None);
    let (executed, results, _) = drive_agent_tool_calls(
        &ground.workspace,
        Some(activation),
        plain,
        PermissionMode::Yolo,
        None,
        vec![(
            "inside-1",
            "Bash",
            serde_json::json!({ "command": "echo PROBE > inside.txt" }),
        )],
    )
    .await;
    assert_eq!(executed, vec!["Bash".to_string()]);
    assert!(!error_for(&results, "inside-1").is_error);
}
