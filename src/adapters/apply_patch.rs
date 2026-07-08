use std::path::{Component, Path, PathBuf};

use crate::domain::errors::ToolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchHunk {
    AddFile {
        path: String,
        new_text: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        old_text: String,
        new_text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPatch {
    pub hunks: Vec<PatchHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    Add,
    Delete,
    Update,
}

#[derive(Debug)]
struct Section {
    kind: SectionKind,
    path: String,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    /// True once any real body line has been accumulated. Lets `finalize` reject
    /// an `Add`/`Update` section that was declared but never given content,
    /// without false-firing on the legitimate empty flush that `@@` triggers at a
    /// hunk boundary (where accumulated lines have already been emitted/cleared).
    had_lines: bool,
}

impl Section {
    fn new(kind: SectionKind, path: &str) -> Result<Self, ToolError> {
        if path.trim().is_empty() {
            return Err(ToolError::InvalidInput(
                "apply_patch path must be non-empty".into(),
            ));
        }
        Ok(Self {
            kind,
            path: path.trim().to_string(),
            old_lines: Vec::new(),
            new_lines: Vec::new(),
            had_lines: false,
        })
    }

    fn has_lines(&self) -> bool {
        !self.old_lines.is_empty() || !self.new_lines.is_empty()
    }

    /// Emit the currently-accumulated lines as a hunk (if any). Used at `@@`
    /// hunk boundaries inside an `Update File` section: the section stays alive
    /// and may accumulate further hunks. Never errors on empty — an empty flush
    /// here is the legitimate "no hunk started yet" state at the first `@@`.
    fn flush_hunk(&mut self, out: &mut Vec<PatchHunk>) -> Result<(), ToolError> {
        match self.kind {
            SectionKind::Add => {
                if self.has_lines() {
                    out.push(PatchHunk::AddFile {
                        path: self.path.clone(),
                        new_text: self.new_lines.join("\n"),
                    });
                    self.old_lines.clear();
                    self.new_lines.clear();
                }
            }
            SectionKind::Delete => {
                out.push(PatchHunk::DeleteFile {
                    path: self.path.clone(),
                });
                self.old_lines.clear();
                self.new_lines.clear();
            }
            SectionKind::Update => {
                if self.has_lines() {
                    out.push(PatchHunk::UpdateFile {
                        path: self.path.clone(),
                        old_text: self.old_lines.join("\n"),
                        new_text: self.new_lines.join("\n"),
                    });
                    self.old_lines.clear();
                    self.new_lines.clear();
                }
            }
        }
        Ok(())
    }

    /// Close a section: emit any trailing hunk, then reject an `Add`/`Update`
    /// section that was declared but never accumulated any body lines. `Delete`
    /// carries no lines by design and is always considered complete.
    fn finalize(self, out: &mut Vec<PatchHunk>) -> Result<(), ToolError> {
        let declared_with_body = match self.kind {
            SectionKind::Delete => true,
            SectionKind::Add | SectionKind::Update => self.had_lines,
        };
        let path = self.path.clone();
        let kind = self.kind;
        let mut section = self;
        section.flush_hunk(out)?;
        if !declared_with_body {
            return Err(ToolError::InvalidInput(format!(
                "apply_patch {} section '{}' has no body lines",
                section_kind_name(kind),
                path
            )));
        }
        Ok(())
    }
}

fn section_kind_name(kind: SectionKind) -> &'static str {
    match kind {
        SectionKind::Add => "Add File",
        SectionKind::Delete => "Delete File",
        SectionKind::Update => "Update File",
    }
}

pub fn parse_apply_patch(patch: &str) -> Result<ParsedPatch, ToolError> {
    // Tolerate trailing whitespace / blank lines after `*** End Patch` (a common
    // model artifact). `trim_end` only strips trailing whitespace, so a leading
    // indentation on `*** Begin Patch` remains a (correct) parse error.
    let trimmed = patch.trim_end();
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.first().copied() != Some("*** Begin Patch") {
        return Err(ToolError::InvalidInput(
            "apply_patch must start with *** Begin Patch".into(),
        ));
    }
    if lines.last().copied() != Some("*** End Patch") {
        return Err(ToolError::InvalidInput(
            "apply_patch must end with *** End Patch".into(),
        ));
    }

    let mut hunks = Vec::new();
    let mut current: Option<Section> = None;

    for line in lines
        .iter()
        .copied()
        .skip(1)
        .take(lines.len().saturating_sub(2))
    {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            if let Some(section) = current.take() {
                section.finalize(&mut hunks)?;
            }
            current = Some(Section::new(SectionKind::Add, path)?);
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            if let Some(section) = current.take() {
                section.finalize(&mut hunks)?;
            }
            current = Some(Section::new(SectionKind::Delete, path)?);
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            if let Some(section) = current.take() {
                section.finalize(&mut hunks)?;
            }
            current = Some(Section::new(SectionKind::Update, path)?);
            continue;
        }
        if line.starts_with("*** ") {
            return Err(ToolError::InvalidInput(format!(
                "unsupported apply_patch directive: {line}"
            )));
        }
        if line == "@@" || line.starts_with("@@ ") {
            if let Some(section) = current.as_mut() {
                if matches!(section.kind, SectionKind::Update) {
                    section.flush_hunk(&mut hunks)?;
                }
            }
            continue;
        }

        let Some(section) = current.as_mut() else {
            if line.trim().is_empty() {
                continue;
            }
            return Err(ToolError::InvalidInput(format!(
                "apply_patch content outside a file section: {line}"
            )));
        };
        match section.kind {
            SectionKind::Add => {
                let Some(added) = line.strip_prefix('+') else {
                    return Err(ToolError::InvalidInput(format!(
                        "Add File '{}' lines must start with '+'",
                        section.path
                    )));
                };
                section.new_lines.push(added.to_string());
                section.had_lines = true;
            }
            SectionKind::Delete => {
                if let Some(old) = line.strip_prefix('-') {
                    section.old_lines.push(old.to_string());
                    section.had_lines = true;
                } else if !line.trim().is_empty() {
                    return Err(ToolError::InvalidInput(format!(
                        "Delete File '{}' must have no body lines (got '{line}')",
                        section.path
                    )));
                }
            }
            SectionKind::Update => {
                if let Some(old) = line.strip_prefix('-') {
                    section.old_lines.push(old.to_string());
                    section.had_lines = true;
                } else if let Some(new) = line.strip_prefix('+') {
                    section.new_lines.push(new.to_string());
                    section.had_lines = true;
                } else if let Some(ctx) = line.strip_prefix(' ') {
                    section.old_lines.push(ctx.to_string());
                    section.new_lines.push(ctx.to_string());
                    section.had_lines = true;
                } else if !line.trim().is_empty() {
                    return Err(ToolError::InvalidInput(format!(
                        "Update File '{}' hunk line must start with ' ', '+', or '-'",
                        section.path
                    )));
                }
            }
        }
    }

    if let Some(section) = current.take() {
        section.finalize(&mut hunks)?;
    }
    if hunks.is_empty() {
        return Err(ToolError::InvalidInput(
            "apply_patch contains no hunks".into(),
        ));
    }
    Ok(ParsedPatch { hunks })
}

/// Resolve a patch-internal path against the workspace, refusing anything that
/// could escape it. This is the **final workspace-confinement gate** for
/// `apply_patch` (its paths live inside the patch body, not a top-level
/// `file_path`, so the permission layer cannot extract them).
pub fn resolve_workspace_path(workspace: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let candidate = Path::new(path);
    // `Path::starts_with` is a component-prefix check that does NOT canonicalize:
    // an absolute `/repo/../etc` would pass `starts_with("/repo")` yet resolve
    // outside the workspace. Reject any `..` component up front. Canonicalizing
    // instead would also fail for not-yet-existing `Add File` targets.
    if candidate
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(ToolError::InvalidInput(format!(
            "apply_patch path '{}' contains '..' and may escape workspace '{}'",
            path,
            workspace.display()
        )));
    }
    if candidate.is_absolute() {
        if candidate.starts_with(workspace) {
            return Ok(candidate.to_path_buf());
        }
        return Err(ToolError::InvalidInput(format!(
            "apply_patch path '{}' escapes workspace '{}'",
            path,
            workspace.display()
        )));
    }
    // Relative: refuse RootDir/Prefix (ParentDir already excluded above).
    if candidate
        .components()
        .any(|c| matches!(c, Component::RootDir | Component::Prefix(_)))
    {
        return Err(ToolError::InvalidInput(format!(
            "apply_patch path '{}' escapes workspace",
            path
        )));
    }
    Ok(workspace.join(candidate))
}
