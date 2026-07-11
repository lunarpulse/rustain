use std::collections::HashMap;

use crate::adapters::command_registry::CommandRegistry;
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

const PREFIX_SCOPE_MAP: &[(char, PaletteScope)] = &[
    ('/', PaletteScope::SlashCommand),
    ('@', PaletteScope::FileMention),
    (':', PaletteScope::Model),
    ('>', PaletteScope::Profile),
    ('!', PaletteScope::Adapter),
];

/// Registry of command palette entries.
/// Populated lazily from `CommandRegistry` on first Ctrl+P.
/// Cached for the session; re-populated only if `CommandRegistry.discovered` changes.
///
/// DF-041 fix: entries split into two buckets so `populate_from_command_registry`
/// never clears entries added via `register()`.
pub struct PaletteRegistry {
    /// Entries managed by `populate_from_command_registry` (slash commands + built-ins).
    slash_entries: Vec<PaletteEntry>,
    /// Entries managed by `register()` (model/profile/adapter entries from epics).
    registered_entries: Vec<PaletteEntry>,
    /// Track whether we populated from a discovered CommandRegistry.
    populated_from_discovered: bool,
}

impl PaletteRegistry {
    pub fn new() -> Self {
        Self {
            slash_entries: Vec::new(),
            registered_entries: Vec::new(),
            populated_from_discovered: false,
        }
    }

    /// Populate from the CommandRegistry. Lazy-loads on first call.
    /// Re-populates if the CommandRegistry discovery state changed.
    /// Only clears `slash_entries`; `registered_entries` survive (DF-041).
    pub fn populate_from_command_registry(&mut self, command_registry: &CommandRegistry) {
        let cr_discovered = command_registry.is_discovered();

        if !self.slash_entries.is_empty() && self.populated_from_discovered == cr_discovered {
            return;
        }

        self.slash_entries.clear();

        let all_commands = command_registry.filter("");
        for cmd in all_commands {
            self.slash_entries.push(PaletteEntry {
                name: format!("/{}", cmd.name),
                description: cmd.description.clone(),
                shortcut: None,
                scope: PaletteScope::SlashCommand,
                action: PaletteAction::ExecuteCommand(cmd.name.clone(), None),
            });
        }

        self.slash_entries.push(PaletteEntry {
            name: "version".to_string(),
            description: "Show rustain version and build info".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::ShowVersion,
        });

        self.slash_entries.push(PaletteEntry {
            name: "new tab".to_string(),
            description: "Open a new conversation tab".to_string(),
            shortcut: Some("Ctrl+T".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::NewTab,
        });
        self.slash_entries.push(PaletteEntry {
            name: "close tab".to_string(),
            description: "Close the current conversation tab".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::CloseTab,
        });
        self.slash_entries.push(PaletteEntry {
            name: "toggle sidebar".to_string(),
            description: "Toggle conversation history sidebar".to_string(),
            shortcut: Some("Ctrl+H".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::ToggleSidebar,
        });
        self.slash_entries.push(PaletteEntry {
            name: "delete all conversations".to_string(),
            description: "Delete all saved conversations (requires confirmation)".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::DeleteAllConversations,
        });

        self.slash_entries.push(PaletteEntry {
            name: "paste image from clipboard".to_string(),
            description: "Paste an image from the OS clipboard into the current message"
                .to_string(),
            shortcut: Some("Alt+V".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::PasteImageFromClipboard,
        });

        for (mode_name, mode_arg, mode_desc) in [
            ("mode: plan", "plan", "Plan mode — read-only tools only"),
            (
                "mode: normal",
                "normal",
                "Normal mode — approve Standard/Elevated tools",
            ),
            (
                "mode: autoedit",
                "autoedit",
                "AutoEdit mode — auto-allow Write/Edit",
            ),
            ("mode: yolo", "yolo", "YOLO mode — all tools auto-approved"),
        ] {
            self.slash_entries.push(PaletteEntry {
                name: mode_name.to_string(),
                description: mode_desc.to_string(),
                shortcut: None,
                scope: PaletteScope::SlashCommand,
                action: PaletteAction::ExecuteCommand(
                    "mode".to_string(),
                    Some(mode_arg.to_string()),
                ),
            });
        }

        self.slash_entries.push(PaletteEntry {
            name: "!cancel-plan".into(),
            description: "Cancel the current executing plan".into(),
            shortcut: Some("Ctrl+X, T then x".into()),
            scope: PaletteScope::All,
            action: PaletteAction::ExecuteCommand("cancel-plan".into(), None),
        });
        self.slash_entries.push(PaletteEntry {
            name: "!reorder-task".into(),
            description: "Move the selected pending task up or down".into(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::ExecuteCommand("reorder-task".into(), None),
        });
        self.slash_entries.push(PaletteEntry {
            name: "!resume-all-tasks".into(),
            description: "Resume every paused task".into(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::ExecuteCommand("resume-all-tasks".into(), None),
        });

        for (port_label, port_dim) in [
            (
                "persona",
                crate::domain::models::profile::PortDimension::Persona,
            ),
            (
                "memory",
                crate::domain::models::profile::PortDimension::Memory,
            ),
            (
                "session",
                crate::domain::models::profile::PortDimension::Session,
            ),
            (
                "tools",
                crate::domain::models::profile::PortDimension::Tools,
            ),
            (
                "channels",
                crate::domain::models::profile::PortDimension::Channels,
            ),
            (
                "scheduler",
                crate::domain::models::profile::PortDimension::Scheduler,
            ),
            (
                "context",
                crate::domain::models::profile::PortDimension::Context,
            ),
        ] {
            self.slash_entries.push(PaletteEntry {
                name: format!("!{port_label}"),
                description: format!("Override {port_label} adapter for this session"),
                shortcut: None,
                scope: PaletteScope::Adapter,
                action: PaletteAction::ApplyAdapterOverride {
                    port: port_dim,
                    adapter: String::new(),
                },
            });
        }

        self.populated_from_discovered = cr_discovered;
    }

    /// Register a single entry. Survives `populate_from_command_registry` calls (DF-041).
    #[allow(dead_code)]
    pub fn register(&mut self, entry: PaletteEntry) {
        self.registered_entries.push(entry);
    }

    fn all_entries_iter(&self) -> impl Iterator<Item = &PaletteEntry> {
        self.slash_entries
            .iter()
            .chain(self.registered_entries.iter())
    }

    #[allow(dead_code)]
    pub fn all_entries(&self) -> Vec<&PaletteEntry> {
        self.all_entries_iter().collect()
    }

    #[allow(dead_code)]
    pub fn entries_for_scope(&self, scope: PaletteScope) -> Vec<&PaletteEntry> {
        self.all_entries_iter()
            .filter(|e| e.scope == scope)
            .collect()
    }

    #[allow(dead_code)]
    pub fn populated_scopes(&self) -> Vec<PaletteScope> {
        let mut seen = HashMap::new();
        for entry in self.all_entries_iter() {
            seen.entry(entry.scope).or_insert(true);
        }
        seen.into_keys().collect()
    }

    pub fn scope_for_prefix(prefix: char) -> Option<PaletteScope> {
        PREFIX_SCOPE_MAP
            .iter()
            .find(|(c, _)| *c == prefix)
            .map(|(_, scope)| *scope)
    }

    pub fn fuzzy_filter(&self, query: &str, scope: Option<PaletteScope>) -> Vec<&PaletteEntry> {
        let lower_query = query.to_lowercase();

        let mut scored: Vec<(&PaletteEntry, u32)> = self
            .all_entries_iter()
            .filter(|e| match scope {
                Some(s) => e.scope == s,
                None => true,
            })
            .filter_map(|entry| {
                if lower_query.is_empty() {
                    return Some((entry, 0));
                }
                let score = fuzzy_score(&entry.name, &entry.description, &lower_query);
                if score > 0 {
                    Some((entry, score))
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        scored.into_iter().map(|(entry, _)| entry).collect()
    }
}

impl Default for PaletteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn fuzzy_score(name: &str, description: &str, lower_query: &str) -> u32 {
    let lower_name = name.to_lowercase();
    let lower_desc = description.to_lowercase();

    if lower_name.starts_with(lower_query) {
        return 300;
    }

    let words_match = lower_name
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(lower_query));
    if words_match {
        return 200;
    }

    if lower_name.contains(lower_query) {
        return 100;
    }

    if lower_desc.contains(lower_query) {
        return 50;
    }

    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

    fn make_entry(name: &str, desc: &str, scope: PaletteScope) -> PaletteEntry {
        PaletteEntry {
            name: name.to_string(),
            description: desc.to_string(),
            shortcut: None,
            scope,
            action: PaletteAction::Noop,
        }
    }

    #[test]
    fn test_registry_register_and_all_entries() {
        let mut reg = PaletteRegistry::new();
        assert!(reg.all_entries().is_empty());

        reg.register(make_entry(
            "/new",
            "Start new session",
            PaletteScope::SlashCommand,
        ));
        reg.register(make_entry(
            "/deploy",
            "Deploy staging",
            PaletteScope::SlashCommand,
        ));
        assert_eq!(reg.all_entries().len(), 2);
    }

    #[test]
    fn test_entries_for_scope() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry(
            "/new",
            "Start new session",
            PaletteScope::SlashCommand,
        ));
        reg.register(make_entry("gpt-4", "OpenAI model", PaletteScope::Model));

        assert_eq!(reg.entries_for_scope(PaletteScope::SlashCommand).len(), 1);
        assert_eq!(reg.entries_for_scope(PaletteScope::Model).len(), 1);
        assert_eq!(reg.entries_for_scope(PaletteScope::Profile).len(), 0);
    }

    #[test]
    fn test_populated_scopes() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));
        reg.register(make_entry("model-x", "Model X", PaletteScope::Model));

        let scopes = reg.populated_scopes();
        assert!(scopes.contains(&PaletteScope::SlashCommand));
        assert!(scopes.contains(&PaletteScope::Model));
        assert!(!scopes.contains(&PaletteScope::Profile));
    }

    #[test]
    fn test_scope_for_prefix() {
        assert_eq!(
            PaletteRegistry::scope_for_prefix('/'),
            Some(PaletteScope::SlashCommand)
        );
        assert_eq!(
            PaletteRegistry::scope_for_prefix('@'),
            Some(PaletteScope::FileMention)
        );
        assert_eq!(
            PaletteRegistry::scope_for_prefix(':'),
            Some(PaletteScope::Model)
        );
        assert_eq!(
            PaletteRegistry::scope_for_prefix('>'),
            Some(PaletteScope::Profile)
        );
        assert_eq!(
            PaletteRegistry::scope_for_prefix('!'),
            Some(PaletteScope::Adapter)
        );
        assert_eq!(PaletteRegistry::scope_for_prefix('x'), None);
    }

    #[test]
    fn test_fuzzy_filter_empty_query_returns_all() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));
        reg.register(make_entry("/deploy", "Deploy", PaletteScope::SlashCommand));

        let results = reg.fuzzy_filter("", None);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_fuzzy_filter_exact_prefix() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));
        reg.register(make_entry("/deploy", "Deploy", PaletteScope::SlashCommand));

        let results = reg.fuzzy_filter("/ne", None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "/new");
    }

    #[test]
    fn test_fuzzy_filter_substring_match() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry(
            "/deploy",
            "Deploy staging",
            PaletteScope::SlashCommand,
        ));
        reg.register(make_entry(
            "/debug",
            "Debug session",
            PaletteScope::SlashCommand,
        ));

        let results = reg.fuzzy_filter("de", None);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_fuzzy_filter_case_insensitive() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/New", "Start new", PaletteScope::SlashCommand));

        let results = reg.fuzzy_filter("new", None);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_fuzzy_filter_scoped() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));
        reg.register(make_entry("gpt-4", "Model", PaletteScope::Model));

        let results = reg.fuzzy_filter("", Some(PaletteScope::SlashCommand));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "/new");
    }

    #[test]
    fn test_fuzzy_filter_no_match() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry("/new", "Start new", PaletteScope::SlashCommand));

        let results = reg.fuzzy_filter("xyz", None);
        assert!(results.is_empty());
    }

    #[test]
    fn test_fuzzy_filter_description_match() {
        let mut reg = PaletteRegistry::new();
        reg.register(make_entry(
            "/deploy",
            "Deploy to staging",
            PaletteScope::SlashCommand,
        ));

        let results = reg.fuzzy_filter("staging", None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "/deploy");
    }

    #[test]
    fn test_fuzzy_score_ranking() {
        assert!(fuzzy_score("/new", "desc", "/ne") > fuzzy_score("/renew", "desc", "/ne"));
        assert!(fuzzy_score("deploy", "desc", "dep") > fuzzy_score("other", "deploy here", "dep"));
    }

    #[test]
    fn test_populate_from_command_registry() {
        let cr = CommandRegistry::new();
        let mut reg = PaletteRegistry::new();

        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.all_entries().len(), 36); // 21 base + 7 port slash + 7 adapter palette + 1 /fanout
        assert!(reg.all_entries().iter().any(|e| e.name == "/new"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/clear"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/export"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/deactivate"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/mode"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/plan"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/config"));
        assert!(reg.all_entries().iter().any(|e| e.name == "version"));
        assert!(reg.all_entries().iter().any(|e| e.name == "new tab"));
        assert!(reg.all_entries().iter().any(|e| e.name == "close tab"));
        assert!(
            reg.all_entries()
                .iter()
                .any(|e| e.name == "delete all conversations")
        );

        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.all_entries().len(), 36); // 21 base + 7 port slash + 7 adapter palette + 1 /fanout
    }

    #[test]
    fn test_populate_does_not_clear_registered_entries() {
        let cr = CommandRegistry::new();
        let mut reg = PaletteRegistry::new();

        reg.register(make_entry(
            "claude-sonnet",
            "Anthropic model",
            PaletteScope::Model,
        ));
        assert_eq!(reg.all_entries().len(), 1);

        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.slash_entries.len(), 36); // 21 base + 7 port slash + 7 adapter palette + 1 /fanout
        assert_eq!(reg.registered_entries.len(), 1);
        assert_eq!(reg.all_entries().len(), 37);
        assert!(reg.all_entries().iter().any(|e| e.name == "claude-sonnet"));

        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.all_entries().len(), 37);
        assert!(reg.all_entries().iter().any(|e| e.name == "claude-sonnet"));

        reg.register(make_entry("gpt-4o", "OpenAI model", PaletteScope::Model));
        assert_eq!(reg.all_entries().len(), 38);

        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.all_entries().len(), 38);
    }
}
