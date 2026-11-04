//! Overlay rows (Story 21.3 AC21, KG-2): Help, CommandPalette, ModelSelector,
//! ProfileSwitcher, UsagePanel, CrossSearch, ReverseSearch, Autocomplete,
//! NotificationLog, QuitConfirm, DecisionPane, RecipientSurface and Login.
//!
//! The rows describe TODAY's hand-written arms exactly (3-B2 moves the dispatch
//! under the resolver and keeps every by-value witness green). Printable keys
//! that edit a query are typing, not rows. Hosts: the attached client reaches
//! Help, CommandPalette, Autocomplete, NotificationLog, QuitConfirm and
//! RecipientSurface through the same local handlers (rows `Both`) or its own
//! arms (rows `attached`). `Ctrl+Q` is a quit request in every attached overlay
//! and swallowed by some local ones; each row says which.
//!
//! Named exclusion: the attached RecipientSurface action keys (`recipient_surface_key`:
//! Accept, Busy, Acknowledge, Share/Decline status, Retro share/leave-out/decline,
//! Respond, Discuss, Edit, Use as context) are chosen per received item (`recipient_surface_actions`),
//! so they are not rows; the docs say so and `story21_3_attached_recipient_action_key_sends_its_frame`
//! pins one of them to the frame it sends.
//!
//! One row per line on purpose (`rustfmt::skip`): the table is data.

use super::model::{Binding, CommandId, Context, Key, bind};

/// Commands of the overlay contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[rustfmt::skip]
pub enum OverlayCmd {
    // Help.
    HelpDown, HelpUp, HelpBottom, HelpTop, HelpClose, QuitRequest,
    // In-frame Log/Help search.
    InspectorSearch, InspectorAccept, InspectorBack,
    // Command palette.
    PaletteUp, PaletteDown, PaletteRun, PaletteClose,
    // Model selector.
    ModelProviderPrev, ModelProviderNext, ModelPrev, ModelNext, ModelSelect, ModelBack,
    ModelBackspace, ModelSearch, ModelWarnYes, ModelWarnNo,
    // Profile switcher.
    ProfilePrev, ProfileNext, ProfileEnter, ProfileBack, ProfileYes, ProfileNo,
    // Usage panel.
    UsageClose, UsagePrev, UsageNext,
    // Cross-search.
    CrossClose, CrossOpen, CrossPrev, CrossNext,
    // Reverse search.
    RevAccept, RevClose, RevOlder, RevNewer, RevPalette, RevLeader,
    // Autocomplete popup.
    AutoPrev, AutoNext, AutoAccept, AutoCancel, AutoBackspace,
    // Notification log.
    NotifClose, NotifNext, NotifPrev, NotifDetail, NotifCopy, NotifDismiss, NotifRetry,
    NotifCompact, NotifFresh,
    // Quit confirmation.
    QuitYes, QuitNo,
    // Decision pane.
    PaneQuit, PaneBack, PaneEnter, PaneEsc, PaneYes, PaneNo, PaneSwitch, PanePause, PaneCompact,
    // Recipient surface.
    RecipientPrev, RecipientNext, RecipientClose, RecipientPageUp, RecipientPageDown,
    RecipientToggle,
    // Login.
    LoginBack, LoginEnter, LoginMove, LoginBackspace, LoginCopy,
}

const fn h(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::Help, keys, CommandId::Overlay(cmd), "")
}
const fn p(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::CommandPalette, keys, CommandId::Overlay(cmd), "")
}
const fn m(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::ModelSelector, keys, CommandId::Overlay(cmd), "").local()
}
const fn f(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::ProfileSwitcher, keys, CommandId::Overlay(cmd), "").local()
}
const fn u(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::UsagePanel, keys, CommandId::Overlay(cmd), "").local()
}
const fn x(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::CrossSearch, keys, CommandId::Overlay(cmd), "").local()
}
const fn r(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::ReverseSearch, keys, CommandId::Overlay(cmd), "").local()
}
const fn a(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::Autocomplete, keys, CommandId::Overlay(cmd), "")
}
const fn n(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::NotificationLog, keys, CommandId::Overlay(cmd), "")
}
const fn q(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::QuitConfirm, keys, CommandId::Overlay(cmd), "")
}
const fn d(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::DecisionPane, keys, CommandId::Overlay(cmd), "").local()
}
const fn s(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::RecipientSurface, keys, CommandId::Overlay(cmd), "")
}
const fn g(keys: &'static [Key], cmd: OverlayCmd) -> Binding {
    bind(Context::Login, keys, CommandId::Overlay(cmd), "").local()
}

#[rustfmt::skip]
pub const ROWS: &[Binding] = &[
    bind(Context::Help, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").does("Search help keybindings"),
    bind(Context::NotificationLog, &[Key::Char('/')], CommandId::Overlay(OverlayCmd::InspectorSearch), "Search").does("Search notification severity, source and message"),
    bind(Context::InspectorSearch, &[Key::Enter], CommandId::Overlay(OverlayCmd::InspectorAccept), "Keep filter").te().does("Keep the filter and return to the list"),
    bind(Context::InspectorSearch, &[Key::Esc], CommandId::Overlay(OverlayCmd::InspectorBack), "Clear").te().does("Clear the filter and leave the search field"),
    bind(Context::InspectorSearch, &[Key::Ctrl('c')], CommandId::Overlay(OverlayCmd::InspectorBack), "Clear").te().does("Clear the filter and leave the search field"),
    bind(Context::InspectorSearch, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").te().does("Ask to quit without closing the search field"),
    bind(Context::InspectorSearch, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").te().does("Open the menu above this search field"),
    bind(Context::InspectorSearch, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").te().does("Open the command palette above this search field"),
    bind(Context::Help, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").does("Open the menu above this frame"),
    bind(Context::Help, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").does("Open the command palette above this frame"),
    bind(Context::CommandPalette, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").does("Open the menu above this frame"),
    bind(Context::CommandPalette, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").does("Return to this command palette"),
    bind(Context::NotificationLog, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").does("Open the menu above this frame"),
    bind(Context::NotificationLog, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").does("Open the command palette above this frame"),
    bind(Context::DecisionPane, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::DecisionPane, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::ModelSelector, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::ModelSelector, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::ProfileSwitcher, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::ProfileSwitcher, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::UsagePanel, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::UsagePanel, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::Login, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::Login, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::SearchTyping, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::SearchTyping, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::SearchNavigating, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::SearchNavigating, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::BookmarkList, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::BookmarkList, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::WaveOverlay, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::WaveOverlay, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    bind(Context::CrossSearch, &[Key::Ctrl('x')], CommandId::Global(super::table::GlobalCmd::OpenLeader), "menu").local().does("Open the menu above this frame"),
    bind(Context::CrossSearch, &[Key::Ctrl('p')], CommandId::Input(super::table_input::InputCmd::OpenPalette), "").local().does("Open the command palette above this frame"),
    // Quit preserves both migrated frames and the remaining legacy windows.
    bind(Context::ModelSelector, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing the model selector"),
    bind(Context::ProfileSwitcher, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing the profile switcher"),
    bind(Context::UsagePanel, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing usage"),
    bind(Context::CrossSearch, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing cross-conversation search"),
    bind(Context::ReverseSearch, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing history search"),
    bind(Context::SearchTyping, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing search"),
    bind(Context::SearchNavigating, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing search"),
    bind(Context::BookmarkList, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing bookmarks"),
    bind(Context::WaveOverlay, &[Key::Ctrl('q')], CommandId::Overlay(OverlayCmd::QuitRequest), "").local().does("Ask to quit without closing waves"),
    // ── Help ──
    h(&[Key::Char('j')], OverlayCmd::HelpDown).does("Scroll the help down one line"),
    h(&[Key::Down], OverlayCmd::HelpDown).does("Scroll the help down one line"),
    h(&[Key::Char('k')], OverlayCmd::HelpUp).does("Scroll the help up one line"),
    h(&[Key::Up], OverlayCmd::HelpUp).does("Scroll the help up one line"),
    h(&[Key::Char('G')], OverlayCmd::HelpBottom).does("Scroll the help to the bottom"),
    h(&[Key::Char('g')], OverlayCmd::HelpTop).does("Scroll the help to the top"),
    h(&[Key::Char('?')], OverlayCmd::HelpClose).does("Close the help"),
    h(&[Key::Esc], OverlayCmd::HelpClose).does("Close the help"),
    h(&[Key::Ctrl('c')], OverlayCmd::HelpClose).does("Close the help (back one frame); it never stops a turn or quits"),
    h(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit without closing help"),
    // ── Command palette (shared caret-field editing rows) ──
    p(&[Key::Up], OverlayCmd::PaletteUp).does("Select the previous entry"),
    p(&[Key::Down], OverlayCmd::PaletteDown).does("Select the next entry"),
    p(&[Key::Enter], OverlayCmd::PaletteRun).does("Run the selected entry"),
    p(&[Key::Esc], OverlayCmd::PaletteClose).does("Close the palette"),
    p(&[Key::Ctrl('c')], OverlayCmd::PaletteClose).does("Close the palette (back one frame)"),
    p(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit without closing the palette"),
    // ── Model selector ──
    m(&[Key::Left], OverlayCmd::ModelProviderPrev).does("Select the previous provider column"),
    m(&[Key::Char('h')], OverlayCmd::ModelProviderPrev).does("Select the previous provider column"),
    m(&[Key::Right], OverlayCmd::ModelProviderNext).does("Select the next provider column"),
    m(&[Key::Char('l')], OverlayCmd::ModelProviderNext).does("Select the next provider column"),
    m(&[Key::Up], OverlayCmd::ModelPrev).does("Select the previous model"),
    m(&[Key::Char('k')], OverlayCmd::ModelPrev).does("Select the previous model (not while searching)"),
    m(&[Key::Down], OverlayCmd::ModelNext).does("Select the next model"),
    m(&[Key::Char('j')], OverlayCmd::ModelNext).does("Select the next model (not while searching)"),
    m(&[Key::Enter], OverlayCmd::ModelSelect).does("Switch to the selected model; on a locked provider, start its sign-in"),
    m(&[Key::Esc], OverlayCmd::ModelBack).does("Leave the search, else close the selector"),
    m(&[Key::Ctrl('c')], OverlayCmd::ModelBack).does("Leave the search, else close the selector (back one frame)"),
    m(&[Key::Backspace], OverlayCmd::ModelBackspace).does("Delete the last search character"),
    m(&[Key::Char('/')], OverlayCmd::ModelSearch).does("Search the models"),
    m(&[Key::Char('y')], OverlayCmd::ModelWarnYes).does("Compact the conversation, then switch (only while the context warning is shown)"),
    m(&[Key::Char('n')], OverlayCmd::ModelWarnNo).does("Dismiss the context warning (only while it is shown)"),
    // ── Profile switcher ──
    f(&[Key::Up], OverlayCmd::ProfilePrev).does("Select the previous profile"),
    f(&[Key::Down], OverlayCmd::ProfileNext).does("Select the next profile"),
    f(&[Key::Enter], OverlayCmd::ProfileEnter).does("Preview the selected profile; in the preview, switch to it"),
    f(&[Key::Esc], OverlayCmd::ProfileBack).does("Leave the preview, else close the switcher"),
    f(&[Key::Ctrl('c')], OverlayCmd::ProfileBack).does("Leave the preview, else close the switcher (back one frame)"),
    f(&[Key::Char('y')], OverlayCmd::ProfileYes).does("Switch to the previewed profile (only in the preview)"),
    f(&[Key::Char('n')], OverlayCmd::ProfileNo).does("Leave the preview (only in the preview)"),
    // ── Usage panel ──
    u(&[Key::Esc], OverlayCmd::UsageClose).does("Close the usage panel"),
    u(&[Key::Ctrl('c')], OverlayCmd::UsageClose).does("Close the usage panel (back one frame)"),
    u(&[Key::Up], OverlayCmd::UsagePrev).does("Select the previous section"),
    u(&[Key::Down], OverlayCmd::UsageNext).does("Select the next section"),
    // ── Cross-search (shared caret editor; j and k still move, never type) ──
    x(&[Key::Esc], OverlayCmd::CrossClose).does("Close the cross-conversation search"),
    x(&[Key::Ctrl('c')], OverlayCmd::CrossClose).does("Close the cross-conversation search (back one frame)"),
    x(&[Key::Enter], OverlayCmd::CrossOpen).does("Open the selected result"),
    x(&[Key::Up], OverlayCmd::CrossPrev).does("Select the previous result"),
    x(&[Key::Char('k')], OverlayCmd::CrossPrev).does("Select the previous result (k is never typed into the query)"),
    x(&[Key::Down], OverlayCmd::CrossNext).does("Select the next result"),
    x(&[Key::Char('j')], OverlayCmd::CrossNext).does("Select the next result (j is never typed into the query)"),
    // ── Reverse search (shared caret editor) ──
    r(&[Key::Enter], OverlayCmd::RevAccept).does("Put the selected history entry in the composer"),
    r(&[Key::Esc], OverlayCmd::RevClose).does("Close the history search"),
    r(&[Key::Ctrl('c')], OverlayCmd::RevClose).does("Close the history search (back one frame)"),
    r(&[Key::Up], OverlayCmd::RevOlder).does("Select the next older match"),
    r(&[Key::Ctrl('r')], OverlayCmd::RevOlder).does("Select the next older match"),
    r(&[Key::Down], OverlayCmd::RevNewer).does("Select the next newer match"),
    r(&[Key::Ctrl('p')], OverlayCmd::RevPalette).does("Close the history search and open the command palette"),
    r(&[Key::Ctrl('x')], OverlayCmd::RevLeader).does("Close the history search and open the Ctrl+X menu"),
    // ── Autocomplete popup (any other key dismisses it and then acts normally) ──
    a(&[Key::Up], OverlayCmd::AutoPrev).does("Select the previous suggestion"),
    a(&[Key::Down], OverlayCmd::AutoNext).does("Select the next suggestion"),
    a(&[Key::Tab], OverlayCmd::AutoAccept).does("Accept the selected suggestion"),
    a(&[Key::Enter], OverlayCmd::AutoAccept).does("Accept the selected suggestion"),
    a(&[Key::Esc], OverlayCmd::AutoCancel).does("Close the popup and restore the composer"),
    a(&[Key::Ctrl('c')], OverlayCmd::AutoCancel).does("Close the popup and restore the composer (back one frame)"),
    a(&[Key::Backspace], OverlayCmd::AutoBackspace).does("Delete the last filter character; at the trigger, close the popup"),
    a(&[Key::Ctrl('h')], OverlayCmd::AutoBackspace).local().does("Delete the last filter character, like Backspace"),
    a(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit without closing the popup"),
    // ── Notification log ──
    n(&[Key::Esc], OverlayCmd::NotifClose).does("Close the notification log"),
    n(&[Key::Ctrl('c')], OverlayCmd::NotifClose).does("Close the notification log (back one frame)"),
    n(&[Key::Char('j')], OverlayCmd::NotifNext).does("Select the next record; scroll the details when open"),
    n(&[Key::Down], OverlayCmd::NotifNext).does("Select the next record; scroll the details when open"),
    n(&[Key::Char('k')], OverlayCmd::NotifPrev).does("Select the previous record; scroll the details when open"),
    n(&[Key::Up], OverlayCmd::NotifPrev).does("Select the previous record; scroll the details when open"),
    n(&[Key::Enter], OverlayCmd::NotifDetail).does("Open or close the selected record's details"),
    n(&[Key::Char('c')], OverlayCmd::NotifCopy).copy().does("Copy the selected record's message"),
    n(&[Key::Char('x')], OverlayCmd::NotifDismiss).does("Dismiss the selected record"),
    n(&[Key::Char('r')], OverlayCmd::NotifRetry).local().does("Retry the selected record's action (when it has one)"),
    n(&[Key::Char('K')], OverlayCmd::NotifCompact).local().does("Compact the conversation (context warnings)"),
    n(&[Key::Char('n')], OverlayCmd::NotifFresh).local().does("Start fresh with a summary (context warnings)"),
    n(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit (guarded by the quit confirmation)"),
    // ── Quit confirmation ──
    q(&[Key::Char('y')], OverlayCmd::QuitYes).does("Quit now"),
    q(&[Key::Char('Y')], OverlayCmd::QuitYes).does("Quit now"),
    q(&[Key::Char('n')], OverlayCmd::QuitNo).does("Stay"),
    q(&[Key::Char('N')], OverlayCmd::QuitNo).does("Stay"),
    q(&[Key::Esc], OverlayCmd::QuitNo).does("Stay"),
    q(&[Key::Ctrl('c')], OverlayCmd::QuitNo).does("Stay (back one frame)"),
    q(&[Key::Enter], OverlayCmd::QuitNo).does("Stay"),
    // ── Decision pane (the key letters depend on the waiting decision's kind) ──
    d(&[Key::Ctrl('c')], OverlayCmd::PaneBack).does("Back one frame without deciding; leave the item waiting"),
    d(&[Key::Ctrl('q')], OverlayCmd::PaneQuit).does("Ask to quit (guarded by the quit confirmation)"),
    d(&[Key::Enter], OverlayCmd::PaneEnter).does("Continue (a recovery decision)"),
    d(&[Key::Esc], OverlayCmd::PaneEsc).does("Decide the safe default, or defer a deferrable decision"),
    d(&[Key::Char('y')], OverlayCmd::PaneYes).does("Accept, continue or confirm (by the decision's kind)"),
    d(&[Key::Char('n')], OverlayCmd::PaneNo).does("Decline, cancel or start fresh (by the decision's kind)"),
    d(&[Key::Char('s')], OverlayCmd::PaneSwitch).does("Switch to a cheaper model (a budget decision)"),
    d(&[Key::Char('p')], OverlayCmd::PanePause).does("Pause (a budget decision)"),
    d(&[Key::Char('k')], OverlayCmd::PaneCompact).does("Compact the conversation (a context warning)"),
    // ── Recipient surface ──
    s(&[Key::Char('p')], OverlayCmd::RecipientPrev).does("Show the earlier part of the thread"),
    s(&[Key::Char('l')], OverlayCmd::RecipientNext).does("Show the later part of the thread"),
    s(&[Key::Esc], OverlayCmd::RecipientClose).does("Close the recipient view"),
    s(&[Key::Ctrl('c')], OverlayCmd::RecipientClose).does("Close the recipient view (back one frame)"),
    s(&[Key::PageUp], OverlayCmd::RecipientPageUp).does("Scroll the recipient view up one page"),
    s(&[Key::PageDown], OverlayCmd::RecipientPageDown).does("Scroll the recipient view down one page"),
    s(&[Key::Tab], OverlayCmd::RecipientToggle).attached().does("Switch between the item and the node view"),
    s(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit (guarded by the quit confirmation)"),
    // ── Login (the overlay keeps handlers::login::on_key; rows only) ──
    g(&[Key::Esc], OverlayCmd::LoginBack).does("Clear the search; go back a step; close the overlay"),
    g(&[Key::Ctrl('c')], OverlayCmd::LoginBack).does("Clear the search; go back a step; close the overlay (back one frame)"),
    g(&[Key::Enter], OverlayCmd::LoginEnter).does("Choose the selected provider or method"),
    g(&[Key::Up], OverlayCmd::LoginMove).does("Select the previous row"),
    g(&[Key::Down], OverlayCmd::LoginMove).does("Select the next row"),
    g(&[Key::PageUp], OverlayCmd::LoginMove).does("Move the selection up one page"),
    g(&[Key::PageDown], OverlayCmd::LoginMove).does("Move the selection down one page"),
    g(&[Key::Backspace], OverlayCmd::LoginBackspace).does("Delete the last search or field character"),
    g(&[Key::Ctrl('q')], OverlayCmd::QuitRequest).does("Ask to quit (guarded by the quit confirmation)"),
    g(&[Key::Alt('c')], OverlayCmd::LoginCopy).alt("c while no field has focus").does("Copy the sign-in link or code"),
    g(&[Key::Alt('l')], OverlayCmd::LoginCopy).alt("l while no field has focus").does("Copy the launch link"),
    g(&[Key::Alt('u')], OverlayCmd::LoginCopy).alt("u while no field has focus").does("Copy the verification link"),
    g(&[Key::Char('c')], OverlayCmd::LoginCopy).copy().does("Copy the sign-in link or code (only while no field has focus)"),
    g(&[Key::Char('l')], OverlayCmd::LoginCopy).does("Copy the launch link (only while no field has focus)"),
    g(&[Key::Char('u')], OverlayCmd::LoginCopy).does("Copy the verification link (only while no field has focus)"),
];
