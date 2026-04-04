use clap::Parser;

/// Rustain — terminal-native AI coding agent.
#[derive(Parser, Debug)]
#[command(name = "rustain", version, about)]
pub struct Cli {
    /// Log level override (default: info)
    #[arg(long, default_value = "info")]
    pub log_level: String,

    /// Start a new session (preserves existing sessions)
    #[arg(long, conflicts_with = "session")]
    pub new: bool,

    /// Resume a specific session by ID
    #[arg(long)]
    pub session: Option<String>,
}
