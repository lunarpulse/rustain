use std::path::PathBuf;

use crate::adapters::cli::peer::PeerAction;
use crate::adapters::cli::relay::RelayAction;
use crate::adapters::cli::session::SessionAction;
use crate::adapters::cli::team::TeamAction;
use clap::{Parser, Subcommand, ValueEnum};

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
    #[arg(long, allow_hyphen_values = true)]
    pub session: Option<String>,

    /// Override snapshot retention count (default: 100, from config)
    #[arg(long)]
    pub snapshot_retention: Option<usize>,

    /// Override the default model. This has the highest priority of all config layers.
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
    /// Choose how tools are exposed to the model each turn. Only `"static-full"` is
    /// accepted today (the default).
    #[arg(long, global = true, value_parser = ["static-full"])]
    pub tool_exposure: Option<String>,
    // Skill exposure defaults to "l1-metadata" (ADR-09-02) because skills, unlike tools,
    // gain from a metadata-first listing.
    /// Choose how skills are exposed to the model each turn: `"l1-metadata"` (the
    /// default) or `"static-full"` (lists every skill in full, like codex).
    #[arg(long, global = true, value_parser = ["l1-metadata", "static-full"])]
    pub skill_exposure: Option<String>,
    /// Choose the sandbox adapter: `"noop"` (the default on all platforms) or
    /// `"landlock"` (Linux builds with the `sandbox` cargo feature only).
    #[arg(long, global = true, value_parser = ["noop", "landlock"])]
    pub sandbox_adapter: Option<String>,
    /// Serve this instance as a loopback-only A2A endpoint. An explicit address
    /// uses `--serve-a2a=127.0.0.1:PORT`; the default is 127.0.0.1:8080.
    #[arg(
        long,
        global = true,
        value_name = "ADDR",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "127.0.0.1:8080"
    )]
    pub serve_a2a: Option<String>,
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
        /// Run adapter conformance smoke-checks
        #[arg(long)]
        adapters: bool,
        /// Machine-readable JSON output
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
    /// Fetch the latest models from providers and update models_variants.json
    UpdateCatalog {
        /// Output path for the updated catalog JSON (default: ./models_variants.json)
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// Only update specific provider IDs (e.g., "openai", "deepseek")
        #[arg(long)]
        provider: Vec<String>,
    },
    /// Configuration management commands
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Profile management commands (list, show, create, edit, switch, validate)
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Inspect the merged BM25 capability index (developer tool, not for everyday use)
    #[cfg(feature = "meta-search")]
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// Run rustain as a background daemon (start, stop, status).
    /// Unix only: Linux and macOS are supported; Windows daemon support (named
    /// pipes) is not available yet.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run rustain as an Agent Client Protocol server over stdio.
    Acp {
        /// Client compatibility profile. R1 profiles are behavior-identical; `auto` inspects initialize.clientInfo for future decoration hooks.
        #[arg(long, value_enum, default_value_t = AcpClientProfile::Auto)]
        client: AcpClientProfile,
    },

    /// Run a one-shot query and print the assistant's response to stdout.
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
        /// Output format for the rendered response
        #[arg(long, value_parser = ["text", "json", "stream-json"], default_value = "text")]
        output_format: String,
        /// Dry-run plan mode: generate a plan without executing state-mutating tools.
        /// No session state is written. Conflicts with --yolo (opposite permission posture).
        #[arg(long, conflicts_with = "yolo")]
        dry_run: bool,
    },
    /// Check for updates or update rustain to the latest version.
    /// Verifies cryptographic signatures and checksums before replacing the binary.
    Update {
        /// Check for updates without downloading or replacing (script-safe; exit 0 always).
        #[arg(long)]
        check: bool,
        /// Output format for --check results
        #[arg(long, value_parser = ["text", "json"], default_value = "text")]
        output_format: String,
    },
    /// Generate a shell completion script and print it to stdout.
    /// Pipe into your shell config, e.g. `rustain completions bash > ~/.local/share/bash-completion/completions/rustain`.
    /// Completions reflect the subcommands compiled into THIS binary — re-run after upgrading.
    Completions {
        /// Target shell. bash/zsh/fish/powershell are supported; others
        /// that clap_complete knows (e.g. elvish) also work but are unadvertised.
        #[arg(value_enum)]
        shell: clap_complete::aot::Shell,
        /// Program name to embed in the script (default: "rustain").
        /// Override for packaging where the installed binary is invoked under a different name.
        #[arg(long)]
        bin_name: Option<String>,
    },
    /// Manage provider authentication credentials.
    /// `auth login` validates and stores API keys; env vars remain highest-priority.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List and manage conversation sessions.
    /// `session list` shows persisted sessions; `session delete` removes them.
    /// The delete guard can detect a daemon-held session, but open TUIs cannot
    /// be detected — close any session windows you care about first.
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Inspect A2A team activity, manage consent, or send through the daemon.
    /// `team log`, `team status`, and `team untrust` operate offline on the
    /// durable room journal. `team send` requires a running daemon and its
    /// free write lock; the daemon owns A2A egress and the send journal.
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
    /// Control which peers may reach this host over the QUIC transport.
    ///
    /// `peer invite` mints a ticket, `peer add` imports one after a fingerprint
    /// confirm, `peer list` and `peer show` read the roster, and `peer revoke`
    /// removes an entry. All five read and write only `.rustain/p2p.json` —
    /// never the independent `.rustain/a2a.json` HTTP allowlist. Offline-safe
    /// and non-billable; `peer add` additionally needs an interactive terminal.
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },
    /// Run the relay this host offers to its peers.
    ///
    /// `relay serve` binds the relay `rustain relay serve` names and prints the
    /// URL peers add to their own `.rustain/relay.json`;
    /// `relay serve --print-service-unit` renders a systemd unit for those same
    /// flags and exits without binding anything. Needs a build with the
    /// `relay-server` feature to serve; the unit renders in any build.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AcpClientProfile {
    Auto,
    Standard,
    Zed,
}

/// Explicit login engine; omitted values use the provider's interactive picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuthLoginMethod {
    Oauth,
    ApiKey,
    Device,
}

/// Auth subcommand actions: login, status, list, logout, accounts, use.
#[derive(Subcommand, Debug, Clone)]
pub enum AuthAction {
    /// Configure API credentials for an AI provider via interactive masked entry
    /// with pre-storage validation.
    Login {
        /// Provider id (e.g. "anthropic", "openai", "ollama").
        provider: String,
        /// Select a login engine without an interactive picker.
        #[arg(long, value_enum)]
        method: Option<AuthLoginMethod>,
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Report configured provider credential status without network validation.
    Status {
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// List all supported providers with auth methods, configured status,
    /// and signup URLs.
    List {
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Remove persisted credentials for a provider, leaving environment intact:
    /// every row, or with `--id` exactly one account (see `auth accounts`).
    Logout {
        provider: String,
        /// The stored account to remove; other accounts stay.
        #[arg(long)]
        id: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// List the stored accounts of one provider (or of every provider with
    /// stored rows), the active account and the source a request would use.
    Accounts {
        provider: Option<String>,
        /// Machine-readable JSON output instead of human text.
        #[arg(long)]
        json: bool,
    },
    /// Make one stored account the provider's active credential.
    Use {
        provider: String,
        /// The stored account id from `auth accounts`.
        #[arg(long)]
        id: u64,
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
        /// `systemd`/`launchd` supervision and debugging.
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
    /// daemon is supervised and restarts automatically after a crash.
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
    /// file is not an error). Touches no daemon runtime state.
    Uninstall {
        /// Match the `--system` scope used at install time.
        #[arg(long)]
        system: bool,
    },
    /// Attach an interactive client to the running daemon over its Unix socket.
    /// The default is the rich multi-channel TUI: unified scrollback with dimmed
    /// channel prefixes, history, read-only multi-attach, and `Ctrl+X d` (or
    /// `/detach`) to detach (the daemon and any in-flight turn keep running).
    /// `--plain` selects the line-based client for scripting.
    Attach {
        /// Use the minimal line-based stdin/stdout client instead of the rich TUI
        /// (scripting / non-TTY contexts).
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
    /// Reload configuration in the running TUI
    Reload,
    /// Display the fully-resolved configuration
    Show {
        /// Machine-readable JSON output instead of TOML
        #[arg(long)]
        json: bool,
    },
    /// Open the active config file in $EDITOR
    Edit {
        /// Edit the user-global config (~/.config/rustain/config.toml) instead of workspace
        #[arg(long)]
        global: bool,
    },
    /// Show config file locations and their precedence order
    Path {
        /// Machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
    /// Validate configuration without launching the TUI
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
    /// Switch the active profile (the TUI must be running)
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
    /// Install a profile from a public git repository (gh:user/repo) or a local path
    ///
    /// A `gh:` spec is fetched over HTTPS into `profiles/community/`; anything else is
    /// treated as a local file and delegated to `import`, landing in `profiles/`.
    /// Unlike `import`, `install` REFUSES a name that shadows a built-in profile:
    /// `import` means "load my file", `install` means "take someone else's", and
    /// silently replacing the default profile with a stranger's TOML is the mistake
    /// the guard exists to stop.
    Install {
        /// Source spec: gh:user/profile-name (optionally with a /path/to/profile.toml suffix), or a local file path
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
