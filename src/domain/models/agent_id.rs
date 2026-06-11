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
        Self(nanoid::nanoid!(12))
    }

    /// Sentinel for the root agent (used in PermissionChain recursion-guard comparisons).
    pub fn root() -> Self {
        Self(String::from("root"))
    }
}
