use crate::domain::models::{SandboxPolicy, SubagentError};

/// Validate that a child sandbox policy is a narrowing of the parent policy.
/// Returns the effective child policy on success, or `PolicyWidensParent` on failure.
///
/// Implements ADR-10-3 narrow-only contract:
/// - Permissive parent → any child (Permissive, WorkspaceWrite, ReadOnly)
/// - WorkspaceWrite parent → WorkspaceWrite or ReadOnly child
/// - ReadOnly parent → ReadOnly child only
/// - Network: parent `network: false` → child `network: false` required
/// - Writable roots: child must be a subset of parent writable roots
/// - Read-only paths: child must be a superset of parent read-only paths
pub fn validate_narrowing(
    parent: &SandboxPolicy,
    child: &SandboxPolicy,
) -> Result<SandboxPolicy, SubagentError> {
    match (parent, child) {
        // Permissive parent allows any child
        (SandboxPolicy::Permissive, _) => Ok(child.clone()),

        // ReadOnly parent only allows ReadOnly child
        (SandboxPolicy::ReadOnly { .. }, SandboxPolicy::ReadOnly { .. }) => {
            check_network(parent, child)?;
            Ok(child.clone())
        }
        (SandboxPolicy::ReadOnly { .. }, _) => Err(SubagentError::PolicyWidensParent {
            dimension: String::from("variant"),
            child_request: format!("{:?}", child),
            parent_ceiling: format!("{:?}", parent),
        }),

        // WorkspaceWrite parent allows WorkspaceWrite or ReadOnly child
        (SandboxPolicy::WorkspaceWrite { .. }, SandboxPolicy::WorkspaceWrite { .. }) => {
            check_network(parent, child)?;
            check_workspace_narrowing(parent, child)?;
            Ok(child.clone())
        }
        (SandboxPolicy::WorkspaceWrite { .. }, SandboxPolicy::ReadOnly { .. }) => {
            check_network(parent, child)?;
            Ok(child.clone())
        }
        (SandboxPolicy::WorkspaceWrite { .. }, SandboxPolicy::Permissive) => {
            Err(SubagentError::PolicyWidensParent {
                dimension: String::from("variant"),
                child_request: format!("{:?}", child),
                parent_ceiling: format!("{:?}", parent),
            })
        }
    }
}

fn check_network(parent: &SandboxPolicy, child: &SandboxPolicy) -> Result<(), SubagentError> {
    let parent_network = network_of(parent);
    let child_network = network_of(child);
    if !parent_network && child_network {
        return Err(SubagentError::PolicyWidensParent {
            dimension: String::from("network"),
            child_request: format!("network={}", child_network),
            parent_ceiling: format!("network={}", parent_network),
        });
    }
    Ok(())
}

fn network_of(policy: &SandboxPolicy) -> bool {
    match policy {
        SandboxPolicy::Permissive => true,
        SandboxPolicy::ReadOnly { network } => *network,
        SandboxPolicy::WorkspaceWrite { network, .. } => *network,
    }
}

fn check_workspace_narrowing(
    parent: &SandboxPolicy,
    child: &SandboxPolicy,
) -> Result<(), SubagentError> {
    let (parent_writable, parent_readonly) = match parent {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            read_only_paths,
            ..
        } => (writable_roots, read_only_paths),
        _ => return Ok(()),
    };
    let (child_writable, child_readonly) = match child {
        SandboxPolicy::WorkspaceWrite {
            writable_roots,
            read_only_paths,
            ..
        } => (writable_roots, read_only_paths),
        _ => return Ok(()),
    };

    // Child writable roots must be subset of parent writable roots
    let parent_writable_set: std::collections::HashSet<_> = parent_writable.iter().collect();
    for root in child_writable {
        if !parent_writable_set.contains(root) {
            return Err(SubagentError::PolicyWidensParent {
                dimension: String::from("writable_roots"),
                child_request: format!("writable_root={:?}", root),
                parent_ceiling: format!("parent_writable={:?}", parent_writable),
            });
        }
    }

    // Child read-only paths must be superset of parent read-only paths
    // (more read-only restriction = narrower)
    let child_readonly_set: std::collections::HashSet<_> = child_readonly.iter().collect();
    for path in parent_readonly {
        if !child_readonly_set.contains(path) {
            return Err(SubagentError::PolicyWidensParent {
                dimension: String::from("read_only_paths"),
                child_request: format!("missing_read_only={:?}", path),
                parent_ceiling: format!("parent_readonly={:?}", parent_readonly),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn permissive_parent_allows_any_child() {
        let parent = SandboxPolicy::Permissive;
        assert!(validate_narrowing(&parent, &SandboxPolicy::Permissive).is_ok());
        assert!(validate_narrowing(&parent, &SandboxPolicy::ReadOnly { network: true }).is_ok());
        assert!(
            validate_narrowing(
                &parent,
                &SandboxPolicy::WorkspaceWrite {
                    writable_roots: vec![PathBuf::from("/tmp")],
                    read_only_paths: vec![],
                    network: true,
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn readonly_parent_rejects_workspace_write() {
        let parent = SandboxPolicy::ReadOnly { network: true };
        let child = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![],
            network: true,
        };
        assert!(validate_narrowing(&parent, &child).is_err());
    }

    #[test]
    fn readonly_parent_rejects_permissive() {
        let parent = SandboxPolicy::ReadOnly { network: true };
        assert!(validate_narrowing(&parent, &SandboxPolicy::Permissive).is_err());
    }

    #[test]
    fn readonly_parent_allows_readonly() {
        let parent = SandboxPolicy::ReadOnly { network: false };
        let child = SandboxPolicy::ReadOnly { network: false };
        assert!(validate_narrowing(&parent, &child).is_ok());
    }

    #[test]
    fn workspace_write_parent_allows_readonly() {
        let parent = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![],
            network: true,
        };
        let child = SandboxPolicy::ReadOnly { network: true };
        assert!(validate_narrowing(&parent, &child).is_ok());
    }

    #[test]
    fn workspace_write_parent_rejects_permissive() {
        let parent = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![],
            network: true,
        };
        assert!(validate_narrowing(&parent, &SandboxPolicy::Permissive).is_err());
    }

    #[test]
    fn workspace_write_parent_allows_workspace_write_subset() {
        let parent = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp"), PathBuf::from("/home")],
            read_only_paths: vec![PathBuf::from("/etc")],
            network: true,
        };
        let child = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![PathBuf::from("/etc"), PathBuf::from("/var")],
            network: true,
        };
        assert!(validate_narrowing(&parent, &child).is_ok());
    }

    #[test]
    fn workspace_write_parent_rejects_extra_writable_root() {
        let parent = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![],
            network: true,
        };
        let child = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp"), PathBuf::from("/home")],
            read_only_paths: vec![],
            network: true,
        };
        assert!(validate_narrowing(&parent, &child).is_err());
    }

    #[test]
    fn workspace_write_parent_rejects_missing_readonly_path() {
        let parent = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![PathBuf::from("/etc")],
            network: true,
        };
        let child = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![PathBuf::from("/tmp")],
            read_only_paths: vec![],
            network: true,
        };
        assert!(validate_narrowing(&parent, &child).is_err());
    }

    #[test]
    fn network_narrowing_true_to_false_ok() {
        let parent = SandboxPolicy::ReadOnly { network: true };
        let child = SandboxPolicy::ReadOnly { network: false };
        assert!(validate_narrowing(&parent, &child).is_ok());
    }

    #[test]
    fn network_narrowing_false_to_true_rejected() {
        let parent = SandboxPolicy::ReadOnly { network: false };
        let child = SandboxPolicy::ReadOnly { network: true };
        assert!(validate_narrowing(&parent, &child).is_err());
    }
}
