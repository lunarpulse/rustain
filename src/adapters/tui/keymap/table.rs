//! The Global and Leader rows (Story 21.3 A1, AC2) and the assembled table.
//!
//! One row per line on purpose (`rustfmt::skip`): the table is data and is read
//! top to bottom. Context groups own their own files (`table_*.rs`).

use super::model::{AvailCtx, Availability, Binding, CommandId, Context, HelpCat, Host, Key, bind};

/// Commands of the Global context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlobalCmd {
    /// `Ctrl+X`: open the untimed leader menu.
    OpenLeader,
    /// `Ctrl+Q`: ask to quit from any focus (guarded by S-QUIT).
    QuitRequest,
}

/// Commands of the Leader context (the follow-up of `Ctrl+X`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderCmd {
    Profile,
    Model,
    SignIn,
    Adapters,
    Agents,
    A2aLog,
    Room,
    Artifacts,
    Tasks,
    Usage,
    History,
    Monitor,
    Detach,
    Help,
    Notifications,
    ClearToasts,
    Decisions,
    Send,
    Cancel,
}

/// Attached refusal for sign-in/model chords: the existing CLI remedy.
pub const ATTACHED_SIGN_IN: &str = "Sign-in is not available in an attached session. Run `rustain auth login <provider>` in a terminal; restart the daemon to unlock newly signed-in providers.";
pub const ATTACHED_PROFILE: &str = "Profile switching is not available in an attached session. Run `rustain profile list` or `rustain profile show <name>` in a terminal.";
pub const ATTACHED_PANEL: &str = "This panel is local to a standalone session and is not projected to an attached client. Detach (Ctrl+X d) and run `rustain` for the panels, or use `/team status`, `/team board` and `/team log` here.";
pub const ATTACHED_DENSITY: &str =
    "Density modes are local to a standalone session; the attached view has a single layout.";
/// Local sidebar panels need the sidebar column (`layout::SIDEBAR_MIN_WIDTH`).
pub const PANEL_NARROW: &str =
    "This panel needs a terminal at least 120 columns wide: widen the window and try again.";

const SIDEBAR_MIN_WIDTH: u16 = 120;

fn attached_profile(c: &AvailCtx) -> Availability {
    host_gate(c, ATTACHED_PROFILE)
}
fn attached_sign_in(c: &AvailCtx) -> Availability {
    host_gate(c, ATTACHED_SIGN_IN)
}
fn attached_density(c: &AvailCtx) -> Availability {
    host_gate(c, ATTACHED_DENSITY)
}
/// A sidebar panel: local only, and only where the sidebar column fits.
fn panel(c: &AvailCtx) -> Availability {
    match c.host {
        Host::Attached => Availability::Unavailable(ATTACHED_PANEL),
        Host::Local if c.width < SIDEBAR_MIN_WIDTH => Availability::Unavailable(PANEL_NARROW),
        Host::Local => Availability::Available,
    }
}
/// Overlays that are local only but do not need the sidebar column (Usage).
fn attached_panel(c: &AvailCtx) -> Availability {
    host_gate(c, ATTACHED_PANEL)
}
fn host_gate(c: &AvailCtx, remedy: &'static str) -> Availability {
    match c.host {
        Host::Attached => Availability::Unavailable(remedy),
        Host::Local => Availability::Available,
    }
}

const fn g(keys: &'static [Key], cmd: GlobalCmd, label: &'static str) -> Binding {
    bind(Context::Global, keys, CommandId::Global(cmd), label)
}

const fn l(follow_up: &'static [Key], cmd: LeaderCmd, label: &'static str) -> Binding {
    bind(Context::Leader, follow_up, CommandId::Leader(cmd), label)
}

const C: HelpCat = HelpCat::Chords;

#[rustfmt::skip]
const GLOBAL: &[Binding] = &[
    g(&[Key::Ctrl('x')], GlobalCmd::OpenLeader, "menu").te().hint().does("Open the Ctrl+X menu; press a follow-up key, or Esc to cancel"),
    g(&[Key::Ctrl('q')], GlobalCmd::QuitRequest, "Quit").te().slash("/quit").help(HelpCat::General, "Ctrl+Q", "Quit (any focus) — asks first if work or a draft exists"),
];

/// Menu order (21-1/21-2 relative order kept): Detach before Help before
/// Notifications, Clear toasts, Decisions, then Send and Cancel.
#[rustfmt::skip]
const LEADER: &[Binding] = &[
    l(&[Key::Char('p')], LeaderCmd::Profile, "Profile").avail(attached_profile).help(C, "Ctrl+X, P", "Profile switcher").witness("Profile Switcher"),
    l(&[Key::Char('m')], LeaderCmd::Model, "Model").avail(attached_sign_in).help(C, "Ctrl+X, M", "Model selector").witness("Select Model"),
    l(&[Key::Char('i')], LeaderCmd::SignIn, "Sign in").avail(attached_sign_in).help(C, "Ctrl+X, I", "Sign in to a model provider (/login)").witness("Sign in"),
    l(&[Key::Char('a')], LeaderCmd::Adapters, "Adapters").avail(panel).help(C, "Ctrl+X, A", "Adapter Status panel (sidebar)").witness(NARROW_WITNESS),
    l(&[Key::Char('s')], LeaderCmd::Agents, "Agents").avail(panel).help(C, "Ctrl+X, S", "Subagent panel").witness(NARROW_WITNESS),
    l(&[Key::Char('l')], LeaderCmd::A2aLog, "A2A log").slash("/team").avail(a2a_log).help(C, "Ctrl+X, L", "Transparency Log panel").witness(NARROW_WITNESS),
    l(&[Key::Char('r')], LeaderCmd::Room, "Room").slash("/room").avail(panel).help(C, "Ctrl+X, R", "Durable Room panel (read-only replay)").witness(NARROW_WITNESS),
    l(&[Key::Char('e')], LeaderCmd::Artifacts, "Artifacts").slash("/artifacts").avail(panel).help(C, "Ctrl+X, E", "Artifacts panel (Evidence, read-only replay)").witness(NARROW_WITNESS),
    l(&[Key::Char('t')], LeaderCmd::Tasks, "Tasks").avail(panel).help(C, "Ctrl+X, T", "Task panel").witness(NARROW_WITNESS),
    l(&[Key::Char('u')], LeaderCmd::Usage, "Usage").avail(attached_panel).help(C, "Ctrl+X, U", "Usage / cost").witness("Usage & Cost"),
    l(&[Key::Char('h')], LeaderCmd::History, "History").avail(panel).help(C, "Ctrl+X, H", "History panel (past conversations)").witness(NARROW_WITNESS),
    l(&[Key::Char('w')], LeaderCmd::Monitor, "Monitor").avail(attached_density).help(C, "Ctrl+X, W", "Toggle Monitor mode (sidebar + banners); again returns to Focus").witness("Density: Monitor"),
    l(&[Key::Char('d')], LeaderCmd::Detach, "Detach").slash("/detach").attached().help(C, "Ctrl+X, D", "Detach from the daemon (attached client only)"),
    l(&[Key::Char('?')], LeaderCmd::Help, "Help").help(C, "Ctrl+X, ?", "This help overlay").witness("Keybindings"),
    l(&[Key::Char('n')], LeaderCmd::Notifications, "Notifications").slash("/notifications").help(C, "Ctrl+X, N", "Notification Log (also /notifications); Log !N counts unseen warnings and errors").witness("Notifications"),
    l(&[Key::Char('x')], LeaderCmd::ClearToasts, "Clear toasts").help(C, "Ctrl+X, X", "Clear toasts (hides every visible toast; the log keeps them)"),
    l(&[Key::Char('q')], LeaderCmd::Decisions, "Decisions").help(C, "Ctrl+X, Q", "Decisions: focus the oldest waiting decision (status cue: Needs you)").witness("No decisions waiting"),
    l(&[Key::Enter], LeaderCmd::Send, "Send").help(C, "Ctrl+X, Enter", "Send the composer from any terminal (multi-line safe send)"),
    l(&[Key::Esc], LeaderCmd::Cancel, "Cancel").does("Close the menu without doing anything"),
    l(&[Key::Ctrl('c')], LeaderCmd::Cancel, "").does("Close the menu without doing anything"),
];

/// What an 80x24 local screen shows after a sidebar-panel chord (the remedy toast).
const NARROW_WITNESS: &str = "This panel needs a terminal at least 120";

/// `Ctrl+X l` is the attached client's own `/team log` read: available in both hosts,
/// but a local panel needs the sidebar column.
fn a2a_log(c: &AvailCtx) -> Availability {
    match c.host {
        Host::Attached => Availability::Available,
        Host::Local => panel(c),
    }
}

/// Every row of the shipped table, groups concatenated in a fixed order.
pub fn all_rows() -> Vec<Binding> {
    [
        GLOBAL,
        LEADER,
        super::table_input::ROWS,
        super::table_chat::ROWS,
        super::table_nav::ROWS,
        super::table_permission::ROWS,
    ]
    .concat()
}
