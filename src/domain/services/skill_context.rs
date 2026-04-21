//! Skill activation context assembly — pure domain service.
//! Builds the system-prompt skill blocks from active skills.
//! NO I/O — string manipulation only.

use std::path::Path;

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

pub fn assemble_system_prompt(
    persona_prompt: &str,
    activation_set: &SkillActivationSet,
    workspace_root: &Path,
) -> String {
    if activation_set.is_empty() {
        return persona_prompt.to_string();
    }
    let mut prompt = persona_prompt.trim_end().to_string();
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
    fn xml_escaping_in_body() {
        let skill = make_skill("test", "use a && b < c > d", "", SkillSource::GlobalAgents);
        let block = render_skill_block(&skill, Path::new("/ws"));
        assert!(block.contains("a &amp;&amp; b &lt; c &gt; d"));
        assert!(!block.contains("&&"));
    }
}
