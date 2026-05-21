use std::io::Write;
use std::path::PathBuf;

use rustain::adapters::noop::NoOpToolSet;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::domain::models::{
    ActiveSkill, MAX_SKILL_ACTIVATION_DEPTH, SkillActivationError, SkillDef, SkillSource,
};
use rustain::domain::services::permission_chain::PermissionDecision;
use rustain::domain::services::{permission_chain, skill_context};

fn write_skill(dir: &std::path::Path, name: &str, body: &str) -> SkillDef {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let file = skill_dir.join("SKILL.md");
    let mut f = std::fs::File::create(&file).unwrap();
    write!(
        f,
        "---\nname: {}\ndescription: Test skill\n---\n{}",
        name, body
    )
    .unwrap();
    let canonical_file = std::fs::canonicalize(&file).unwrap();
    let canonical_dir = std::fs::canonicalize(&skill_dir).unwrap();
    SkillDef {
        name: name.to_string(),
        description: "Test skill".to_string(),
        file: canonical_file,
        directory: canonical_dir,
        source: SkillSource::WorkspaceAgents,
        allowed_tools: None,
    }
}

fn write_skill_with_tools(
    dir: &std::path::Path,
    name: &str,
    allowed: Option<Vec<String>>,
    body: &str,
) -> SkillDef {
    let mut def = write_skill(dir, name, body);
    def.allowed_tools = allowed;
    def
}

fn write_global_skill(dir: &std::path::Path, name: &str, body: &str) -> SkillDef {
    let mut def = write_skill(dir, name, body);
    def.source = SkillSource::GlobalAgents;
    def
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activate_skill_loads_tier2_body() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "greet", "# Greet Instructions\nSay hello.\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    let result = activator.activate(&def, String::new(), "conv-1", 0).await;
    assert!(result.is_ok());
    let active = result.unwrap();
    assert!(active.body.contains("# Greet Instructions"));
    assert!(active.body.contains("Say hello."));
    assert!(!active.body.contains("name: greet"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_allowed_tools_enforced_denies_bash() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill_with_tools(
        tmp.path(),
        "readonly",
        Some(vec!["Read".to_string(), "Grep".to_string()]),
        "# Read-only\n",
    );
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();

    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let skills: Option<&[ActiveSkill]> = Some(snap.active_skills());
    let security = rustain::adapters::noop::NoOpSecurity;
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
        matches!(decision, PermissionDecision::Deny(_)),
        "Bash should be denied by allowed_tools"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_allowed_tools_empty_denies_all_except_activate_skill() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill_with_tools(tmp.path(), "noop", Some(vec![]), "# No-op\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();

    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let skills: Option<&[ActiveSkill]> = Some(snap.active_skills());
    let security = rustain::adapters::noop::NoOpSecurity;

    let deny = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(matches!(deny, PermissionDecision::Deny(_)));

    let allow = permission_chain::check(
        &security,
        "activate_skill",
        &serde_json::json!({"name": "other"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(
        !matches!(allow, PermissionDecision::Deny(_)),
        "activate_skill should bypass allowed_tools"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_allowed_tools_none_is_no_constraint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill_with_tools(tmp.path(), "unrestricted", None, "# Open\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();

    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let skills: Option<&[ActiveSkill]> = Some(snap.active_skills());
    let security = rustain::adapters::noop::NoOpSecurity;
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
        "None allowed_tools should not constrain"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_multi_skill_intersection() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def_a = write_skill_with_tools(
        tmp.path(),
        "reader",
        Some(vec!["Read".to_string(), "Grep".to_string()]),
        "# Reader\n",
    );
    let def_b = write_skill_with_tools(
        tmp.path(),
        "writer",
        Some(vec!["Read".to_string(), "Write".to_string()]),
        "# Writer\n",
    );
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def_a, String::new(), "conv-1", 0)
        .await
        .unwrap();
    activator
        .activate(&def_b, String::new(), "conv-1", 0)
        .await
        .unwrap();

    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let skills: Option<&[ActiveSkill]> = Some(snap.active_skills());
    let security = rustain::adapters::noop::NoOpSecurity;

    let read_ok = permission_chain::check(
        &security,
        "Read",
        &serde_json::json!({"file_path": "/workspace/src/main.rs"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(!matches!(read_ok, PermissionDecision::Deny(_)));

    let bash_deny = permission_chain::check(
        &security,
        "Bash",
        &serde_json::json!({"command": "ls"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(matches!(bash_deny, PermissionDecision::Deny(_)));

    let grep_deny = permission_chain::check(
        &security,
        "Grep",
        &serde_json::json!({"pattern": "test", "path": "/workspace/src"}),
        skills,
        None,
        &NoOpToolSet,
    )
    .await;
    assert!(
        matches!(grep_deny, PermissionDecision::Deny(_)),
        "Grep not in intersection [Read]"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activate_skill_unknown_name_returns_error() {
    let activator = SkillActivator::new();
    let result = activator
        .activate_by_name("nonexistent", String::new(), "conv-1", 0)
        .await;
    assert!(matches!(result, Err(SkillActivationError::NotFound(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_depth_exceeded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "deep", "# Deep\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    let result = activator
        .activate(&def, String::new(), "conv-1", MAX_SKILL_ACTIVATION_DEPTH)
        .await;
    assert!(matches!(
        result,
        Err(SkillActivationError::DepthExceeded { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trust_prompt_required_for_workspace_tier_model_driven() {
    // AC9 + NFR17: model-driven `activate_by_name` for a workspace-tier skill
    // MUST emit AppEvent::SkillTrustPrompt and wait on the carried oneshot.
    // Without the trust channel (no event_tx), the untrusted workspace skill
    // would activate silently — the security bypass this test guards against.
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::SkillTrustResponse;

    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "workspace-skill", "# Workspace\n");
    let activator = std::sync::Arc::new(SkillActivator::new());
    activator.on_new_conversation("conv-1").await;
    activator
        .set_registry(
            rustain::adapters::skill_registry::SkillRegistry::from_skills(vec![def.clone()]),
        )
        .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    activator.set_event_tx(tx).await;

    // Spawn the activation — it will await trust resolution.
    let act_handle = {
        let activator = std::sync::Arc::clone(&activator);
        tokio::spawn(async move {
            activator
                .activate_by_name("workspace-skill", String::new(), "conv-1", 1)
                .await
        })
    };

    // Expect the event on the channel; respond Accepted via its oneshot.
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("trust prompt event should fire")
        .expect("channel should deliver event");
    match event {
        AppEvent::SkillTrustPrompt {
            skill_name,
            response_tx,
            ..
        } => {
            assert_eq!(skill_name, "workspace-skill");
            response_tx.send(SkillTrustResponse::Accepted).unwrap();
        }
        other => panic!("expected SkillTrustPrompt, got {:?}", other),
    }

    let result = act_handle.await.unwrap();
    assert!(
        matches!(
            result,
            Ok(rustain::domain::models::SkillActivationOutcome::Activated(
                _
            ))
        ),
        "activation should proceed after Accept; got {:?}",
        result
    );
    assert!(activator.is_trusted("conv-1", &def.file).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trust_declined_returns_error_model_driven() {
    // AC4 + AC9: when the user declines the trust prompt for a model-driven
    // activation, `activate_by_name` returns SkillActivationError::TrustDeclined
    // and the skill is NOT activated.
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::SkillTrustResponse;

    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "distrusted-skill", "# Distrusted\n");
    let activator = std::sync::Arc::new(SkillActivator::new());
    activator.on_new_conversation("conv-1").await;
    activator
        .set_registry(
            rustain::adapters::skill_registry::SkillRegistry::from_skills(vec![def.clone()]),
        )
        .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    activator.set_event_tx(tx).await;

    let act_handle = {
        let activator = std::sync::Arc::clone(&activator);
        tokio::spawn(async move {
            activator
                .activate_by_name("distrusted-skill", String::new(), "conv-1", 1)
                .await
        })
    };

    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("trust prompt event should fire")
        .unwrap();
    let AppEvent::SkillTrustPrompt { response_tx, .. } = event else {
        panic!("expected SkillTrustPrompt");
    };
    response_tx.send(SkillTrustResponse::Declined).unwrap();

    let result = act_handle.await.unwrap();
    assert!(
        matches!(
            result,
            Ok(rustain::domain::models::SkillActivationOutcome::TrustDeclined(_))
        ),
        "declined activation should return TrustDeclined outcome (Decision 4), got {:?}",
        result
    );
    assert!(!activator.is_trusted("conv-1", &def.file).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trust_bypassed_for_global_tier_model_driven() {
    // AC4 + AC9: GlobalAgents skills must NOT trigger a trust prompt on
    // model-driven activation — no AppEvent emitted; activation proceeds immediately.
    use rustain::domain::events::AppEvent;

    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_global_skill(tmp.path(), "global-skill", "# Global\n");
    let activator = std::sync::Arc::new(SkillActivator::new());
    activator.on_new_conversation("conv-1").await;
    activator
        .set_registry(
            rustain::adapters::skill_registry::SkillRegistry::from_skills(vec![def.clone()]),
        )
        .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    activator.set_event_tx(tx).await;

    let result = activator
        .activate_by_name("global-skill", String::new(), "conv-1", 1)
        .await
        .expect("global-tier activation must not fail mechanically");
    match result {
        rustain::domain::models::SkillActivationOutcome::Activated(active) => {
            assert_eq!(active.source, SkillSource::GlobalAgents);
        }
        other => panic!("expected Activated outcome, got {:?}", other),
    }

    // No trust prompt should have been emitted.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "GlobalAgents skill must not trigger trust prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trust_session_sticky() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "sticky", "# Sticky\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    assert!(!activator.is_trusted("conv-1", &def.file).await);
    activator.mark_trusted("conv-1", def.file.clone()).await;
    assert!(activator.is_trusted("conv-1", &def.file).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_deactivate_all_clears_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def_a = write_skill(tmp.path(), "a", "# A\n");
    let def_b = write_skill(tmp.path(), "b", "# B\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def_a, String::new(), "conv-1", 0)
        .await
        .unwrap();
    activator
        .activate(&def_b, String::new(), "conv-1", 0)
        .await
        .unwrap();
    assert_eq!(activator.active_count("conv-1").await, 2);
    let deactivated = activator.deactivate_all("conv-1").await;
    assert_eq!(deactivated.len(), 2);
    assert_eq!(activator.active_count("conv-1").await, 0);
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    assert!(snap.effective_allowed_tools().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_system_prompt_contains_skill_block() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "review", "# Review code\nCheck for bugs.\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, "--verbose".to_string(), "conv-1", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let prompt = skill_context::assemble_system_prompt(
        "You are a helpful assistant.",
        &snap,
        std::path::Path::new("/workspace"),
    );
    assert!(prompt.starts_with("You are a helpful assistant."));
    assert!(prompt.contains("<skill name=\"review\""));
    assert!(prompt.contains("# Review code"));
    assert!(prompt.contains("<arguments>--verbose</arguments>"));
    assert!(prompt.contains("</skill>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_system_prompt_referenced_files_detected() {
    let body = "Use ./config.json and ./scripts/run.py for setup.";
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "setup", body);
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let prompt =
        skill_context::assemble_system_prompt("system", &snap, std::path::Path::new("/ws"));
    assert!(prompt.contains("<referenced_files>"));
    assert!(prompt.contains("config.json"));
    assert!(prompt.contains("scripts/run.py"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_system_prompt_no_referenced_files_omits_element() {
    let body = "No file references here.";
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "clean", body);
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let prompt =
        skill_context::assemble_system_prompt("system", &snap, std::path::Path::new("/ws"));
    assert!(!prompt.contains("<referenced_files>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_xml_escaping_in_body() {
    let body = "Use x < y && z > w for checks.";
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "math", body);
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let prompt =
        skill_context::assemble_system_prompt("system", &snap, std::path::Path::new("/ws"));
    assert!(prompt.contains("&lt;"));
    assert!(prompt.contains("&gt;"));
    assert!(prompt.contains("&amp;"));
    assert!(!prompt.contains("< instructions"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activation_per_conversation_isolation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "iso", "# Isolated\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-a").await;
    activator.on_new_conversation("conv-b").await;
    activator
        .activate(&def, String::new(), "conv-a", 0)
        .await
        .unwrap();
    let snap_b = activator.snapshot_for_turn("conv-b").await.unwrap();
    assert!(snap_b.is_empty());
    let snap_a = activator.snapshot_for_turn("conv-a").await.unwrap();
    assert_eq!(snap_a.active_skills().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_file_too_large_rejects_activation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let skill_dir = tmp.path().join("huge");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let file = skill_dir.join("SKILL.md");
    let big_body = "x".repeat(1_048_577);
    let content = format!("---\nname: huge\ndescription: big\n---\n{}", big_body);
    std::fs::write(&file, content).unwrap();
    let def = SkillDef {
        name: "huge".to_string(),
        description: "big".to_string(),
        file: std::fs::canonicalize(&file).unwrap(),
        directory: std::fs::canonicalize(&skill_dir).unwrap(),
        source: SkillSource::GlobalAgents,
        allowed_tools: None,
    };
    let activator = SkillActivator::new();
    let result = activator.activate(&def, String::new(), "conv-1", 0).await;
    assert!(matches!(
        result,
        Err(SkillActivationError::FileTooLarge { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_file_missing_at_activation() {
    let def = SkillDef {
        name: "ghost".to_string(),
        description: "gone".to_string(),
        file: PathBuf::from("/nonexistent/ghost/SKILL.md"),
        directory: PathBuf::from("/nonexistent/ghost"),
        source: SkillSource::GlobalAgents,
        allowed_tools: None,
    };
    let activator = SkillActivator::new();
    let result = activator.activate(&def, String::new(), "conv-1", 0).await;
    assert!(matches!(
        result,
        Err(SkillActivationError::FileMissing { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_deactivate_unknown_returns_none() {
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    let result = activator.deactivate("conv-1", "nope").await;
    assert!(result.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_deactivate_specific_skill() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def_a = write_skill(tmp.path(), "keep", "# Keep\n");
    let def_b = write_skill(tmp.path(), "remove", "# Remove\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def_a, String::new(), "conv-1", 0)
        .await
        .unwrap();
    activator
        .activate(&def_b, String::new(), "conv-1", 0)
        .await
        .unwrap();
    assert_eq!(activator.active_count("conv-1").await, 2);
    let removed = activator.deactivate("conv-1", "remove").await;
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().name, "remove");
    assert_eq!(activator.active_count("conv-1").await, 1);
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    assert_eq!(snap.active_skills()[0].name, "keep");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_depth_guard_chain() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "chainer", "# Chain\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    activator
        .activate(&def, String::new(), "conv-1", 1)
        .await
        .unwrap();
    activator
        .activate(&def, String::new(), "conv-1", 2)
        .await
        .unwrap();
    let exceeded = activator.activate(&def, String::new(), "conv-1", 3).await;
    assert!(matches!(
        exceeded,
        Err(SkillActivationError::DepthExceeded { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activate_disabled_skill_returns_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let skill_dir = tmp.path().join("bad-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let file = skill_dir.join("SKILL.md");
    std::fs::write(
        &file,
        "---\nname: bad-skill\ndescription: bad\n---\n# Bad\n",
    )
    .unwrap();
    let canonical_file = std::fs::canonicalize(&file).unwrap();
    let canonical_dir = std::fs::canonicalize(&skill_dir).unwrap();
    let def = SkillDef {
        name: "bad-skill".to_string(),
        description: "bad".to_string(),
        file: canonical_file,
        directory: canonical_dir,
        source: SkillSource::GlobalAgents,
        allowed_tools: None,
    };
    let registry = rustain::adapters::skill_registry::SkillRegistry::from_disabled(vec![def]);
    let activator = SkillActivator::new();
    activator.set_registry(registry).await;
    let result = activator
        .activate_by_name("bad-skill", String::new(), "conv-1", 0)
        .await;
    assert!(matches!(result, Err(SkillActivationError::Disabled(_))));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trust_session_sticky_after_accept() {
    // AC4: after Accept, the skill is trusted for the session — subsequent
    // activations MUST skip the prompt entirely (no event emitted).
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::SkillTrustResponse;

    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "sticky-skill", "# Sticky\n");
    let activator = std::sync::Arc::new(SkillActivator::new());
    activator.on_new_conversation("conv-1").await;
    activator
        .set_registry(
            rustain::adapters::skill_registry::SkillRegistry::from_skills(vec![def.clone()]),
        )
        .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AppEvent>();
    activator.set_event_tx(tx).await;

    // First activation: prompt fires, accept.
    let act1 = {
        let activator = std::sync::Arc::clone(&activator);
        tokio::spawn(async move {
            activator
                .activate_by_name("sticky-skill", String::new(), "conv-1", 1)
                .await
        })
    };
    let event = rx.recv().await.unwrap();
    let AppEvent::SkillTrustPrompt { response_tx, .. } = event else {
        panic!("expected SkillTrustPrompt");
    };
    response_tx.send(SkillTrustResponse::Accepted).unwrap();
    act1.await.unwrap().unwrap();

    // Second activation: trusted, no prompt expected.
    let result = activator
        .activate_by_name("sticky-skill", String::new(), "conv-1", 1)
        .await;
    assert!(matches!(
        result,
        Ok(rustain::domain::models::SkillActivationOutcome::Activated(
            _
        ))
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .is_err(),
        "subsequent activation of trusted skill must not re-prompt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_skill_directory_added_to_readable_paths() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "reader", "# Reader\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    let dirs = activator.active_skill_dirs("conv-1").await;
    assert_eq!(dirs.len(), 1);
    assert!(dirs[0].ends_with("reader"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_arguments_omitted_when_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let def = write_skill(tmp.path(), "no-args", "# No args\n");
    let activator = SkillActivator::new();
    activator.on_new_conversation("conv-1").await;
    activator
        .activate(&def, String::new(), "conv-1", 0)
        .await
        .unwrap();
    let snap = activator.snapshot_for_turn("conv-1").await.unwrap();
    let prompt =
        skill_context::assemble_system_prompt("system", &snap, std::path::Path::new("/ws"));
    assert!(!prompt.contains("<arguments>"));
}
