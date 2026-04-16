use std::path::PathBuf;

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
    /// Import conversation history from another tool
    Migrate {
        /// Source tool identifier (only "claude-code" supported in v1)
        #[arg(long)]
        from: String,
        /// Override the source session directory
        #[arg(long)]
        path: Option<PathBuf>,
        /// Import all discovered sessions without prompting
        #[arg(long, short, conflicts_with = "select")]
        yes: bool,
        /// Interactive per-session selection
        #[arg(long, short, conflicts_with = "yes")]
        select: bool,
        /// Discover and print the candidate list without writing anything
        #[arg(long)]
        dry_run: bool,
    },
}
