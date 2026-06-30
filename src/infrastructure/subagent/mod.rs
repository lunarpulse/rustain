use std::collections::HashMap;
use std::path::PathBuf;

pub mod node_handle;
pub mod node_tree;
pub mod registry;
pub mod spool;

pub use node_handle::{NodeHandle, NodeHandleError};
pub use node_tree::{
    AgentHandle, CascadeKillError, LocalMessageBus, MAX_CHILDREN, MAX_DEPTH, NodeTree,
    OwnerCommandError, RegistryEntry,
};
pub use spool::{SpoolMeta, SubagentSpool};
