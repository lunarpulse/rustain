//! Custom agents: discovered from .claude/agents/, activated via @Agents/<name>. See Story 5.4.

use std::io::{BufRead, BufReader};
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

use crate::domain::models::{
    AgentDef, MAX_AGENT_FILE_SIZE, MAX_AGENT_SCAN_FILES, validate_agent_frontmatter,
};
use crate::domain::services::frontmatter;

#[derive(Debug, Clone)]
pub struct AgentRegistry {
    agents: Vec<AgentDef>,
    warnings_count: usize,
    #[allow(dead_code)]
    discovered: bool,
}

enum ScanOutcome {
    Valid(AgentDef),
    NotAnAgent,
    Invalid,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            warnings_count: 0,
            discovered: false,
        }
    }

    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

    #[allow(dead_code)]
    pub fn from_agents(agents: Vec<AgentDef>) -> Self {
        Self {
            agents,
            warnings_count: 0,
            discovered: true,
        }
    }

    pub fn discover(workspace: &Path) -> Self {
        let agents_dir = workspace.join(".claude").join("agents");

        if !agents_dir.is_dir() {
            return Self {
                agents: Vec::new(),
                warnings_count: 0,
                discovered: true,
            };
        }

        let mut agents: Vec<AgentDef> = Vec::new();
        let mut warnings = 0usize;
        let mut file_count = 0usize;

        let entries = match std::fs::read_dir(&agents_dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "Failed to read agents directory {}: {}",
                    agents_dir.display(),
                    e
                );
                return Self {
                    agents: Vec::new(),
                    warnings_count: 0,
                    discovered: true,
                };
            }
        };

        let mut sorted_entries: Vec<_> = entries
            .filter_map(|e| {
                if let Err(ref err) = e {
                    tracing::warn!("Skipping agent dir entry: {}", err);
                }
                e.ok()
            })
            .collect();
        sorted_entries.sort_by_key(|e| e.file_name());

        for entry in sorted_entries {
            let path = entry.path();

            if !path.is_file() {
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }

            file_count += 1;
            if file_count > MAX_AGENT_SCAN_FILES {
                tracing::warn!(
                    "Agent scan capped at {} files — skipping remaining entries in {}",
                    MAX_AGENT_SCAN_FILES,
                    agents_dir.display()
                );
                break;
            }

            match parse_agent_file(&path) {
                ScanOutcome::Valid(def) => {
                    if def.name == "default" {
                        tracing::warn!(
                            "Agent '{}' excluded: 'default' is a reserved synthetic name",
                            path.display()
                        );
                        warnings += 1;
                        continue;
                    }
                    if agents.iter().any(|a| a.name == def.name) {
                        tracing::warn!(
                            "Duplicate agent name '{}' — skipping {}",
                            def.name,
                            path.display()
                        );
                        warnings += 1;
                        continue;
                    }
                    agents.push(def);
                }
                ScanOutcome::Invalid => warnings += 1,
                ScanOutcome::NotAnAgent => {}
            }
        }

        Self {
            agents,
            warnings_count: warnings,
            discovered: true,
        }
    }

    pub fn agents(&self) -> &[AgentDef] {
        &self.agents
    }

    pub fn warnings_count(&self) -> usize {
        self.warnings_count
    }

    pub fn filter(&self, query: &str) -> Vec<&AgentDef> {
        let lower_query = query.to_lowercase();
        let mut results: Vec<&AgentDef> = self
            .agents
            .iter()
            .filter(|a| lower_query.is_empty() || a.name.to_lowercase().contains(&lower_query))
            .collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    pub fn find(&self, name: &str) -> Option<&AgentDef> {
        if name == "default" {
            return None;
        }
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn remove(&mut self, name: &str) -> Option<AgentDef> {
        if let Some(index) = self.agents.iter().position(|a| a.name == name) {
            Some(self.agents.remove(index))
        } else {
            None
        }
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_agent_file(path: &Path) -> ScanOutcome {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "Agent '{}' excluded: I/O error reading metadata: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    if metadata.len() > MAX_AGENT_FILE_SIZE {
        tracing::warn!(
            "Agent '{}' excluded: file size {} exceeds {} limit",
            path.display(),
            metadata.len(),
            MAX_AGENT_FILE_SIZE
        );
        return ScanOutcome::Invalid;
    }

    let content = match read_frontmatter_only(path, MAX_AGENT_FILE_SIZE) {
        Ok(Some(c)) => c,
        Ok(None) => return ScanOutcome::NotAnAgent,
        Err(e) => {
            tracing::warn!("Agent '{}' excluded: I/O error: {}", path.display(), e);
            return ScanOutcome::Invalid;
        }
    };

    let (fm, _body) = match frontmatter::parse_frontmatter(&content) {
        Some(pair) => pair,
        None => return ScanOutcome::NotAnAgent,
    };

    let expected_name = match path.file_stem() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return ScanOutcome::NotAnAgent,
    };

    let name = match frontmatter::extract_field(fm, "name") {
        Some(v) => v,
        None => {
            tracing::warn!(
                "Agent '{}' excluded: missing or empty required field 'name'",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
    };

    let description = match frontmatter::extract_field(fm, "description") {
        Some(v) => v,
        None => {
            tracing::warn!(
                "Agent '{}' excluded: missing or empty required field 'description'",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
    };

    if let Err(e) = validate_agent_frontmatter(&name, &description, &expected_name) {
        tracing::warn!("Agent '{}' excluded: {}", path.display(), e);
        return ScanOutcome::Invalid;
    }

    let allowed_tools = frontmatter::extract_list_field(fm, "allowed-tools");
    let exclude_tools = frontmatter::extract_list_field(fm, "exclude-tools");
    let model = frontmatter::extract_field(fm, "model");

    let canonical_file = match std::fs::canonicalize(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "Agent '{}' excluded: canonicalization failed: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    ScanOutcome::Valid(AgentDef {
        name,
        description,
        file: canonical_file,
        allowed_tools,
        exclude_tools,
        model,
    })
}

fn read_frontmatter_only(path: &Path, max_size: u64) -> std::io::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();

    if reader.read_line(&mut buf)? == 0 {
        return Ok(None);
    }
    let first_line = buf
        .trim_start_matches('\u{FEFF}')
        .trim_end_matches(['\n', '\r']);
    if first_line != "---" {
        return Ok(None);
    }

    loop {
        let line_start = buf.len();
        let bytes_read = reader.read_line(&mut buf)?;
        if bytes_read == 0 {
            return Ok(None);
        }

        let line = &buf[line_start..];
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Ok(Some(buf));
        }

        if buf.len() as u64 > max_size {
            tracing::warn!(
                "Agent file frontmatter exceeds {} bytes — skipping",
                max_size
            );
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_agent_file(dir: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let agents_dir = dir.join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let path = agents_dir.join(format!("{}.md", name));
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "---\n{}---\n{}", frontmatter, body).unwrap();
        path
    }

    #[test]
    fn discover_returns_empty_when_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 0);
    }

    #[test]
    fn discover_returns_empty_when_dir_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
    }

    #[test]
    fn discover_finds_valid_agent() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "code-reviewer",
            "name: code-reviewer\ndescription: Reviews code\n",
            "You are a code reviewer.\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        assert_eq!(reg.agents().len(), 1);
        assert_eq!(reg.agents()[0].name, "code-reviewer");
    }

    #[test]
    fn discover_skips_file_without_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("README.md"), "# Not an agent\n").unwrap();
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 0);
    }

    #[test]
    fn discover_warns_on_missing_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "description: some desc\n", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 1);
    }

    #[test]
    fn discover_warns_on_name_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "foo",
            "name: bar\ndescription: some desc\n",
            "body\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 1);
    }

    #[test]
    fn discover_skips_file_too_large() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let path = agents_dir.join("big.md");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "---\nname: big\ndescription: big file\n---\n").unwrap();
        let padding = "x".repeat(1_100_000);
        write!(f, "{}", padding).unwrap();
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 1);
    }

    #[test]
    fn discover_extracts_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "my-agent",
            "name: my-agent\ndescription: test\nallowed-tools:\n  - Read\n  - Grep\nexclude-tools:\n  - Bash\nmodel: claude-opus-4-7\n",
            "body\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        let agent = &reg.agents()[0];
        assert_eq!(agent.name, "my-agent");
        assert_eq!(
            agent.allowed_tools.as_ref().unwrap(),
            &vec!["Read".to_string(), "Grep".to_string()]
        );
        assert_eq!(
            agent.exclude_tools.as_ref().unwrap(),
            &vec!["Bash".to_string()]
        );
        assert_eq!(agent.model.as_ref().unwrap(), "claude-opus-4-7");
    }

    #[test]
    fn discover_caps_at_100_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        for i in 0..105 {
            let name = format!("agent-{:03}", i);
            let path = agents_dir.join(format!("{}.md", name));
            std::fs::write(
                &path,
                format!("---\nname: {}\ndescription: agent {}\n---\nbody\n", name, i),
            )
            .unwrap();
        }
        let reg = AgentRegistry::discover(tmp.path());
        assert_eq!(reg.agents().len(), 100);
    }

    #[test]
    fn filter_returns_matching_agents() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "code-reviewer",
            "name: code-reviewer\ndescription: reviews\n",
            "body\n",
        );
        write_agent_file(
            tmp.path(),
            "security-auditor",
            "name: security-auditor\ndescription: audits\n",
            "body\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        let filtered = reg.filter("code");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "code-reviewer");
    }

    #[test]
    fn find_returns_agent_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "foo",
            "name: foo\ndescription: test\n",
            "body\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.find("foo").is_some());
        assert!(reg.find("bar").is_none());
        assert!(reg.find("default").is_none());
    }

    #[test]
    fn discover_warns_on_default_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(
            tmp.path(),
            "default",
            "name: default\ndescription: reserved\n",
            "body\n",
        );
        let reg = AgentRegistry::discover(tmp.path());
        assert!(reg.agents().is_empty());
        assert_eq!(reg.warnings_count(), 1);
    }
}
