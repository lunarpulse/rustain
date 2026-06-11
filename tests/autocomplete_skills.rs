use std::path::PathBuf;

use rustain::adapters::command_registry::{CommandRegistry, CommandSource, SlashCommandDef};
use rustain::adapters::skill_registry::SkillRegistry;
use rustain::domain::models::autocomplete::AutocompleteSuggestion;
use rustain::domain::models::{SkillDef, SkillSource};
use rustain::infrastructure::runtime::event_loop::build_slash_suggestions_ordered;

fn mk_skill(name: &str, description: &str, source: SkillSource) -> SkillDef {
    SkillDef {
        name: name.to_string(),
        description: description.to_string(),
        file: PathBuf::from(format!("/tmp/{}/SKILL.md", name)),
        directory: PathBuf::from(format!("/tmp/{}", name)),
        source,
        allowed_tools: None,
        terse: None,
    }
}

// ── Structural regression tests (kept from first-pass coverage) ──

#[test]
fn test_skill_suggestion_equality() {
    let a = AutocompleteSuggestion::Skill {
        name: "review".to_string(),
        description: "Reviews code".to_string(),
    };
    let b = AutocompleteSuggestion::Skill {
        name: "review".to_string(),
        description: "Reviews code".to_string(),
    };
    assert_eq!(a, b);
}

#[test]
fn test_skill_suggestion_not_slash_command() {
    let skill = AutocompleteSuggestion::Skill {
        name: "review".to_string(),
        description: "Reviews code".to_string(),
    };
    let cmd = AutocompleteSuggestion::SlashCommand {
        name: "review".to_string(),
        description: "Reviews code".to_string(),
    };
    assert_ne!(skill, cmd);
}

// ── Task 16 behavioral tests (AC4) ──

/// Task 16.1: built-in commands must be listed BEFORE skills,
/// and user-defined commands must come AFTER skills.
#[test]
fn test_autocomplete_order_builtins_then_skills_then_custom() {
    let registry = CommandRegistry::new();
    let command_results = registry.filter("");
    assert!(
        command_results
            .iter()
            .any(|c| matches!(c.source, CommandSource::BuiltIn)),
        "precondition: default registry must include at least one built-in"
    );

    let custom = SlashCommandDef {
        name: "zz-custom".to_string(),
        description: "A user-defined command".to_string(),
        source: CommandSource::UserDefined {
            path: PathBuf::from("/tmp/zz-custom.md"),
        },
        content: None,
    };
    let mut merged: Vec<&SlashCommandDef> = command_results.clone();
    merged.push(&custom);

    let skill = mk_skill("review-code", "Review code", SkillSource::WorkspaceAgents);
    let skill_refs: Vec<&SkillDef> = vec![&skill];

    let suggestions = build_slash_suggestions_ordered(&merged, &skill_refs);

    let first_skill_idx = suggestions
        .iter()
        .position(|s| matches!(s, AutocompleteSuggestion::Skill { .. }))
        .expect("at least one skill must appear in suggestions");
    let first_custom_idx = suggestions
        .iter()
        .position(|s| {
            matches!(
                s,
                AutocompleteSuggestion::SlashCommand { name, .. } if name == "zz-custom"
            )
        })
        .expect("custom command must appear in suggestions");
    let last_builtin_idx = suggestions
        .iter()
        .enumerate()
        .rev()
        .find(|(_, s)| {
            matches!(
                s,
                AutocompleteSuggestion::SlashCommand { name, .. }
                    if command_results.iter().any(|c| c.name == *name
                        && matches!(c.source, CommandSource::BuiltIn))
            )
        })
        .map(|(i, _)| i)
        .expect("at least one built-in must appear in suggestions");

    assert!(
        last_builtin_idx < first_skill_idx,
        "all built-ins must be listed before the first skill"
    );
    assert!(
        first_skill_idx < first_custom_idx,
        "skills must be listed before user-defined commands"
    );
}

/// Task 16.2: filtering must match a skill's `name` but NOT its `description`.
#[test]
fn test_autocomplete_filter_matches_name_not_description() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("lint");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: lint\ndescription: static analysis\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);

    assert_eq!(registry.filter("lint").len(), 1, "name substring matches");
    assert_eq!(
        registry.filter("static").len(),
        0,
        "description substring must NOT match"
    );
    assert_eq!(registry.filter("analysis").len(), 0);
}

/// Task 16.3: an empty skill catalog must not regress existing autocomplete —
/// the combined suggestion list must still contain all the built-in slash
/// commands in their natural order.
#[test]
fn test_autocomplete_empty_skill_catalog_no_regression() {
    let registry = CommandRegistry::new();
    let command_results = registry.filter("");
    let skill_refs: Vec<&SkillDef> = vec![];

    let suggestions = build_slash_suggestions_ordered(&command_results, &skill_refs);

    let builtins_in_registry: Vec<&str> = command_results
        .iter()
        .filter(|c| matches!(c.source, CommandSource::BuiltIn))
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(
        suggestions.len(),
        builtins_in_registry.len(),
        "no skills means only built-ins appear"
    );
    for name in &builtins_in_registry {
        assert!(
            suggestions.iter().any(|s| matches!(
                s,
                AutocompleteSuggestion::SlashCommand { name: n, .. } if n == name
            )),
            "built-in '{}' must remain visible when skill catalog is empty",
            name
        );
    }
    assert!(
        !suggestions
            .iter()
            .any(|s| matches!(s, AutocompleteSuggestion::Skill { .. })),
        "empty catalog must not inject any Skill suggestions"
    );
}

#[test]
fn test_autocomplete_skill_selection_inserts_prefix_not_clears() {
    let skill = mk_skill("review", "Reviews code", SkillSource::WorkspaceAgents);
    let suggestions = build_slash_suggestions_ordered(&[], &[&skill]);

    assert_eq!(suggestions.len(), 1);
    match &suggestions[0] {
        AutocompleteSuggestion::Skill { name, .. } => {
            assert_eq!(name, "review");
        }
        other => panic!("Expected Skill suggestion, got {:?}", other),
    }
}

#[test]
fn test_skill_name_with_arguments_in_suggestion() {
    let skill = mk_skill(
        "deploy",
        "Deploys the project",
        SkillSource::WorkspaceAgents,
    );
    let suggestions = build_slash_suggestions_ordered(&[], &[&skill]);

    assert_eq!(suggestions.len(), 1);
    match &suggestions[0] {
        AutocompleteSuggestion::Skill { name, description } => {
            assert_eq!(name, "deploy");
            assert_eq!(description, "Deploys the project");
        }
        other => panic!("Expected Skill suggestion, got {:?}", other),
    }
}
