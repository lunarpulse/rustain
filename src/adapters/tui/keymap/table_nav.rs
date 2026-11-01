//! Sidebar(History), Search and BookmarkList rows (Story 21.3 AC5).
//!
//! `Sidebar` rows are the arms common to every sidebar panel; `SidebarHistory`
//! rows are the History panel's own. Panel-specific arms of the other panels
//! (Tasks reorder/arrows/`x p s`, Agents inspector, Room/Artifacts `Enter`,
//! A2A log drill) belong to slice 3-B and are not rows here.
//!
//! The sidebar is a local-only surface (the attached client has no sidebar), so
//! every Sidebar row is `.local()`; Search and BookmarkList are local overlays
//! too. One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the Sidebar and SidebarHistory contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SidebarCmd {
    /// `j`: move the selection down one entry.
    SelectNext,
    /// `k`: move the selection up one entry.
    SelectPrev,
    /// `q`: quit request.
    Quit,
    /// `Esc`: return focus to Chat.
    FocusChat,
    /// `Tab`: move focus to the composer.
    FocusInput,
    /// `Shift+Tab`: previous tab.
    PrevTab,
    /// `Ctrl+T`: new tab.
    NewTab,
    /// `Ctrl+P`: command palette.
    OpenPalette,
    /// `Ctrl+C`: stop active work; when idle, guarded quit.
    CancelOrQuit,
    /// `Alt+V`: paste from the system clipboard.
    PasteClipboard,
    /// History `Enter`: open the selected conversation.
    OpenConversation,
    /// History `d`: delete the selected conversation.
    DeleteConversation,
    /// History `/`: cross-conversation search.
    OpenCrossSearch,
}

/// Commands of the SearchTyping and SearchNavigating contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SearchCmd {
    /// `Esc` (both sub-states): close the overlay, keep the scroll.
    Close,
    /// Typing `Enter`: commit the query and start match navigation.
    Commit,
    /// Typing `Backspace`: pop the last query character and re-scan.
    DeleteBack,
    /// Navigating `Backspace`: return to typing and pop a character.
    DeleteBackToTyping,
    /// `Ctrl+U` (both sub-states): clear the query, back to typing.
    ClearQuery,
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

const fn s(keys: &'static [Key], cmd: SidebarCmd, label: &'static str) -> Binding {
    bind(Context::Sidebar, keys, CommandId::Sidebar(cmd), label)
}

const fn h(keys: &'static [Key], cmd: SidebarCmd, label: &'static str) -> Binding {
    bind(
        Context::SidebarHistory,
        keys,
        CommandId::Sidebar(cmd),
        label,
    )
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

/// One slice (consts cannot be concatenated): Search, then BookmarkList, then
/// Sidebar and SidebarHistory. Search rows come first and keep the shipped help
/// order (Enter, n / N, Ctrl+U, Esc) so the generated Search category reads as
/// it did before; Chat's `m` and `'` follow in `table_chat`.
///
/// Search, typing sub-state: every printable appends to the query (a text
/// context, no command); the rows are the non-printable keys that act.
/// Search, navigating sub-state: `n`/`N` move between matches; any other
/// printable returns to typing and appends itself (not a row: it is the whole
/// printable range).
#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    // ── Search ──
    t(&[Key::Enter], SearchCmd::Commit, "").local().help(SB, "Enter", "Commit query → enable n/N navigation"),
    n(&[Key::Char('n')], SearchCmd::NextMatch, "").local().help(SB, "n / N", "Next / previous match (Navigating mode)"),
    n(&[Key::Char('N')], SearchCmd::PrevMatch, "").local().does("Jump to the previous match"),
    t(&[Key::Ctrl('u')], SearchCmd::ClearQuery, "").local().help(SB, "Ctrl+U", "Clear search query"),
    t(&[Key::Esc], SearchCmd::Close, "").local().help(SB, "Esc (search)", "Close search, preserve scroll"),
    t(&[Key::Ctrl('c')], SearchCmd::Close, "").local().does("Close search and keep the scroll position (back one frame)"),
    t(&[Key::Backspace], SearchCmd::DeleteBack, "").local().does("Delete the last query character and search again"),
    n(&[Key::Esc], SearchCmd::Close, "").local().does("Close search and keep the scroll position"),
    n(&[Key::Ctrl('c')], SearchCmd::Close, "").local().does("Close search and keep the scroll position (back one frame)"),
    n(&[Key::Backspace], SearchCmd::DeleteBackToTyping, "").local().does("Return to typing and delete the last query character"),
    n(&[Key::Ctrl('u')], SearchCmd::ClearQuery, "").local().does("Clear the query and return to typing"),
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
    // ── Sidebar (every panel) ──
    s(&[Key::Char('j')], SidebarCmd::SelectNext, "").local().does("Select the next entry"),
    s(&[Key::Char('k')], SidebarCmd::SelectPrev, "").local().does("Select the previous entry"),
    s(&[Key::Char('q')], SidebarCmd::Quit, "").local().does("Quit; asks first if work or a draft exists"),
    s(&[Key::Esc], SidebarCmd::FocusChat, "").local().does("Return focus to the chat"),
    s(&[Key::Tab], SidebarCmd::FocusInput, "").local().does("Move focus to the input box"),
    s(&[Key::ShiftTab], SidebarCmd::PrevTab, "").local().does("Switch to the previous tab"),
    s(&[Key::Ctrl('t')], SidebarCmd::NewTab, "").local().does("Open a new tab"),
    s(&[Key::Ctrl('p')], SidebarCmd::OpenPalette, "").local().does("Open the command palette"),
    s(&[Key::Ctrl('c')], SidebarCmd::CancelOrQuit, "").local().does("Stop active work; when idle, ask to quit"),
    s(&[Key::Alt('v')], SidebarCmd::PasteClipboard, "").local().alt("palette: paste image from clipboard").does("Paste an image or text from the system clipboard"),
    s(&[Key::Enter], SidebarCmd::OpenConversation, "").local().does("Open the selected entry (the event loop decides): a conversation in History, the inspector in Agents; Tasks, Artifacts, the A2A log and Room have their own Enter"),
    // ── SidebarHistory ──
    h(&[Key::Char('d')], SidebarCmd::DeleteConversation, "").local().does("Delete the selected conversation"),
    h(&[Key::Char('/')], SidebarCmd::OpenCrossSearch, "").local().help(SB, "/ (in sidebar)", "Cross-conversation search"),
];
