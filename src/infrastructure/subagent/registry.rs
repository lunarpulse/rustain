//! Legacy module — all types promoted to [`super::node_tree`] in Story 14.1.
//!
//! This module re-exports from `node_tree` for backward compatibility.
//! External callers should import from `crate::infrastructure::subagent` directly.

pub use super::node_tree::{
    AgentHandle, CascadeKillError, NodeTree, OwnerCommandError, RegistryEntry,
};
