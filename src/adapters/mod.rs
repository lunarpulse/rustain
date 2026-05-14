pub mod agent_activation;
pub mod agent_registry;
pub mod approval_persistence_toml;
pub mod cli;
pub mod clipboard_adapter;
pub mod command_registry;
pub mod file_scanner;
pub mod filesystem;
pub mod importers;
pub mod ledger;
pub mod noop;
pub mod palette_registry;
pub mod persona_adapter;
pub mod project_context_loader;
pub mod provider;
pub mod security_adapter;
pub mod skill_activation;
pub mod skill_registry;
pub mod toolset_adapter;
pub mod tui;

#[cfg(feature = "anthropic")]
pub mod anthropic;

#[cfg(feature = "openai")]
pub mod openai;

#[cfg(feature = "ollama")]
pub mod ollama;
