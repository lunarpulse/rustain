//! The one pathname-confinement rule for model-suggested paths (Story Q-6 AC2).
//!
//! `SecurityAdapter` (the permission boundary, shared by approval Step 2.5 and
//! Step 3) and `ToolSetAdapter`'s `Write`/`Edit` executors (the I/O boundary)
//! both call [`confine`], so the two boundaries cannot drift apart.
//!
//! The rule distinguishes four leaf states for a path that does not
//! canonicalize:
//! - **absent ordinary leaf** under an existing, verified in-workspace parent —
//!   a new-file candidate, allowed;
//! - **link or indeterminate leaf** (a dangling symlink, or an entry that exists
//!   but cannot be resolved) — denied: whatever it points at was never verified;
//! - **unverifiable parent** — denied;
//! - **traversal** — denied.
//!
//! This is a pathname check, not a promise of race-proof filesystem isolation or
//! of containing an arbitrary program's I/O.

use std::path::{Path, PathBuf};

use crate::domain::errors::PermissionError;
use crate::domain::models::{FileContextProvenance, FileOperation, PathAccessType};

/// Absolute prefixes no model-suggested path may name, before or after
/// resolution.
pub const DEFAULT_BLOCKED_PATH_PREFIXES: &[&str] = &[
    "/etc/", "/bin/", "/usr/", "/sys/", "/proc/", "/dev/", "/boot/", "/root/",
];

fn violation(reason: &str) -> PermissionError {
    PermissionError::WorkspaceViolation(reason.to_string())
}

/// Classify `path` against `workspace`.
///
/// `read_exempt` names the extra roots an in-policy *read* may resolve into (the
/// active skill directories); a write never consults it.
pub fn confine(
    workspace: &Path,
    blocked: &[impl AsRef<str>],
    path: &Path,
    op: FileOperation,
    provenance: FileContextProvenance,
    read_exempt: &dyn Fn(&Path) -> bool,
) -> Result<PathAccessType, PermissionError> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(violation("Path traversal not allowed"));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };

    let workspace_canonical =
        std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());

    // Apply the blocklist to the requested path even before canonicalization so
    // user-provided paths cannot name blocked prefixes by a non-existent path.
    let absolute_str = absolute.to_string_lossy();
    for blocked in blocked.iter().map(AsRef::as_ref) {
        if absolute_str.starts_with(blocked) {
            return Err(PermissionError::WorkspaceViolation(format!(
                "Access to {blocked} is not allowed"
            )));
        }
    }

    let resolved = match std::fs::canonicalize(&absolute) {
        Ok(p) => p,
        Err(_) => {
            if provenance == FileContextProvenance::UserProvided {
                return Ok(PathAccessType::External);
            }
            if provenance == FileContextProvenance::RemoteProvided {
                return Err(violation(
                    "Remote-provided path must exist and resolve within workspace",
                ));
            }
            resolve_unresolved_leaf(path, &absolute, &workspace_canonical)?
        }
    };

    let resolved_str = resolved.to_string_lossy();
    for blocked in blocked.iter().map(AsRef::as_ref) {
        if resolved_str.starts_with(blocked) {
            return Err(PermissionError::WorkspaceViolation(format!(
                "Access to {blocked} is not allowed"
            )));
        }
    }

    if resolved.starts_with(&workspace_canonical) {
        Ok(PathAccessType::Workspace)
    } else if provenance == FileContextProvenance::RemoteProvided {
        Err(violation("Remote-provided path resolves outside workspace"))
    } else if provenance == FileContextProvenance::UserProvided && op == FileOperation::Read {
        Ok(PathAccessType::External)
    } else if op == FileOperation::Read && read_exempt(&resolved) {
        Ok(PathAccessType::Workspace)
    } else {
        Err(violation("Path outside workspace"))
    }
}

/// `absolute` did not canonicalize: accept it only as an absent ordinary leaf
/// under an existing parent that resolves inside the workspace.
fn resolve_unresolved_leaf(
    requested: &Path,
    absolute: &Path,
    workspace_canonical: &Path,
) -> Result<PathBuf, PermissionError> {
    let Some(parent) = absolute.parent() else {
        return Err(PermissionError::WorkspaceViolation(format!(
            "Path '{}' not in workspace — no parent directory",
            requested.display()
        )));
    };
    let Ok(canon_parent) = std::fs::canonicalize(parent) else {
        return Err(PermissionError::WorkspaceViolation(format!(
            "Path '{}' not in workspace — parent does not exist",
            requested.display()
        )));
    };
    if !canon_parent.starts_with(workspace_canonical) {
        return Err(violation("Parent directory resolves outside workspace"));
    }
    let Some(file_name) = absolute.file_name() else {
        return Ok(canon_parent);
    };
    let leaf = canon_parent.join(file_name);
    if !leaf.starts_with(&canon_parent) {
        return Err(violation("Path traversal detected via symlink"));
    }
    match std::fs::canonicalize(&leaf) {
        Ok(final_resolved) => {
            if final_resolved.starts_with(workspace_canonical) {
                Ok(final_resolved)
            } else {
                Err(violation("Symlink escape detected"))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `canonicalize` also reports NotFound for a dangling symlink, so
            // NotFound alone does not prove the leaf is absent: only a leaf the
            // filesystem reports as absent *without following a link* is a
            // new-file candidate.
            match std::fs::symlink_metadata(&leaf) {
                Err(meta) if meta.kind() == std::io::ErrorKind::NotFound => Ok(leaf),
                _ => Err(violation(
                    "Path leaf is a link or could not be resolved — rejecting for safety",
                )),
            }
        }
        Err(_) => Err(violation("Path resolution failed — rejecting for safety")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked() -> Vec<String> {
        DEFAULT_BLOCKED_PATH_PREFIXES
            .iter()
            .map(|prefix| (*prefix).to_string())
            .collect()
    }

    fn check(workspace: &Path, path: &Path, op: FileOperation) -> Result<PathAccessType, String> {
        confine(
            workspace,
            &blocked(),
            path,
            op,
            FileContextProvenance::ModelSuggested,
            &|_| false,
        )
        .map_err(|e| e.to_string())
    }

    #[test]
    fn an_absent_leaf_under_an_existing_workspace_parent_is_a_new_file_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        assert_eq!(
            check(tmp.path(), Path::new("sub/new.txt"), FileOperation::Write),
            Ok(PathAccessType::Workspace)
        );
    }

    #[test]
    fn an_unverifiable_parent_and_traversal_are_denied() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            check(
                tmp.path(),
                Path::new("missing/new.txt"),
                FileOperation::Write
            )
            .unwrap_err()
            .contains("parent does not exist")
        );
        assert!(
            check(tmp.path(), Path::new("../x"), FileOperation::Write)
                .unwrap_err()
                .contains("traversal")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_leaf_is_denied_whether_it_points_inside_or_outside() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = std::fs::canonicalize(tmp.path()).unwrap();
        std::os::unix::fs::symlink(ws.join("not-yet"), ws.join("inside")).unwrap();
        std::os::unix::fs::symlink("/nonexistent-q6-target", ws.join("outside")).unwrap();
        for leaf in ["inside", "outside"] {
            for op in [FileOperation::Write, FileOperation::Read] {
                assert!(
                    check(&ws, Path::new(leaf), op)
                        .unwrap_err()
                        .contains("link or could not be resolved"),
                    "{leaf}/{op:?}"
                );
            }
        }
    }
}
