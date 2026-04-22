//! Integration tests for Story 5-4 Custom Agents.
//!
//! Covers cross-module behaviour:
//!   - persona replacement via `assemble_system_prompt_with_agent`
//!   - skill composition when agent + active skills coexist
//!   - tool filtering (`allowed_tools`, `exclude_tools`, intersection)
//!   - `AgentActivator` round-trip with real files and `NoOpSecurity`
//!   - model field propagation and switching

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use rustain::adapters::agent_activation::AgentActivator;
use rustain::adapters::agent_registry::AgentRegistry;
use rustain::adapters::noop::NoOpSecurity;
use rustain::domain::models::{ActiveAgent, SkillActivationSet};
use rustain::domain::services::skill_context::assemble_system_prompt_with_agent;

fn write_agent_file(workspace: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let agents_dir = workspace.join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let path = agents_dir.join(format!("{}.md", name));
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        "---\nname: {}\ndescription: test agent\n---\n{}",
        name, body
    )
    .unwrap();
    path
}

fn write_agent_with_model(
    workspace: &std::path::Path,
    name: &str,
    body: &str,
    model: &str,
) -> PathBuf {
    let agents_dir = workspace.join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let path = agents_dir.join(format!("{}.md", name));
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
        f,
        "---\nname: {}\ndescription: test agent\nmodel: {}\n---\n{}",
        name, model, body
    )
    .unwrap();
    path
}

fn write_agent_with_tools(
    workspace: &std::path::Path,
    name: &str,
    body: &str,
    allowed: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
) -> PathBuf {
    let agents_dir = workspace.join(".claude").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    let path = agents_dir.join(format!("{}.md", name));
    let mut f = std::fs::File::create(&path).unwrap();
    write!(f, "---\nname: {}\ndescription: test agent", name).unwrap();
    if let Some(ref list) = allowed {
        write!(f, "\nallowed-tools:").unwrap();
        for t in list {
            write!(f, "\n  - {}", t).unwrap();
        }
    }
    if let Some(ref list) = exclude {
        write!(f, "\nexclude-tools:").unwrap();
        for t in list {
            write!(f, "\n  - {}", t).unwrap();
        }
    }
    write!(f, "\n---\n{}", body).unwrap();
    path
}

// ── Tests: system prompt assembly ───────────────────────────────────────────

#[test]
fn test_persona_replaced_by_agent_body() {
    let persona = "You are a helpful assistant.";
    let agent_body = "You are a code reviewer. Focus on security bugs.";
    let activation_set = SkillActivationSet::new();
    let prompt = assemble_system_prompt_with_agent(
        persona,
        Some(agent_body),
        &activation_set,
        std::path::Path::new("/tmp"),
    );
    assert!(
        prompt.contains("code reviewer"),
        "Agent body should replace persona"
    );
    assert!(
        !prompt.contains("helpful assistant"),
        "Persona should not appear when agent body is present"
    );
}

#[test]
fn test_fallback_to_persona_when_agent_body_empty() {
    let persona = "You are a helpful assistant.";
    let activation_set = SkillActivationSet::new();
    let prompt = assemble_system_prompt_with_agent(
        persona,
        Some("   \n  "),
        &activation_set,
        std::path::Path::new("/tmp"),
    );
    assert!(
        prompt.contains("helpful assistant"),
        "Empty agent body should fall back to persona"
    );
}

#[test]
fn test_skill_composition_appended_after_agent_body() {
    let persona = "You are a helpful assistant.";
    let agent_body = "You are a code reviewer.";
    let mut activation_set = SkillActivationSet::new();
    activation_set.push(rustain::domain::models::ActiveSkill {
        name: "audit".to_string(),
        directory: PathBuf::from("/tmp/audit"),
        allowed_tools: None,
        body: "## Audit checklist\n- Check auth\n".to_string(),
        arguments: String::new(),
        activation_depth: 0,
        source: rustain::domain::models::SkillSource::WorkspaceAgents,
    });
    let prompt = assemble_system_prompt_with_agent(
        persona,
        Some(agent_body),
        &activation_set,
        std::path::Path::new("/tmp"),
    );
    assert!(
        prompt.starts_with("You are a code reviewer."),
        "Agent body should be the base"
    );
    assert!(
        prompt.contains("Audit checklist"),
        "Skill block should be appended after agent body"
    );
}

// ── Tests: tool filtering ──────────────────────────────────────────────────

#[test]
fn test_allowed_tools_filter() {
    let agent = ActiveAgent {
        name: "readonly".to_string(),
        file: PathBuf::from("/tmp/a.md"),
        body: "Read only".to_string(),
        allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
        exclude_tools: None,
        model: None,
    };
    let all = vec![
        "Read".to_string(),
        "Write".to_string(),
        "Bash".to_string(),
        "Grep".to_string(),
    ];
    let filter = agent.effective_tool_filter(&all).unwrap();
    assert_eq!(
        filter,
        HashSet::from(["Read".to_string(), "Grep".to_string()])
    );
}

#[test]
fn test_exclude_tools_filter() {
    let agent = ActiveAgent {
        name: "safe".to_string(),
        file: PathBuf::from("/tmp/a.md"),
        body: "No bash".to_string(),
        allowed_tools: None,
        exclude_tools: Some(vec!["Bash".to_string()]),
        model: None,
    };
    let all = vec![
        "Read".to_string(),
        "Write".to_string(),
        "Bash".to_string(),
        "Grep".to_string(),
    ];
    let filter = agent.effective_tool_filter(&all).unwrap();
    assert!(!filter.contains("Bash"));
    assert!(filter.contains("Read"));
    assert!(filter.contains("Write"));
    assert!(filter.contains("Grep"));
}

#[test]
fn test_allowed_and_exclude_intersection() {
    // allowlist wins: only tools in allowed_tools survive, then exclude is applied
    let agent = ActiveAgent {
        name: "restricted".to_string(),
        file: PathBuf::from("/tmp/a.md"),
        body: "Restricted".to_string(),
        allowed_tools: Some(vec!["Read".to_string(), "Bash".to_string()]),
        exclude_tools: Some(vec!["Bash".to_string()]),
        model: None,
    };
    let all = vec![
        "Read".to_string(),
        "Write".to_string(),
        "Bash".to_string(),
        "Grep".to_string(),
    ];
    let filter = agent.effective_tool_filter(&all).unwrap();
    assert_eq!(filter, HashSet::from(["Read".to_string()]));
}

// ── Tests: AgentActivator round-trip ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_activator_round_trip_loads_body_and_tools() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_agent_with_tools(
        tmp.path(),
        "reviewer",
        "# Review everything\n",
        Some(vec!["Read".to_string()]),
        None,
    );
    let reg = AgentRegistry::discover(tmp.path());
    let activator = AgentActivator::new(Arc::new(NoOpSecurity));
    activator.set_registry(reg).await;

    let active = activator.activate("conv-1", "reviewer").await.unwrap();
    assert_eq!(active.name, "reviewer");
    assert!(active.body.contains("Review everything"));
    assert_eq!(active.allowed_tools, Some(vec!["Read".to_string()]));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activator_model_propagation() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_agent_with_model(tmp.path(), "fast", "Be concise.\n", "claude-3-haiku");
    let reg = AgentRegistry::discover(tmp.path());
    let activator = AgentActivator::new(Arc::new(NoOpSecurity));
    activator.set_registry(reg).await;

    let active = activator.activate("conv-1", "fast").await.unwrap();
    assert_eq!(active.model, Some("claude-3-haiku".to_string()));

    let snap = activator.snapshot("conv-1").await.unwrap();
    assert_eq!(snap.model, Some("claude-3-haiku".to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_activator_switching_replaces_prior() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_agent_file(tmp.path(), "agent-a", "Body A\n");
    write_agent_file(tmp.path(), "agent-b", "Body B\n");
    let reg = AgentRegistry::discover(tmp.path());
    let activator = AgentActivator::new(Arc::new(NoOpSecurity));
    activator.set_registry(reg).await;

    activator.activate("conv-1", "agent-a").await.unwrap();
    let first = activator.snapshot("conv-1").await.unwrap();
    assert_eq!(first.name, "agent-a");

    activator.activate("conv-1", "agent-b").await.unwrap();
    let second = activator.snapshot("conv-1").await.unwrap();
    assert_eq!(second.name, "agent-b");
    assert!(second.body.contains("Body B"));
}
