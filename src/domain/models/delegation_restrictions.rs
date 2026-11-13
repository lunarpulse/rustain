//! The restrictions a delegating call carries across a delegation boundary
//! (Story 19.29, R6/R7/R11).
//!
//! One value, one rail parser: `SubagentProvider::invoke_task` (the `task`
//! tool) and `ToolSetAdapter::execute_propose_plan` (the plan a turn proposes)
//! both read the two JSON rail keys through [`DelegationRestrictions::take_from_rail`],
//! and `CompositeToolsetAdapter` writes them through
//! [`DelegationRestrictions::write_to_rail`] from the call's own invocation.

use serde::{Deserialize, Serialize};

use super::agent::AgentToolRestriction;
use super::skill_restriction::SkillToolRestriction;

/// JSON key carrying the delegating call's agent restriction.
pub const PARENT_TOOL_RESTRICTION_KEY: &str = "__parent_tool_restriction";
/// JSON key carrying the delegating call's skill restriction.
pub const PARENT_SKILL_RESTRICTION_KEY: &str = "__parent_skill_restriction";

/// Which axis of the rail could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailAxis {
    Tool,
    Skill,
}

impl RailAxis {
    /// `tool` / `skill`, as the refusal texts name the axis.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Skill => "skill",
        }
    }
}

/// A rail key was present but unreadable: delegation must be refused, because
/// a child launched on `None` would run wider than its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailError {
    pub axis: RailAxis,
    pub detail: String,
}

impl std::fmt::Display for RailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// The agent-origin and skill-origin restrictions of one delegating call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRestrictions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentToolRestriction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<SkillToolRestriction>,
}

impl DelegationRestrictions {
    pub fn new(agent: Option<AgentToolRestriction>, skill: Option<SkillToolRestriction>) -> Self {
        Self { agent, skill }
    }

    pub fn is_empty(&self) -> bool {
        self.agent.is_none() && self.skill.is_none()
    }

    /// `None` when the call carried no restriction at all.
    pub fn into_option(self) -> Option<Self> {
        (!self.is_empty()).then_some(self)
    }

    /// Write both rail keys into a tool input — ALWAYS both, `null` when
    /// absent — overwriting anything the model put there.
    pub fn write_to_rail(
        input: &mut serde_json::Value,
        agent: Option<&AgentToolRestriction>,
        skill: Option<&SkillToolRestriction>,
    ) {
        if let Some(object) = input.as_object_mut() {
            object.insert(PARENT_TOOL_RESTRICTION_KEY.into(), serde_json::json!(agent));
            object.insert(
                PARENT_SKILL_RESTRICTION_KEY.into(),
                serde_json::json!(skill),
            );
        }
    }

    /// Read and strip both rail keys. Absent or `null` is `None`; present but
    /// unreadable is an error (Story 19.28 P4: fail closed — `None` means *no
    /// narrowing at all*).
    pub fn take_from_rail(input: &mut serde_json::Value) -> Result<Self, RailError> {
        let tool =
            take_key::<AgentToolRestriction>(input, PARENT_TOOL_RESTRICTION_KEY, RailAxis::Tool);
        let skill =
            take_key::<SkillToolRestriction>(input, PARENT_SKILL_RESTRICTION_KEY, RailAxis::Skill);
        Ok(Self {
            agent: tool?,
            skill: skill?,
        })
    }
}

fn take_key<T: serde::de::DeserializeOwned>(
    input: &mut serde_json::Value,
    key: &str,
    axis: RailAxis,
) -> Result<Option<T>, RailError> {
    let Some(value) = input.as_object_mut().and_then(|object| object.remove(key)) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| RailError {
            axis,
            detail: error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn skill() -> SkillToolRestriction {
        SkillToolRestriction {
            skill_names: vec!["kube".into()],
            items: BTreeSet::from(["Bash(kubectl:*)".to_string()]),
            inherited: false,
        }
    }

    #[test]
    fn write_then_take_round_trips_and_strips_both_keys() {
        let mut input = serde_json::json!({"description": "d"});
        let skill = skill();
        DelegationRestrictions::write_to_rail(&mut input, None, Some(&skill));
        assert_eq!(input[PARENT_TOOL_RESTRICTION_KEY], serde_json::Value::Null);
        let taken = DelegationRestrictions::take_from_rail(&mut input).unwrap();
        assert_eq!(taken.skill, Some(skill));
        assert!(taken.agent.is_none());
        assert_eq!(input, serde_json::json!({"description": "d"}));
    }

    #[test]
    fn absent_and_null_keys_mean_no_restriction() {
        let mut absent = serde_json::json!({});
        assert!(
            DelegationRestrictions::take_from_rail(&mut absent)
                .unwrap()
                .is_empty()
        );
        let mut nulls = serde_json::json!({
            PARENT_TOOL_RESTRICTION_KEY: null,
            PARENT_SKILL_RESTRICTION_KEY: null,
        });
        assert!(
            DelegationRestrictions::take_from_rail(&mut nulls)
                .unwrap()
                .into_option()
                .is_none()
        );
    }

    #[test]
    fn a_present_but_unreadable_key_fails_closed_on_either_axis() {
        for (key, axis) in [
            (PARENT_TOOL_RESTRICTION_KEY, RailAxis::Tool),
            (PARENT_SKILL_RESTRICTION_KEY, RailAxis::Skill),
        ] {
            let mut input = serde_json::json!({ key: {"not": "a restriction"} });
            let error = DelegationRestrictions::take_from_rail(&mut input).unwrap_err();
            assert_eq!(error.axis, axis);
            assert!(!error.to_string().is_empty());
            assert!(
                input.get(key).is_none(),
                "the key is stripped even on error"
            );
        }
    }
}
