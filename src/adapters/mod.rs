pub mod agent_activation;
pub mod agent_registry;
pub mod approval_persistence_toml;
pub mod auth_store;
pub mod authority;
pub mod budget;
pub mod builtin;
#[cfg(feature = "telegram")]
pub mod channel;
pub mod cli;
pub mod clipboard_adapter;
pub mod command_registry;
pub mod daemon;
pub mod daily_log_memory;
pub mod file_scanner;
pub mod filesystem;
pub mod importers;
pub mod isolation;
pub mod ledger;
pub mod long_term_memory;
pub mod memory_context;
pub mod model_catalog_cache;
#[cfg(feature = "models-dev")]
pub mod models_dev;
pub mod noop;
pub mod palette_registry;
pub mod persona_adapter;
pub mod profile_resolver;
pub mod project_context_loader;
pub mod project_scoped_memory;
pub mod provider;
pub mod sandbox;
#[cfg(feature = "cron")]
pub mod scheduler;
pub mod security_adapter;
#[cfg(feature = "self-update")]
pub mod self_update;
pub mod skill_activation;
pub mod skill_exposure;
pub mod skill_provider;
pub mod skill_registry;
pub mod subagent;
pub mod tool_exposure;
pub mod toolset_adapter;
pub mod tui;
pub mod workspace_registry;

// Story 11.3a — local semantic-search memory adapter. Feature-gated so the
// default build pulls neither fastembed/ort nor bincode (NFR9 preserved).
#[cfg(feature = "vector-search")]
pub mod vector_search;

#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "mcp")]
pub mod composite_toolset_adapter;

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "ollama")]
pub mod ollama;
