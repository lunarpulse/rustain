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
    /// `Ctrl+Q` while the menu is open: ask to quit without closing it.
    QuitRequest,
    Cancel,
}

/// Attached refusal for sign-in/model chords: the existing CLI remedy.
pub const ATTACHED_SIGN_IN: &str = "Sign-in is not available in an attached session. Run `rustain auth login <provider>` in a terminal; restart the daemon to unlock newly signed-in providers.";
pub const ATTACHED_PROFILE: &str = "Profile switching is not available in an attached session. Run `rustain profile list` or `rustain profile show <name>` in a terminal.";
/// Attached refusal for local sidebar panels; the detach chord is the registry's own.
pub fn attached_panel_remedy() -> &'static str {
    static TEXT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "This panel is local to a standalone session and is not projected to an attached client. Detach ({}) and run `rustain` for the panels, or use `/team status`, `/team board` and `/team log` here.",
            super::render::leader_follow_up_long(LeaderCmd::Detach)
        )
    });
    TEXT.as_str()
}
pub const ATTACHED_DENSITY: &str =
    "Density modes are local to a standalone session; the attached view has a single layout.";
/// Local sidebar panels need the sidebar column (`layout::SIDEBAR_MIN_WIDTH`).
pub const PANEL_NARROW: &str =
    "This panel needs a terminal at least 120 columns wide: widen the window and try again.";

const SIDEBAR_MIN_WIDTH: u16 = 120;

/// Interim ownership remedy, shared by leader rows and nested palette actions.
pub const CLOSE_WINDOW_FIRST: &str = "Please close the window first to open this surface.";

fn base_target(c: &AvailCtx) -> Availability {
    if c.frame_origin {
        Availability::Unavailable(CLOSE_WINDOW_FIRST)
    } else {
        Availability::Available
    }
}

/// Every resolved pane effect declares its presentation target. No wildcard:
/// a new effect cannot silently inherit permission to open an unmigrated surface.
pub fn pane_effect_availability(
    effect: crate::adapters::tui::decision_queue::DecisionEffect,
) -> Availability {
    use crate::adapters::tui::decision_queue::DecisionEffect;
    match effect {
        DecisionEffect::BudgetSwitchCheaper
        | DecisionEffect::RecoveryContinue
        | DecisionEffect::RecoveryNewSession
        | DecisionEffect::CarryoverAccept
        | DecisionEffect::CarryoverDecline
        | DecisionEffect::BudgetContinue
        | DecisionEffect::BudgetPause
        | DecisionEffect::ContextCompact
        | DecisionEffect::AttachLargeImage
        | DecisionEffect::CancelLargeImage
        | DecisionEffect::ConfirmDelete
        | DecisionEffect::PermissionAllowOnce
        | DecisionEffect::PermissionAllowSession
        | DecisionEffect::PermissionAllowAlways
        | DecisionEffect::PermissionDeny
        | DecisionEffect::PermissionDenyFeedback
        | DecisionEffect::ConsentOnce
        | DecisionEffect::ConsentAlways
        | DecisionEffect::ConsentDecline
        | DecisionEffect::CancelDelete => Availability::Available,
    }
}

/// The decision request's honest refusal when nothing waits, shared with its owning menu.
pub fn decision_waiting_remedy() -> &'static str {
    "No decisions waiting"
}

use crate::domain::models::palette::PaletteAction;

/// Every action variant and command argument shape declares its presentation
/// effect here. Unknown extension/skill commands fail closed while nested;
/// adding a PaletteAction variant requires an exhaustive classification.
#[derive(Clone, Copy)]
enum PaletteTarget {
    NoLegacySurface,
    /// Migrated only in the local host; attached interim routes stay unchanged.
    LocalSurface,
    LegacySurface,
    Unclassified,
}

fn palette_command_target(name: &str, args: Option<&str>) -> PaletteTarget {
    use PaletteTarget::{LegacySurface, LocalSurface, NoLegacySurface, Unclassified};
    let args = args.map(str::trim).filter(|args| !args.is_empty());
    let subcommand = args.and_then(|args| args.split_whitespace().next());
    match (name, args) {
        ("login" | "logout" | "accounts" | "models", _) => LocalSurface,
        ("retract-auto" | "cancel-plan" | "reorder-task", _) => LegacySurface,
        ("room" | "artifacts", None) => LegacySurface,
        ("artifact", Some(_)) if matches!(subcommand, Some("apply" | "resolve")) => LegacySurface,
        ("team", Some(_)) if matches!(subcommand, Some("view" | "retract")) => LegacySurface,
        ("peer", Some(_)) if matches!(subcommand, Some("add" | "reach")) => LegacySurface,
        ("memory", Some(_)) if matches!(subcommand, Some("forget" | "consolidate")) => {
            LegacySurface
        }
        ("fanout", Some(_)) => LegacySurface,
        (
            "new" | "clear" | "export" | "deactivate" | "mode" | "plan" | "compact" | "config"
            | "persona" | "memory" | "session" | "tools" | "channels" | "scheduler" | "context"
            | "fanout" | "team" | "peer" | "artifact" | "ml" | "quit" | "notifications" | "detach"
            | "resume-all-tasks",
            None,
        ) => NoLegacySurface,
        ("mode", Some("plan" | "normal" | "autoedit" | "yolo"))
        | ("plan", Some("on" | "off" | "toggle")) => NoLegacySurface,
        ("room" | "artifacts" | "artifact", Some(_))
            if matches!(subcommand, Some("list" | "show")) =>
        {
            NoLegacySurface
        }
        ("team", Some(_)) if matches!(subcommand, Some("status" | "board" | "log")) => {
            NoLegacySurface
        }
        ("peer", Some(_)) if matches!(subcommand, Some("list" | "status" | "log")) => {
            NoLegacySurface
        }
        _ => Unclassified,
    }
}

/// The same frame-origin flag gates keyboard chords and palette handoffs.
/// No default-available arm: dynamic entries need an audited command policy;
/// absent policy (including unrecognized arguments) is unavailable when nested.
pub fn palette_availability(action: &PaletteAction, c: &AvailCtx) -> Availability {
    use PaletteAction::*;
    let target = match action {
        ExecuteCommand(name, args) => palette_command_target(name, args.as_deref()),
        OpenPanel(_) | DeleteAllConversations | PasteImageFromClipboard => {
            PaletteTarget::LegacySurface
        }
        SwitchModel { .. } | SwitchProfile(_) => PaletteTarget::LocalSurface,
        InsertMention(_) | ShowVersion | NewTab | CloseTab | ApplyAdapterOverride { .. } | Noop => {
            PaletteTarget::NoLegacySurface
        }
    };
    match target {
        PaletteTarget::LocalSurface if c.host == Host::Attached => base_target(c),
        PaletteTarget::NoLegacySurface | PaletteTarget::LocalSurface => Availability::Available,
        PaletteTarget::LegacySurface | PaletteTarget::Unclassified => base_target(c),
    }
}

fn attached_profile(c: &AvailCtx) -> Availability {
    local_surface(c, ATTACHED_PROFILE)
}
fn attached_sign_in(c: &AvailCtx) -> Availability {
    local_surface(c, ATTACHED_SIGN_IN)
}
fn attached_density(c: &AvailCtx) -> Availability {
    host_gate(c, ATTACHED_DENSITY)
}
/// A sidebar panel: local only, and only where the sidebar column fits.
fn panel(c: &AvailCtx) -> Availability {
    if c.frame_origin {
        return base_target(c);
    }
    match c.host {
        Host::Attached => Availability::Unavailable(attached_panel_remedy()),
        Host::Local if c.width < SIDEBAR_MIN_WIDTH => Availability::Unavailable(PANEL_NARROW),
        Host::Local => Availability::Available,
    }
}
/// Overlays that are local only but do not need the sidebar column (Usage).
fn attached_panel(c: &AvailCtx) -> Availability {
    local_surface(c, attached_panel_remedy())
}
fn local_surface(c: &AvailCtx, remedy: &'static str) -> Availability {
    match c.host {
        Host::Attached => host_gate(c, remedy),
        Host::Local => Availability::Available,
    }
}
fn host_gate(c: &AvailCtx, remedy: &'static str) -> Availability {
    if c.frame_origin {
        return base_target(c);
    }
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
    l(&[Key::Char('q')], LeaderCmd::Decisions, "Decisions").help(C, "Ctrl+X, Q", "Decisions: open the Needs-you queue (status cue: Needs you)").witness("No decisions waiting"),
    l(&[Key::Enter], LeaderCmd::Send, "Send").help(C, "Ctrl+X, Enter", "Send the composer from any terminal (multi-line safe send)"),
    l(&[Key::Esc], LeaderCmd::Cancel, "Cancel").does("Close the menu without doing anything"),
    l(&[Key::Ctrl('c')], LeaderCmd::Cancel, "").does("Close the menu without doing anything"),
    l(&[Key::Ctrl('q')], LeaderCmd::QuitRequest, "").does("Ask to quit without closing the menu, like Ctrl+Q in any window"),
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
        super::table_input::FIELD,
        super::table_input::TEXT,
        super::table_chat::ROWS,
        super::table_nav::ROWS,
        super::table_panels::ROWS,
        super::table_inspector::ROWS,
        super::table_overlay::ROWS,
        super::table_permission::ROWS,
        super::table_card::ROWS,
        super::table_viewer::ROWS,
        super::table_tool::ROWS,
        super::table_block::ROWS,
    ]
    .concat()
}
