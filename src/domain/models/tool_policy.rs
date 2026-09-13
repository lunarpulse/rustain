use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPolicy {
    InheritFromParent,
    Allowlist {
        tools: BTreeSet<String>,
    },
    Denylist {
        tools: BTreeSet<String>,
    },
    /// A delegated policy narrowed by the parent's declared items. `effective`
    /// is the exact-name intersection from [`ToolPolicy::resolve`]; `parent`
    /// and `child` retain command patterns so Bash can be checked without
    /// widening either side to the bare tool.
    ResolvedAgainstParent {
        effective: BTreeSet<String>,
        parent: BTreeSet<String>,
        child: Box<ToolPolicy>,
    },
}

impl ToolPolicy {
    /// Resolve exact declared items against the parent's declared-item set.
    /// Command patterns remain declared items here; execution evaluates them
    /// through the shared matcher rather than expanding them before inheritance.
    pub fn resolve(&self, parent: &BTreeSet<String>) -> BTreeSet<String> {
        match self {
            ToolPolicy::InheritFromParent => parent.clone(),
            ToolPolicy::Allowlist { tools } => tools.intersection(parent).cloned().collect(),
            ToolPolicy::Denylist { tools } => parent.difference(tools).cloned().collect(),
            ToolPolicy::ResolvedAgainstParent { effective, .. } => {
                effective.intersection(parent).cloned().collect()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::iter::FromIterator;

    fn bt(els: &[&str]) -> BTreeSet<String> {
        BTreeSet::from_iter(els.iter().map(|s| s.to_string()))
    }

    #[test]
    fn inherit_from_parent() {
        let parent = bt(&["a", "b", "c"]);
        let policy = ToolPolicy::InheritFromParent;
        assert_eq!(policy.resolve(&parent), parent);
    }

    #[test]
    fn allowlist_intersection() {
        let parent = bt(&["a", "b", "c"]);
        let policy = ToolPolicy::Allowlist {
            tools: bt(&["b", "c", "d"]),
        };
        assert_eq!(policy.resolve(&parent), bt(&["b", "c"]));
    }

    #[test]
    fn allowlist_empty_parent() {
        let parent = BTreeSet::new();
        let policy = ToolPolicy::Allowlist {
            tools: bt(&["a", "b"]),
        };
        assert_eq!(policy.resolve(&parent), BTreeSet::new());
    }

    #[test]
    fn allowlist_disjoint() {
        let parent = bt(&["a", "b"]);
        let policy = ToolPolicy::Allowlist {
            tools: bt(&["c", "d"]),
        };
        assert_eq!(policy.resolve(&parent), BTreeSet::new());
    }

    #[test]
    fn allowlist_overlapping() {
        let parent = bt(&["a", "b", "c", "d"]);
        let policy = ToolPolicy::Allowlist {
            tools: bt(&["b", "d", "e"]),
        };
        assert_eq!(policy.resolve(&parent), bt(&["b", "d"]));
    }

    #[test]
    fn denylist_difference() {
        let parent = bt(&["a", "b", "c"]);
        let policy = ToolPolicy::Denylist { tools: bt(&["b"]) };
        assert_eq!(policy.resolve(&parent), bt(&["a", "c"]));
    }

    #[test]
    fn denylist_empty_parent() {
        let parent = BTreeSet::new();
        let policy = ToolPolicy::Denylist {
            tools: bt(&["a", "b"]),
        };
        assert_eq!(policy.resolve(&parent), BTreeSet::new());
    }

    #[test]
    fn denylist_disjoint() {
        let parent = bt(&["a", "b"]);
        let policy = ToolPolicy::Denylist {
            tools: bt(&["c", "d"]),
        };
        assert_eq!(policy.resolve(&parent), bt(&["a", "b"]));
    }

    #[test]
    fn denylist_overlapping() {
        let parent = bt(&["a", "b", "c", "d"]);
        let policy = ToolPolicy::Denylist {
            tools: bt(&["b", "d", "e"]),
        };
        assert_eq!(policy.resolve(&parent), bt(&["a", "c"]));
    }
}
