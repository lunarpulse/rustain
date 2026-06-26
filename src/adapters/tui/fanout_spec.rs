//! Turn-seam adapter DTO for the `/fanout` command (Story 14.3b, DD-B1).
//!
//! `FanOutSpec` is the parsed CLI grammar. The adapter translates it to a
//! [`ForkJoinRequest`] — the stable additive producer-seam — at the boundary.
//! The orchestrator owns the wave lifecycle and emits the 14.3a `AppEvent`
//! variants (`ForkJoinStarted` / `SpokeCompleted` / `SynthesisReady` /
//! `WaveCancelled`) via the event bus; this module only parses + builds the
//! request, keeping the event-loop slash-dispatch arm lean.

use crate::domain::models::AgentId;
use crate::domain::models::orchestration::{FORK_JOIN_SPAWN_CAP, SpokeSpec, WaitPolicy};
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
}

/// Parse `/fanout <N> <prompt>` into a [`FanOutSpec`].
///
/// `N` must be a positive integer ≤ [`FORK_JOIN_SPAWN_CAP`] (8). `prompt` is
/// the remainder of the line (trimmed, non-empty). Returns a human-readable
/// error string on parse failure (surfaced as a `SystemNotice`).
pub fn parse_fanout(arg: Option<&str>) -> Result<FanOutSpec, String> {
    let raw = arg.unwrap_or("").trim();
    // Split off the leading token (N) from the prompt remainder on the first
    // whitespace run, so the prompt may itself contain spaces.
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

/// Translate the parsed spec into a [`ForkJoinRequest`] at the boundary.
///
/// Per DD-B1, spokes are DISTINGUISHABLE by slot: `label = SPOKE-{slot}`. The
/// coordinator is the root agent — the executor asserts the coordinator owns
/// its root authority (`request.coordinator == root_authority.scope`). R1 floors
/// `wait_policy = All` and `concurrency = N`.
pub fn to_request(spec: &FanOutSpec, model: &str) -> ForkJoinRequest {
    match spec {
        FanOutSpec::Identical { count, prompt } => {
            let spokes = (0..*count)
                .map(|slot| SpokeSpec {
                    label: format!("SPOKE-{slot}"),
                    prompt: prompt.clone(),
                    // The spoke runs on the caller's ACTIVE model. The runner
                    // holds no model router, so an empty `effective_model` ships
                    // `model: ""` to the provider → 400 "Empty input messages"
                    // and every spoke fails (14.3c AI-12.3 human-smoke fix).
                    effective_model: model.to_string(),
                    tier: ModelTier::Flagship,
                    tools_allow: ToolPolicy::InheritFromParent,
                    waits_for: Vec::new(),
                })
                .collect();
            ForkJoinRequest {
                coordinator: AgentId::root(),
                spokes,
                wait_policy: WaitPolicy::All,
                concurrency: *count,
            }
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
        let req = to_request(&spec, "test-model");
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
        let req = to_request(&spec, "test-model");
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
        let req = to_request(&spec, "deepseek-v4-flash");
        assert_eq!(req.spokes.len(), 3);
        for spoke in &req.spokes {
            assert_eq!(spoke.effective_model, "deepseek-v4-flash");
            assert!(
                !spoke.effective_model.is_empty(),
                "empty spoke model → provider 400 'Empty input messages'"
            );
        }
    }
}
