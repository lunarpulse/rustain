//! Search and BookmarkList rows (Story 21.3 AC5).
//!
//! The History sidebar panel and its `Sidebar` rows are gone (Story 21.7 7-E): History is a
//! frame of its own with its rows in `table_mission`. Search and BookmarkList are local frames.
//! One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the SearchTyping and SearchNavigating contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchCmd {
    /// `Esc` (both sub-states): close the frame and restore the pre-search view.
    Close,
    /// `Enter`: commit a typed query, or keep the position and close navigation.
    Commit,
    /// Navigating `n`: next match.
    NextMatch,
    /// Navigating `N`: previous match.
    PrevMatch,
}

/// Commands of the BookmarkList context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BookmarkCmd {
    /// `j` / `Down`: select the next bookmark.
    SelectNext,
    /// `k` / `Up`: select the previous bookmark.
    SelectPrev,
    /// `d` / `Del` / `Backspace`: delete the selected bookmark.
    Delete,
    /// `u`: undo the last delete (within five seconds).
    Undo,
    /// `Enter`: jump to the selected bookmark.
    Jump,
    /// `Esc`: close the list.
    Close,
}

const fn t(keys: &'static [Key], cmd: SearchCmd, label: &'static str) -> Binding {
    bind(Context::SearchTyping, keys, CommandId::Search(cmd), label)
}

const fn n(keys: &'static [Key], cmd: SearchCmd, label: &'static str) -> Binding {
    bind(
        Context::SearchNavigating,
        keys,
        CommandId::Search(cmd),
        label,
    )
}

const fn b(keys: &'static [Key], cmd: BookmarkCmd, label: &'static str) -> Binding {
    bind(Context::BookmarkList, keys, CommandId::Bookmark(cmd), label)
}

const SB: HelpCat = HelpCat::Search;

/// One slice (consts cannot be concatenated): Search, then BookmarkList. Search rows come
/// first and keep the shipped help
/// order (Enter, n / N, Ctrl+U, Esc) so the generated Search category reads as
/// it did before; Chat's `m` and `'` follow in `table_chat`.
///
/// Search Typing is a caret field; Navigating keeps `n`/`N` match navigation.
/// Other printables and editing motions return to Typing through the shared field.
#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── Search ──
    t(&[Key::Enter], SearchCmd::Commit, "").local().help(SB, "Enter", "Commit query → enable n/N navigation"),
    n(&[Key::Enter], SearchCmd::Commit, "").local().help(SB, "Enter (navigating)", "Keep position and close search"),
    n(&[Key::Char('n')], SearchCmd::NextMatch, "").local().help(SB, "n / N", "Next / previous match (Navigating mode)"),
    n(&[Key::Char('N')], SearchCmd::PrevMatch, "").local().does("Jump to the previous match"),
    bind(Context::SearchTyping, &[Key::Ctrl('u')], CommandId::Field(super::table_input::FieldCmd::Clear), "").local().help(SB, "Ctrl+U", "Delete search query to the caret"),
    t(&[Key::Esc], SearchCmd::Close, "").local().help(SB, "Esc (search)", "Close search, restore pre-search view"),
    t(&[Key::Ctrl('c')], SearchCmd::Close, "").local().does("Close search and restore the pre-search view (back one frame)"),
    n(&[Key::Esc], SearchCmd::Close, "").local().does("Close search and restore the pre-search view"),
    n(&[Key::Ctrl('c')], SearchCmd::Close, "").local().does("Close search and restore the pre-search view (back one frame)"),
    // ── BookmarkList ──
    b(&[Key::Char('d')], BookmarkCmd::Delete, "").local().help(SB, "d / Del / Backspace", "Delete selected bookmark (in list)"),
    b(&[Key::Delete], BookmarkCmd::Delete, "").local().does("Delete the selected bookmark"),
    b(&[Key::Backspace], BookmarkCmd::Delete, "").local().does("Delete the selected bookmark"),
    b(&[Key::Char('u')], BookmarkCmd::Undo, "").local().help(SB, "u (in list)", "Undo last bookmark delete (within 5s)"),
    b(&[Key::Char('j')], BookmarkCmd::SelectNext, "").local().does("Select the next bookmark"),
    b(&[Key::Down], BookmarkCmd::SelectNext, "").local().does("Select the next bookmark"),
    b(&[Key::Char('k')], BookmarkCmd::SelectPrev, "").local().does("Select the previous bookmark"),
    b(&[Key::Up], BookmarkCmd::SelectPrev, "").local().does("Select the previous bookmark"),
    b(&[Key::Enter], BookmarkCmd::Jump, "").local().does("Jump to the selected bookmark"),
    b(&[Key::Esc], BookmarkCmd::Close, "").local().does("Close the bookmark list"),
    b(&[Key::Ctrl('c')], BookmarkCmd::Close, "").local().does("Close the bookmark list (back one frame)"),
];
