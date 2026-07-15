//! Turn-seam adapter DTO for the `/fanout` command (Story 14.3b, DD-B1).
//!
//! `FanOutSpec` is the parsed CLI grammar. The adapter translates it to a
//! [`ForkJoinRequest`] — the stable additive producer-seam — at the boundary.
//! The orchestrator owns the wave lifecycle and emits the 14.3a `AppEvent`
//! variants (`ForkJoinStarted` / `SpokeCompleted` / `SynthesisReady` /
//! `WaveCancelled`) via the event bus; this module only parses + builds the
//! request, keeping the event-loop slash-dispatch arm lean.

use crate::domain::models::AgentId;
use crate::domain::models::orchestration::{
    FORK_JOIN_SPAWN_CAP, MAX_NESTED_BREADTH, OrchestrationError, SpokeRole, SpokeSpec, WaitPolicy,
};
use crate::domain::models::router::ModelTier;
use crate::domain::models::tool_policy::ToolPolicy;
use crate::domain::ports::ForkJoinRequest;

/// Turn-seam adapter DTO for the `/fanout` command (DD-B1).
/// Parsed CLI grammar; the adapter translates FanOutSpec → ForkJoinRequest
/// at the boundary. `ForkJoinRequest` is the stable additive producer-seam.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum FanOutSpec {
    /// N spokes all running the same prompt (R1 floor — sampling-for-diversity).
    Identical { count: usize, prompt: String },
    /// Root coordinators, each driving an identical leaf-only child wave.
    Nested {
        coordinators: usize,
        grandchildren: usize,
        prompt: String,
    },
}

/// Parse `/fanout <N> <prompt>` or
/// `/fanout nested <COORDINATORS> <GRANDCHILDREN> <prompt>`.
///
/// Flat waves retain the legacy cap. Nested input additionally caps the total
/// declared nodes (`coordinators + coordinators × grandchildren`) so the
/// executor never receives a multiplicative fan-bomb.
pub fn parse_fanout(arg: Option<&str>) -> Result<FanOutSpec, String> {
    let raw = arg.unwrap_or("").trim();
    if let Some(rest) = raw
        .strip_prefix("nested")
        .and_then(|rest| rest.strip_prefix(char::is_whitespace).map(str::trim_start))
    {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let coordinators_raw = parts.next().unwrap_or("");
        let grandchildren_raw = parts.next().unwrap_or("");
        let prompt = parts.next().unwrap_or("").trim();
        let coordinators = coordinators_raw.parse::<usize>().map_err(|_| {
            format!(
                "Usage: /fanout nested <COORDINATORS> <GRANDCHILDREN> <prompt> — '{coordinators_raw}' is not a number"
            )
        })?;
        let grandchildren = grandchildren_raw.parse::<usize>().map_err(|_| {
            format!(
                "Usage: /fanout nested <COORDINATORS> <GRANDCHILDREN> <prompt> — '{grandchildren_raw}' is not a number"
            )
        })?;
        if coordinators == 0 || grandchildren == 0 {
            return Err(
                "Usage: /fanout nested <COORDINATORS> <GRANDCHILDREN> <prompt> — counts must be at least 1"
                    .into(),
            );
        }
        let total = coordinators.saturating_add(coordinators.saturating_mul(grandchildren));
        if total > MAX_NESTED_BREADTH {
            return Err(format!(
                "Nested fan-out declares {total} nodes; maximum is {MAX_NESTED_BREADTH}"
            ));
        }
        if prompt.is_empty() {
            return Err(
                "Usage: /fanout nested <COORDINATORS> <GRANDCHILDREN> <prompt> — prompt is required"
                    .into(),
            );
        }
        return Ok(FanOutSpec::Nested {
            coordinators,
            grandchildren,
            prompt: prompt.into(),
        });
    }

    let mut parts = raw.splitn(2, char::is_whitespace);
    let n_str = parts.next().unwrap_or("");
    let prompt = parts.next().unwrap_or("").trim();
    let count: usize = n_str
        .parse()
        .map_err(|_| format!("Usage: /fanout <N> <prompt> — '{n_str}' is not a number"))?;
    if count == 0 {
        return Err("Usage: /fanout <N> <prompt> — N must be at least 1".to_string());
    }
    if count > FORK_JOIN_SPAWN_CAP {
        return Err(format!(
            "Usage: /fanout <N> <prompt> — N must be at most {FORK_JOIN_SPAWN_CAP}"
        ));
    }
    if prompt.is_empty() {
        return Err("Usage: /fanout <N> <prompt> — prompt is required".to_string());
    }
    Ok(FanOutSpec::Identical {
        count,
        prompt: prompt.to_string(),
    })
}

/// Translate the parsed spec into a validated [`ForkJoinRequest`].
///
/// Flat requests preserve the original root-only shape. Nested requests remain
/// root-owned at the outer layer and declare leaf-only child waves on each
/// coordinator spoke.
pub fn to_request(spec: &FanOutSpec, model: &str) -> Result<ForkJoinRequest, OrchestrationError> {
    let leaf = |label: String, prompt: String| SpokeSpec {
        id: AgentId::new(),
        label,
        prompt,
        effective_model: model.to_string(),
        tier: ModelTier::Flagship,
        tools_allow: ToolPolicy::InheritFromParent,
        waits_for: Vec::new(),
        role: SpokeRole::Leaf,
    };
    match spec {
        FanOutSpec::Identical { count, prompt } => Ok(ForkJoinRequest {
            coordinator: AgentId::root(),
            spokes: (0..*count)
                .map(|slot| leaf(format!("SPOKE-{slot}"), prompt.clone()))
                .collect(),
            wait_policy: WaitPolicy::All,
            concurrency: *count,
        }),
        FanOutSpec::Nested {
            coordinators,
            grandchildren,
            prompt,
        } => {
            let total = coordinators.saturating_add(coordinators.saturating_mul(*grandchildren));
            if total > MAX_NESTED_BREADTH {
                return Err(OrchestrationError::NestedBreadthExceeded {
                    cap: MAX_NESTED_BREADTH,
                    attempted: total,
                });
            }
            let spokes = (0..*coordinators)
                .map(|coordinator_slot| {
                    let children: Vec<SpokeSpec> = (0..*grandchildren)
                        .map(|child_slot| {
                            leaf(
                                format!("SPOKE-{coordinator_slot}-{child_slot}"),
                                prompt.clone(),
                            )
                        })
                        .collect();
                    SpokeSpec {
                        id: AgentId::new(),
                        label: format!("COORDINATOR-{coordinator_slot}"),
                        prompt: "delegate declared nested wave".into(),
                        effective_model: model.to_string(),
                        tier: ModelTier::Flagship,
                        tools_allow: ToolPolicy::InheritFromParent,
                        waits_for: Vec::new(),
                        role: SpokeRole::Coordinator {
                            grandchildren: children.into_boxed_slice(),
                            concurrency: *grandchildren,
                            wait_policy: WaitPolicy::All,
                        },
                    }
                })
                .collect();
            Ok(ForkJoinRequest {
                coordinator: AgentId::root(),
                spokes,
                wait_policy: WaitPolicy::All,
                concurrency: *coordinators,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identical_count_and_prompt() {
        let spec = parse_fanout(Some("3 explore the module")).unwrap();
        match spec {
            FanOutSpec::Identical { count, prompt } => {
                assert_eq!(count, 3);
                assert_eq!(prompt, "explore the module");
            }
            FanOutSpec::Nested { .. } => panic!("flat syntax parsed as nested"),
        }
    }

    #[test]
    fn rejects_zero_count() {
        assert!(parse_fanout(Some("0 do thing")).is_err());
    }

    #[test]
    fn rejects_above_cap() {
        assert!(parse_fanout(Some("9 thing")).is_err());
    }

    #[test]
    fn accepts_cap() {
        let spec = parse_fanout(Some("8 thing")).unwrap();
        let req = to_request(&spec, "test-model").unwrap();
        assert_eq!(req.spokes.len(), 8);
        assert_eq!(req.spokes[0].label, "SPOKE-0");
        assert_eq!(req.spokes[7].label, "SPOKE-7");
    }

    #[test]
    fn rejects_non_numeric_n() {
        assert!(parse_fanout(Some("x do thing")).is_err());
    }

    #[test]
    fn rejects_empty_prompt() {
        assert!(parse_fanout(Some("3   ")).is_err());
        assert!(parse_fanout(Some("3")).is_err());
    }

    #[test]
    fn to_request_coordinator_is_root() {
        let spec = parse_fanout(Some("2 hi")).unwrap();
        let req = to_request(&spec, "test-model").unwrap();
        assert_eq!(req.coordinator, AgentId::root());
        assert_eq!(req.wait_policy, WaitPolicy::All);
        assert_eq!(req.concurrency, 2);
    }

    #[test]
    fn spokes_carry_the_active_model_not_empty() {
        // 14.3c AI-12.3 human-smoke regression: an empty `effective_model` ships
        // `model: ""` to the provider → 400 "Empty input messages" and every
        // spoke fails. Each spoke must carry the caller's active model.
        let spec = parse_fanout(Some("3 hello")).unwrap();
        let req = to_request(&spec, "deepseek-v4-flash").unwrap();
        assert_eq!(req.spokes.len(), 3);
        for spoke in &req.spokes {
            assert_eq!(spoke.effective_model, "deepseek-v4-flash");
            assert!(
                !spoke.effective_model.is_empty(),
                "empty spoke model → provider 400 'Empty input messages'"
            );
        }
    }

    #[test]
    fn nested_dsl_builds_root_coordinators_with_leaf_only_child_waves() {
        let spec = parse_fanout(Some("nested 2 3 compare implementations")).unwrap();
        let request = to_request(&spec, "test-model").unwrap();
        assert_eq!(request.coordinator, AgentId::root());
        assert_eq!(request.spokes.len(), 2);
        assert_eq!(request.concurrency, 2);
        for coordinator in &request.spokes {
            match &coordinator.role {
                SpokeRole::Coordinator {
                    grandchildren,
                    concurrency,
                    wait_policy,
                } => {
                    assert_eq!(grandchildren.len(), 3);
                    assert_eq!(*concurrency, 3);
                    assert_eq!(*wait_policy, WaitPolicy::All);
                    assert!(
                        grandchildren
                            .iter()
                            .all(|spoke| matches!(spoke.role, SpokeRole::Leaf))
                    );
                }
                SpokeRole::Leaf => panic!("nested DSL emitted a root leaf"),
            }
        }
    }

    #[test]
    fn nested_dsl_refuses_multiplicative_breadth_before_request_construction() {
        assert!(parse_fanout(Some("nested 2 4 too wide")).is_err());
        assert!(parse_fanout(Some("nested 0 1 empty")).is_err());
        assert!(parse_fanout(Some("nested 1 0 empty")).is_err());
    }
}
