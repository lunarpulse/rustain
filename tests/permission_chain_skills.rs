use rustain::adapters::noop::{NoOpSecurity, NoOpToolSet};
use rustain::domain::models::ActiveSkill;
use rustain::domain::services::permission_chain;
use rustain::domain::services::permission_chain::PermissionDecision;

use std::path::PathBuf;

fn make_active_skill(name: &str, allowed: Option<Vec<String>>) -> ActiveSkill {
    ActiveSkill {
        name: name.to_string(),
        directory: PathBuf::from("/tmp"),
        allowed_tools: allowed,
        body: String::new(),
        arguments: String::new(),
        activation_depth: 1,
        source: rustain::domain::models::SkillSource::WorkspaceAgents,
    }
}

#[tokio::test]
async fn test_step1_active_skill_allowed_tools_denies_non_listed() {
    let security = NoOpSecurity;
    let skill = make_active_skill("review", Some(vec!["Read".to_string(), "Grep".to_string()]));
    let skills: Option<&[ActiveSkill]> = Some(&[skill]);

    let decision = permission_chain::check(
        &security,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;

    match decision {
        PermissionDecision::Deny(msg) => {
            assert!(
                msg.contains("review"),
                "Deny message should name the skill: {}",
                msg
            );
            assert!(
                msg.contains("Read"),
                "Deny message should list allowed tools: {}",
                msg
            );
        }
        other => panic!("Expected Deny, got {:?}", other),
    }
}

#[tokio::test]
async fn test_step1_activate_skill_tool_always_allowed() {
    let security = NoOpSecurity;
    let skill = make_active_skill("strict", Some(vec!["Read".to_string()]));
    let skills: Option<&[ActiveSkill]> = Some(&[skill]);

    let decision = permission_chain::check(
        &security,
        "activate_skill",
        &serde_json::json!({"name": "other"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;

    assert!(
        !matches!(decision, PermissionDecision::Deny(_)),
        "activate_skill should bypass Step 1 even with restrictive allowed_tools"
    );
}

#[tokio::test]
async fn test_step1_none_is_pass_through() {
    let security = NoOpSecurity;
    let skills: Option<&[ActiveSkill]> = None;

    let decision = permission_chain::check(
        &security,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;

    assert!(
        !matches!(decision, PermissionDecision::Deny(_)),
        "None should preserve pre-5-2 behavior (pass-through Step 1)"
    );
}

#[tokio::test]
async fn test_step1_empty_allowed_tools_denies_everything() {
    let security = NoOpSecurity;
    let skill = make_active_skill("noop", Some(vec![]));
    let skills: Option<&[ActiveSkill]> = Some(&[skill]);

    let decision = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({"file_path": "/workspace/test.rs"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(matches!(decision, PermissionDecision::Deny(_)));
}

#[tokio::test]
async fn test_step1_multi_skill_intersection_enforced() {
    let security = NoOpSecurity;
    let skill_a = make_active_skill("reader", Some(vec!["Read".to_string(), "Grep".to_string()]));
    let skill_b = make_active_skill(
        "writer",
        Some(vec!["Read".to_string(), "Write".to_string()]),
    );
    let skills: Option<&[ActiveSkill]> = Some(&[skill_a, skill_b]);

    let read_ok = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({"file_path": "/workspace/test.rs"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(!matches!(read_ok, PermissionDecision::Deny(_)));

    let grep_deny = permission_chain::check(
        &security,
        "Grep",
        &serde_json::json!({"pattern": "test", "path": "/workspace/src"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(matches!(grep_deny, PermissionDecision::Deny(_)));
}

async fn check_bash(skill: &ActiveSkill, command: &str) -> PermissionDecision {
    permission_chain::check(
        &NoOpSecurity,
        "Bash",
        &serde_json::json!({"command": command}),
        Some(std::slice::from_ref(skill)),
        None,
        &NoOpToolSet,
    )
    .await
}

#[tokio::test]
async fn bash_patterns_enforce_command_prefixes_through_permission_chain() {
    let skill = make_active_skill(
        "safe-deploy",
        Some(vec![
            "Bash(kubectl:*)".to_string(),
            "Bash(helm:*)".to_string(),
            "Read".to_string(),
        ]),
    );

    for command in [
        "kubectl get pods",
        "helm diff release chart",
        "kubectl",
        "kubectl get pods && helm diff release chart",
        "kubectl get pods 2>&1",
        "  kubectl   get\tpods",
        "kubectl get -o jsonpath='{.a && .b}'",
    ] {
        let decision = check_bash(&skill, command).await;
        assert!(
            !matches!(decision, PermissionDecision::Deny(_)),
            "declared command {command:?} was denied: {decision:?}"
        );
    }

    for command in [
        "printf 'x'",
        "kubectl-evil --all",
        "kubectlfoo",
        "KUBECTL get pods",
    ] {
        let decision = check_bash(&skill, command).await;
        match decision {
            PermissionDecision::Deny(message) => assert!(
                message.starts_with("Tool 'Bash'"),
                "command deny lost its stable opening: {message}"
            ),
            other => panic!("undeclared command {command:?} was admitted: {other:?}"),
        }
    }

    let unrestricted = make_active_skill("shell", Some(vec!["Bash".to_string()]));
    assert!(!matches!(
        check_bash(&unrestricted, "printf 'x'").await,
        PermissionDecision::Deny(_)
    ));
}

#[tokio::test]
async fn bash_patterns_fail_closed_on_unmatched_or_unsegmentable_chains() {
    let skill = make_active_skill(
        "safe-deploy",
        Some(vec![
            "Bash(kubectl:*)".to_string(),
            "Bash(helm:*)".to_string(),
            "Read".to_string(),
        ]),
    );

    for command in [
        "kubectl get pods && printf x",
        "kubectl get pods; printf x",
        "kubectl get pods | tee /tmp/x",
        "printf x && kubectl get pods",
        "kubectl get pods &&",
        "echo $(printf x)",
        "kubectl get `printf x`",
        "kubectl get \"unbalanced",
        r"kubectl get pods --note=\'x;printf SECOND-CMD-RAN\'z",
        "kubectl apply -f - <<EOF",
        "KUBECONFIG=/tmp/x kubectl get pods",
    ] {
        assert!(
            matches!(
                check_bash(&skill, command).await,
                PermissionDecision::Deny(_)
            ),
            "unsafe command {command:?} was admitted"
        );
    }
}
