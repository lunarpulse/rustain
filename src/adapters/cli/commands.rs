use clap::Parser;

/// Rustain — terminal-native AI coding agent.
#[derive(Parser, Debug)]
#[command(name = "rustain", version, about)]
pub struct Cli {
    /// Log level override (default: info)
    #[arg(long, default_value = "info")]
    pub log_level: String,
}
