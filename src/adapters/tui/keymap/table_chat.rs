//! Chat rows (Story 21.3 AC5): every key arm of Chat focus, both hosts.
//!
//! The rows describe TODAY's dispatch (`app::handle_char` / `handle_special_key`
//! for `FocusState::Chat`, `attach_keys::chat_char` and the attached key chain),
//! one row per (keys, host-set). Two keys with one effect share a command.
//!
//! Sub-states that override a Chat key while they last are NOT rows of this
//! context (they are owned by later slices and named in the 3-A report):
//! task-panel drill-down (`r s p e` and the Up/Down/Enter arms; `c j k g G`
//! are folded into the shared rows below because their meaning is the same
//! "act on the viewed content"), reorder-mode `Esc`, wave `Enter`/`Esc`, and
//! every inline card (`y n e d l x ...`, owned by the Permission/3-C slice).
//!
//! One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{AvailCtx, Availability, Binding, CommandId, Context, HelpCat, Key, bind};

/// Commands of the Chat context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatCmd {
    /// `Esc`: Chat <-> Input focus toggle (never sends or quits).
    FocusToggle,
    /// `i`: focus the composer.
    FocusInput,
    /// `q`: quit request (asks first when work or a draft exists).
    Quit,
    /// `?`: open the help overlay.
    OpenHelp,
    /// `Ctrl+P`: command palette.
    OpenPalette,
    /// `Ctrl+C`: stop active work; when idle, guarded quit.
    CancelOrQuit,
    /// `Ctrl+D`: scroll half a page down.
    ScrollHalfPageDown,
    /// `Ctrl+U`: scroll half a page up.
    ScrollHalfPageUp,
    /// `Ctrl+F` (local) / `PgDn` (attached): scroll a full page down.
    ScrollPageDown,
    /// `Ctrl+B` (local) / `PgUp` (attached): scroll a full page up.
    ScrollPageUp,
    /// `j`: scroll one line toward newer content.
    ScrollLineDown,
    /// `k`: scroll one line toward older content.
    ScrollLineUp,
    /// `g`, `gg`, `z g`: jump to the oldest message.
    ScrollToTop,
    /// `G`: jump to the newest message.
    ScrollToBottom,
    /// `J`: jump to the next content block boundary.
    BlockJumpNext,
    /// `K`: jump to the previous content block boundary.
    BlockJumpPrev,
    /// `{`: jump to the previous user message.
    UserMessagePrev,
    /// `}`: jump to the next user message.
    UserMessageNext,
    /// `Tab`: cycle tool calls, thinking, code blocks and tables in the focused turn (else tab/sidebar cycling).
    CycleInvocation,
    /// `Shift+Tab`: previous tab.
    PrevTab,
    /// `Ctrl+T`: new tab.
    NewTab,
    /// `1`..`9`: switch directly to tab `n`.
    SwitchToTab(u8),
    /// `Alt+V`: paste (image or text) from the system clipboard.
    PasteClipboard,
    /// `Enter`: inspect the render-focused tool in its own frame.
    Inspect,
    /// `c`: copy the focused content (the Copy command, RU-21).
    Copy,
    /// `f`: fork the conversation at the focused message.
    ForkAtMessage,
    /// `R`: rewind the conversation to the focused message.
    RewindAtMessage,
    /// `m`: toggle the bookmark on the focused message.
    ToggleBookmark,
    /// `'`: open the bookmark list.
    OpenBookmarkList,
    /// `z a`: toggle the fold of the focused turn.
    FoldToggle,
    /// `z c`: collapse the focused turn.
    FoldCollapse,
    /// `z o`: expand the focused turn.
    FoldExpand,
    /// `z M`: collapse every turn.
    CollapseAllTurns,
    /// `z R`: expand every turn.
    ExpandAllTurns,
    /// `z s`: toggle the summary tier globally.
    ToggleSummaryTier,
    /// `z z`: recenter on the focused anchor.
    RecenterAnchor,
    /// `] ]`: next assistant-prose turn.
    NextProseAnchor,
    /// `[ [`: previous assistant-prose turn.
    PrevProseAnchor,
    /// `] P`: latest assistant-prose turn.
    LatestProseAnchor,
}

const fn c(keys: &'static [Key], cmd: ChatCmd, label: &'static str) -> Binding {
    bind(Context::Chat, keys, CommandId::Chat(cmd), label)
}

fn stage_target(ctx: &AvailCtx) -> Availability {
    if ctx.stage_target {
        Availability::Available
    } else {
        Availability::Unavailable(
            "Focus a tool call, thinking, code block or table in Chat to inspect it",
        )
    }
}

const N: HelpCat = HelpCat::Navigation;
const S: HelpCat = HelpCat::ScrollMouse;
const V: HelpCat = HelpCat::VimFold;

#[rustfmt::skip]
const CHAT: &[Binding] = &[
    // ── focus, quit, global-ish keys that are live in Chat (both hosts) ──
    c(&[Key::Esc], ChatCmd::FocusToggle, "").help(N, "Esc", "Switch focus / dismiss overlay (never sends or quits)"),
    c(&[Key::Char('i')], ChatCmd::FocusInput, "type").hint().help(N, "i", "Focus input box"),
    c(&[Key::Char('q')], ChatCmd::Quit, "").help(HelpCat::General, "q", "Quit (from chat focus) — asks first if work or a draft exists"),
    c(&[Key::Char('?')], ChatCmd::OpenHelp, "help").hint().help(HelpCat::General, "?", "Toggle this help overlay (Chat focus). In Input `?` types a question mark — use Ctrl+X ?"),
    c(&[Key::Ctrl('p')], ChatCmd::OpenPalette, "").does("Open the command palette"),
    c(&[Key::Ctrl('c')], ChatCmd::CancelOrQuit, "").does("Stop active work; when idle, ask to quit"),
    c(&[Key::Ctrl('d')], ChatCmd::ScrollHalfPageDown, "").help(S, "Ctrl+d", "Scroll half page down"),
    // ── attached transcript paging (the attached host pages on PgUp/PgDn) ──
    c(&[Key::PageUp], ChatCmd::ScrollPageUp, "").attached().does("Scroll the transcript one page up"),
    c(&[Key::PageDown], ChatCmd::ScrollPageDown, "").attached().does("Scroll the transcript one page down"),
    // ── scrolling (local) ──
    c(&[Key::Char('j')], ChatCmd::ScrollLineDown, "scroll").hint().local().help(N, "j / k", "Scroll down / up"),
    c(&[Key::Char('k')], ChatCmd::ScrollLineUp, "scroll").hint().local().does("Scroll up one line"),
    c(&[Key::Ctrl('u')], ChatCmd::ScrollHalfPageUp, "").help(S, "Ctrl+u", "Scroll half page up"),
    c(&[Key::Ctrl('f')], ChatCmd::ScrollPageDown, "").local().help(S, "Ctrl+f", "Scroll full page down (Chat focus — search elsewhere)"),
    c(&[Key::Ctrl('b')], ChatCmd::ScrollPageUp, "").local().help(S, "Ctrl+b", "Scroll full page up"),
    c(&[Key::Char('G')], ChatCmd::ScrollToBottom, "").local().help(V, "G", "Jump to newest message — no effect if already at the bottom"),
    c(&[Key::Char('g')], ChatCmd::ScrollToTop, "").local().help(V, "gg (or g)", "Jump to oldest message (legacy single-g preserved from v1.4)"),
    c(&[Key::Char('J')], ChatCmd::BlockJumpNext, "blocks").hint().local().help(N, "J / K", "Jump to next / previous content block"),
    c(&[Key::Char('K')], ChatCmd::BlockJumpPrev, "blocks").hint().local().does("Jump to the previous content block"),
    c(&[Key::Char('{')], ChatCmd::UserMessagePrev, "").local().help(N, "{ / }", "Jump to previous / next user message"),
    c(&[Key::Char('}')], ChatCmd::UserMessageNext, "").local().does("Jump to the next user message"),
    c(&[Key::Tab], ChatCmd::CycleInvocation, "items").hint().local().help(N, "Tab (Chat)", "Cycle tools, thinking, code, tables (needs 2+); else next tab"),
    // ── vim z-prefix folds, bracket prose-anchor jumps (local sequences) ──
    c(&[Key::Char('z'), Key::Char('a')], ChatCmd::FoldToggle, "").local().help(V, "za", "Toggle fold on focused turn"),
    c(&[Key::Char('z'), Key::Char('c')], ChatCmd::FoldCollapse, "").local().help(V, "zc", "Collapse focused turn"),
    c(&[Key::Char('z'), Key::Char('o')], ChatCmd::FoldExpand, "").local().help(V, "zo", "Expand focused turn"),
    c(&[Key::Char('z'), Key::Char('M')], ChatCmd::CollapseAllTurns, "").local().help(V, "zM", "Collapse all turns (global)"),
    c(&[Key::Char('z'), Key::Char('R')], ChatCmd::ExpandAllTurns, "").local().help(V, "zR", "Expand all turns (global)"),
    c(&[Key::Char('z'), Key::Char('s')], ChatCmd::ToggleSummaryTier, "").local().help(V, "zs", "Toggle summary tier (Tier1/Tier2) globally"),
    c(&[Key::Char('z'), Key::Char('z')], ChatCmd::RecenterAnchor, "").local().help(V, "zz", "Recenter view on focused (sticky-reply) anchor"),
    c(&[Key::Char('z'), Key::Char('g')], ChatCmd::ScrollToTop, "").local().does("Jump to the oldest message"),
    c(&[Key::Char(']'), Key::Char(']')], ChatCmd::NextProseAnchor, "").local().help(V, "]]", "Next assistant-prose turn / Release anchor (when pinned)"),
    c(&[Key::Char('['), Key::Char('[')], ChatCmd::PrevProseAnchor, "").local().help(V, "[[", "Previous assistant-prose turn / Release anchor (when pinned)"),
    c(&[Key::Char(']'), Key::Char('P')], ChatCmd::LatestProseAnchor, "").local().help(V, "]P", "Jump to latest assistant-prose turn"),
    // ── tabs, clipboard ──
    c(&[Key::ShiftTab], ChatCmd::PrevTab, "").local().does("Switch to the previous tab"),
    c(&[Key::Ctrl('t')], ChatCmd::NewTab, "").local().does("Open a new tab"),
    c(&[Key::Char('1')], ChatCmd::SwitchToTab(1), "").local().does("Switch to tab 1"),
    c(&[Key::Char('2')], ChatCmd::SwitchToTab(2), "").local().does("Switch to tab 2"),
    c(&[Key::Char('3')], ChatCmd::SwitchToTab(3), "").local().does("Switch to tab 3"),
    c(&[Key::Char('4')], ChatCmd::SwitchToTab(4), "").local().does("Switch to tab 4"),
    c(&[Key::Char('5')], ChatCmd::SwitchToTab(5), "").local().does("Switch to tab 5"),
    c(&[Key::Char('6')], ChatCmd::SwitchToTab(6), "").local().does("Switch to tab 6"),
    c(&[Key::Char('7')], ChatCmd::SwitchToTab(7), "").local().does("Switch to tab 7"),
    c(&[Key::Char('8')], ChatCmd::SwitchToTab(8), "").local().does("Switch to tab 8"),
    c(&[Key::Char('9')], ChatCmd::SwitchToTab(9), "").local().does("Switch to tab 9"),
    c(&[Key::Alt('v')], ChatCmd::PasteClipboard, "").local().alt("palette: paste image from clipboard").does("Paste an image or text from the system clipboard"),
    c(&[Key::Char('c')], ChatCmd::Copy, "").local().copy().help(HelpCat::Clipboard, "c", "Copy focused content to clipboard"),
    // ── the focused message / tool block ──
    c(&[Key::Enter], ChatCmd::Inspect, "Inspect").hint().local().avail(stage_target).does("Inspect the focused tool call, thinking, code block or table"),
    c(&[Key::Char('f')], ChatCmd::ForkAtMessage, "").local().does("Fork the conversation at the focused message"),
    c(&[Key::Char('R')], ChatCmd::RewindAtMessage, "").local().does("Rewind the conversation to the focused message"),
    c(&[Key::Char('m')], ChatCmd::ToggleBookmark, "").local().help(HelpCat::Search, "m", "Toggle bookmark on current message"),
    c(&[Key::Char('\'')], ChatCmd::OpenBookmarkList, "").local().help(HelpCat::Search, "'", "Open bookmark list panel"),
    // The fan-out wave keys (`d` `s` `r`, live only while a retained wave exists) are rows of ChatWave.
];

pub const ROWS: &[Binding] = CHAT;
