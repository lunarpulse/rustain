use std::path::PathBuf;

use crate::adapters::cli::session::SessionAction;
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

    /// Override a config value for this invocation only. Dot-paths for nested keys: -c router.threshold_tokens=100000. See available keys: rustain config show --json
    #[arg(short = 'c', long = "set", global = true, value_name = "KEY=VALUE")]
    pub config_override: Vec<String>,
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
        /// Machine-readable JSON output (Story 13.2 AC9)
        #[arg(long)]
        json: bool,
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
    /// Run a one-shot query and print the assistant's response to stdout (Story 13.1a).
    /// Non-interactive: no TUI launched. Composable with pipes and scripts.
    Ask {
        /// The query to send to the LLM
        query: String,
        /// Attach file(s) as context (repeatable; read OS-permission-bound, no workspace check)
        #[arg(long)]
        file: Vec<PathBuf>,
        /// Auto-approve all tool calls (blocklist still enforced)
        #[arg(long)]
        yolo: bool,
        /// Print only the final assistant text block; quiet stderr
        #[arg(long)]
        final_message_only: bool,
        /// Output format for the rendered response (Story 13.1b)
        #[arg(long, value_parser = ["text", "json", "stream-json"], default_value = "text")]
        output_format: String,
        /// Dry-run plan mode: generate a plan without executing state-mutating tools (Story 13.1c, FR101).
        /// No session state is written. Conflicts with --yolo (opposite permission posture).
        #[arg(long, conflicts_with = "yolo")]
        dry_run: bool,
    },
    /// Check for updates or update rustain to the latest version (Story 13.3a, FR103).
    /// Verifies cryptographic signatures and checksums before replacing the binary.
    Update {
        /// Check for updates without downloading or replacing (script-safe; exit 0 always).
        #[arg(long)]
        check: bool,
        /// Output format for --check results
        #[arg(long, value_parser = ["text", "json"], default_value = "text")]
        output_format: String,
    },
    /// Generate a shell completion script and print it to stdout (Story 13.3b, FR104).
    /// Pipe into your shell config, e.g. `rustain completions bash > ~/.local/share/bash-completion/completions/rustain`.
    /// Completions reflect the subcommands compiled into THIS binary — re-run after upgrading.
    Completions {
        /// Target shell. bash/zsh/fish/powershell are supported (FR104); others
        /// that clap_complete knows (e.g. elvish) also work but are unadvertised.
        #[arg(value_enum)]
        shell: clap_complete::aot::Shell,
        /// Program name to embed in the script (default: "rustain").
        /// Override for packaging where the installed binary is invoked under a different name.
        #[arg(long)]
        bin_name: Option<String>,
    },
    /// Manage provider authentication credentials (Story 13.4a, FR123).
    /// `auth login` validates and stores API keys; env vars remain highest-priority.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List and manage conversation sessions (Stories 13.5a / 13.5a-1 list,
    /// 13.5b delete, FR125).
    /// `session list` shows persisted sessions; `session delete` removes them.
    /// The delete guard can detect a daemon-held session, but open TUIs cannot
    /// be detected — close any session windows you care about first.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

/// Auth subcommand actions (Story 13.4a login; 13.4b status; 13.4c adds list).
#[derive(Subcommand, Debug, Clone)]
pub enum AuthAction {
    /// Configure API credentials for an AI provider via interactive masked entry
    /// with pre-storage validation (Story 13.4a, FR123).
    Login {
        /// Provider id (e.g. "anthropic", "openai", "ollama").
        provider: String,
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Report configured provider credential status without network validation
    /// (Story 13.4b, FR123).
    Status {
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// List all supported providers with auth methods, configured status,
    /// and signup URLs (Story 13.4c, FR123).
    List {
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
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
    /// (Story 12.2b/12.2c). The default is the rich multi-channel TUI
    /// (`run_attached`): unified scrollback with dimmed channel prefixes, history,
    /// read-only multi-attach, and `Ctrl+D` to detach (the daemon and any in-flight
    /// turn keep running). `--plain` selects the line-based client for scripting.
    Attach {
        /// Use the minimal line-based stdin/stdout client instead of the rich TUI
        /// (scripting / non-TTY contexts; Story 12.2b `run_attach`).
        #[arg(long)]
        plain: bool,
    },
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
    /// Display the fully-resolved configuration (Story 13.2a AC2, FR124)
    Show {
        /// Machine-readable JSON output instead of TOML
        #[arg(long)]
        json: bool,
    },
    /// Open the active config file in $EDITOR (Story 13.2a AC3, FR124)
    Edit {
        /// Edit the user-global config (~/.config/rustain/config.toml) instead of workspace
        #[arg(long)]
        global: bool,
    },
    /// Show config file locations and their precedence order (Story 13.2a AC4, FR124)
    Path {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Validate configuration without launching the TUI (Story 13.2a AC5, FR124)
    Validate {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
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
