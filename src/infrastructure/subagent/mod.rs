use std::collections::HashMap;
use std::path::PathBuf;

pub mod registry;
pub mod spool;

pub use registry::{AgentHandle, CascadeKillError, RegistryEntry, SubagentRegistry};
pub use spool::{SpoolMeta, SubagentSpool};
