//! Command-palette populate gate (Story 21-2 AC3).
//!
//! `CommandPaletteState::open` clears `filtered_entries`, so the palette must
//! be populated on every inactive→active transition — not only when the filter
//! text differs from the previously cached text (an empty query on reopen is
//! "unchanged" but the list is empty). [`PaletteGate`] caches the last
//! populated key as `Option`: `None` means "inactive / never populated" and is
//! deliberately distinct from `Some(key)` with an empty query.
//!
//! The gate is host-neutral: the local runtime calls it after every input
//! event and the attached host can call the same [`PaletteGate::sync`] with its
//! own registries. The only I/O is the one-off command discovery scan inside
//! [`refresh_entries`], guarded by `CommandRegistry::is_discovered`; the
//! per-frame key comparison is in-memory.

use std::path::Path;

use crate::adapters::command_registry::CommandRegistry;
use crate::adapters::palette_registry::PaletteRegistry;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::palette::PaletteScope;

/// What the palette list was last populated for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PaletteKey {
    /// Which `open` produced the list (catches close+reopen with an equal query).
    open_epoch: u64,
    filter_text: String,
    scope: Option<PaletteScope>,
    /// `(discovered, command count)` of the command registry.
    discovery: (bool, usize),
}

/// Decides when the palette's `filtered_entries` must be (re)computed.
#[derive(Debug, Default)]
pub struct PaletteGate {
    /// `None` = inactive or invalidated; `Some` = populated for that key.
    populated_for: Option<PaletteKey>,
    refreshes: u64,
}

impl PaletteGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of times the entries were actually recomputed. Structural
    /// observation point for tests and diagnostics (no sleeps needed).
    pub fn refresh_count(&self) -> u64 {
        self.refreshes
    }

    /// Force the next [`sync`](Self::sync) to repopulate (discovery changed in
    /// a way the registry signature cannot observe).
    pub fn invalidate(&mut self) {
        self.populated_for = None;
    }

    fn key_for(state: &TuiState, command_registry: &CommandRegistry) -> PaletteKey {
        PaletteKey {
            open_epoch: state.command_palette.open_epoch,
            filter_text: state.command_palette.filter_text.clone(),
            scope: state.command_palette.current_scope,
            discovery: PaletteRegistry::command_signature(command_registry),
        }
    }

    /// Bring `state.command_palette.filtered_entries` up to date. Returns
    /// `true` when entries were recomputed on this call.
    ///
    /// Call after every input event and after any discovery event while the
    /// palette may be open.
    pub async fn sync(
        &mut self,
        state: &mut TuiState,
        command_registry: &mut CommandRegistry,
        palette_registry: &mut PaletteRegistry,
        workspace_path: &Path,
    ) -> bool {
        if !state.command_palette.active {
            self.populated_for = None;
            return false;
        }
        let key = Self::key_for(state, command_registry);
        if self.populated_for.as_ref() == Some(&key) {
            return false;
        }
        refresh_entries(state, command_registry, palette_registry, workspace_path).await;
        // Discovery inside the refresh may have changed the registry signature;
        // cache the post-refresh key so the next frame is a no-op.
        self.populated_for = Some(Self::key_for(state, command_registry));
        self.refreshes += 1;
        true
    }
}

/// Populate the palette's filtered entries from the registries.
async fn refresh_entries(
    state: &mut TuiState,
    command_registry: &mut CommandRegistry,
    palette_registry: &mut PaletteRegistry,
    workspace_path: &Path,
) {
    // Ensure command registry is discovered (one filesystem scan, then cached).
    if !command_registry.is_discovered() {
        command_registry.refresh(workspace_path);
        crate::adapters::command_registry::announce_shadows(state, command_registry);
        let guard = state.skill_registry.read().await;
        command_registry.warn_skill_shadows(&guard);
    }

    // Cached; rebuilds only if the discovery signature changed.
    palette_registry.populate_from_command_registry(command_registry);

    // Determine scope from filter prefix
    let (scope, query) = if let Some(palette_scope) = state.command_palette.current_scope {
        // Strip prefix character from query
        let q = if state.command_palette.filter_text.len() > 1 {
            state
                .command_palette
                .filter_text
                .chars()
                .skip(1)
                .collect::<String>()
        } else {
            String::new()
        };
        (Some(palette_scope), q)
    } else {
        (None, state.command_palette.filter_text.clone())
    };

    let entries: Vec<_> = palette_registry
        .fuzzy_filter(&query, scope)
        .into_iter()
        .cloned()
        .collect();
    state.command_palette.filtered_entries = entries;

    // Clamp selection and scroll_offset to valid range after filter update
    let max_idx = state
        .command_palette
        .filtered_entries
        .len()
        .saturating_sub(1);
    state.command_palette.selected_index = state.command_palette.selected_index.min(max_idx);
    state.command_palette.scroll_offset = state.command_palette.scroll_offset.min(max_idx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::app::handle_input;
    use crate::adapters::tui::theme::Theme;
    use crate::adapters::tui::widgets::command_palette;
    use crate::domain::events::{DomainInputEvent, DomainKey};
    use ratatui::{Terminal, backend::TestBackend};

    struct Rig {
        state: TuiState,
        commands: CommandRegistry,
        palette: PaletteRegistry,
        gate: PaletteGate,
        workspace: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                state: TuiState::new(100, 40),
                commands: CommandRegistry::new(),
                palette: PaletteRegistry::new(),
                gate: PaletteGate::new(),
                workspace: tempfile::tempdir().unwrap(),
            }
        }

        /// Real key ingress, then the same gate the runtime calls per event.
        async fn key(&mut self, event: DomainInputEvent) {
            let _ = handle_input(&mut self.state, &event);
            self.frame().await;
        }

        async fn frame(&mut self) -> bool {
            self.gate
                .sync(
                    &mut self.state,
                    &mut self.commands,
                    &mut self.palette,
                    self.workspace.path(),
                )
                .await
        }

        async fn ctrl_p(&mut self) {
            self.key(DomainInputEvent::SpecialKey(DomainKey::CtrlP))
                .await;
        }

        async fn esc(&mut self) {
            self.key(DomainInputEvent::SpecialKey(DomainKey::Esc)).await;
        }

        async fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                self.key(DomainInputEvent::KeyPress(c)).await;
            }
        }

        fn names(&self) -> Vec<String> {
            self.state
                .command_palette
                .filtered_entries
                .iter()
                .map(|e| e.name.clone())
                .collect()
        }

        fn slash_names(&self) -> Vec<String> {
            self.state
                .command_palette
                .filtered_entries
                .iter()
                .filter(|e| e.scope == PaletteScope::SlashCommand)
                .map(|e| e.name.clone())
                .collect()
        }

        fn assert_lists_catalogue(&self) {
            let listed = self.slash_names();
            for command in self.commands.filter("") {
                let name = format!("/{}", command.name);
                assert!(listed.contains(&name), "{name} missing from {listed:?}");
            }
            let expected: Vec<String> = self
                .palette
                .fuzzy_filter("", None)
                .iter()
                .map(|e| e.name.clone())
                .collect();
            assert_eq!(self.names(), expected, "stable existing order");
        }

        fn render(&self) -> String {
            let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
            let theme = Theme::dark();
            terminal
                .draw(|frame| {
                    command_palette::render(
                        frame,
                        frame.area(),
                        &self.state.command_palette,
                        &theme,
                    )
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[tokio::test]
    async fn story21_2_first_ctrl_p_lists_available_catalogue_without_typing() {
        let mut rig = Rig::new();
        rig.ctrl_p().await;

        assert!(rig.state.command_palette.active);
        assert_eq!(rig.gate.refresh_count(), 1);
        // Independent oracle: every command-registry command is listed, in
        // the existing stable registry order.
        rig.assert_lists_catalogue();
        assert!(!rig.slash_names().is_empty());
        let painted = rig.render();
        assert!(!painted.contains("No matches"), "{painted}");
        assert!(painted.contains("/accounts"), "{painted}");

        // Selection works on the immediately-listed entries.
        rig.key(DomainInputEvent::SpecialKey(DomainKey::Down)).await;
        assert_eq!(rig.state.command_palette.selected_index, 1);
        let selected = rig.state.command_palette.selected().unwrap().name.clone();
        assert_eq!(selected, rig.names()[1]);
        assert_eq!(rig.gate.refresh_count(), 1, "navigation must not refresh");
    }

    #[tokio::test]
    async fn story21_2_close_and_reopen_repopulates_empty_query() {
        let mut rig = Rig::new();
        rig.ctrl_p().await;
        let first = rig.names();
        rig.esc().await;
        assert!(!rig.state.command_palette.active);
        assert!(rig.state.command_palette.filtered_entries.is_empty());

        rig.ctrl_p().await;
        assert_eq!(rig.gate.refresh_count(), 2);
        assert_eq!(rig.names(), first);
        assert!(!rig.render().contains("No matches"));

        // Close and reopen between two gate calls (no inactive frame observed).
        let _ = handle_input(
            &mut rig.state,
            &DomainInputEvent::SpecialKey(DomainKey::Esc),
        );
        let _ = handle_input(
            &mut rig.state,
            &DomainInputEvent::SpecialKey(DomainKey::CtrlP),
        );
        assert!(rig.state.command_palette.filtered_entries.is_empty());
        assert!(rig.frame().await);
        assert_eq!(rig.names(), first);
    }

    #[tokio::test]
    async fn story21_2_backspace_to_empty_restores_full_list_and_nonmatching_shows_no_matches() {
        let mut rig = Rig::new();
        rig.ctrl_p().await;
        let all = rig.names();

        rig.type_text("new").await;
        assert!(rig.names().len() < all.len());
        for _ in 0..3 {
            rig.key(DomainInputEvent::SpecialKey(DomainKey::Backspace))
                .await;
        }
        assert_eq!(rig.state.command_palette.filter_text, "");
        assert_eq!(rig.names(), all);
        assert!(!rig.render().contains("No matches"));

        rig.type_text("zzzzqqqq").await;
        assert!(rig.state.command_palette.filtered_entries.is_empty());
        assert!(rig.render().contains("No matches"));
    }

    #[tokio::test]
    async fn story21_2_discovery_change_refreshes_once_and_idle_frames_do_not_rescan() {
        let mut rig = Rig::new();
        rig.ctrl_p().await;
        assert!(rig.commands.is_discovered());
        let after_open = rig.gate.refresh_count();

        // Idle frames: no refresh, and the filesystem is not re-scanned.
        let dir = rig.workspace.path().join(".claude").join("commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zebra21.md"), "late file").unwrap();
        for _ in 0..5 {
            assert!(!rig.frame().await);
        }
        assert_eq!(rig.gate.refresh_count(), after_open);
        assert!(!rig.slash_names().contains(&"/zebra21".to_owned()));

        // A discovery change (new command registered) refreshes exactly once.
        rig.commands.register_user_command(
            "story21probe".to_owned(),
            "probe".to_owned(),
            dir.join("story21probe.md"),
            None,
        );
        assert!(rig.frame().await);
        assert_eq!(rig.gate.refresh_count(), after_open + 1);
        assert!(rig.slash_names().contains(&"/story21probe".to_owned()));
        rig.assert_lists_catalogue();
        for _ in 0..3 {
            assert!(!rig.frame().await);
        }
        assert_eq!(rig.gate.refresh_count(), after_open + 1);

        // Explicit invalidation (host-signalled discovery) also refreshes once.
        rig.gate.invalidate();
        assert!(rig.frame().await);
        assert!(!rig.frame().await);
    }

    #[tokio::test]
    async fn story21_2_selection_and_scroll_survive_refresh_clamp() {
        let mut rig = Rig::new();
        rig.ctrl_p().await;
        for _ in 0..15 {
            rig.key(DomainInputEvent::SpecialKey(DomainKey::Down)).await;
        }
        let state = &rig.state.command_palette;
        assert_eq!(state.selected_index, 15);
        assert!(state.scroll_offset > 0, "scroll follows selection");
        // Narrowing the filter clamps selection/scroll into the shorter list.
        rig.type_text("new").await;
        let state = &rig.state.command_palette;
        assert!(state.selected_index < state.filtered_entries.len());
        assert!(state.scroll_offset < state.filtered_entries.len());
    }
}
