use serde::{Deserialize, Serialize};

/// Newtype for an agent identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentId {
    /// Generate a fresh 12-character URL-safe ID.
    pub fn new() -> Self {
        let id = nanoid::nanoid!(12);
        debug_assert!(!id.contains('/'), "nanoid must not contain path separator");
        Self(id)
    }

    /// Sentinel for the root agent (used in PermissionChain recursion-guard comparisons).
    pub fn root() -> Self {
        Self(String::from("root"))
    }

    /// Split the id into its `'/'`-separated path segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Returns `true` when this id has no path separator (a single local segment).
    pub fn is_local(&self) -> bool {
        !self.0.contains('/')
    }

    /// Build an id by joining `segs` with `'/'`.
    ///
    /// Debug-asserts the segments are well-formed (non-empty, no embedded
    /// `/`, none equal to the `"root"` sentinel) so a malformed R2 caller is
    /// caught in tests — consistent with the `debug_assert` guard in
    /// [`Self::new`].
    pub fn from_segments(segs: &[&str]) -> Self {
        debug_assert!(
            !segs.is_empty()
                && segs
                    .iter()
                    .all(|s| !s.is_empty() && !s.contains('/') && *s != "root"),
            "AgentId segments must be non-empty, free of '/', and not \"root\""
        );
        Self(segs.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::AgentId;

    #[test]
    fn segments_single_local_id() {
        let id = AgentId(String::from("abc"));
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["abc"]);
    }

    #[test]
    fn segments_multiple_path_id() {
        let id = AgentId(String::from("peer1/sub/child"));
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["peer1", "sub", "child"]);
    }

    #[test]
    fn is_local_true_for_local_id() {
        let id = AgentId(String::from("abc"));
        assert!(id.is_local());
    }

    #[test]
    fn is_local_false_for_path_id() {
        let id = AgentId(String::from("peer1/sub"));
        assert!(!id.is_local());
    }

    #[test]
    fn from_segments_roundtrip() {
        let id = AgentId::from_segments(&["peer1", "sub", "child"]);
        assert_eq!(id.0, "peer1/sub/child");
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["peer1", "sub", "child"]);
    }

    #[test]
    fn root_sentinel_is_preserved_and_local() {
        let root = AgentId::root();
        assert_eq!(root.0, "root");
        assert!(root.is_local());
    }

    #[test]
    fn new_is_always_local() {
        for _ in 0..100 {
            assert!(AgentId::new().is_local());
        }
    }
}
