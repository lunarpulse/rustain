//! The skill-origin tool restriction as a value (Story 19.29, R5).
//!
//! One constructor derives it from the active skills, so the permission chain,
//! the offer filter (`SkillActivationSet::effective_allowed_tools`) and the
//! delegation rail never keep their own copy of the intersection.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::skill::ActiveSkill;

/// Suffix the deny text appends to the origin when the restriction crossed a
/// delegation boundary.
pub const INHERITED_ORIGIN_SUFFIX: &str = " (inherited from the delegating turn)";

/// The raw `allowed-tools` intersection of every constrained active skill.
///
/// Items are the declared strings with patterns intact (`Bash(kubectl:*)`),
/// never the expanded tool set (Story 19.28 A14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillToolRestriction {
    /// Names of the skills that declare `allowed-tools`, in activation order.
    pub skill_names: Vec<String>,
    /// Raw item intersection; empty means no non-carve-out tool is admitted.
    pub items: BTreeSet<String>,
    /// True when this restriction crossed a delegation boundary (a child
    /// carries its parent's skills' restriction as a second axis).
    #[serde(default)]
    pub inherited: bool,
}

impl SkillToolRestriction {
    /// `None` when no active skill declares `allowed-tools`; otherwise the raw
    /// item intersection of the constrained skills.
    pub fn from_active_skills(skills: &[ActiveSkill]) -> Option<Self> {
        let mut constrained = skills
            .iter()
            .filter_map(|skill| skill.allowed_tools.as_ref().map(|tools| (skill, tools)));
        let (first_skill, first_tools) = constrained.next()?;
        let mut items: BTreeSet<String> = first_tools.iter().cloned().collect();
        let mut skill_names = vec![first_skill.name.clone()];
        for (skill, tools) in constrained {
            items.retain(|item| tools.contains(item));
            skill_names.push(skill.name.clone());
        }
        Some(Self {
            skill_names,
            items,
            inherited: false,
        })
    }

    /// The same restriction, marked as carried across a delegation boundary.
    pub fn into_inherited(mut self) -> Self {
        self.inherited = true;
        self
    }

    /// The skills as the deny texts name them: `skill 'a'` / `skills 'a, b'`.
    pub fn subject_label(&self) -> String {
        let noun = if self.skill_names.len() == 1 {
            "skill"
        } else {
            "skills"
        };
        format!("{noun} '{}'", self.skill_names.join(", "))
    }

    /// The origin as the deny texts name it: the subject, followed by the
    /// inherited suffix when the restriction crossed a delegation.
    pub fn origin_label(&self) -> String {
        if self.inherited {
            format!("{}{INHERITED_ORIGIN_SUFFIX}", self.subject_label())
        } else {
            self.subject_label()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SkillSource;
    use std::path::PathBuf;

    fn skill(name: &str, allowed: Option<&[&str]>) -> ActiveSkill {
        ActiveSkill {
            name: name.to_string(),
            directory: PathBuf::from("/tmp"),
            allowed_tools: allowed.map(|items| items.iter().map(|i| (*i).to_string()).collect()),
            body: String::new(),
            arguments: String::new(),
            activation_depth: 0,
            source: SkillSource::WorkspaceRustain,
        }
    }

    #[test]
    fn no_constrained_skill_yields_none() {
        assert!(SkillToolRestriction::from_active_skills(&[]).is_none());
        assert!(SkillToolRestriction::from_active_skills(&[skill("a", None)]).is_none());
    }

    #[test]
    fn single_skill_keeps_declared_items_and_pattern_text() {
        let restriction = SkillToolRestriction::from_active_skills(&[skill(
            "kube",
            Some(&["Bash(kubectl:*)", "Read"]),
        )])
        .unwrap();
        assert_eq!(restriction.skill_names, vec!["kube".to_string()]);
        assert_eq!(
            restriction.items,
            BTreeSet::from(["Bash(kubectl:*)".to_string(), "Read".to_string()])
        );
        assert!(!restriction.inherited);
    }

    #[test]
    fn two_skills_intersect_raw_strings_in_activation_order() {
        let restriction = SkillToolRestriction::from_active_skills(&[
            skill("a", Some(&["Bash(kubectl:*)", "Read"])),
            skill("b", Some(&["Bash", "Read"])),
        ])
        .unwrap();
        assert_eq!(restriction.items, BTreeSet::from(["Read".to_string()]));
        assert_eq!(
            restriction.skill_names,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn mixed_unconstrained_and_constrained_skill_is_the_constrained_one() {
        let restriction = SkillToolRestriction::from_active_skills(&[
            skill("free", None),
            skill("kube", Some(&["Read"])),
        ])
        .unwrap();
        assert_eq!(restriction.skill_names, vec!["kube".to_string()]);
        assert_eq!(restriction.items, BTreeSet::from(["Read".to_string()]));
    }

    #[test]
    fn empty_declaration_is_an_empty_restriction_not_none() {
        let restriction =
            SkillToolRestriction::from_active_skills(&[skill("none", Some(&[]))]).unwrap();
        assert!(restriction.items.is_empty());
    }

    #[test]
    fn origin_label_pluralises_and_marks_inherited() {
        let one = SkillToolRestriction::from_active_skills(&[skill("a", Some(&["Read"]))]).unwrap();
        assert_eq!(one.origin_label(), "skill 'a'");
        assert_eq!(
            one.clone().into_inherited().origin_label(),
            "skill 'a' (inherited from the delegating turn)"
        );
        let two = SkillToolRestriction::from_active_skills(&[
            skill("a", Some(&["Read"])),
            skill("b", Some(&["Read"])),
        ])
        .unwrap();
        assert_eq!(two.origin_label(), "skills 'a, b'");
    }

    #[test]
    fn serde_round_trip_defaults_inherited_to_false() {
        let json = serde_json::json!({"skill_names": ["a"], "items": ["Read"]});
        let restriction: SkillToolRestriction = serde_json::from_value(json).unwrap();
        assert!(!restriction.inherited);
        let value = serde_json::to_value(restriction.clone().into_inherited()).unwrap();
        assert_eq!(value["inherited"], true);
    }
}
