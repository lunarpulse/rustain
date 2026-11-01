//! Custom slash commands: discovered from `.claude/commands/`, with `{{args}}` and `@{path}` interpolation.
//! See Story 5.3.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::domain::services::command_normalize;
use crate::domain::services::frontmatter;

const MAX_COMMAND_FILE_SIZE: u64 = 1_048_576;
const MAX_COMMAND_DESCRIPTION_BYTES: usize = 200;
const MAX_COMMAND_SCAN_FILES: usize = 200;
const COMMAND_SCAN_BUDGET_MS: u64 = 50;
const MAX_FILE_REF_CONTENT_BYTES: usize = 102_400;
const MAX_FILE_REF_DIR_ENTRIES: usize = 100;

#[derive(Debug, Clone)]
pub enum CommandSource {
    BuiltIn,
    UserDefined {
        #[allow(dead_code)]
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct SlashCommandDef {
    pub name: String,
    pub description: String,
    pub source: CommandSource,
    pub content: Option<String>,
    /// Help-overlay usage rows of a built-in (Story 21.3 A11); empty for custom commands.
    pub usage: &'static [UsageRow],
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FileRefError {
    pub raw_path: String,
    pub reason: String,
}

pub struct CommandRegistry {
    commands: Vec<SlashCommandDef>,
    discovered: bool,
    /// Wall-clock budget for one refresh scan; always `COMMAND_SCAN_BUDGET_MS` outside this module's unit tests.
    scan_budget: Duration,
    /// Custom command names a built-in hides (Story 21.3 A12).
    shadowed: Vec<String>,
    shadow_generation: u64,
}

/// Post one Warning per (shadowed name, discovery generation) through the
/// notification store (Story 21.3 A12, DF-21-2-CUSTOM-COMMAND-SHADOWED-BY-BUILTIN).
/// Built-ins keep precedence; the user learns why their file never runs.
pub fn announce_shadows(
    state: &mut crate::adapters::tui::state::TuiState,
    registry: &CommandRegistry,
) {
    let generation = registry.shadow_generation();
    for name in registry.shadowed() {
        if state.shadows_announced.insert((name.clone(), generation)) {
            crate::adapters::tui::notifications::warning(
                state,
                crate::adapters::tui::notifications::NoticeSource::System,
                format!(
                    "Custom command /{name} is hidden by the built-in /{name}; rename the file in .claude/commands to use it"
                ),
            );
        }
    }
}

/// One help row of a built-in's usage: (category title, key text, description, available).
pub type UsageRow = (&'static str, &'static str, &'static str, bool);

const A2A: bool = cfg!(feature = "a2a");
const COMMANDS: &str = "COMMANDS";

#[rustfmt::skip]
const EXPORT_USAGE: &[UsageRow] = &[("Search, Bookmarks & Export", "/export", "Export conversation to markdown", true)];
#[rustfmt::skip]
const MODE_USAGE: &[UsageRow] = &[("PERMISSIONS", "/mode", "Switch permission mode (plan, normal, autoedit, yolo)", true)];
#[rustfmt::skip]
const PLAN_USAGE: &[UsageRow] = &[(COMMANDS, "/plan on|off|toggle", "Toggle Plan mode", true)];
#[rustfmt::skip]
const ML_USAGE: &[UsageRow] = &[("INPUT", "/ml", "Toggle multi-line mode via slash command", true)];
#[rustfmt::skip]
const NOTIFICATIONS_USAGE: &[UsageRow] = &[("INPUT", "/notifications", "Open the Notification Log (also Ctrl+X, N)", true)];
#[rustfmt::skip]
const TEAM_USAGE: &[UsageRow] = &[
    (COMMANDS, "/team log", "A2A transparency log (also Ctrl+X, L); an unfiltered, displayed visit clears the `log: N` reminder — not an acknowledgement", true),
    (COMMANDS, "/team board [--last | --actions | --action <action-id> | <peer-id>]", "Daemon-served board/inventory over the composed egress; read-only attachment is allowed, Peer tier is refused; explicit reads only (a2a feature)", A2A),
    (COMMANDS, "/team send [--type <type>] [--subject <label>] [--stories <key>[=<alias>],…] <peer-id>[,<peer-id>…] <text…>", "Daemon-served typed send; trusted-local writer; --subject is local, --stories states story holders (a2a feature)", A2A),
    (COMMANDS, "/team send [--type <type>] [--subject <label>] --holders-of <key>[,<key>…] <text…>", "Send to holders named by this host's received and own story lists; refuse disagreement; selection grants nothing (a2a feature)", A2A),
    (COMMANDS, "/team view <item-id>", "Read this host's bounded received thread without a model turn; offline/read-only is supported", true),
    (COMMANDS, "/team context [<item-id>] | /team context off <item-id>", "Inspect cached daemon peer context; select/stop an item's body as writer (clears on /clear or restart)", true),
    (COMMANDS, "/team ack <item-id>", "Record receipt only, not an answer (daemon-attached session)", true),
    (COMMANDS, "/team reply <item-id> <text…>", "Answer an item that arrived on this host (daemon-attached session)", A2A),
    (COMMANDS, "/team reply --accept <item-id> [<text…>] | --discuss <item-id> <text…>", "Verdict on an item carrying a story list — accept when it assigns a story to you; discuss concerns on any readable story list (daemon-attached session)", A2A),
    (COMMANDS, "/team defer <item-id>", "Record Busy — defer 30m without resolving the answer (daemon-attached)", A2A),
    (COMMANDS, "/team thread <peer-id> <item-id>", "Read one thread on a peer's host; trusted-local read-only attachments are allowed", A2A),
    (COMMANDS, "/team followup <peer-id> <turn-id> <text…>", "Answer a turn in a peer thread; trusted-local writer attachment only", A2A),
    (COMMANDS, "/team status", "Daemon-authoritative effective sender-consent status over its composed roster; read-only attachment is allowed, Peer tier is refused", true),
    (COMMANDS, "/team untrust <peer>", "Durably revoke standing consent; trusted-local writer attachment only, Peer/read-only refused", true),
];
#[rustfmt::skip]
const ROOM_USAGE: &[UsageRow] = &[
    (COMMANDS, "/room", "Durable room replay, read-only (also Ctrl+X, R)", true),
    (COMMANDS, "/room role list", "Show journaled room roles (owner/editor/viewer)", true),
    (COMMANDS, "/room role grant <peer> <role>", "Record a room role for a configured peer (room edits only)", true),
    (COMMANDS, "/room role revoke <peer>", "Record withdrawal of a peer's room role", true),
];
#[rustfmt::skip]
const ARTIFACTS_USAGE: &[UsageRow] = &[(COMMANDS, "/artifacts", "Durable artifact list with lineage, read-only (also Ctrl+X, E)", true)];
#[rustfmt::skip]
const ARTIFACT_USAGE: &[UsageRow] = &[
    (COMMANDS, "/artifact show <id>", "Drill into one artifact: kind, lineage, apply decision, policy", true),
    (COMMANDS, "/artifact review <id> <verdict>", "Record approve|request-changes|reject; approval is not apply success", true),
    (COMMANDS, "/artifact apply <id>", "Preview impact, confirm, then apply an eligible patch", true),
    (COMMANDS, "/artifact resolve <id> present|absent", "After an interrupted apply: record whether you found its changes in the tree", true),
];
// Story 18.4b. ONE section: the COMMANDS category. No CHORDS entry: this story mints no chord.
#[rustfmt::skip]
const PEER_USAGE: &[UsageRow] = &[
    (COMMANDS, "/peer list", "Which peers may reach this host (configuration, not connection status)", true),
    (COMMANDS, "/peer show <alias>", "Print one peer's pinned key and id in full, for comparing", true),
    (COMMANDS, "/peer invite", "Mint an expiring ticket for one person; grants reach, not authority", true),
    (COMMANDS, "/peer add <alias> <ticket>", "Import a ticket: confirm the fingerprint, then pin it. No bypass", true),
    (COMMANDS, "/peer revoke <peer>", "Remove a peer; their next frame is refused, without restart", true),
];

impl CommandRegistry {
    pub fn new() -> Self {
        let commands = vec![
            SlashCommandDef {
                name: "new".to_string(),
                description: "Start a new conversation".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "clear".to_string(),
                description: "Start a clean conversation and clear provenance taint".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "export".to_string(),
                description: "Export conversation to markdown (optional filename arg)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: EXPORT_USAGE,
            },
            SlashCommandDef {
                name: "deactivate".to_string(),
                description: "Deactivate all active skills in this conversation".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "mode".to_string(),
                description: "Set permission mode (plan, normal, autoedit, yolo)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: MODE_USAGE,
            },
            SlashCommandDef {
                name: "plan".to_string(),
                description: "Toggle Plan mode (read-only exploration with reminder injection)"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: PLAN_USAGE,
            },
            SlashCommandDef {
                name: "compact".to_string(),
                description: "Summarize and compact the conversation to free context space"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "config".to_string(),
                description: "Manage configuration (reload)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "persona".to_string(),
                description: "Override persona adapter for this session (e.g. /persona coding). No-arg shows current."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "memory".to_string(),
                description: "Override memory adapter for this session (e.g. /memory daily-log). No-arg shows current."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "session".to_string(),
                description: "Override session adapter for this session."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "tools".to_string(),
                description: "Override tools adapter for this session."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "channels".to_string(),
                description: "Override channels adapter for this session."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "scheduler".to_string(),
                description: "Override scheduler adapter for this session."
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "context".to_string(),
                description: "Inspect/toggle injected context: show | off | on"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "fanout".to_string(),
                description: "Fan out N identical spokes: /fanout <N> <prompt>".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "team".to_string(),
                description: "A2A team interactions: /team log (transparency log) [--filter=…] [--json] [--export] | /team board [--last | --actions | --action <action-id> | <peer-id>] (daemon-served, read-only attachment allowed) | /team send [--type <type>] [--subject <label>] [--stories <key>[=<alias>],…] <peer-id>[,<peer-id>…] <text…> (trusted-local writer attachment only) | /team view <item-id> | /team context [<item-id>] | /team context off <item-id> | /team ack <item-id> | /team remove <item-id> | /team retract <peer-id> <item-id> (trusted-local writer attachment, confirmation required) | /team reply <item-id> <text…> | /team reply --accept <item-id> [<text…>] | /team reply --discuss <item-id> <text…> | /team defer <item-id> | /team thread <peer-id> <item-id> (trusted-local read-only attachment allowed) | /team followup <peer-id> <turn-id> <text…> (trusted-local writer attachment only) | /team status (daemon-authoritative read, read-only attachment allowed) | /team untrust <alias-or-peer-id> (trusted-local writer attachment only)"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: TEAM_USAGE,
            },
            SlashCommandDef {
                name: "room".to_string(),
                description: "Durable orchestration room (read-only replay): /room | /room role list | /room role grant <alias-or-peer-id> <owner|editor|viewer> | /room role revoke <alias-or-peer-id>"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: ROOM_USAGE,
            },
            // Story 18.4b. The description names EVERY sub-verb: that is a
            // tested convention (`conformance_18_3a_room.rs` asserts the /room
            // entry's description names each of its own), because the palette
            // description is the only place an operator discovers them.
            SlashCommandDef {
                name: "peer".to_string(),
                description: "Who may reach this host over QUIC, from .rustain/p2p.json: /peer | /peer list [--json] | /peer show <alias> | /peer invite [--ttl=<dur>] [--name=<name>] | /peer add <alias> <ticket> | /peer revoke <alias-or-peer-id>"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: PEER_USAGE,
            },
            SlashCommandDef {
                name: "artifacts".to_string(),
                description: "Durable artifact list with depends-on lineage (read-only replay): /artifacts"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: ARTIFACTS_USAGE,
            },
            SlashCommandDef {
                name: "artifact".to_string(),
                description: "Inspect, review, apply or resolve one durable patch: /artifact show <id> | /artifact review <id> approve|request-changes|reject | /artifact apply <id> (confirmed workspace write) | /artifact resolve <id> present|absent (record what you found after an interrupted apply)"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: ARTIFACT_USAGE,
            },
            SlashCommandDef {
                name: "login".to_string(),
                description: "Sign in to a model provider (optional provider id)".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "models".to_string(),
                description: "Open the model picker".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "logout".to_string(),
                description: "Log out of one provider account".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "accounts".to_string(),
                description: "Choose the active account for a provider".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "ml".to_string(),
                description: "Toggle multi-line input mode".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: ML_USAGE,
            },
            SlashCommandDef {
                name: "quit".to_string(),
                description: "Quit rustain (asks first when work or a draft exists)"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
            SlashCommandDef {
                name: "notifications".to_string(),
                description: "Open the Notification Log".to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: NOTIFICATIONS_USAGE,
            },
            SlashCommandDef {
                name: "detach".to_string(),
                description: "Detach this attached client (daemon work keeps running)"
                    .to_string(),
                source: CommandSource::BuiltIn,
                content: None,
                usage: &[],
            },
        ];
        Self {
            commands,
            discovered: false,
            scan_budget: Duration::from_millis(COMMAND_SCAN_BUDGET_MS),
            shadowed: Vec::new(),
            shadow_generation: 0,
        }
    }

    pub fn is_discovered(&self) -> bool {
        self.discovered
    }

    #[allow(dead_code)]
    pub fn register_user_command(
        &mut self,
        name: String,
        description: String,
        path: PathBuf,
        content: Option<String>,
    ) {
        self.commands.push(SlashCommandDef {
            name,
            description,
            source: CommandSource::UserDefined { path },
            content,
            usage: &[],
        });
    }

    pub fn refresh(&mut self, workspace: &Path) {
        self.commands
            .retain(|c| matches!(c.source, CommandSource::BuiltIn));

        let cmd_dir = workspace.join(".claude").join("commands");
        let user_commands = scan_commands_dir(&cmd_dir, self.scan_budget);

        let builtin_names: HashSet<&str> = self.commands.iter().map(|c| c.name.as_str()).collect();
        let mut shadowed: Vec<String> = Vec::new();
        for cmd in &user_commands {
            if builtin_names.contains(cmd.name.as_str()) {
                tracing::warn!(
                    "User command '{}' shadowed by built-in — rename the custom file",
                    cmd.name
                );
                shadowed.push(cmd.name.clone());
            }
        }
        shadowed.sort();
        shadowed.dedup();
        if shadowed != self.shadowed {
            self.shadowed = shadowed;
            self.shadow_generation += 1;
        }

        self.commands.extend(user_commands);
        self.discovered = true;
    }

    /// Custom commands hidden behind a built-in of the same name (A12), sorted.
    pub fn shadowed(&self) -> &[String] {
        &self.shadowed
    }

    /// Changes whenever the shadowed set changes; one notice per (name, generation).
    pub fn shadow_generation(&self) -> u64 {
        self.shadow_generation
    }

    pub fn warn_skill_shadows(
        &self,
        skill_registry: &crate::adapters::skill_registry::SkillRegistry,
    ) {
        for cmd in &self.commands {
            if matches!(cmd.source, CommandSource::UserDefined { .. }) {
                if skill_registry.find(&cmd.name).is_some() {
                    tracing::warn!(
                        "User command '{}' shadowed by skill '{}' — rename one to disambiguate",
                        cmd.name,
                        cmd.name
                    );
                }
            }
        }
    }

    #[deprecated(note = "Use refresh() instead; discover_into is kept for call-site compatibility")]
    #[allow(dead_code)]
    pub fn discover_into(&mut self, workspace: &Path) {
        self.refresh(workspace);
    }

    /// Local-host discovery (the TUI's autocomplete / palette).
    pub fn filter(&self, query: &str) -> Vec<&SlashCommandDef> {
        self.filter_for_host(query, crate::adapters::builtin_commands::CommandHost::Local)
    }

    /// Discovery for one host (Story 21.2 AC1, RU-34): a built-in is offered only
    /// where `builtin_commands` says its dispatcher exists; custom files and
    /// discovered skills keep their existing discovery in every host.
    pub fn filter_for_host(
        &self,
        query: &str,
        host: crate::adapters::builtin_commands::CommandHost,
    ) -> Vec<&SlashCommandDef> {
        let lower_query = query.to_lowercase();
        let mut builtins: Vec<&SlashCommandDef> = Vec::new();
        let mut user_defined: Vec<&SlashCommandDef> = Vec::new();

        for cmd in &self.commands {
            if lower_query.is_empty() || cmd.name.to_lowercase().contains(&lower_query) {
                match cmd.source {
                    CommandSource::BuiltIn => {
                        if crate::adapters::builtin_commands::offered_in(host, &cmd.name) {
                            builtins.push(cmd)
                        }
                    }
                    CommandSource::UserDefined { .. } => user_defined.push(cmd),
                }
            }
        }

        user_defined.sort_by(|a, b| a.name.cmp(&b.name));
        builtins.extend(user_defined);
        builtins
    }

    pub fn find(&self, name: &str) -> Option<&SlashCommandDef> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// Help-overlay usage rows of the built-ins, catalogue order (Story 21.3 A11).
    pub fn builtin_usage(&self) -> impl Iterator<Item = &'static UsageRow> + '_ {
        self.commands
            .iter()
            .filter(|c| matches!(c.source, CommandSource::BuiltIn))
            .flat_map(|c| c.usage.iter())
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Scans `.claude/commands/` for user-defined commands.
///
/// The `budget` is measured from before directory collection, but it can stop the scan only
/// after the first sorted entry has been examined, so a loaded machine can never drop entry 0.
/// The overrun past the budget is bounded by one file (at most `MAX_COMMAND_FILE_SIZE`).
fn scan_commands_dir(cmd_dir: &Path, budget: Duration) -> Vec<SlashCommandDef> {
    if !cmd_dir.is_dir() {
        return Vec::new();
    }

    let mut results: Vec<SlashCommandDef> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    let start = Instant::now();

    let mut entries: Vec<PathBuf> = Vec::new();
    collect_md_files(cmd_dir, &mut entries, 0);

    entries.sort_by(|a, b| {
        a.strip_prefix(cmd_dir)
            .unwrap_or(a)
            .cmp(b.strip_prefix(cmd_dir).unwrap_or(b))
    });

    for (file_count, path) in entries.into_iter().enumerate() {
        if file_count >= MAX_COMMAND_SCAN_FILES {
            tracing::warn!("Command scan truncated at {} files", MAX_COMMAND_SCAN_FILES);
            break;
        }
        if file_count > 0 && start.elapsed() >= budget {
            tracing::warn!(
                "Command scan truncated after {}ms ({} files)",
                start.elapsed().as_millis(),
                file_count
            );
            break;
        }

        let key = match command_normalize::normalize_command_path(cmd_dir, &path) {
            Some(k) => k,
            None => {
                tracing::warn!(
                    "Skipping command file {:?}: empty name after normalization",
                    path
                );
                continue;
            }
        };

        if seen_keys.contains(&key) {
            tracing::warn!("Duplicate command key '{}' — skipping {:?}", key, path);
            continue;
        }

        if let Some(cmd) = parse_command_file(&path, &key) {
            seen_keys.insert(key);
            results.push(cmd);
        }
    }

    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

fn collect_md_files(current: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > command_normalize::MAX_COMMAND_NAMESPACE_DEPTH as usize {
        return;
    }
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        if out.len() >= MAX_COMMAND_SCAN_FILES {
            break;
        }
        if entry.file_type().is_ok_and(|ft| ft.is_symlink()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, out, depth + 1);
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
}

fn parse_command_file(path: &Path, name: &str) -> Option<SlashCommandDef> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_COMMAND_FILE_SIZE {
        tracing::warn!(
            "Skipping command file {} ({} bytes exceeds {} limit)",
            path.display(),
            metadata.len(),
            MAX_COMMAND_FILE_SIZE
        );
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let (description, body) = parse_command_description_and_body(&content);

    Some(SlashCommandDef {
        name: name.to_string(),
        description,
        source: CommandSource::UserDefined {
            path: path.to_path_buf(),
        },
        content: Some(body),
        usage: &[],
    })
}

fn parse_command_description_and_body(content: &str) -> (String, String) {
    if let Some((frontmatter_text, body)) = frontmatter::parse_frontmatter(content) {
        let description =
            frontmatter::extract_field(frontmatter_text, "description").unwrap_or_default();
        if description.is_empty() {
            return (first_line_description(body), body.to_string());
        }
        return (description, body.to_string());
    }

    let description = first_line_description(content);
    (description, content.to_string())
}

fn strip_heading_prefix(line: &str) -> &str {
    let mut hash_count = 0;
    for ch in line.chars() {
        if ch == '#' && hash_count < 3 {
            hash_count += 1;
        } else {
            break;
        }
    }
    let after_hashes = &line[hash_count..];
    if hash_count > 0
        && after_hashes
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace())
    {
        after_hashes.trim_start()
    } else {
        line.trim_start()
    }
}

fn first_line_description(text: &str) -> String {
    let first_line = match text.lines().find(|line| !line.trim().is_empty()) {
        Some(line) => line.trim(),
        None => return "(no description)".to_string(),
    };

    let stripped = strip_heading_prefix(first_line);

    if stripped.is_empty() {
        return "(no description)".to_string();
    }

    if stripped.len() > MAX_COMMAND_DESCRIPTION_BYTES {
        #[allow(clippy::incompatible_msrv)]
        let boundary = stripped.floor_char_boundary(MAX_COMMAND_DESCRIPTION_BYTES);
        format!("{}…", &stripped[..boundary])
    } else {
        stripped.to_string()
    }
}

pub fn resolve_file_refs(
    body: &str,
    workspace: &Path,
    security: &dyn crate::domain::ports::SecurityPort,
) -> (String, Vec<FileRefError>) {
    let mut result = String::with_capacity(body.len());
    let mut errors = Vec::new();
    let mut last_end = 0;

    let mut i = 0;
    let bytes = body.as_bytes();
    while i < body.len() {
        if bytes[i] == b'@' && i + 1 < body.len() && bytes[i + 1] == b'{' {
            let brace_start = i + 2;
            let mut j = brace_start;
            let mut path_chars = 0u32;
            let mut found_end = false;
            while j < body.len() {
                let c = body.as_bytes()[j];
                if c == b'}' {
                    found_end = true;
                    break;
                }
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' || c == b'/' {
                    path_chars += 1;
                    j += 1;
                } else {
                    break;
                }
            }

            if found_end && path_chars > 0 {
                let raw_path = &body[brace_start..j];
                result.push_str(&body[last_end..i]);

                let full_path = workspace.join(raw_path);
                let canonical = match full_path.canonicalize() {
                    Ok(p) => p,
                    Err(_) => {
                        result.push_str(&format!("[File not found: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("File not found: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }
                };

                use crate::domain::models::FileOperation;
                if security
                    .check_workspace_access(&canonical, FileOperation::Read)
                    .is_err()
                {
                    result.push_str(&format!("[File not found: {raw_path}]"));
                    errors.push(FileRefError {
                        raw_path: raw_path.to_string(),
                        reason: format!("File not found: {raw_path}"),
                    });
                    last_end = j + 1;
                    i = j + 1;
                    continue;
                }

                if canonical.is_dir() {
                    let mut entries: Vec<String> = Vec::new();
                    if let Ok(dir_entries) = std::fs::read_dir(&canonical) {
                        for entry in dir_entries.flatten() {
                            if entries.len() >= MAX_FILE_REF_DIR_ENTRIES {
                                entries.push("...".to_string());
                                break;
                            }
                            if let Some(name) = entry.file_name().to_str() {
                                entries.push(name.to_string());
                            }
                        }
                    }
                    result.push_str(&entries.join("\n"));
                } else {
                    // Bounded read to avoid OOM on multi-gigabyte files
                    let file = match std::fs::File::open(&canonical) {
                        Ok(f) => f,
                        Err(_) => {
                            result.push_str(&format!("[File not found: {raw_path}]"));
                            errors.push(FileRefError {
                                raw_path: raw_path.to_string(),
                                reason: format!("File not found: {raw_path}"),
                            });
                            last_end = j + 1;
                            i = j + 1;
                            continue;
                        }
                    };

                    const READ_SLACK: usize = 16;
                    let read_limit = MAX_FILE_REF_CONTENT_BYTES + READ_SLACK;
                    let mut buf = Vec::with_capacity(read_limit);
                    let mut reader = file.take(read_limit as u64);
                    if std::io::Read::read_to_end(&mut reader, &mut buf).is_err() {
                        result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("Binary file skipped: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }

                    // Check for null bytes in first 8 KiB (binary detection)
                    if buf.iter().take(8192).any(|&b| b == 0) {
                        result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                        errors.push(FileRefError {
                            raw_path: raw_path.to_string(),
                            reason: format!("Binary file skipped: {raw_path}"),
                        });
                        last_end = j + 1;
                        i = j + 1;
                        continue;
                    }

                    // Decode as UTF-8
                    let content = match String::from_utf8(buf) {
                        Ok(s) => s,
                        Err(e) => {
                            let valid_up_to = e.utf8_error().valid_up_to();
                            if valid_up_to > MAX_FILE_REF_CONTENT_BYTES {
                                let mut buf = e.into_bytes();
                                buf.truncate(valid_up_to);
                                match String::from_utf8(buf) {
                                    Ok(s) => s,
                                    Err(_) => {
                                        result.push_str(&format!(
                                            "[Binary file skipped: {raw_path}]"
                                        ));
                                        errors.push(FileRefError {
                                            raw_path: raw_path.to_string(),
                                            reason: format!("Binary file skipped: {raw_path}"),
                                        });
                                        last_end = j + 1;
                                        i = j + 1;
                                        continue;
                                    }
                                }
                            } else {
                                result.push_str(&format!("[Binary file skipped: {raw_path}]"));
                                errors.push(FileRefError {
                                    raw_path: raw_path.to_string(),
                                    reason: format!("Binary file skipped: {raw_path}"),
                                });
                                last_end = j + 1;
                                i = j + 1;
                                continue;
                            }
                        }
                    };

                    if content.len() > MAX_FILE_REF_CONTENT_BYTES {
                        #[allow(clippy::incompatible_msrv)]
                        let boundary = content.floor_char_boundary(MAX_FILE_REF_CONTENT_BYTES);
                        result.push_str(&content[..boundary]);
                        result.push_str("\n...[truncated]");
                    } else {
                        result.push_str(&content);
                    }
                }

                last_end = j + 1;
                i = j + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    result.push_str(&body[last_end..]);
    (result, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(budget: Duration) -> CommandRegistry {
        CommandRegistry {
            scan_budget: budget,
            ..CommandRegistry::new()
        }
    }

    fn write_command(root: &Path, rel: &str, body: &str) {
        let path = root.join(".claude").join("commands").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Three files written out of sorted order: `b.md`, `c.md`, then `a/z.md`.
    /// Sorted relative-path order is `a/z.md`, `b.md`, `c.md`.
    fn three_file_workspace() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write_command(tmp.path(), "b.md", "B");
        write_command(tmp.path(), "c.md", "C");
        write_command(tmp.path(), "a/z.md", "Z");
        tmp
    }

    fn user_names(registry: &CommandRegistry) -> Vec<String> {
        registry
            .filter("")
            .into_iter()
            .filter(|c| matches!(c.source, CommandSource::UserDefined { .. }))
            .map(|c| c.name.clone())
            .collect()
    }

    #[test]
    fn story21_3_shadowed_custom_command_is_announced_once_per_generation() {
        use crate::adapters::tui::notifications::NoticeSeverity;
        use crate::adapters::tui::state::TuiState;
        let tmp = tempfile::tempdir().unwrap();
        write_command(tmp.path(), "quit.md", "shadowed");
        write_command(tmp.path(), "deploy.md", "not shadowed");
        let mut registry = CommandRegistry::new();
        registry.refresh(tmp.path());
        assert_eq!(registry.shadowed(), ["quit".to_string()]);
        let mut state = TuiState::new(80, 24);
        announce_shadows(&mut state, &registry);
        let warnings = |state: &TuiState| {
            state
                .notifications
                .records()
                .filter(|r| r.severity == NoticeSeverity::Warning)
                .map(|r| r.message.clone())
                .collect::<Vec<_>>()
        };
        let first = warnings(&state);
        assert_eq!(
            first.len(),
            1,
            "one Warning for the shadowed name: {first:?}"
        );
        assert!(
            first[0].contains("/quit") && first[0].contains("built-in"),
            "{first:?}"
        );
        assert!(
            !first[0].contains("deploy"),
            "a non-shadowed command is silent"
        );
        // The same discovery generation never repeats the warning.
        announce_shadows(&mut state, &registry);
        registry.refresh(tmp.path());
        announce_shadows(&mut state, &registry);
        assert_eq!(warnings(&state).len(), 1, "same generation posts nothing");
        // A changed shadow set is a new generation and announces again.
        write_command(tmp.path(), "new.md", "also shadowed");
        registry.refresh(tmp.path());
        announce_shadows(&mut state, &registry);
        // The repeated `/quit` warning coalesces into its existing record (x2); `/new` is new.
        let after = warnings(&state);
        assert_eq!(after.len(), 2, "{after:?}");
        assert!(after.iter().any(|m| m.contains("/new")), "{after:?}");
    }

    #[test]
    fn k0_production_budget_is_fifty_milliseconds() {
        assert_eq!(
            CommandRegistry::new().scan_budget,
            Duration::from_millis(50)
        );
        assert_eq!(
            CommandRegistry::default().scan_budget,
            Duration::from_millis(50)
        );
    }

    #[test]
    fn k1_expired_budget_keeps_only_the_first_sorted_entry() {
        let tmp = three_file_workspace();
        let mut registry = registry_with(Duration::ZERO);
        registry.refresh(tmp.path());

        let names = user_names(&registry);
        assert_eq!(names, vec!["a:z".to_string()], "user names: {names:?}");
        assert_eq!(
            registry.find("a:z").and_then(|c| c.content.as_deref()),
            Some("Z"),
            "user names: {names:?}"
        );
        assert!(registry.find("b").is_none(), "user names: {names:?}");
        assert!(registry.find("c").is_none(), "user names: {names:?}");
        assert!(registry.is_discovered());
        assert!(registry.find("new").is_some(), "built-in `new` must remain");
    }

    #[test]
    fn k1c_expired_budget_orders_by_path_components_not_raw_string() {
        // `Path` orders by component: `x/z.md` precedes `x.md`; a raw-string order would put `x.md` first ('.' < '/').
        let tmp = tempfile::tempdir().unwrap();
        write_command(tmp.path(), "x/z.md", "Z");
        write_command(tmp.path(), "x.md", "X");
        let mut registry = registry_with(Duration::ZERO);
        registry.refresh(tmp.path());

        let names = user_names(&registry);
        assert_eq!(names, vec!["x:z".to_string()], "user names: {names:?}");
        assert_eq!(
            registry.find("x:z").and_then(|c| c.content.as_deref()),
            Some("Z"),
            "user names: {names:?}"
        );
    }

    #[test]
    fn k1b_expired_budget_still_loads_the_three_ci_shapes() {
        let cases = [
            (
                "---\ndescription: \"\"\n---\nFirst body line.\nMore text.",
                "First body line.",
            ),
            ("## Code Review\nBody text here.", "Code Review"),
            ("", "(no description)"),
        ];
        for budget in [Duration::ZERO, Duration::MAX] {
            for (content, expected) in cases {
                let tmp = tempfile::tempdir().unwrap();
                write_command(tmp.path(), "cmd.md", content);
                let mut registry = registry_with(budget);
                registry.refresh(tmp.path());

                let names = user_names(&registry);
                let found = registry.find("cmd");
                assert!(
                    found.is_some(),
                    "budget {budget:?}, content {content:?}: `cmd` missing; user names: {names:?}"
                );
                assert_eq!(
                    found.map(|c| c.description.as_str()),
                    Some(expected),
                    "budget {budget:?}, content {content:?}; user names: {names:?}"
                );
            }
        }
    }

    #[test]
    fn k2_non_expiring_budget_loads_everything_in_order() {
        let tmp = three_file_workspace();
        let mut registry = registry_with(Duration::MAX);
        registry.refresh(tmp.path());

        let names = user_names(&registry);
        assert_eq!(
            names,
            vec!["a:z".to_string(), "b".to_string(), "c".to_string()],
            "user names: {names:?}"
        );
        for (name, content) in [("a:z", "Z"), ("b", "B"), ("c", "C")] {
            assert_eq!(
                registry.find(name).and_then(|c| c.content.as_deref()),
                Some(content),
                "{name}; user names: {names:?}"
            );
        }
    }

    #[test]
    fn k3_scan_cap_is_exactly_two_hundred_files() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..=MAX_COMMAND_SCAN_FILES {
            write_command(tmp.path(), &format!("cmd-{i:03}.md"), "body");
        }
        let mut registry = registry_with(Duration::MAX);
        registry.refresh(tmp.path());

        let names = user_names(&registry);
        assert_eq!(
            names.len(),
            200,
            "user name count; first/last: {:?}/{:?}",
            names.first(),
            names.last()
        );
        assert!(
            registry.find("cmd-000").is_some(),
            "cmd-000 missing; last: {:?}",
            names.last()
        );
        assert!(
            registry.find("cmd-199").is_some(),
            "cmd-199 missing; last: {:?}",
            names.last()
        );
        assert!(registry.find("cmd-200").is_none(), "cmd-200 must be absent");
    }
}
