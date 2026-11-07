//! Generated help overlay rows (Story 21.3 A11): the ten categories, their order
//! and the row text come from the keymap table (`Binding::help`) and the slash
//! usage of the command catalogue (`SlashCommandDef::usage`). Pure; no I/O.

use crate::adapters::command_registry::CommandRegistry;

use super::model::{CommandId, HelpCat, Key};
use super::resolve::table;
use super::table::GlobalCmd;
use super::table_chat::ChatCmd;
use super::table_input::InputCmd;

/// A single help row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpRow {
    pub key: String,
    pub description: String,
    /// Whether this row is available in the current build/profile.
    pub available: bool,
}

/// A named group of help rows (one per `HelpCat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpGroup {
    pub name: &'static str,
    pub rows: Vec<HelpRow>,
}

/// A known tmux/screen prefix key conflict.
pub struct TmuxConflict {
    pub key: &'static str,
    pub conflict_with: &'static str,
    pub alternative: Option<&'static str>,
}

/// Help rows that document a gesture or a duplicate route rather than a registry
/// binding: (category, insert after the row whose key equals this, else append, key, description).
#[rustfmt::skip]
const DOC_ROWS: &[(HelpCat, &str, &str, &str)] = &[
    (HelpCat::ScrollMouse, "Ctrl+b", "Mouse wheel", "Scroll by wheel_lines (default 3 lines)"),
    (HelpCat::ScrollMouse, "Mouse wheel", "Shift+Wheel", "Scroll half page per tick"),
    (HelpCat::ScrollMouse, "Shift+Wheel", "Hold Shift", "Select text for copy (bypasses mouse capture)"),
    (HelpCat::VimFold, "[[", "Tab", "Cycle tools, thinking, code, tables (needs 2+); else sidebar focus, else next tab"),
    (HelpCat::Input, "Ctrl+Enter", "Ctrl+X, Enter", "Send from any terminal (portable multi-line send)"),
    (HelpCat::Clipboard, "c", "Paste", "Attach image from clipboard"),
];

impl HelpRow {
    fn new(key: &str, description: &str, available: bool) -> Self {
        Self {
            key: key.to_string(),
            description: description.to_string(),
            available,
        }
    }
}

/// The help overlay's groups: registry rows in table order, documentation rows at
/// their anchor, then the slash usage of the built-in commands in catalogue order.
/// Host-neutral: a row appears whichever host owns its binding.
pub fn help_groups() -> Vec<HelpGroup> {
    let registry = CommandRegistry::new();
    HelpCat::ALL
        .into_iter()
        .map(|cat| {
            let mut rows: Vec<HelpRow> = table()
                .iter()
                .filter_map(|row| row.help.filter(|help| help.cat == cat))
                .map(|help| HelpRow::new(help.key, help.desc, true))
                .collect();
            for &(_, after, key, description) in DOC_ROWS.iter().filter(|doc| doc.0 == cat) {
                let at = rows
                    .iter()
                    .position(|row| row.key == after)
                    .map_or(rows.len(), |i| i + 1);
                rows.insert(at, HelpRow::new(key, description, true));
            }
            rows.extend(
                registry
                    .builtin_usage()
                    .filter(|usage| usage.0 == cat.title())
                    .map(|&(_, key, description, available)| {
                        HelpRow::new(key, description, available)
                    }),
            );
            HelpGroup {
                name: cat.title(),
                rows,
            }
        })
        .collect()
}

/// Every help row, flat (`key`, `description`, `available`).
pub fn help_rows() -> Vec<HelpRow> {
    help_groups()
        .into_iter()
        .flat_map(|group| group.rows)
        .collect()
}

/// The overlay's prose introduction, built from the registry's own labels.
pub fn intro() -> String {
    let label = |command: CommandId| {
        table()
            .iter()
            .find(|row| row.command == command)
            .map(|row| row.long_label())
            .unwrap_or_default()
    };
    format!(
        "Rustain is a composable AI agent platform. Navigate with {}/{}, type with {}, \
         open commands with {}, extend with {} chords. Press {} or {} to close.",
        label(CommandId::Chat(ChatCmd::ScrollLineDown)),
        label(CommandId::Chat(ChatCmd::ScrollLineUp)),
        label(CommandId::Chat(ChatCmd::FocusInput)),
        label(CommandId::Input(InputCmd::OpenPalette)),
        label(CommandId::Global(GlobalCmd::OpenLeader)),
        label(CommandId::Chat(ChatCmd::OpenHelp)),
        Key::Esc.label(),
    )
}

/// Known tmux/screen prefix key conflicts.
static TMUX_CONFLICTS: &[TmuxConflict] = &[
    TmuxConflict {
        key: "Ctrl+B",
        conflict_with: "tmux default prefix",
        alternative: Some("Use Ctrl+X chords as alternative"),
    },
    TmuxConflict {
        key: "Ctrl+A",
        conflict_with: "screen default prefix",
        alternative: Some("Use Ctrl+X chords as alternative"),
    },
];

/// Returns the known tmux/screen prefix key conflicts.
pub fn tmux_conflicts() -> &'static [TmuxConflict] {
    TMUX_CONFLICTS
}

/// Returns `true` if the process is running inside a terminal multiplexer
/// (tmux or GNU screen). Detection is via `$TMUX` (tmux) or `$STY` (screen).
// Covers: UX-DR62 (AC3: tmux/screen compatibility notice)
pub fn is_multiplexer_session() -> bool {
    crate::infrastructure::utils::env_var_is_set("TMUX")
        || crate::infrastructure::utils::env_var_is_set("STY")
}

#[cfg(test)]
mod tests {
    use super::super::model::{Context, Host};
    use super::super::resolve::{Resolved, resolve};
    use super::*;

    #[test]
    fn test_help_groups_non_empty() {
        let groups = help_groups();
        assert!(!groups.is_empty());
        for group in &groups {
            assert!(!group.rows.is_empty(), "Group '{}' has no rows", group.name);
        }
    }

    #[test]
    fn test_tmux_conflicts_contains_known() {
        let conflicts = tmux_conflicts();
        assert!(conflicts.len() >= 2);
        assert!(
            conflicts.iter().any(|c| c.key == "Ctrl+B"),
            "Expected Ctrl+B conflict"
        );
        assert!(
            conflicts.iter().any(|c| c.key == "Ctrl+A"),
            "Expected Ctrl+A conflict"
        );
    }

    #[test]
    fn test_is_multiplexer_session_false_when_unset() {
        // SAFETY: single-threaded test; env vars restored implicitly (test runner)
        unsafe { std::env::remove_var("TMUX") };
        unsafe { std::env::remove_var("STY") };
        assert!(!is_multiplexer_session());
    }

    #[test]
    fn story21_3_help_keeps_the_ten_category_titles_in_order() {
        let names: Vec<&str> = help_groups().iter().map(|group| group.name).collect();
        assert_eq!(
            names,
            [
                "NAVIGATION",
                "SCROLL & MOUSE",
                "VIM FOLD & MOTION",
                "INPUT",
                "COMMANDS",
                "CHORDS (Ctrl+X prefix)",
                "PERMISSIONS",
                "CLIPBOARD & IMAGES",
                "Search, Bookmarks & Export",
                "GENERAL",
            ]
        );
    }

    #[test]
    fn story21_3_generated_intro_is_true() {
        let intro = intro();
        assert!(!intro.contains("search with Ctrl+P"), "{intro}");
        assert!(
            intro.contains("Navigate with j/k, type with i, open commands with Ctrl+P, extend with Ctrl+X chords. Press ? or Esc to close."),
            "{intro}"
        );
    }

    #[test]
    fn story21_3_help_has_no_ctrl_h_row_and_no_dashboard_chord() {
        let rows = help_rows();
        assert!(rows.iter().all(|row| row.key != "Ctrl+H"));
        assert!(
            rows.iter()
                .all(|row| !row.description.contains("Dashboard"))
        );
        assert!(
            rows.iter().any(|row| row.key == "Ctrl+X, H"),
            "positive control"
        );
    }

    #[test]
    fn story21_3_chord_rows_equal_the_leader_rows() {
        let leader: Vec<(String, String)> = table()
            .iter()
            .filter(|row| row.context == Context::Leader)
            .filter_map(|row| row.help)
            .map(|help| (help.key.to_string(), help.desc.to_string()))
            .collect();
        assert!(!leader.is_empty());
        let chords: Vec<(String, String)> = help_groups()
            .into_iter()
            .find(|group| group.name == HelpCat::Chords.title())
            .expect("CHORDS group")
            .rows
            .into_iter()
            .map(|row| (row.key, row.description))
            .collect();
        assert_eq!(chords, leader);
    }
    #[test]
    fn story21_3_slash_usage_rows_come_from_the_command_catalogue() {
        let rows = help_rows();
        let find = |key: &str| rows.iter().find(|row| row.key == key);
        assert!(find("/artifact resolve <id> present|absent").is_some_and(|row| row.available));
        assert!(find("/ml").is_some() && find("/notifications").is_some());
        assert!(find("/mode").is_some() && find("/export").is_some());
        let board = find("/team board [--last | --actions | --action <action-id> | <peer-id>]");
        assert_eq!(board.map(|row| row.available), Some(cfg!(feature = "a2a")));
    }
}
