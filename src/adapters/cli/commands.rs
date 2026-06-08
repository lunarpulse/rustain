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
    /// Override per-turn tool exposure strategy (Story 9.4). Phase A: only
    /// `"static-full"` is accepted (the default). Reserved Phase B: `"meta-search"`
    /// (Story 9.7).
    #[arg(long, global = true, value_parser = ["static-full"])]
    pub tool_exposure: Option<String>,
    /// Override per-turn skill exposure strategy (Story 9.6). Phase A: `"l1-metadata"`
    /// (the DEFAULT per ADR-09-02 — INVERTED from Tools track default per evidence
    /// asymmetry) or `"static-full"` (codex-parity opt-in). Reserved Phase B:
    /// `"meta-search"` (Story 9.7).
    #[arg(long, global = true, value_parser = ["l1-metadata", "static-full"])]
    pub skill_exposure: Option<String>,
    /// Override sandbox adapter (Story 9.5). Phase A: `"noop"` (default on all
    /// platforms) or `"landlock"` (Linux + `sandbox` cargo feature only).
    #[arg(long, global = true, value_parser = ["noop", "landlock"])]
    pub sandbox_adapter: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
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
    /// Inspect the merged BM25 capability index (developer tool — not a user feature; per ADR-09-02 v2 §Audience Split)
    #[cfg(feature = "meta-search")]
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Run rustain as a background daemon (Story 12.1a — start/stop/status).
    /// Unix only (Linux P0, macOS P1); Windows daemon support (named pipes) is
    /// deferred to P2 (NFR33).
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum DaemonAction {
    /// Start a background (detached) daemon for this workspace.
    Start {
        /// Run the lifecycle loop in the foreground (do not detach). Useful for
        /// `systemd`/`launchd` supervision (Story 12.1b) and debugging.
        #[arg(long)]
        foreground: bool,
    },
    /// Gracefully stop the running daemon for this workspace.
    Stop,
    /// Print a status snapshot for this workspace's daemon.
    Status {
        /// Output as JSON for scripting.
        #[arg(long)]
        json: bool,
    },
    /// Install a service-manager unit (systemd on Linux, launchd on macOS) so the
    /// daemon is supervised + auto-restarts on crash (Story 12.1b, NFR50).
    Install {
        /// Render the unit/plist to stdout only — no filesystem write (for
        /// inspection / piping).
        #[arg(long)]
        print: bool,
        /// Install to the system location (`/etc/systemd/system`, writes `User=`,
        /// needs root) instead of the default per-user scope.
        #[arg(long)]
        system: bool,
    },
    /// Remove the service-manager unit for this workspace (idempotent — a missing
    /// file is a no-op success). Touches no daemon runtime state (Story 12.1b).
    Uninstall {
        /// Match the `--system` scope used at install time.
        #[arg(long)]
        system: bool,
    },
    /// Attach an interactive client to the running daemon over its Unix socket
    /// (Story 12.2b). Streams the daemon's conversation; type a line to submit a
    /// turn, Ctrl-D to detach (the daemon and any in-flight turn keep running).
    /// The rich multi-channel TUI client lands in Story 12.2c.
    Attach,
    /// INTERNAL — the detached child entrypoint (re-exec target of `start`).
    /// Hidden because it is not a user verb: `start` re-execs the current binary
    /// with this action after `setsid`-detaching. Running it by hand runs the
    /// daemon lifecycle loop in the foreground without writing the readiness
    /// handshake the parent `start` expects.
    #[command(name = "__run", hide = true)]
    Run,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Reload configuration in the running TUI (cross-process reload stub — see AC-9)
    Reload,
}

#[derive(Subcommand, Debug, Clone)]
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
    /// Install a profile from a public git repository (gh:user/repo) (Story 8.6b)
    Install {
        /// Source spec (e.g., gh:user/profile-name; optionally with /path/to/profile.toml suffix)
        spec: String,
        /// Override the installed profile's name (rewrites name = field)
        #[arg(long)]
        name: Option<String>,
        /// Overwrite existing community profile without prompting
        #[arg(long)]
        force: bool,
        /// Fail on AdapterFeatureGated instead of auto-flipping to preview = true
        #[arg(long)]
        strict_features: bool,
    },
}

#[cfg(feature = "meta-search")]
#[derive(Subcommand, Debug, Clone)]
pub enum CatalogAction {
    /// List all indexed capabilities (developer/CI tool)
    List {
        #[arg(long, value_parser = ["tool", "skill", "any"], default_value = "any")]
        kind: String,
        #[arg(long)]
        json: bool,
        /// Connect to MCP servers and include their tools (slower; default off)
        #[arg(long)]
        with_mcp: bool,
    },
    /// Print full profile of a single capability (developer/CI tool)
    Explain {
        /// DocKey in display form (tool::<name> or skill::<name>); optional provider prefix: tool::<provider>:<name>
        doc_key: String,
    },
    /// Print index health metrics for CI consumption (developer/CI tool)
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Dry-run a query against the index — exact SearchHit payload the LLM would receive (developer/CI tool)
    Search {
        /// Query string (must be non-empty)
        query: String,
        #[arg(long, value_parser = ["tool", "skill", "any"], default_value = "any")]
        kind: String,
        /// Top-K result count (1..=20)
        #[arg(long, default_value = "5")]
        top_k: usize,
        #[arg(long)]
        json: bool,
        /// Disable matched_terms (LLM-prod-parity mode)
        #[arg(long)]
        no_matched_terms: bool,
    },
}
