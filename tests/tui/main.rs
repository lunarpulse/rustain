//! Area test binary `tui` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 44 former `tests/*.rs` roots, compiled and linked once instead of 44 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[allow(dead_code, unused_imports)]
#[path = "../common/mod.rs"]
mod common;

#[path = "../common/render_helpers.rs"]
mod render_helpers;

#[path = "../chat_pane.rs"]
mod chat_pane;

#[path = "../command_palette.rs"]
mod command_palette;

#[path = "../slash_argument_parsing.rs"]
mod slash_argument_parsing;

#[path = "../conformance_19_1_line_diff.rs"]
mod conformance_19_1_line_diff;

#[path = "../conformance_density_mode.rs"]
mod conformance_density_mode;

#[path = "../conformance_tui_orchestration.rs"]
mod conformance_tui_orchestration;

#[path = "../conformance_vim_keymap_conflicts.rs"]
mod conformance_vim_keymap_conflicts;

#[path = "../conformance_vim_motion_outcomes.rs"]
mod conformance_vim_motion_outcomes;

#[path = "../e2e_autocomplete.rs"]
mod e2e_autocomplete;

#[path = "../e2e_bookmarks.rs"]
mod e2e_bookmarks;

#[path = "../e2e_clipboard.rs"]
mod e2e_clipboard;

#[path = "../e2e_command_palette.rs"]
mod e2e_command_palette;

#[path = "../e2e_compaction.rs"]
mod e2e_compaction;

#[path = "../e2e_crash_recovery.rs"]
mod e2e_crash_recovery;

#[path = "../e2e_cross_search.rs"]
mod e2e_cross_search;

#[path = "../e2e_event_bus.rs"]
mod e2e_event_bus;

#[path = "../e2e_export.rs"]
mod e2e_export;

#[path = "../e2e_export_filesystem.rs"]
mod e2e_export_filesystem;

#[path = "../e2e_filesystem.rs"]
mod e2e_filesystem;

#[path = "../e2e_fork.rs"]
mod e2e_fork;

#[path = "../e2e_fork_sidebar.rs"]
mod e2e_fork_sidebar;

#[path = "../e2e_harness.rs"]
pub mod e2e_harness;

#[path = "../e2e_help_overlay.rs"]
mod e2e_help_overlay;

#[path = "../e2e_history_import.rs"]
mod e2e_history_import;

#[path = "../e2e_image_persistence.rs"]
mod e2e_image_persistence;

#[path = "../e2e_input_history.rs"]
mod e2e_input_history;

#[path = "../e2e_markdown.rs"]
mod e2e_markdown;

#[path = "../e2e_model_switcher.rs"]
mod e2e_model_switcher;

#[path = "../e2e_multi_tab.rs"]
mod e2e_multi_tab;

#[path = "../e2e_rewind.rs"]
mod e2e_rewind;

#[path = "../e2e_search.rs"]
mod e2e_search;

#[path = "../e2e_sidebar.rs"]
mod e2e_sidebar;

#[path = "../help_overlay.rs"]
mod help_overlay;

#[path = "../input_box.rs"]
mod input_box;

#[path = "../input_history.rs"]
mod input_history;

#[path = "../render_interleaved_parts.rs"]
mod render_interleaved_parts;

#[path = "../render_snapshots.rs"]
mod render_snapshots;

#[path = "../scroll.rs"]
mod scroll;

#[path = "../snapshot_semantic_clusters.rs"]
mod snapshot_semantic_clusters;

#[path = "../snapshot_tier_divergence.rs"]
mod snapshot_tier_divergence;

#[path = "../status_bar.rs"]
mod status_bar;

#[path = "../theme.rs"]
mod theme;

#[path = "../tui_render.rs"]
mod tui_render;

#[path = "../virtual_scroll.rs"]
mod virtual_scroll;

#[path = "../widget_in_context.rs"]
mod widget_in_context;
