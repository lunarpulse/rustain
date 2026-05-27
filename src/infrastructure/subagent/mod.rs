use std::collections::HashMap;
use std::path::PathBuf;

pub mod registry;
pub mod spool;

pub use registry::SubagentRegistry;
pub use spool::{SpoolMeta, SubagentSpool};
