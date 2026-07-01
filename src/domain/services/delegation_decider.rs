use crate::domain::models::PermissionMode;
use crate::domain::models::agent::AgentDef;
use crate::domain::models::plan::{Plan, PlanTask};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationSuggestion {
    pub task_number: u32,
    pub agent_name: String,
    pub reason: DelegationReason,
    /// True iff the user should be prompted (false in YOLO mode).
    pub auto_proceed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegationReason {
    /// The task description matches the agent's `description` keywords
    /// (case-insensitive substring or token overlap ≥ MIN_OVERLAP).
    DescriptionMatch { overlap_score: u32 },
    /// The plan task explicitly names the agent in its description
    /// (e.g., "ask code-reviewer to ..."); strongest signal.
    ExplicitAgentMention,
    /// No specialised agent matched; this variant is NEVER returned
    /// (DelegationDecider returns `None` instead). Reserved for future
    /// telemetry surface.
    Heuristic,
}

pub struct DelegationDecider;

impl DelegationDecider {
    /// Pick at most one agent for `task` based on the available `agents` and
    /// the user's current `mode`. Returns `None` when:
    ///  - No agent's name or description tokens overlap with the task's title
    ///    + description (overlap_score < MIN_OVERLAP).
    ///  - The plan's `task.delegated_to` is already set (idempotent — never
    ///    re-suggest for a task that's already in flight or delegated).
    pub fn suggest(
        _plan: &Plan,
        task: &PlanTask,
        agents: &[AgentDef],
        mode: PermissionMode,
    ) -> Option<DelegationSuggestion> {
        // Idempotent: never re-suggest for already-delegated tasks
        if task.delegated_to.is_some() {
            return None;
        }

        if agents.is_empty() {
            return None;
        }

        let task_text = format!("{} {}", task.title, task.description);
        let task_tokens = tokenize(&task_text);

        let mut best: Option<(AgentMatch, &AgentDef)> = None;

        for agent in agents {
            let agent_tokens = tokenize(&agent.description);
            let overlap: HashSet<&String> = task_tokens.intersection(&agent_tokens).collect();
            let overlap_score = overlap.len() as u32;

            // Check for explicit mention (strongest signal)
            let explicit = is_explicit_mention(&task.description, &agent.name);

            let agent_match = if explicit {
                AgentMatch::Explicit
            } else if overlap_score >= MIN_OVERLAP {
                AgentMatch::Keyword(overlap_score)
            } else {
                continue;
            };

            match (&best, &agent_match) {
                (None, _) => {
                    best = Some((agent_match, agent));
                }
                (Some((AgentMatch::Explicit, _)), AgentMatch::Keyword(_)) => {
                    // Existing explicit beats new keyword
                }
                (Some((AgentMatch::Keyword(_), _)), AgentMatch::Explicit) => {
                    // New explicit beats existing keyword
                    best = Some((agent_match, agent));
                }
                (Some((AgentMatch::Keyword(s1), a1)), AgentMatch::Keyword(s2)) => {
                    if s2 > s1 || (s2 == s1 && agent.name < a1.name) {
                        best = Some((agent_match, agent));
                    }
                }
                (Some((AgentMatch::Explicit, a1)), AgentMatch::Explicit) => {
                    // Tie on explicit: lexicographic break
                    if agent.name < a1.name {
                        best = Some((agent_match, agent));
                    }
                }
            }
        }

        best.map(|(m, agent)| DelegationSuggestion {
            task_number: task.number,
            agent_name: agent.name.clone(),
            reason: match m {
                AgentMatch::Explicit => DelegationReason::ExplicitAgentMention,
                AgentMatch::Keyword(score) => DelegationReason::DescriptionMatch {
                    overlap_score: score,
                },
            },
            auto_proceed: mode == PermissionMode::Yolo,
        })
    }

    /// Compute the parallel fan-out bound — the **smaller** of (a) the count
    /// of plan tasks currently eligible for parallel delegation, (b) the
    /// `plan_concurrent_tasks_max` configuration knob (default 4), and
    /// (c) NFR15's per-parent children cap (10).
    pub const MAX_PARALLEL_DELEGATE: usize = 4;
    pub const NFR15_CHILDREN_CAP: usize = 10;

    pub fn fan_out_bound(eligible_count: usize, plan_concurrent_max: usize) -> usize {
        eligible_count
            .min(plan_concurrent_max.max(1))
            .min(Self::NFR15_CHILDREN_CAP)
    }
}

const MIN_OVERLAP: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentMatch {
    Explicit,
    Keyword(u32),
}

fn tokenize(text: &str) -> HashSet<String> {
    let text = text.to_lowercase();
    let stop_words: HashSet<String> = [
        "the", "a", "and", "or", "to", "of", "for", "in", "on", "is", "was",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    text.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|t| !t.is_empty() && !stop_words.contains(*t))
        .map(|t| t.to_string())
        .collect()
}

fn is_explicit_mention(description: &str, agent_name: &str) -> bool {
    let desc = description.to_lowercase();
    let name = agent_name.to_lowercase();
    // Check for whole-word boundary match
    desc.split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .any(|word| word == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::plan::{DelegationInfo, PlanStatus, PlanTaskStatus};
    use std::path::PathBuf;

    fn make_agent(name: &str, description: &str) -> AgentDef {
        AgentDef {
            name: name.to_string(),
            description: description.to_string(),
            file: PathBuf::new(),
            allowed_tools: None,
            exclude_tools: None,
            model: None,
            isolated: false,
        }
    }

    fn make_task(number: u32, title: &str, description: &str) -> PlanTask {
        PlanTask {
            number,
            title: title.to_string(),
            description: description.to_string(),
            depends_on: vec![],
            status: PlanTaskStatus::Pending,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            waiting_on: vec![],
            delegated_to: None,
            sub_tasks: vec![],
        }
    }

    #[test]
    fn explicit_mention_beats_keyword_match() {
        let agents = vec![
            make_agent("auditor", "security audit and review tool"),
            make_agent("formatter", "code formatting and style"),
        ];
        // "auditor" explicitly mentioned; "review" overlaps with agent 0
        let task = make_task(
            1,
            "Run auditor on auth module",
            "Run auditor on auth module",
        );
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &agents,
            PermissionMode::Normal,
        );
        assert!(result.is_some());
        let s = result.unwrap();
        assert_eq!(s.agent_name, "auditor");
        assert_eq!(s.reason, DelegationReason::ExplicitAgentMention);
    }

    #[test]
    fn lexicographic_tie_break() {
        let agents = vec![
            make_agent("z-agent", "code review tool"),
            make_agent("a-agent", "code review helper"),
        ];
        let task = make_task(1, "Review code", "Review code");
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &agents,
            PermissionMode::Normal,
        );
        assert!(result.is_some());
        let s = result.unwrap();
        // Both have same keyword overlap; lexicographic tie-break picks "a-agent"
        assert_eq!(s.agent_name, "a-agent");
    }

    #[test]
    fn yolo_sets_auto_proceed_true() {
        let agents = vec![make_agent("helper", "helper tool")];
        let task = make_task(1, "Use helper", "Use helper");
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &agents,
            PermissionMode::Yolo,
        );
        assert!(result.is_some());
        assert!(result.unwrap().auto_proceed);
    }

    #[test]
    fn already_delegated_returns_none() {
        let agents = vec![make_agent("helper", "helper tool")];
        let mut task = make_task(1, "Use helper", "Use helper");
        task.delegated_to = Some(DelegationInfo {
            agent_name: "helper".to_string(),
            agent_id: None,
            delegated_at_ms: 0,
            spool_task_id: None,
        });
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &agents,
            PermissionMode::Normal,
        );
        assert!(result.is_none());
    }

    #[test]
    fn zero_agents_returns_none() {
        let task = make_task(1, "Do thing", "Do thing");
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &[],
            PermissionMode::Normal,
        );
        assert!(result.is_none());
    }

    #[test]
    fn stop_word_filtering() {
        let agents = vec![make_agent("helper", "the a and or to of for in on is was")];
        let task = make_task(1, "the a and", "the a and");
        let result = DelegationDecider::suggest(
            &Plan {
                id: "p".to_string(),
                title: "t".to_string(),
                tasks: vec![task.clone()],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 0,
                resolved_at: None,
                host_message_id: None,
            },
            &task,
            &agents,
            PermissionMode::Normal,
        );
        // All words are stop-words; no overlap
        assert!(result.is_none());
    }

    #[test]
    fn fan_out_bound_nfr15_cap_binds() {
        assert_eq!(DelegationDecider::fan_out_bound(20, 15), 10);
    }

    #[test]
    fn fan_out_bound_eligible_less_than_max() {
        assert_eq!(DelegationDecider::fan_out_bound(2, 4), 2);
    }

    #[test]
    fn fan_out_bound_max_one() {
        assert_eq!(DelegationDecider::fan_out_bound(5, 1), 1);
    }
}
