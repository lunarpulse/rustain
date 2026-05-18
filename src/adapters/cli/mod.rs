pub mod commands;
pub mod config_cmd;
pub mod doctor;
pub mod init;
pub mod migrate;
pub mod profile_cmd;
#[cfg(feature = "openai")]
pub mod update_catalog;
