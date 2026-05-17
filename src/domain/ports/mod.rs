// Port traits — used by adapters (noop.rs, future real adapters) and services.
// Suppress unused warnings: traits are consumed by adapter impls, not by domain code.
#[allow(unused_imports)]
mod approval_persistence;
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
mod scheduler;
#[allow(unused_imports)]
mod security;
#[allow(unused_imports)]
mod session_port;
#[allow(unused_imports)]
mod storage;
#[allow(unused_imports)]
mod toolset;
#[allow(unused_imports)]
mod usage_ledger;

pub use approval_persistence::ApprovalPersistencePort;
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
pub use scheduler::SchedulerPort;
pub use security::SecurityPort;
pub use session_port::SessionPort;
pub use storage::StoragePort;
pub use toolset::ToolSetPort;
pub use usage_ledger::UsageLedgerPort;
