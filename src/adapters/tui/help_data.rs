/// Static keybinding data and multiplexer conflict information for the help overlay.
// Covers: FR108, UX-DR94, UX-DR62

use std::sync::LazyLock;

/// A single keybinding entry in the help overlay.
pub struct HelpBinding {
    pub key: &'static str,
    pub description: &'static str,
    /// Whether this binding is available in the current profile.
    /// For MVP, all bindings are `true`. Extension point for Epic 8 profile-awareness.
    pub available: bool,
}

/// A named group of keybindings.
pub struct HelpCategory {
    pub name: &'static str,
    pub bindings: Vec<HelpBinding>,
}

/// A known tmux/screen prefix key conflict.
pub struct TmuxConflict {
    pub key: &'static str,
    pub conflict_with: &'static str,
    pub alternative: Option<&'static str>,
}

/// Static keybinding categories — allocated once, reused on every render.
static HELP_CATEGORIES: LazyLock<Vec<HelpCategory>> = LazyLock::new(|| {
    vec![
        HelpCategory {
            name: "NAVIGATION",
            bindings: vec![
                HelpBinding {
                    key: "Esc",
                    description: "Switch focus / dismiss overlay",
                    available: true,
                },
                HelpBinding {
                    key: "i",
                    description: "Focus input box",
                    available: true,
                },
                HelpBinding {
                    key: "j / k",
                    description: "Scroll down / up",
                    available: true,
                },
                HelpBinding {
                    key: "J / K",
                    description: "Jump to next / previous content block",
                    available: true,
                },
                HelpBinding {
                    key: "{ / }",
                    description: "Jump to next / previous user message",
                    available: true,
                },
                HelpBinding {
                    key: "Tab",
                    description: "Toggle sidebar focus",
                    available: true,
                },
            ],
        },
        HelpCategory {
            name: "INPUT",
            bindings: vec![
                HelpBinding {
                    key: "Enter",
                    description: "Send message",
                    available: true,
                },
                HelpBinding {
                    key: "Shift+Enter",
                    description: "New line (multi-line mode)",
                    available: true,
                },
                HelpBinding {
                    key: "Alt+Enter (VS Code)",
                    description: "New line — alternative when Shift+Enter is intercepted",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+E",
                    description: "Toggle multi-line mode",
                    available: true,
                },
                HelpBinding {
                    key: "Alt+M (VS Code)",
                    description: "Toggle multi-line mode — alternative when Ctrl+E is intercepted",
                    available: true,
                },
                HelpBinding {
                    key: "/ml",
                    description: "Toggle multi-line mode via slash command",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+R",
                    description: "Reverse search history",
                    available: true,
                },
                HelpBinding {
                    key: "Up / Down",
                    description: "Cycle input history (when input empty)",
                    available: true,
                },
            ],
        },
        HelpCategory {
            name: "COMMANDS",
            bindings: vec![
                HelpBinding {
                    key: "/",
                    description: "Slash command autocomplete",
                    available: true,
                },
                HelpBinding {
                    key: "@",
                    description: "File / agent mention",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+P",
                    description: "Command palette",
                    available: true,
                },
            ],
        },
        HelpCategory {
            name: "CHORDS (Ctrl+X prefix)",
            bindings: vec![
                HelpBinding {
                    key: "Ctrl+X, P",
                    description: "Profile switcher",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, M",
                    description: "Model selector",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, A",
                    description: "Adapter panel",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, S",
                    description: "Subagent panel",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, L",
                    description: "Log panel",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, T",
                    description: "Task panel",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, U",
                    description: "Usage / cost",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, W",
                    description: "Watch / monitor",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, D",
                    description: "Dashboard",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+X, ?",
                    description: "This help overlay",
                    available: true,
                },
            ],
        },
        HelpCategory {
            name: "CLIPBOARD & IMAGES",
            bindings: vec![
                HelpBinding {
                    key: "c",
                    description: "Copy focused content to clipboard",
                    available: true,
                },
                HelpBinding {
                    key: "Paste",
                    description: "Attach image from clipboard",
                    available: true,
                },
            ],
        },
        HelpCategory {
            name: "GENERAL",
            bindings: vec![
                HelpBinding {
                    key: "?",
                    description: "Toggle this help overlay",
                    available: true,
                },
                HelpBinding {
                    key: "Ctrl+C",
                    description: "Interrupt / cancel",
                    available: true,
                },
                HelpBinding {
                    key: "q",
                    description: "Quit (from chat focus)",
                    available: true,
                },
            ],
        },
    ]
});

/// Returns all keybinding categories for the help overlay (zero allocation after first call).
pub fn help_categories() -> &'static [HelpCategory] {
    &HELP_CATEGORIES
}

/// Static multiplexer conflict list — allocated once.
static TMUX_CONFLICTS: LazyLock<Vec<TmuxConflict>> = LazyLock::new(|| {
    vec![
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
    ]
});

/// Returns the static list of known tmux/screen prefix key conflicts (zero allocation after first call).
pub fn tmux_conflicts() -> &'static [TmuxConflict] {
    &TMUX_CONFLICTS
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
    use super::*;

    #[test]
    fn test_help_categories_non_empty() {
        let categories = help_categories();
        assert!(!categories.is_empty());
        for cat in categories {
            assert!(
                !cat.bindings.is_empty(),
                "Category '{}' has no bindings",
                cat.name
            );
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
}
