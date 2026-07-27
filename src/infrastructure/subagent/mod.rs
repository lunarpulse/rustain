use std::collections::HashMap;
use std::path::PathBuf;

pub mod node_handle;
pub mod node_journal;
pub mod node_recovery;
pub mod node_tree;
pub mod registry;
pub mod spool;

pub use node_handle::{NodeHandle, NodeHandleError};
pub use node_journal::{
    JournalArtifactSink, JournalError, NodeJournal, NodeRoomJournal, RecoveryError,
    WorkspaceJournalReader,
};
pub use node_recovery::{
    DaemonSingletonLock, NodeRecovery, RecoveredPark, RecoveryReport, current_host_binding,
    current_host_id, fold_parked_records,
};
pub use node_tree::{
    AgentHandle, CascadeKillError, LocalMessageBus, MAILBOX_CAP, MAX_CHILDREN, MAX_DEPTH,
    MailboxBudget, NodeTree, OwnerCommandError, RegistryEntry,
};
pub use spool::{SpoolMeta, SubagentSpool};
