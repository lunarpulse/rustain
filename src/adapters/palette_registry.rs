use std::collections::HashMap;

use crate::adapters::command_registry::CommandRegistry;
use crate::domain::models::palette::{PaletteAction, PaletteEntry, PaletteScope};

/// Prefix characters that map to palette scopes (AC2).
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
pub struct PaletteRegistry {
    entries: Vec<PaletteEntry>,
    /// Track whether we populated from a discovered CommandRegistry.
    populated_from_discovered: bool,
}

impl PaletteRegistry {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            populated_from_discovered: false,
        }
    }

    /// Populate from the CommandRegistry. Lazy-loads on first call.
    /// Re-populates if the CommandRegistry discovery state changed.
    pub fn populate_from_command_registry(&mut self, command_registry: &CommandRegistry) {
        let cr_discovered = command_registry.is_discovered();

        // Skip if already populated with same discovery state
        if !self.entries.is_empty() && self.populated_from_discovered == cr_discovered {
            return;
        }

        // Clear and rebuild
        self.entries.clear();

        // Map all slash commands to palette entries
        let all_commands = command_registry.filter("");
        for cmd in all_commands {
            self.entries.push(PaletteEntry {
                name: format!("/{}", cmd.name),
                description: cmd.description.clone(),
                shortcut: None,
                scope: PaletteScope::SlashCommand,
                action: PaletteAction::ExecuteCommand(cmd.name.clone(), None),
            });
        }

        // Built-in: version info
        // Covers: FR109
        self.entries.push(PaletteEntry {
            name: "version".to_string(),
            description: "Show rustain version and build info".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::ShowVersion,
        });

        // Built-in: tab management
        self.entries.push(PaletteEntry {
            name: "new tab".to_string(),
            description: "Open a new conversation tab".to_string(),
            shortcut: Some("Ctrl+T".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::NewTab,
        });
        self.entries.push(PaletteEntry {
            name: "close tab".to_string(),
            description: "Close the current conversation tab".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::CloseTab,
        });
        self.entries.push(PaletteEntry {
            name: "toggle sidebar".to_string(),
            description: "Toggle conversation history sidebar".to_string(),
            shortcut: Some("Ctrl+H".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::ToggleSidebar,
        });
        self.entries.push(PaletteEntry {
            name: "delete all conversations".to_string(),
            description: "Delete all saved conversations (requires confirmation)".to_string(),
            shortcut: None,
            scope: PaletteScope::All,
            action: PaletteAction::DeleteAllConversations,
        });

        // Clipboard paste
        self.entries.push(PaletteEntry {
            name: "paste image from clipboard".to_string(),
            description: "Paste an image from the OS clipboard into the current message"
                .to_string(),
            shortcut: Some("Alt+V".to_string()),
            scope: PaletteScope::All,
            action: PaletteAction::PasteImageFromClipboard,
        });

        // Permission mode entries (Story 5-0b AC9)
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
            self.entries.push(PaletteEntry {
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

        self.populated_from_discovered = cr_discovered;
    }

    /// Register a single entry (for future epics to add their entries).
    #[allow(dead_code)]
    pub fn register(&mut self, entry: PaletteEntry) {
        self.entries.push(entry);
    }

    /// All entries, unfiltered.
    #[allow(dead_code)]
    pub fn all_entries(&self) -> &[PaletteEntry] {
        &self.entries
    }

    /// Entries for a specific scope.
    #[allow(dead_code)]
    pub fn entries_for_scope(&self, scope: PaletteScope) -> Vec<&PaletteEntry> {
        self.entries.iter().filter(|e| e.scope == scope).collect()
    }

    /// Scopes that have at least one entry (AC8: dynamic scope visibility).
    #[allow(dead_code)]
    pub fn populated_scopes(&self) -> Vec<PaletteScope> {
        let mut seen = HashMap::new();
        for entry in &self.entries {
            seen.entry(entry.scope).or_insert(true);
        }
        seen.into_keys().collect()
    }

    /// Map a prefix character to a scope.
    pub fn scope_for_prefix(prefix: char) -> Option<PaletteScope> {
        PREFIX_SCOPE_MAP
            .iter()
            .find(|(c, _)| *c == prefix)
            .map(|(_, scope)| *scope)
    }

    /// Fuzzy filter entries. Returns entries sorted by relevance score.
    ///
    /// Scoring: exact prefix > word-boundary match > substring match.
    /// When `scope` is Some, only entries matching that scope are returned.
    /// When `scope` is None, all entries from populated scopes are searched.
    pub fn fuzzy_filter(&self, query: &str, scope: Option<PaletteScope>) -> Vec<&PaletteEntry> {
        let lower_query = query.to_lowercase();

        let mut scored: Vec<(&PaletteEntry, u32)> = self
            .entries
            .iter()
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

        // Sort by score descending, then name ascending for stability
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.name.cmp(&b.0.name)));
        scored.into_iter().map(|(entry, _)| entry).collect()
    }
}

impl Default for PaletteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute a fuzzy match score for an entry against a query.
/// Returns 0 if no match.
///
/// Scoring tiers:
///   300 — exact prefix match on name
///   200 — word-boundary match on name
///   100 — substring match on name
///    50 — substring match on description
fn fuzzy_score(name: &str, description: &str, lower_query: &str) -> u32 {
    let lower_name = name.to_lowercase();
    let lower_desc = description.to_lowercase();

    // Exact prefix match on name (highest priority)
    if lower_name.starts_with(lower_query) {
        return 300;
    }

    // Word-boundary match on name: query matches start of any word
    let words_match = lower_name
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(lower_query));
    if words_match {
        return 200;
    }

    // Substring match on name
    if lower_name.contains(lower_query) {
        return 100;
    }

    // Substring match on description
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
        // Exact prefix should rank higher than substring
        assert!(fuzzy_score("/new", "desc", "/ne") > fuzzy_score("/renew", "desc", "/ne"));
        // Name match should rank higher than description match
        assert!(fuzzy_score("deploy", "desc", "dep") > fuzzy_score("other", "deploy here", "dep"));
    }

    #[test]
    fn test_populate_from_command_registry() {
        let cr = CommandRegistry::new(); // has built-in /new and /export
        let mut reg = PaletteRegistry::new();

        reg.populate_from_command_registry(&cr);
        // 5 slash commands (/new, /export, /deactivate, /mode, /plan) + 4 mode palette entries +
        // 6 built-ins (version, new tab, close tab, toggle sidebar, delete all, paste image from clipboard) = 15
        assert_eq!(reg.all_entries().len(), 15);
        assert!(reg.all_entries().iter().any(|e| e.name == "/new"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/export"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/deactivate"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/mode"));
        assert!(reg.all_entries().iter().any(|e| e.name == "/plan"));
        assert!(reg.all_entries().iter().any(|e| e.name == "version"));
        assert!(reg.all_entries().iter().any(|e| e.name == "new tab"));
        assert!(reg.all_entries().iter().any(|e| e.name == "close tab"));
        assert!(
            reg.all_entries()
                .iter()
                .any(|e| e.name == "delete all conversations")
        );

        // Second call should be a no-op (cached)
        reg.populate_from_command_registry(&cr);
        assert_eq!(reg.all_entries().len(), 15);
    }
}
