// Port traits — used by adapters (noop.rs, future real adapters) and services.
// Suppress unused warnings: traits are consumed by adapter impls, not by domain code.
#[allow(unused_imports)]
mod agent_message_bus;
#[allow(unused_imports)]
mod agent_transport;
#[allow(unused_imports)]
mod approval_persistence;
#[allow(unused_imports)]
mod artifact_sink;
#[allow(unused_imports)]
mod artifact_store;
mod auth_store;
#[allow(unused_imports)]
mod authority_provider;
#[allow(unused_imports)]
mod capability_provider;
#[allow(unused_imports)]
mod catalog_observer;
mod channel;
#[allow(unused_imports)]
mod clipboard;
mod config_store;
#[allow(unused_imports)]
mod context;
#[allow(unused_imports)]
mod context_assembler;
#[allow(unused_imports)]
mod event_emitter;
#[allow(unused_imports)]
mod execution_sandbox;
#[allow(unused_imports)]
mod inbound_peer_runtime;
#[allow(unused_imports)]
mod isolation_provider;
mod ledger_journal_sink;
#[allow(unused_imports)]
mod memory;
#[allow(unused_imports)]
mod node_orchestrator;
#[allow(unused_imports)]
mod patch_applier;
#[allow(unused_imports)]
mod persona;
#[allow(unused_imports)]
mod profile_resolver;
#[allow(unused_imports)]
mod provider;
#[allow(unused_imports)]
mod provider_info;
#[allow(unused_imports)]
mod recall_provider;
#[allow(unused_imports)]
mod room_journal;
#[allow(unused_imports)]
mod sandbox;
#[allow(unused_imports)]
mod scheduler;
#[allow(unused_imports)]
mod security;
#[cfg(feature = "self-update")]
pub mod self_update;
#[allow(unused_imports)]
mod session_holder;
#[allow(unused_imports)]
mod session_port;
#[allow(unused_imports)]
mod skill_exposure;
#[allow(unused_imports)]
mod storage;
#[allow(unused_imports)]
mod subagent_runner;
mod supervised_nodes;
mod task_nodes;
#[allow(unused_imports)]
mod tool_exposure;
#[allow(unused_imports)]
mod toolset;
#[allow(unused_imports)]
mod usage_ledger;
#[allow(unused_imports)]
pub mod wave_handle;
#[allow(unused_imports)]
mod workspace_registry;
#[cfg(feature = "self-update")]
pub use self_update::{BinaryReplacerPort, SelfUpdatePort};

pub use agent_message_bus::{
    AgentMessageBus, DeliveryError, DeliveryPolicy, RelationshipDeliveryPolicy,
};
pub use agent_transport::{AgentTransport, AgentTransportError};
pub use approval_persistence::ApprovalPersistencePort;
pub use artifact_sink::{ArtifactSink, ArtifactSinkError};
pub use artifact_store::{ArtifactError, ArtifactStore};
pub use auth_store::AuthStorePort;
pub use authority_provider::{AuthorityError, AuthorityProvider};
pub use capability_provider::CapabilityProvider;
pub use catalog_observer::CatalogObserver;
pub use catalog_observer::{ObserverError, SubscriptionHandle, SubscriptionId};
pub use channel::ChannelPort;
pub use clipboard::ClipboardPort;
pub use config_store::ConfigStorePort;
pub use context::ContextPort;
pub use context_assembler::ContextAssemblerPort;
pub use event_emitter::EventEmitter;
pub use execution_sandbox::{ExecutionSandbox, ExecutionSandboxError};
pub use inbound_peer_runtime::{
    InboundApprovalTicket, InboundPeerError, InboundPeerRuntime, InboundPeerTask,
};
pub use isolation_provider::IsolationProvider;
pub use ledger_journal_sink::{LedgerJournalError, LedgerJournalSink};
pub use memory::MemoryPort;
pub use node_orchestrator::{ForkJoinRequest, Orchestrator};
pub use patch_applier::{PatchApplier, PatchApplyError};
pub use persona::PersonaPort;
pub use profile_resolver::ProfileResolver;
pub use provider::{ProbeOutcome, StreamingProvider};
pub use provider_info::ProviderInfoPort;
pub use recall_provider::RecallProviderPort;
pub use room_journal::{RoomJournal, RoomJournalError, RoomJournalReader};
pub use sandbox::SandboxManager;
pub use scheduler::SchedulerPort;
pub use security::SecurityPort;
pub use session_holder::{HeldSession, HolderState, SessionHolderPort};
pub use session_port::SessionPort;
pub use subagent_runner::SubagentRunner;
pub use supervised_nodes::{SupervisedNodes, SupervisedNodesError};
pub use task_nodes::{TaskNodeHandle, TaskNodes, TaskNodesError};
pub use wave_handle::{DrillBody, RerunOutcome, WaveHandle, WaveSnapshot};
pub mod search;
pub use search::{IndexableItem, MetaSearchEngine, MetaSearchError};
pub use skill_exposure::SkillExposurePort;
pub use storage::StoragePort;
pub use tool_exposure::ToolExposurePort;
pub use toolset::ToolSetPort;
pub use usage_ledger::UsageLedgerPort;
pub use workspace_registry::{
    WorkspaceEntry, WorkspaceRegistrarPort, WorkspaceRegistryError, WorkspaceRegistryReaderPort,
};
