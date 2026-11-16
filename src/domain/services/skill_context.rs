//! Skill activation context assembly — pure domain service.
//! Builds the system-prompt skill blocks from active skills.
//! NO I/O — string manipulation only.

use std::path::Path;

use crate::domain::models::skill_metadata::SkillMetadata;
use crate::domain::models::{ActiveSkill, SkillActivationSet, SkillSource};

pub const MAX_REFERENCED_FILES_IN_PROMPT: usize = 32;

pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
}

fn scan_referenced_files(body: &str) -> Vec<String> {
    let mut files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        if bytes[i] == b'.' && i + 1 < body.len() && bytes[i + 1] == b'/' {
            let start = i;
            let mut end = i + 2;
            while end < body.len() {
                let c = bytes[end];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' || c == b'/' {
                    end += 1;
                } else {
                    break;
                }
            }
            if end > start + 2 {
                files.insert(body[start..end].to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    files
        .into_iter()
        .take(MAX_REFERENCED_FILES_IN_PROMPT)
        .collect()
}

pub fn render_skill_block(active: &ActiveSkill, workspace_root: &Path) -> String {
    let source_label = match active.source {
        SkillSource::WorkspaceAgents => "WorkspaceAgents",
        SkillSource::WorkspaceRustain => "WorkspaceRustain",
        SkillSource::WorkspaceClaude => "WorkspaceClaude",
        SkillSource::GlobalAgents => "GlobalAgents",
    };

    let mut block = format!(
        "<skill name=\"{}\" source=\"{}\">\n",
        escape_xml_attr(&active.name),
        source_label
    );

    block.push_str("<instructions>\n");
    block.push_str(&escape_xml(&active.body));
    block.push_str("\n</instructions>\n");

    block.push_str(&format!(
        "<skill_directory>{}</skill_directory>\n",
        escape_xml(&active.directory.to_string_lossy())
    ));
    block.push_str(&format!(
        "<workspace_root>{}</workspace_root>\n",
        escape_xml(&workspace_root.to_string_lossy())
    ));

    let refs = scan_referenced_files(&active.body);
    if !refs.is_empty() {
        block.push_str("<referenced_files>\n");
        for f in &refs {
            block.push_str(&escape_xml(f));
            block.push('\n');
        }
        block.push_str("</referenced_files>\n");
    }

    if !active.arguments.is_empty() {
        block.push_str(&format!(
            "<arguments>{}</arguments>\n",
            escape_xml(&active.arguments)
        ));
    }

    block.push_str("</skill>");
    block
}

/// Upper bound of the Tier-1 skill catalog, in estimated tokens (FR41, D4).
pub const SKILL_CATALOG_TOKEN_CAP_MAX: usize = 4_000;

/// Catalog cap for a model: 2% of its context window, never above
/// [`SKILL_CATALOG_TOKEN_CAP_MAX`]; the maximum when the window is unknown (`0`).
pub fn skill_catalog_token_cap(context_window: u32) -> usize {
    if context_window == 0 {
        SKILL_CATALOG_TOKEN_CAP_MAX
    } else {
        SKILL_CATALOG_TOKEN_CAP_MAX.min(context_window as usize / 50)
    }
}

/// One skill offered to [`render_skill_catalog`]. `body` is `Some` only for
/// entries whose full SKILL.md may be shown (static-full exposure, trusted skill).
pub struct SkillCatalogEntry<'a> {
    pub metadata: &'a SkillMetadata,
    pub body: Option<&'a str>,
}

/// Result of [`render_skill_catalog`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSkillCatalog {
    pub block: String,
    /// Entries written into the block.
    pub listed: usize,
    /// Entries left out by the cap (not counting `unrendered`).
    pub omitted: usize,
    /// Estimated tokens of the listed entries.
    pub tokens: usize,
}

const SKILL_CATALOG_HEADER: &str = "<available_skills>\nInstalled skills you can load with the activate_skill tool. When a request matches a skill's description, call activate_skill with its exact name.";

/// Render the Tier-1 `<available_skills>` block. Entries are taken in input
/// order and the listing stops at the first entry that would push the running
/// token estimate over `cap_tokens`. `unrendered` counts skills the exposure
/// port did not hand back; they join the trailer count. `None` iff there is
/// nothing to list and nothing unrendered.
pub fn render_skill_catalog(
    entries: &[SkillCatalogEntry<'_>],
    cap_tokens: usize,
    unrendered: usize,
) -> Option<RenderedSkillCatalog> {
    if entries.is_empty() && unrendered == 0 {
        return None;
    }
    let mut block = String::from(SKILL_CATALOG_HEADER);
    let mut tokens = 0usize;
    let mut listed = 0usize;
    for entry in entries {
        let cost = entry.metadata.estimated_tokens() + entry.body.map_or(0, |b| b.len() / 4);
        if tokens + cost > cap_tokens {
            break;
        }
        tokens += cost;
        listed += 1;
        block.push_str("\n<skill_entry name=\"");
        block.push_str(&escape_xml_attr(&entry.metadata.name));
        block.push('"');
        if entry.metadata.is_workspace_scoped() {
            block.push_str(" trust=\"workspace\"");
        }
        block.push('>');
        block.push_str(&escape_xml(&entry.metadata.description));
        if let Some(body) = entry.body {
            block.push_str("\n<instructions>\n");
            block.push_str(&escape_xml(body));
            block.push_str("\n</instructions>\n");
        }
        block.push_str("</skill_entry>");
    }
    let omitted = entries.len() - listed;
    let more = omitted + unrendered;
    if more > 0 {
        block.push_str(&format!(
            "\n<omitted count=\"{more}\">{more} more installed skills are not listed.</omitted>"
        ));
    }
    block.push_str("\n</available_skills>");
    Some(RenderedSkillCatalog {
        block,
        listed,
        omitted,
        tokens,
    })
}

#[allow(dead_code)]
pub fn assemble_system_prompt(
    persona_prompt: &str,
    activation_set: &SkillActivationSet,
    workspace_root: &Path,
) -> String {
    assemble_system_prompt_with_agent(persona_prompt, None, None, activation_set, workspace_root)
}

/// Compose the system prompt: base, then the Tier-1 `catalog` block (if any),
/// then each active Tier-2 `<skill>` block as the exact suffix (FR41 D3).
/// `catalog == None` is byte-identical to the pre-FR41 composition.
pub fn assemble_system_prompt_with_agent(
    persona_prompt: &str,
    agent_body: Option<&str>,
    catalog: Option<&str>,
    activation_set: &SkillActivationSet,
    workspace_root: &Path,
) -> String {
    let base = match agent_body {
        Some(body) if !body.trim().is_empty() => body,
        _ => {
            if agent_body.is_some() {
                tracing::debug!("Agent has empty body; using persona prompt as base");
            }
            persona_prompt
        }
    };

    if activation_set.is_empty() && catalog.is_none() {
        return base.to_string();
    }
    let mut prompt = base.trim_end().to_string();
    if let Some(catalog) = catalog {
        prompt.push_str("\n\n");
        prompt.push_str(catalog);
    }
    for skill in activation_set.active_skills() {
        prompt.push_str("\n\n");
        prompt.push_str(&render_skill_block(skill, workspace_root));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_skill(name: &str, body: &str, arguments: &str, source: SkillSource) -> ActiveSkill {
        ActiveSkill {
            name: name.to_string(),
            directory: PathBuf::from(format!("/tmp/skills/{}", name)),
            allowed_tools: None,
            body: body.to_string(),
            arguments: arguments.to_string(),
            activation_depth: 1,
            source,
        }
    }

    #[test]
    fn escape_xml_handles_special_chars() {
        assert_eq!(escape_xml("a&b<c>d"), "a&amp;b&lt;c&gt;d");
    }

    #[test]
    fn escape_xml_no_double_escape() {
        let input = "plain text";
        assert_eq!(escape_xml(input), input);
    }

    #[test]
    fn referenced_files_detected() {
        let body = "use ./config.json and ./scripts/run.py";
        let files = scan_referenced_files(body);
        assert_eq!(files, vec!["./config.json", "./scripts/run.py"]);
    }

    #[test]
    fn referenced_files_cap_at_32() {
        let mut body = String::new();
        for i in 0..40 {
            body.push_str(&format!("./file{}.txt ", i));
        }
        let files = scan_referenced_files(&body);
        assert_eq!(files.len(), MAX_REFERENCED_FILES_IN_PROMPT);
    }

    #[test]
    fn referenced_files_no_match_omits_element() {
        let skill = make_skill("test", "no file refs here", "", SkillSource::GlobalAgents);
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(!block.contains("<referenced_files>"));
    }

    #[test]
    fn arguments_rendered_when_present() {
        let skill = make_skill("test", "body", "--verbose", SkillSource::GlobalAgents);
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(block.contains("<arguments>--verbose</arguments>"));
    }

    #[test]
    fn arguments_omitted_when_empty() {
        let skill = make_skill("test", "body", "", SkillSource::GlobalAgents);
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(!block.contains("<arguments>"));
    }

    #[test]
    fn multi_skill_ordering() {
        let mut set = SkillActivationSet::new();
        set.push(make_skill("alpha", "body-a", "", SkillSource::GlobalAgents));
        set.push(make_skill("beta", "body-b", "", SkillSource::GlobalAgents));
        let prompt = assemble_system_prompt("persona", &set, Path::new("/ws"));
        let alpha_pos = prompt.find("<skill name=\"alpha\"").unwrap();
        let beta_pos = prompt.find("<skill name=\"beta\"").unwrap();
        assert!(alpha_pos < beta_pos);
    }

    #[test]
    fn skill_block_format() {
        let skill = ActiveSkill {
            name: "review-code".to_string(),
            directory: PathBuf::from("/ws/.agents/skills/review-code"),
            allowed_tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            body: "# Code Review\nRead ./checklist.md".to_string(),
            arguments: "src/main.rs".to_string(),
            activation_depth: 1,
            source: SkillSource::WorkspaceAgents,
        };
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(block.starts_with("<skill name=\"review-code\" source=\"WorkspaceAgents\">"));
        assert!(block.contains("<instructions>"));
        assert!(block.contains("# Code Review"));
        assert!(block.contains("</instructions>"));
        assert!(
            block.contains("<skill_directory>/ws/.agents/skills/review-code</skill_directory>")
        );
        assert!(block.contains("<workspace_root>/ws</workspace_root>"));
        assert!(block.contains("<referenced_files>"));
        assert!(block.contains("./checklist.md"));
        assert!(block.contains("<arguments>src/main.rs</arguments>"));
        assert!(block.ends_with("</skill>"));
    }

    #[test]
    fn assemble_empty_set_returns_persona() {
        let set = SkillActivationSet::new();
        let prompt = assemble_system_prompt("my persona", &set, Path::new("/ws"));
        assert_eq!(prompt, "my persona");
    }

    #[test]
    fn agent_body_replaces_persona() {
        let set = SkillActivationSet::new();
        let prompt = assemble_system_prompt_with_agent(
            "my persona",
            Some("agent system prompt"),
            None,
            &set,
            Path::new("/ws"),
        );
        assert_eq!(prompt, "agent system prompt");
    }

    #[test]
    fn agent_empty_body_falls_back_to_persona() {
        let set = SkillActivationSet::new();
        let prompt = assemble_system_prompt_with_agent(
            "my persona",
            Some("   "),
            None,
            &set,
            Path::new("/ws"),
        );
        assert_eq!(prompt, "my persona");
    }

    #[test]
    fn agent_none_body_uses_persona() {
        let set = SkillActivationSet::new();
        let prompt =
            assemble_system_prompt_with_agent("my persona", None, None, &set, Path::new("/ws"));
        assert_eq!(prompt, "my persona");
    }

    #[test]
    fn agent_with_skills_composes() {
        let mut set = SkillActivationSet::new();
        set.push(make_skill(
            "alpha",
            "skill-body",
            "",
            SkillSource::GlobalAgents,
        ));
        let prompt = assemble_system_prompt_with_agent(
            "my persona",
            Some("agent body"),
            None,
            &set,
            Path::new("/ws"),
        );
        assert!(prompt.starts_with("agent body"));
        assert!(prompt.contains("<skill name=\"alpha\""));
        assert!(prompt.contains("skill-body"));
    }

    #[test]
    fn no_agent_with_skills_matches_original() {
        let mut set = SkillActivationSet::new();
        set.push(make_skill(
            "alpha",
            "skill-body",
            "",
            SkillSource::GlobalAgents,
        ));
        let with_agent =
            assemble_system_prompt_with_agent("my persona", None, None, &set, Path::new("/ws"));
        let original = assemble_system_prompt("my persona", &set, Path::new("/ws"));
        assert_eq!(with_agent, original);
    }

    #[test]
    fn xml_escaping_in_body() {
        let skill = make_skill("test", "use a && b < c > d", "", SkillSource::GlobalAgents);
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(block.contains("a &amp;&amp; b &lt; c &gt; d"));
        assert!(!block.contains("&&"));
    }

    fn meta(name: &str, description: &str, source: SkillSource) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            source,
            terse: None,
        }
    }

    const HEADER: &str = "<available_skills>\nInstalled skills you can load with the activate_skill tool. When a request matches a skill's description, call activate_skill with its exact name.";

    #[test]
    fn u1_empty_entries_render_nothing_unless_unrendered() {
        assert_eq!(render_skill_catalog(&[], 100, 0), None);
        let rendered = render_skill_catalog(&[], 100, 2).unwrap();
        assert_eq!(
            rendered.block,
            format!(
                "{HEADER}\n<omitted count=\"2\">2 more installed skills are not listed.</omitted>\n</available_skills>"
            )
        );
        assert_eq!(
            (rendered.listed, rendered.omitted, rendered.tokens),
            (0, 0, 0)
        );
    }

    #[test]
    fn u2_exact_block_bytes_for_two_metadata_entries() {
        let alpha = meta("alpha", "Alpha desc", SkillSource::WorkspaceAgents);
        let beta = meta("beta", "Beta desc", SkillSource::GlobalAgents);
        let entries = [
            SkillCatalogEntry {
                metadata: &alpha,
                body: None,
            },
            SkillCatalogEntry {
                metadata: &beta,
                body: None,
            },
        ];
        let rendered = render_skill_catalog(&entries, 4_000, 0).unwrap();
        let expected = "<available_skills>\nInstalled skills you can load with the activate_skill tool. When a request matches a skill's description, call activate_skill with its exact name.\n<skill_entry name=\"alpha\" trust=\"workspace\">Alpha desc</skill_entry>\n<skill_entry name=\"beta\">Beta desc</skill_entry>\n</available_skills>";
        assert_eq!(rendered.block, expected);
        assert_eq!((rendered.listed, rendered.omitted), (2, 0));
    }

    #[test]
    fn u3_names_and_descriptions_are_escaped() {
        let odd = meta("a<b&\"c\nd", "x < y & z", SkillSource::GlobalAgents);
        let entries = [SkillCatalogEntry {
            metadata: &odd,
            body: None,
        }];
        let rendered = render_skill_catalog(&entries, 4_000, 0).unwrap();
        assert_eq!(
            rendered.block,
            format!(
                "{HEADER}\n<skill_entry name=\"a&lt;b&amp;&quot;c&#10;d\">x &lt; y &amp; z</skill_entry>\n</available_skills>"
            )
        );
    }

    #[test]
    fn u4_bodies_render_escaped_instructions_and_count_toward_tokens() {
        let m = meta("alpha", "Alpha desc", SkillSource::GlobalAgents);
        let body = "---\nname: alpha\n---\nuse a < b & c";
        let entries = [SkillCatalogEntry {
            metadata: &m,
            body: Some(body),
        }];
        let rendered = render_skill_catalog(&entries, 4_000, 0).unwrap();
        assert_eq!(
            rendered.block,
            format!(
                "{HEADER}\n<skill_entry name=\"alpha\">Alpha desc\n<instructions>\n---\nname: alpha\n---\nuse a &lt; b &amp; c\n</instructions>\n</skill_entry>\n</available_skills>"
            )
        );
        assert_eq!(rendered.tokens, 18 + body.len() / 4);
    }

    #[test]
    fn u5_cap_boundary_is_inclusive() {
        let a = meta("alpha", "Alpha desc", SkillSource::GlobalAgents);
        let b = meta("beta", "Beta desc", SkillSource::GlobalAgents);
        let entries = [
            SkillCatalogEntry {
                metadata: &a,
                body: None,
            },
            SkillCatalogEntry {
                metadata: &b,
                body: None,
            },
        ];
        let at_cap = render_skill_catalog(&entries, 36, 0).unwrap();
        assert_eq!((at_cap.listed, at_cap.omitted, at_cap.tokens), (2, 0, 36));
        let over = render_skill_catalog(&entries, 35, 0).unwrap();
        assert_eq!((over.listed, over.omitted, over.tokens), (1, 1, 18));
        assert_eq!(
            over.block,
            format!(
                "{HEADER}\n<skill_entry name=\"alpha\">Alpha desc</skill_entry>\n<omitted count=\"1\">1 more installed skills are not listed.</omitted>\n</available_skills>"
            )
        );
    }

    #[test]
    fn u6_listing_stops_at_the_first_entry_that_overflows() {
        let big = meta("a-big", &"d".repeat(400), SkillSource::GlobalAgents);
        let s1 = meta("b-small", "s", SkillSource::GlobalAgents);
        let s2 = meta("c-small", "s", SkillSource::GlobalAgents);
        let entries = [
            SkillCatalogEntry {
                metadata: &big,
                body: None,
            },
            SkillCatalogEntry {
                metadata: &s1,
                body: None,
            },
            SkillCatalogEntry {
                metadata: &s2,
                body: None,
            },
        ];
        let rendered = render_skill_catalog(&entries, 100, 0).unwrap();
        assert_eq!(
            (rendered.listed, rendered.omitted, rendered.tokens),
            (0, 3, 0)
        );
        assert_eq!(
            rendered.block,
            format!(
                "{HEADER}\n<omitted count=\"3\">3 more installed skills are not listed.</omitted>\n</available_skills>"
            )
        );
    }

    #[test]
    fn u7_tokens_sum_listed_entries_and_unrendered_joins_the_trailer() {
        let a = meta("alpha", "Alpha desc", SkillSource::GlobalAgents);
        let b = meta("beta", "Beta desc", SkillSource::GlobalAgents);
        let entries = [
            SkillCatalogEntry {
                metadata: &a,
                body: Some("12345678"),
            },
            SkillCatalogEntry {
                metadata: &b,
                body: None,
            },
        ];
        let rendered = render_skill_catalog(&entries, 4_000, 3).unwrap();
        assert_eq!(rendered.tokens, (18 + 2) + 18);
        assert_eq!((rendered.listed, rendered.omitted), (2, 0));
        assert!(rendered.block.ends_with(
            "<omitted count=\"3\">3 more installed skills are not listed.</omitted>\n</available_skills>"
        ));
    }

    #[test]
    fn u8_cap_is_two_percent_of_the_window_up_to_the_maximum() {
        assert_eq!(skill_catalog_token_cap(0), 4_000);
        assert_eq!(skill_catalog_token_cap(8_192), 163);
        assert_eq!(skill_catalog_token_cap(128_000), 2_560);
        assert_eq!(skill_catalog_token_cap(200_000), 4_000);
        assert_eq!(skill_catalog_token_cap(1_000_000), 4_000);
        assert_eq!(skill_catalog_token_cap(u32::MAX), 4_000);
    }

    #[test]
    fn u9_no_catalog_is_byte_identical_to_the_pre_catalog_composition() {
        let root = Path::new("/ws");
        let empty = SkillActivationSet::new();
        assert_eq!(
            assemble_system_prompt_with_agent("PERSONA\n", None, None, &empty, root),
            "PERSONA\n"
        );
        assert_eq!(
            assemble_system_prompt_with_agent("PERSONA", Some(""), None, &empty, root),
            "PERSONA"
        );
        assert_eq!(
            assemble_system_prompt_with_agent("PERSONA", Some("AGENT\n"), None, &empty, root),
            "AGENT\n"
        );
        let mut set = SkillActivationSet::new();
        set.push(make_skill("alpha", "body", "", SkillSource::GlobalAgents));
        assert_eq!(
            assemble_system_prompt_with_agent("PERSONA\n\n", None, None, &set, root),
            "PERSONA\n\n<skill name=\"alpha\" source=\"GlobalAgents\">\n<instructions>\nbody\n</instructions>\n<skill_directory>/tmp/skills/alpha</skill_directory>\n<workspace_root>/ws</workspace_root>\n</skill>"
        );
    }

    #[test]
    fn catalog_goes_between_the_base_and_the_active_blocks() {
        let mut set = SkillActivationSet::new();
        set.push(make_skill("alpha", "body", "", SkillSource::GlobalAgents));
        let prompt = assemble_system_prompt_with_agent(
            "PERSONA\n",
            None,
            Some("<available_skills>\nX\n</available_skills>"),
            &set,
            Path::new("/ws"),
        );
        assert_eq!(
            prompt,
            "PERSONA\n\n<available_skills>\nX\n</available_skills>\n\n<skill name=\"alpha\" source=\"GlobalAgents\">\n<instructions>\nbody\n</instructions>\n<skill_directory>/tmp/skills/alpha</skill_directory>\n<workspace_root>/ws</workspace_root>\n</skill>"
        );
        let no_active = assemble_system_prompt_with_agent(
            "",
            None,
            Some("<available_skills>\nX\n</available_skills>"),
            &SkillActivationSet::new(),
            Path::new("/ws"),
        );
        assert_eq!(no_active, "\n\n<available_skills>\nX\n</available_skills>");
    }

    /// Rust port of `_skill_names` in `tests_tui/fixtures/scene_provider.py`
    /// (the J3 request-log parser): canonical `<skill …>` blocks only count
    /// when they are the trailing `\n\n`-separated run of the prompt.
    fn j3_skill_names(system: &str) -> Vec<String> {
        let re = regex::Regex::new(
            r#"(?ms)^<skill name="([^"]+)" source="(?:WorkspaceAgents|WorkspaceRustain|WorkspaceClaude|GlobalAgents)">\n<instructions>\n.*?\n</instructions>\n<skill_directory>.*?</skill_directory>\n<workspace_root>.*?</workspace_root>\n(?:<referenced_files>\n.*?\n</referenced_files>\n)?(?:<arguments>.*?</arguments>\n)?</skill>"#,
        )
        .unwrap();
        let matches: Vec<_> = re.captures_iter(system).collect();
        let Some(last) = matches.last() else {
            return Vec::new();
        };
        if last.get(0).unwrap().end() != system.len() {
            return Vec::new();
        }
        let mut trailing = vec![last];
        let mut cursor = last.get(0).unwrap().start();
        for m in matches[..matches.len() - 1].iter().rev() {
            let whole = m.get(0).unwrap();
            if &system[whole.end()..cursor] != "\n\n" {
                break;
            }
            trailing.push(m);
            cursor = whole.start();
        }
        trailing.iter().rev().map(|m| m[1].to_string()).collect()
    }

    #[test]
    fn u10_j3_parser_reads_active_names_when_the_catalog_precedes_them() {
        let mut set = SkillActivationSet::new();
        set.push(make_skill("alpha", "a-body", "", SkillSource::GlobalAgents));
        set.push(make_skill(
            "beta",
            "b-body",
            "",
            SkillSource::WorkspaceAgents,
        ));
        let a = meta("gamma", "Gamma desc", SkillSource::GlobalAgents);
        let catalog = render_skill_catalog(
            &[SkillCatalogEntry {
                metadata: &a,
                body: None,
            }],
            4_000,
            0,
        )
        .unwrap()
        .block;
        let good = assemble_system_prompt_with_agent(
            "PERSONA",
            None,
            Some(&catalog),
            &set,
            Path::new("/ws"),
        );
        assert_eq!(j3_skill_names(&good), vec!["alpha", "beta"]);
        // A catalog placed after the blocks would hide them from the parser.
        let blocks_then_catalog = format!(
            "{}\n\n{catalog}",
            assemble_system_prompt_with_agent("PERSONA", None, None, &set, Path::new("/ws"))
        );
        assert_eq!(j3_skill_names(&blocks_then_catalog), Vec::<String>::new());
    }
}
