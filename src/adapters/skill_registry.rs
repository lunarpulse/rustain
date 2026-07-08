use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::domain::models::{
    MAX_SKILL_FILE_SIZE, SkillDef, SkillSource, SkillValidationWarning, validate_skill_frontmatter,
};
use crate::domain::services::frontmatter;

#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Vec<SkillDef>,
    all_skills: Vec<SkillDef>,
    warnings_count: usize,
    /// Structured warning records for the adapter-status panel (Story 9.6).
    /// Each record carries the skill name + file path + validation error
    /// for actionable surface.
    warnings: Vec<SkillValidationWarning>,
    discovered: bool,
}

/// Outcome of attempting to parse a single filesystem entry as a skill.
/// AC7/AC8 require distinguishing "no skill here" (silent) from "malformed skill" (warning).
enum ScanOutcome {
    Valid(SkillDef),
    /// Skill loaded successfully but frontmatter lint warnings were found
    /// (non-fatal Anthropic spec violations per AC-9-6-6).
    ValidLinted(SkillDef, SkillValidationWarning),
    /// No skill file was attempted — silent skip (AC8, AC5).
    NotASkill,
    /// A skill file was attempted but failed validation — already logged, count once.
    Invalid,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Vec::new(),
            all_skills: Vec::new(),
            warnings_count: 0,
            warnings: Vec::new(),
            discovered: false,
        }
    }

    #[allow(dead_code)]
    pub fn from_disabled(all: Vec<SkillDef>) -> Self {
        Self {
            skills: Vec::new(),
            all_skills: all,
            warnings_count: 0,
            warnings: Vec::new(),
            discovered: true,
        }
    }

    /// Pre-populate the registry with explicit skills — primarily used by
    /// integration tests that exercise `activate_by_name` without invoking
    /// filesystem discovery. `all_skills` mirrors `skills` so `is_skill_disabled`
    /// behaves correctly.
    #[allow(dead_code)]
    pub fn from_skills(skills: Vec<SkillDef>) -> Self {
        Self {
            skills: skills.clone(),
            all_skills: skills,
            warnings_count: 0,
            warnings: Vec::new(),
            discovered: true,
        }
    }

    /// Access structured warning records for the adapter-status panel (Story 9.6).
    pub fn warnings(&self) -> &[SkillValidationWarning] {
        &self.warnings
    }

    /// Discover skills from workspace + global directories.
    ///
    /// This function performs blocking filesystem I/O. Callers that run inside
    /// a tokio runtime MUST wrap this in `tokio::task::spawn_blocking` to avoid
    /// stalling the async runtime thread.
    pub fn discover(workspace: &Path, home: Option<&Path>, disabled: &[String]) -> Self {
        let workspace_rel: &[(&str, SkillSource)] = &[
            (".agents/skills", SkillSource::WorkspaceAgents),
            (".rustain/skills", SkillSource::WorkspaceRustain),
            (".claude/skills", SkillSource::WorkspaceClaude),
        ];

        let mut all_candidates: Vec<SkillDef> = Vec::new();
        let mut warnings = 0usize;
        let mut warning_records: Vec<SkillValidationWarning> = Vec::new();

        // Track canonical paths already scanned so that `workspace == home`
        // does not double-scan `.agents/skills` (AC7, prevents spurious shadow warnings).
        let mut scanned: Vec<PathBuf> = Vec::new();

        for (rel, source) in workspace_rel {
            let skill_dir = workspace.join(rel);
            if skill_dir.is_dir() {
                let canonical = std::fs::canonicalize(&skill_dir).unwrap_or(skill_dir.clone());
                if scanned.contains(&canonical) {
                    continue;
                }
                scanned.push(canonical);
                let (defs, warns, wr) = scan_dir(&skill_dir, *source);
                all_candidates.extend(defs);
                warnings += warns;
                warning_records.extend(wr);
            }
        }

        if let Some(home_path) = home {
            let global_dir = home_path.join(".agents/skills");
            if global_dir.is_dir() {
                let canonical = std::fs::canonicalize(&global_dir).unwrap_or(global_dir.clone());
                if !scanned.contains(&canonical) {
                    scanned.push(canonical);
                    let (defs, warns, wr) = scan_dir(&global_dir, SkillSource::GlobalAgents);
                    all_candidates.extend(defs);
                    warnings += warns;
                    warning_records.extend(wr);
                }
            }
        }

        let deduped = deduplicate_by_priority(all_candidates);

        let disabled_set: std::collections::HashSet<&str> =
            disabled.iter().map(|s| s.as_str()).collect();

        let mut active = Vec::new();
        let mut all = Vec::new();
        for def in deduped {
            let is_disabled = disabled_set.contains(def.name.as_str());
            all.push(def.clone());
            if !is_disabled {
                active.push(def);
            }
        }

        Self {
            skills: active,
            all_skills: all,
            warnings_count: warnings,
            warnings: warning_records,
            discovered: true,
        }
    }

    pub fn skills(&self) -> &[SkillDef] {
        &self.skills
    }

    #[allow(dead_code)]
    pub fn all_including_disabled(&self) -> &[SkillDef] {
        &self.all_skills
    }

    pub fn warnings_count(&self) -> usize {
        self.warnings_count
    }

    #[allow(dead_code)]
    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

    pub fn filter(&self, query: &str) -> Vec<&SkillDef> {
        let lower_query = query.to_lowercase();
        let mut results: Vec<&SkillDef> = self
            .skills
            .iter()
            .filter(|s| lower_query.is_empty() || s.name.to_lowercase().contains(&lower_query))
            .collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        results
    }

    #[allow(dead_code)]
    pub fn find(&self, name: &str) -> Option<&SkillDef> {
        self.skills.iter().find(|s| s.name == name)
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn scan_dir(
    dir: &Path,
    source: SkillSource,
) -> (Vec<SkillDef>, usize, Vec<SkillValidationWarning>) {
    let mut candidates = Vec::new();
    let mut warnings = 0usize;
    let mut warning_records: Vec<SkillValidationWarning> = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("Failed to read skill directory {}: {}", dir.display(), e);
            return (candidates, 0, warning_records);
        }
    };

    let mut sorted_entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    sorted_entries.sort_by_key(|e| e.file_name());

    for entry in sorted_entries {
        let path = entry.path();
        let outcome = if path.is_dir() {
            parse_skill_directory(&path, source)
        } else if path.extension().is_some_and(|ext| ext == "md") {
            parse_flat_skill_file(&path, source)
        } else {
            ScanOutcome::NotASkill
        };

        match outcome {
            ScanOutcome::Valid(def) => candidates.push(def),
            ScanOutcome::ValidLinted(def, warning) => {
                warnings += 1;
                warning_records.push(warning);
                candidates.push(def);
            }
            ScanOutcome::NotASkill => {}
            ScanOutcome::Invalid => {
                warnings += 1;
                // Build a structured warning record for the adapter-status panel (Story 9.6).
                // Derive skill_name from the file/dir name.
                let skill_name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
                warning_records.push(SkillValidationWarning {
                    skill_name,
                    skill_file: path.clone(),
                    error: crate::domain::models::SkillValidationError::IoError(
                        "skill validation failed — see startup warnings".into(),
                    ),
                });
            }
        }
    }

    (candidates, warnings, warning_records)
}

fn parse_skill_directory(dir: &Path, source: SkillSource) -> ScanOutcome {
    let dir_name = match dir.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return ScanOutcome::NotASkill,
    };

    // AC8: prefer SKILL.md (case-sensitive uppercase) if present.
    if dir.join("SKILL.md").is_file() {
        return parse_skill_file(&dir.join("SKILL.md"), dir, &dir_name, source);
    }

    // Fallback: try each .md file alphabetically, return first Valid (AC8).
    let mut md_files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .map(|e| e.path())
            .collect(),
        Err(_) => return ScanOutcome::NotASkill,
    };
    md_files.sort();

    if md_files.is_empty() {
        return ScanOutcome::NotASkill; // AC8: silent skip
    }

    let mut saw_invalid = false;
    let mut pending_warning: Option<SkillValidationWarning> = None;
    for file in md_files {
        match parse_skill_file(&file, dir, &dir_name, source) {
            ScanOutcome::Valid(def) => return ScanOutcome::Valid(def),
            ScanOutcome::ValidLinted(def, warning) => {
                // Propagate the warning — the first linted skill wins per
                // first-Valid semantics.
                if pending_warning.is_none() {
                    pending_warning = Some(warning);
                }
                return ScanOutcome::ValidLinted(def, pending_warning.unwrap());
            }
            ScanOutcome::Invalid => saw_invalid = true,
            ScanOutcome::NotASkill => {}
        }
    }

    if saw_invalid {
        ScanOutcome::Invalid
    } else {
        ScanOutcome::NotASkill
    }
}

fn parse_flat_skill_file(path: &Path, source: SkillSource) -> ScanOutcome {
    let name = match path.file_stem() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return ScanOutcome::NotASkill,
    };
    let parent = match path.parent() {
        Some(p) => p,
        None => return ScanOutcome::NotASkill,
    };
    parse_skill_file(path, parent, &name, source)
}

fn parse_skill_file(
    path: &Path,
    skill_dir: &Path,
    expected_name: &str,
    source: SkillSource,
) -> ScanOutcome {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                "Skill '{}' excluded: I/O error reading metadata: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    if metadata.len() > MAX_SKILL_FILE_SIZE {
        tracing::warn!(
            "Skill '{}' excluded: file size {} exceeds {} limit",
            path.display(),
            metadata.len(),
            MAX_SKILL_FILE_SIZE
        );
        return ScanOutcome::Invalid;
    }

    // AC2: read only up to the frontmatter closing delimiter — do NOT buffer the body.
    let content = match read_frontmatter_only(path, MAX_SKILL_FILE_SIZE) {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::warn!(
                "Skill '{}' excluded: missing or malformed YAML frontmatter",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
        Err(e) => {
            tracing::warn!(
                "Skill '{}' excluded: I/O error reading file: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    let (fm, _body) = match frontmatter::parse_frontmatter(&content) {
        Some(pair) => pair,
        None => {
            tracing::warn!(
                "Skill '{}' excluded: YAML frontmatter delimiters not found",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
    };

    let name = match frontmatter::extract_field(fm, "name") {
        Some(v) => v,
        None => {
            tracing::warn!(
                "Skill '{}' excluded: missing or empty required field 'name'",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
    };

    let description = match frontmatter::extract_field(fm, "description") {
        Some(v) => v,
        None => {
            tracing::warn!(
                "Skill '{}' excluded: missing or empty required field 'description'",
                path.display()
            );
            return ScanOutcome::Invalid;
        }
    };

    let mut lint_warning: Option<crate::domain::models::SkillValidationError> = None;

    if let Err(e) = validate_skill_frontmatter(&name, &description, expected_name) {
        // Story 9.6: the 3 new Anthropic spec variants (NameMustStartWithLetter,
        // DescriptionTooShort, DescriptionMissingWhenTrigger) are NON-FATAL
        // lint warnings — skills still load but are flagged. Pre-existing
        // fatal variants (MissingField, NameTooLong, NameInvalid,
        // NameDirectoryMismatch) continue to exclude the skill.
        let is_non_fatal = matches!(
            e,
            crate::domain::models::SkillValidationError::NameMustStartWithLetter(_)
                | crate::domain::models::SkillValidationError::DescriptionTooShort(_)
                | crate::domain::models::SkillValidationError::DescriptionMissingWhenTrigger(_)
        );
        if is_non_fatal {
            tracing::warn!(
                "Skill '{}' frontmatter lint (loaded with warning): {}",
                path.display(),
                e
            );
            lint_warning = Some(e);
            // Fall through — skill loads, warning is recorded at the return site.
        } else {
            tracing::warn!("Skill '{}' excluded: {}", path.display(), e);
            return ScanOutcome::Invalid;
        }
    }

    let allowed_tools = frontmatter::extract_list_field(fm, "allowed-tools");

    let terse = frontmatter::extract_field(fm, "terse").map(|s| s.to_string());

    let canonical_dir = match std::fs::canonicalize(skill_dir) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "Skill '{}' excluded: canonicalization failed: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    let canonical_file = match std::fs::canonicalize(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "Skill '{}' excluded: file canonicalization failed: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    };

    #[cfg(feature = "skills-validation")]
    {
        if let Err(e) = crate::domain::services::skills_validation::validate(path) {
            tracing::warn!(
                "Skill '{}' excluded: skills-ref-rs validation failed: {}",
                path.display(),
                e
            );
            return ScanOutcome::Invalid;
        }
    }

    let def = SkillDef {
        name,
        description,
        file: canonical_file,
        directory: canonical_dir,
        source,
        allowed_tools,
        terse,
    };

    if let Some(warning_err) = lint_warning {
        let warning = SkillValidationWarning {
            skill_name: def.name.clone(),
            skill_file: def.file.clone(),
            error: warning_err,
        };
        ScanOutcome::ValidLinted(def, warning)
    } else {
        ScanOutcome::Valid(def)
    }
}

/// Read a file up to and including the closing YAML frontmatter delimiter, without
/// buffering the skill body (AC2). Returns `Ok(Some(content))` with frontmatter +
/// closing `---` line, `Ok(None)` if the file is not a valid frontmatter document,
/// or an I/O error.
fn read_frontmatter_only(path: &Path, max_size: u64) -> std::io::Result<Option<String>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut buf = String::new();

    // First line must be `---` (optionally CRLF terminated)
    if reader.read_line(&mut buf)? == 0 {
        return Ok(None);
    }
    if buf.trim_end_matches(['\n', '\r']) != "---" {
        return Ok(None);
    }

    loop {
        let line_start = buf.len();
        let bytes_read = reader.read_line(&mut buf)?;
        if bytes_read == 0 {
            // EOF reached without closing delimiter
            return Ok(None);
        }

        let line = &buf[line_start..];
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return Ok(Some(buf));
        }

        if buf.len() as u64 > max_size {
            return Ok(None);
        }
    }
}

fn deduplicate_by_priority(candidates: Vec<SkillDef>) -> Vec<SkillDef> {
    let mut best: HashMap<String, SkillDef> = HashMap::new();

    let mut sorted = candidates;
    sorted.sort_by_key(|candidate| candidate.source.priority());

    for def in sorted {
        match best.entry(def.name.clone()) {
            std::collections::hash_map::Entry::Occupied(occupied) => {
                // AC7: conflicts are debug-log only — do NOT increment warnings_count.
                tracing::debug!(
                    "Skill '{}' from {} shadowed by {}",
                    def.name,
                    def.file.display(),
                    occupied.get().file.display(),
                );
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert(def);
            }
        }
    }

    let mut result: Vec<SkillDef> = best.into_values().collect();
    result.sort_by(|a, b| a.name.cmp(&b.name));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn read_frontmatter_only_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.md");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(
            b"---\nname: foo\ndescription: bar\n---\n# Long body text that should NOT be read\n",
        )
        .unwrap();
        let content = read_frontmatter_only(&path, 1024).unwrap().unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: foo"));
        assert!(!content.contains("Long body text"));
    }

    #[test]
    fn read_frontmatter_only_missing_close() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.md");
        std::fs::write(&path, "---\nname: foo\nno closing").unwrap();
        let out = read_frontmatter_only(&path, 1024).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn read_frontmatter_only_no_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.md");
        std::fs::write(&path, "no frontmatter here").unwrap();
        let out = read_frontmatter_only(&path, 1024).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn read_frontmatter_only_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.md");
        std::fs::write(
            &path,
            "---\r\nname: foo\r\ndescription: bar\r\n---\r\nbody\r\n",
        )
        .unwrap();
        let out = read_frontmatter_only(&path, 1024).unwrap();
        assert!(out.is_some());
        let content = out.unwrap();
        assert!(content.contains("name: foo"));
        assert!(!content.contains("body"));
    }
}
