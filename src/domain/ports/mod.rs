// Port traits — used by adapters (noop.rs, future real adapters) and services.
// Suppress unused warnings: traits are consumed by adapter impls, not by domain code.
#[allow(unused_imports)]
mod approval_persistence;
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
mod event_emitter;
#[allow(unused_imports)]
mod memory;
#[allow(unused_imports)]
mod persona;
#[allow(unused_imports)]
mod profile_resolver;
#[allow(unused_imports)]
mod provider;
#[allow(unused_imports)]
mod provider_info;
#[allow(unused_imports)]
mod sandbox;
#[allow(unused_imports)]
mod scheduler;
#[allow(unused_imports)]
mod security;
#[allow(unused_imports)]
mod session_port;
#[allow(unused_imports)]
mod skill_exposure;
#[allow(unused_imports)]
mod storage;
#[allow(unused_imports)]
mod tool_exposure;
#[allow(unused_imports)]
mod toolset;
#[allow(unused_imports)]
mod usage_ledger;

pub use approval_persistence::ApprovalPersistencePort;
pub use capability_provider::CapabilityProvider;
pub use catalog_observer::CatalogObserver;
pub use catalog_observer::{ObserverError, SubscriptionHandle, SubscriptionId};
pub use channel::ChannelPort;
pub use clipboard::ClipboardPort;
pub use config_store::ConfigStorePort;
pub use context::ContextPort;
pub use event_emitter::EventEmitter;
pub use memory::MemoryPort;
pub use persona::PersonaPort;
pub use profile_resolver::ProfileResolver;
pub use provider::StreamingProvider;
pub use provider_info::ProviderInfoPort;
pub use sandbox::SandboxManager;
pub use scheduler::SchedulerPort;
pub use security::SecurityPort;
pub use session_port::SessionPort;
pub mod search;
pub use search::{IndexableItem, MetaSearchEngine, MetaSearchError};
pub use skill_exposure::SkillExposurePort;
pub use storage::StoragePort;
pub use tool_exposure::ToolExposurePort;
pub use toolset::ToolSetPort;
pub use usage_ledger::UsageLedgerPort;
