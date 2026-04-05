use clap::{Parser, Subcommand};

/// Rustain — terminal-native AI coding agent.
#[derive(Parser, Debug)]
#[command(name = "rustain", version, about)]
pub struct Cli {
    /// Log level override (default: info)
    #[arg(long, default_value = "info", global = true)]
    pub log_level: String,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start a new session (preserves existing sessions)
    #[arg(long, conflicts_with = "session")]
    pub new: bool,

    /// Resume a specific session by ID
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize rustain configuration
    Init,
    /// Check setup health and diagnose problems
    Doctor {
        /// Show detailed terminal diagnostics
        #[arg(long)]
        terminal: bool,
    },
}
