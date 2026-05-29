use crate::domain::models::agent::AgentDef;
use crate::domain::models::plan::PlanTask;
use crate::domain::models::{AgentLaunchSpec, ModelTier, ToolPolicy, TraceContext};
use crate::domain::services::plan_runtime::format_task_prompt;

pub struct LaunchSpecBuilder;

impl LaunchSpecBuilder {
    /// Build a launch spec for a plan task delegated to `agent_def`.
    /// Default model fallback chain:
    ///   agent_def.model.clone()
    ///     .unwrap_or_else(|| default_model.to_string())
    pub fn from_plan_task(
        task: &PlanTask,
        agent_def: &AgentDef,
        default_model: &str,
        parent_ctx_tokens: u32,
        parent_trace: Option<TraceContext>,
    ) -> AgentLaunchSpec {
        let prompt = format_task_prompt(task);
        let effective_model = agent_def
            .model
            .clone()
            .unwrap_or_else(|| default_model.to_string());
        let tier = ModelTier::CheapAgentic;
        let tools_allow = match &agent_def.allowed_tools {
            Some(allow) if !allow.is_empty() => ToolPolicy::Allowlist {
                tools: allow.iter().cloned().collect(),
            },
            _ => match &agent_def.exclude_tools {
                Some(deny) if !deny.is_empty() => ToolPolicy::Denylist {
                    tools: deny.iter().cloned().collect(),
                },
                _ => ToolPolicy::InheritFromParent,
            },
        };
        AgentLaunchSpec {
            prompt,
            effective_model,
            tier,
            tools_allow,
            parent_ctx_tokens,
            sandbox_override: None,
            parent_trace,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn make_agent(
        name: &str,
        model: Option<&str>,
        allowed: Option<Vec<&str>>,
        excluded: Option<Vec<&str>>,
    ) -> AgentDef {
        AgentDef {
            name: name.to_string(),
            description: "test".to_string(),
            file: PathBuf::new(),
            allowed_tools: allowed.map(|v| v.iter().map(|s| s.to_string()).collect()),
            exclude_tools: excluded.map(|v| v.iter().map(|s| s.to_string()).collect()),
            model: model.map(|s| s.to_string()),
        }
    }

    fn make_task(title: &str, description: &str) -> PlanTask {
        PlanTask {
            number: 1,
            title: title.to_string(),
            description: description.to_string(),
            depends_on: vec![],
            status: crate::domain::models::PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
        }
    }

    #[test]
    fn uses_agent_model_when_present() {
        let agent = make_agent("test", Some("claude-3-opus"), None, None);
        let task = make_task("Do thing", "Do the thing");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        assert_eq!(spec.effective_model, "claude-3-opus");
    }

    #[test]
    fn falls_back_to_default_model() {
        let agent = make_agent("test", None, None, None);
        let task = make_task("Do thing", "");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        assert_eq!(spec.effective_model, "gpt-4");
    }

    #[test]
    fn allowed_tools_becomes_allowlist() {
        let agent = make_agent("test", None, Some(vec!["bash", "read"]), None);
        let task = make_task("Do thing", "");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        match spec.tools_allow {
            ToolPolicy::Allowlist { tools } => {
                let set: HashSet<_> = tools.iter().cloned().collect();
                assert!(set.contains("bash"));
                assert!(set.contains("read"));
            }
            _ => panic!("expected Allowlist"),
        }
    }

    #[test]
    fn excluded_tools_becomes_denylist() {
        let agent = make_agent("test", None, None, Some(vec!["read"]));
        let task = make_task("Do thing", "");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        match spec.tools_allow {
            ToolPolicy::Denylist { tools } => {
                assert!(tools.contains("read"));
            }
            _ => panic!("expected Denylist"),
        }
    }

    #[test]
    fn none_tools_becomes_inherit() {
        let agent = make_agent("test", None, None, None);
        let task = make_task("Do thing", "");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        assert_eq!(spec.tools_allow, ToolPolicy::InheritFromParent);
    }

    #[test]
    fn prompt_contains_task_title() {
        let agent = make_agent("test", None, None, None);
        let task = make_task("Run tests", "Run all unit tests");
        let spec = LaunchSpecBuilder::from_plan_task(&task, &agent, "gpt-4", 0, None);
        assert!(spec.prompt.contains("Run tests"));
    }
}
