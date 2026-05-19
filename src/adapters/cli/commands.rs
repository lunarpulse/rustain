use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Rustain — terminal-native AI coding agent.
#[derive(Parser, Debug)]
#[command(name = "rustain", version, about)]
pub struct Cli {
    /// Log level override (default: info). Absent → defer to config layers.
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Start a new session (preserves existing sessions)
    #[arg(long, conflicts_with = "session")]
    pub new: bool,

    /// Resume a specific session by ID
    #[arg(long)]
    pub session: Option<String>,

    /// Override snapshot retention count (default: 100, from config)
    #[arg(long)]
    pub snapshot_retention: Option<usize>,

    /// Override the default model (Story 8.1 AC-2 — highest-priority config layer)
    #[arg(long)]
    pub model: Option<String>,

    /// Path to a workspace-level config file (overrides {workspace}/.rustain/config.toml)
    #[arg(long)]
    pub config_file: Option<PathBuf>,
    /// Active profile name. Overrides RUSTAIN_PROFILE env var and active_profile config field. Default: coding.
    #[arg(long, short = 'p', global = true)]
    pub profile: Option<String>,
    /// Override persona adapter for this session. NOT persisted to profile.
    #[arg(long, global = true)]
    pub persona: Option<String>,
    /// Override memory adapter for this session.
    #[arg(long, global = true)]
    pub memory: Option<String>,
    /// Override session adapter for this session (named --session-adapter to avoid collision with --session resume flag).
    #[arg(long, global = true)]
    pub session_adapter: Option<String>,
    /// Override tools adapter for this session.
    #[arg(long, global = true)]
    pub tools: Option<String>,
    /// Override channels adapter for this session.
    #[arg(long, global = true)]
    pub channels: Option<String>,
    /// Override scheduler adapter for this session.
    #[arg(long, global = true)]
    pub scheduler: Option<String>,
    /// Override context adapter for this session.
    #[arg(long, global = true)]
    pub context: Option<String>,
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
        /// Run adapter conformance smoke-checks (NFR44)
        #[arg(long)]
        adapters: bool,
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
    /// Fetch latest models from providers and update models_variants.json (Story 7.7 AC4)
    UpdateCatalog {
        /// Output path for the updated catalog JSON (default: ./models_variants.json)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Only update specific provider IDs (e.g., "openai", "deepseek")
        #[arg(long)]
        provider: Vec<String>,
    },
    /// Configuration management commands (Story 8.1 AC-9)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Profile management commands (Story 8.4 — switch only; remaining commands ship in Story 8.6a)
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Reload configuration in the running TUI (cross-process reload stub — see AC-9)
    Reload,
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    /// List all available profiles
    List {
        /// Output as JSON for scripting
        #[arg(long)]
        json: bool,
    },
    /// Show full resolved profile configuration
    Show {
        /// Profile name
        name: String,
        /// Output as JSON
        #[arg(long, conflicts_with = "toml_out")]
        json: bool,
        /// Output as flat shareable TOML
        #[arg(long = "toml", conflicts_with = "json")]
        toml_out: bool,
    },
    /// Interactive profile builder
    Create {
        /// Pre-populate name (skips name prompt)
        #[arg(long)]
        name: Option<String>,
        /// Pre-populate extends parent
        #[arg(long)]
        extends: Option<String>,
        /// Copy adapter selections from an existing profile
        #[arg(long)]
        from: Option<String>,
    },
    /// Open profile TOML in $EDITOR
    Edit {
        /// Profile name
        name: String,
        /// Skip post-save validation
        #[arg(long)]
        no_validate: bool,
    },
    /// Switch the active profile (Story 8.4 — TUI must be running)
    Switch {
        /// Target profile name
        name: String,
        /// Launch a new rustain TUI with this profile (instead of in-place switch)
        #[arg(long)]
        start: bool,
    },
    /// Validate profile configuration
    Validate {
        /// Profile name (omit with --all to validate all known profiles)
        name: Option<String>,
        /// Validate every profile in the registry
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Output as JSON for CI scripting
        #[arg(long)]
        json: bool,
    },
    /// Export profile as shareable flat TOML
    Export {
        /// Profile name
        name: String,
        /// Output path (use `-` for stdout; default stdout)
        #[arg(long, short)]
        output: Option<String>,
    },
    /// Import profile TOML from a local path
    Import {
        /// Source path (use `-` to read TOML from stdin)
        path: String,
        /// Override the profile's destination name
        #[arg(long)]
        name: Option<String>,
        /// Overwrite existing profile without prompting
        #[arg(long)]
        force: bool,
    },
}
