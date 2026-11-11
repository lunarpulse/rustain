//! Keymap registry types (Story 21.3 A1/A2): keys, contexts, rows, availability.
//!
//! Pure data and small pure functions: no I/O, no terminal, no `TuiState`.
//! Both hosts (`app::handle_input`, `attach_keys::handle_attached_key`) and every
//! discovery surface (help, which-key, palette, status hints, docs) read the
//! same rows.

use crate::domain::events::{DomainInputEvent, DomainKey};
use crate::domain::models::visual::DensityMode;

use super::table::{GlobalCmd, LeaderCmd};
use super::table_block::{BlockCmd, BodyCmd};
use super::table_card::CardCmd;
use super::table_chat::ChatCmd;
use super::table_input::{FieldCmd, InputCmd};
use super::table_inspector::InspectorCmd;
use super::table_mission::MissionCmd;
use super::table_nav::{BookmarkCmd, SearchCmd, SidebarCmd};
use super::table_overlay::OverlayCmd;
use super::table_panels::PanelCmd;
use super::table_permission::PermissionCmd;
use super::table_thread::ThreadCmd;
use super::table_tool::ToolCmd;
use crate::adapters::tui::presentation::viewer::ViewerCmd;

// ── keys ──────────────────────────────────────────────────────────────────

/// One normalized key press, host independent. Ctrl and Alt letters are always
/// lower case; `Char` is the character the terminal produced (`J` stays `J`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Enter,
    ShiftEnter,
    CtrlEnter,
    AltEnter,
    Esc,
    Tab,
    ShiftTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
    /// A character held with more than one modifier (Ctrl+Shift, Alt+Shift, Ctrl+Alt):
    /// bits 1 = Ctrl, 2 = Alt, 4 = Shift. Never equal to the plain Ctrl/Alt key (AC20).
    Combo(char, u8),
    /// Any key the registry has no name for (Insert, media keys, …).
    Other,
}

/// How a key travels through a terminal (RU-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Has canonical PTY bytes every terminal sends.
    Portable,
    /// Needs a keyboard protocol most terminals do not speak (Ctrl/Shift+Enter).
    ExtendedKeyboard,
    /// Alt-as-meta (`ESC` prefix): terminal-dependent.
    AltMeta,
    /// Not declared: rejected by `validate`.
    Unspecified,
}

const fn ascii_table() -> [u8; 128] {
    let mut table = [0u8; 128];
    let mut i = 0;
    while i < 128 {
        table[i] = i as u8;
        i += 1;
    }
    table
}

const fn ctrl_table() -> [u8; 26] {
    let mut table = [0u8; 26];
    let mut i = 0;
    while i < 26 {
        table[i] = (i + 1) as u8;
        i += 1;
    }
    table
}

const fn alt_table() -> [[u8; 2]; 26] {
    let mut table = [[0x1b, 0]; 26];
    let mut i = 0;
    while i < 26 {
        table[i][1] = b'a' + i as u8;
        i += 1;
    }
    table
}

static ASCII: [u8; 128] = ascii_table();
static CTRL: [u8; 26] = ctrl_table();
static ALT: [[u8; 2]; 26] = alt_table();

impl Key {
    /// Reachability class of this key alone.
    pub const fn reach(self) -> Reach {
        match self {
            Key::ShiftEnter | Key::CtrlEnter => Reach::ExtendedKeyboard,
            Key::Alt(_) | Key::AltEnter => Reach::AltMeta,
            _ => Reach::Portable,
        }
    }

    /// Canonical PTY bytes of a portable key (`0x18` for Ctrl+X, `0x1b` for Esc).
    pub const fn pty(self) -> Option<&'static [u8]> {
        match self {
            Key::Char(c) if (c as u32) < 128 => Some(std::slice::from_ref(&ASCII[c as usize])),
            Key::Ctrl(c) if c.is_ascii_lowercase() => {
                Some(std::slice::from_ref(&CTRL[(c as u8 - b'a') as usize]))
            }
            Key::Alt(c) if c.is_ascii_lowercase() => Some(&ALT[(c as u8 - b'a') as usize]),
            Key::Enter => Some(b"\r"),
            Key::Esc => Some(b"\x1b"),
            Key::Tab => Some(b"\t"),
            Key::Backspace => Some(b"\x7f"),
            Key::ShiftTab => Some(b"\x1b[Z"),
            Key::Up => Some(b"\x1b[A"),
            Key::Down => Some(b"\x1b[B"),
            Key::Right => Some(b"\x1b[C"),
            Key::Left => Some(b"\x1b[D"),
            Key::Home => Some(b"\x1b[H"),
            Key::End => Some(b"\x1b[F"),
            Key::PageUp => Some(b"\x1b[5~"),
            Key::PageDown => Some(b"\x1b[6~"),
            Key::Delete => Some(b"\x1b[3~"),
            _ => None,
        }
    }

    /// Long label used by help, the palette shortcut column and prose.
    pub fn label(self) -> String {
        match self {
            Key::Char(' ') => "Space".to_owned(),
            Key::Char(c) => c.to_string(),
            Key::Ctrl(c) => format!("Ctrl+{}", c.to_ascii_uppercase()),
            Key::Alt(c) => format!("Alt+{}", c.to_ascii_uppercase()),
            Key::Enter => "Enter".into(),
            Key::ShiftEnter => "Shift+Enter".into(),
            Key::CtrlEnter => "Ctrl+Enter".into(),
            Key::AltEnter => "Alt+Enter".into(),
            Key::Esc => "Esc".into(),
            Key::Tab => "Tab".into(),
            Key::ShiftTab => "Shift+Tab".into(),
            Key::Backspace => "Backspace".into(),
            Key::Delete => "Del".into(),
            Key::Up => "Up".into(),
            Key::Down => "Down".into(),
            Key::Left => "Left".into(),
            Key::Right => "Right".into(),
            Key::Home => "Home".into(),
            Key::End => "End".into(),
            Key::PageUp => "PgUp".into(),
            Key::PageDown => "PgDn".into(),
            Key::F(n) => format!("F{n}"),
            Key::Combo(c, mask) => {
                let names = [(1, "Ctrl"), (2, "Alt"), (4, "Shift")];
                let held = names.iter().filter(|(bit, _)| mask & bit != 0);
                let mut label: String = held.map(|(_, name)| format!("{name}+")).collect();
                label.push(c.to_ascii_uppercase());
                label
            }
            Key::Other => "this key".into(),
        }
    }

    /// Compact label for the status row and rails: `^X`, otherwise the long label.
    pub fn compact(self) -> String {
        match self {
            Key::Ctrl(c) => format!("^{}", c.to_ascii_uppercase()),
            other => other.label(),
        }
    }

    /// The registry's matching form: Ctrl/Alt letters lower case; `Char` lower
    /// cased too when `fold_case` (leader follow-ups, A2).
    pub fn normalized(self, fold_case: bool) -> Key {
        match self {
            Key::Ctrl(c) => Key::Ctrl(c.to_ascii_lowercase()),
            Key::Alt(c) => Key::Alt(c.to_ascii_lowercase()),
            Key::Char(c) if fold_case => Key::Char(c.to_ascii_lowercase()),
            other => other,
        }
    }

    /// Local host decoding: a `DomainInputEvent` that is a key press.
    pub fn from_domain(event: &DomainInputEvent) -> Option<Key> {
        match event {
            DomainInputEvent::KeyPress(c) => Some(Key::Char(*c)),
            DomainInputEvent::SpecialKey(key) => Some(Key::from_domain_key(*key)),
            _ => None,
        }
    }

    pub fn from_domain_key(key: DomainKey) -> Key {
        match key {
            DomainKey::Enter => Key::Enter,
            DomainKey::ShiftEnter => Key::ShiftEnter,
            DomainKey::CtrlEnter => Key::CtrlEnter,
            DomainKey::AltEnter => Key::AltEnter,
            DomainKey::AltM => Key::Alt('m'),
            DomainKey::AltV => Key::Alt('v'),
            DomainKey::AltC => Key::Alt('c'),
            DomainKey::AltL => Key::Alt('l'),
            DomainKey::AltU => Key::Alt('u'),
            DomainKey::Esc => Key::Esc,
            DomainKey::Backspace => Key::Backspace,
            DomainKey::Delete => Key::Delete,
            DomainKey::Up => Key::Up,
            DomainKey::Down => Key::Down,
            DomainKey::Left => Key::Left,
            DomainKey::Right => Key::Right,
            DomainKey::Home => Key::Home,
            DomainKey::End => Key::End,
            DomainKey::PageUp => Key::PageUp,
            DomainKey::PageDown => Key::PageDown,
            DomainKey::Tab => Key::Tab,
            DomainKey::ShiftTab => Key::ShiftTab,
            DomainKey::CtrlA => Key::Ctrl('a'),
            DomainKey::CtrlB => Key::Ctrl('b'),
            DomainKey::CtrlC => Key::Ctrl('c'),
            DomainKey::CtrlD => Key::Ctrl('d'),
            DomainKey::CtrlE => Key::Ctrl('e'),
            DomainKey::CtrlF => Key::Ctrl('f'),
            DomainKey::CtrlH => Key::Ctrl('h'),
            DomainKey::CtrlK => Key::Ctrl('k'),
            DomainKey::CtrlP => Key::Ctrl('p'),
            DomainKey::CtrlQ => Key::Ctrl('q'),
            DomainKey::CtrlR => Key::Ctrl('r'),
            DomainKey::CtrlT => Key::Ctrl('t'),
            DomainKey::CtrlU => Key::Ctrl('u'),
            DomainKey::CtrlW => Key::Ctrl('w'),
            DomainKey::CtrlX => Key::Ctrl('x'),
            DomainKey::UnboundCtrl(c) => Key::Ctrl(c.to_ascii_lowercase()),
            DomainKey::UnboundAlt(c) => Key::Alt(c.to_ascii_lowercase()),
            DomainKey::UnboundFn(n) => Key::F(n),
            DomainKey::UnboundCombo(c, mask) => Key::Combo(c.to_ascii_lowercase(), mask),
        }
    }

    /// Attached host decoding: a raw crossterm key event.
    pub fn from_crossterm(key: &crossterm::event::KeyEvent) -> Key {
        use crossterm::event::{KeyCode, KeyModifiers};
        let m = key.modifiers;
        let ctrl = m.contains(KeyModifiers::CONTROL);
        let alt = m.contains(KeyModifiers::ALT);
        let shift = m.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char(c) if (ctrl && (alt || shift)) || (alt && shift) => {
                let mask = u8::from(ctrl) | (u8::from(alt) << 1) | (u8::from(shift) << 2);
                Key::Combo(c.to_ascii_lowercase(), mask)
            }
            KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
            KeyCode::Char(c) if alt => Key::Alt(c.to_ascii_lowercase()),
            KeyCode::Char(c) if shift => Key::Char(c.to_ascii_uppercase()),
            KeyCode::Char(c) => Key::Char(c),
            KeyCode::Enter if shift => Key::ShiftEnter,
            KeyCode::Enter if alt => Key::AltEnter,
            KeyCode::Enter if ctrl => Key::CtrlEnter,
            KeyCode::Enter => Key::Enter,
            KeyCode::Esc => Key::Esc,
            KeyCode::Tab => Key::Tab,
            KeyCode::BackTab => Key::ShiftTab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Delete => Key::Delete,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Left => Key::Left,
            KeyCode::Right => Key::Right,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::F(n) => Key::F(n),
            _ => Key::Other,
        }
    }

    /// Every key a context's negative sweep drives (K4): printable ASCII, every
    /// named key, every Ctrl and Alt letter, a few function keys.
    pub fn universe() -> Vec<Key> {
        let mut keys: Vec<Key> = (0x20u8..0x7f).map(|b| Key::Char(b as char)).collect();
        keys.extend(('a'..='z').map(Key::Ctrl));
        keys.extend(('a'..='z').map(Key::Alt));
        keys.extend([
            Key::Enter,
            Key::ShiftEnter,
            Key::CtrlEnter,
            Key::AltEnter,
            Key::Esc,
            Key::Tab,
            Key::ShiftTab,
            Key::Backspace,
            Key::Delete,
            Key::Up,
            Key::Down,
            Key::Left,
            Key::Right,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
        ]);
        keys.extend((1..=12).map(Key::F));
        // Combined-modifier forms of the keys with the most dangerous plain meaning.
        keys.extend(
            [('c', 5), ('x', 5), ('q', 5), ('p', 5), ('m', 6), ('a', 3)]
                .map(|(c, m)| Key::Combo(c, m)),
        );
        keys
    }
}

/// `Ctrl+X, M` / `Ctrl+X, Enter` / `Ctrl+T`: the long label of a whole sequence.
pub fn sequence_label(context: Context, keys: &[Key]) -> String {
    let body = keys
        .iter()
        .map(|key| match (context, key) {
            (Context::Leader, Key::Char(c)) => c.to_ascii_uppercase().to_string(),
            (_, key) => key.label(),
        })
        .collect::<Vec<_>>()
        .join(if context == Context::Leader { "" } else { " " });
    if context == Context::Leader {
        format!("Ctrl+X, {body}")
    } else {
        body
    }
}

// ── contexts ──────────────────────────────────────────────────────────────

/// A place keys are interpreted. Parents form an explicit acyclic graph rooted
/// at `Global` (AD-2103); `Leader` and `Permission` are isolated roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Context {
    Global,
    /// The follow-up of `Ctrl+X` (the untimed menu).
    Leader,
    /// Parent of every text field: inherits only the rows marked `text_entry_ok`.
    TextEntry,
    Input,
    Chat,
    Sidebar,
    SidebarHistory,
    SearchTyping,
    SearchNavigating,
    BookmarkList,
    Permission,
    /// Rows shared by every append-only text field (A4): `Ctrl+W/U/H` act,
    /// `Ctrl+A/E/K` are `(n/a)`. Parent of those fields' contexts.
    FieldEdit,
    /// Caret-editing query fields, distinct from the shared composer context.
    CaretField,
    /// In-frame Log/Help filtering; Enter keeps the filter, Esc clears and leaves it.
    InspectorSearch,
    /// Read-only frame navigation without any Global or Chat inheritance.
    InspectorBody,
    /// Horizontal block-local scrolling shared by every Markdown body window.
    MarkdownBody,
    BlockViewer,
    ToolInspector,
    /// The task-panel drill-down (focus Chat): its own keys plus only the
    /// non-character keys of Chat (`i`, `q`, `z`, `?` stay unreachable there).
    ChatDrill,
    /// Chat while a retained wave exists (no drill-down, no reorder).
    ChatWave,
    /// Chat while a task reorder is active.
    ChatReorder,
    SidebarTasks,
    TasksReorder,
    SidebarAgents,
    /// The agent inspector (the drill-down on one agent): its own keys, then the
    /// Sidebar rows.
    SidebarAgentInspector,
    /// The Mission Control hub (S-MC): the tab row, the list/detail focus and the RU-25
    /// scroll rows of `InspectorBody`; a frame of its own, never the composer.
    MissionControl,
    /// The artifact window (Story 21.7 A15): one artifact's facts and body, `v` rendered or
    /// source, `c` copies the original source.
    ArtifactWindow,
    WaveOverlay,
    Help,
    CommandPalette,
    ModelSelector,
    ProfileSwitcher,
    UsagePanel,
    CrossSearch,
    ReverseSearch,
    Autocomplete,
    NotificationLog,
    QuitConfirm,
    Decision,
    /// The Needs-you list (S-QUEUE): select, open, back.
    DecisionQueue,
    /// The A2A thread inspector (S-THREAD): a received item's thread, paged and scrolled
    /// in its own frame; the writer rows are the attached host's.
    ThreadInspector,
    /// The thread window's reply field (attached writers): readline editing of the text of a
    /// reply to the opened item, `Enter` sends, `Esc` clears and leaves.
    ThreadReply,
    /// The thread window's block viewport: `h l 0 $` move the wide blocks' offset, thread
    /// letters are suspended, `Esc` leaves.
    BodyViewport,
    Login,
    /// A decision frame's own text field (Deny + feedback, the question answer):
    /// readline editing, never the composer.
    DecisionCompose,
    /// The AskUserQuestion answer field is open (a child of `DecisionCompose`).
    Question,
    /// The AskUserQuestion frame while its field is closed (Esc left it).
    QuestionIdle,
    SkillTrust,
    SkillTrustInspect,
    PlanApproval,
    /// The plan card and the plan deviation card (same `y/e/n` grammar).
    PlanDecision,
    SkipCascade,
    Delegation,
    SpawnGate,
    /// The `y` / `n` confirmations that are decision frames: cancel plan, agent kill, fork,
    /// artifact apply and resolve, `/peer add`, `/team retract`, consolidation (both hosts).
    DecisionConfirm,
    Forget,
    Rewind,
    ExportOverwrite,
    TeamProposal,
    /// Attached sender-consent card.
    Consent,
    /// A peer's pending response: approve / edit / reject / write your own (attached).
    RecipientDraft,
    /// An MCP task's input-request ticket (Accumulate).
    Ticket,
    /// A peer's reply was observed (Accumulate).
    ReplyArrived,
}

impl Context {
    pub const ALL: [Context; 63] = [
        Context::Global,
        Context::Leader,
        Context::TextEntry,
        Context::Input,
        Context::Chat,
        Context::Sidebar,
        Context::SidebarHistory,
        Context::SearchTyping,
        Context::SearchNavigating,
        Context::BookmarkList,
        Context::Permission,
        Context::FieldEdit,
        Context::CaretField,
        Context::InspectorSearch,
        Context::InspectorBody,
        Context::MarkdownBody,
        Context::BlockViewer,
        Context::ToolInspector,
        Context::ChatDrill,
        Context::ChatWave,
        Context::ChatReorder,
        Context::SidebarTasks,
        Context::TasksReorder,
        Context::SidebarAgents,
        Context::SidebarAgentInspector,
        Context::MissionControl,
        Context::ArtifactWindow,
        Context::WaveOverlay,
        Context::Help,
        Context::CommandPalette,
        Context::ModelSelector,
        Context::ProfileSwitcher,
        Context::UsagePanel,
        Context::CrossSearch,
        Context::ReverseSearch,
        Context::Autocomplete,
        Context::NotificationLog,
        Context::QuitConfirm,
        Context::Decision,
        Context::DecisionQueue,
        Context::ThreadInspector,
        Context::ThreadReply,
        Context::BodyViewport,
        Context::Login,
        Context::DecisionCompose,
        Context::Question,
        Context::QuestionIdle,
        Context::SkillTrust,
        Context::SkillTrustInspect,
        Context::PlanApproval,
        Context::PlanDecision,
        Context::SkipCascade,
        Context::Delegation,
        Context::SpawnGate,
        Context::DecisionConfirm,
        Context::Forget,
        Context::Rewind,
        Context::ExportOverwrite,
        Context::TeamProposal,
        Context::Consent,
        Context::RecipientDraft,
        Context::Ticket,
        Context::ReplyArrived,
    ];

    /// Name painted in the "No binding for Ctrl+X <key> in <context>" toast and docs.
    pub const fn name(self) -> &'static str {
        match self {
            Context::Global => "Global",
            Context::Leader => "Leader",
            Context::TextEntry => "Text field",
            Context::Input => "Input",
            Context::Chat => "Chat",
            Context::Sidebar => "Sidebar",
            Context::SidebarHistory => "History",
            Context::SearchTyping => "Search (typing)",
            Context::SearchNavigating => "Search (navigating)",
            Context::BookmarkList => "Bookmarks",
            Context::Permission => "Permission",
            Context::FieldEdit => "Append-only text field",
            Context::CaretField => "Caret text field",
            Context::InspectorSearch => "Inspector search",
            Context::InspectorBody => "Inspector body",
            Context::MarkdownBody => "Markdown body",
            Context::BlockViewer => "Block viewer",
            Context::ToolInspector => "Tool inspector",
            Context::ChatDrill => "Task drill-down",
            Context::ChatWave => "Chat (wave)",
            Context::ChatReorder => "Chat (reorder)",
            Context::SidebarTasks => "Tasks",
            Context::TasksReorder => "Tasks (reorder)",
            Context::SidebarAgents => "Agents",
            Context::SidebarAgentInspector => "Agent inspector",
            Context::MissionControl => "Mission Control",
            Context::ArtifactWindow => "Artifact window",
            Context::WaveOverlay => "Wave overlay",
            Context::Help => "Help",
            Context::CommandPalette => "Command palette",
            Context::ModelSelector => "Model selector",
            Context::ProfileSwitcher => "Profile switcher",
            Context::UsagePanel => "Usage",
            Context::CrossSearch => "Cross-search",
            Context::ReverseSearch => "Reverse search",
            Context::Autocomplete => "Autocomplete",
            Context::NotificationLog => "Notification log",
            Context::QuitConfirm => "Quit confirmation",
            Context::Decision => "Decisions",
            Context::DecisionQueue => "Needs-you list",
            Context::ThreadInspector => "Thread inspector",
            Context::ThreadReply => "Thread reply field",
            Context::BodyViewport => "Block viewport",
            Context::Login => "Login",
            Context::DecisionCompose => "Decision field",
            Context::Question => "Question",
            Context::QuestionIdle => "Question (field closed)",
            Context::SkillTrust => "Skill trust",
            Context::SkillTrustInspect => "Skill trust (inspect)",
            Context::PlanApproval => "Plan approval",
            Context::PlanDecision => "Plan card",
            Context::SkipCascade => "Skip cascade",
            Context::Delegation => "Delegation",
            Context::SpawnGate => "Spawn gate",
            Context::DecisionConfirm => "Confirmation",
            Context::Forget => "Forget",
            Context::Rewind => "Rewind",
            Context::ExportOverwrite => "Export overwrite",
            Context::TeamProposal => "Team proposal",
            Context::Consent => "Consent",
            Context::RecipientDraft => "Recipient draft",
            Context::Ticket => "Ticket",
            Context::ReplyArrived => "Reply arrived",
        }
    }

    /// Declared parents, nearest first.
    pub const fn parents(self) -> &'static [Context] {
        match self {
            // Legacy Bookmark/Wave have no Global parent. Migrated frames declare
            // their menu/quit rows explicitly; fields keep their edit inheritance.
            Context::Global
            | Context::Leader
            | Context::DecisionQueue
            | Context::BookmarkList
            | Context::FieldEdit
            | Context::WaveOverlay
            | Context::Help
            | Context::InspectorBody
            | Context::ProfileSwitcher
            | Context::UsagePanel
            | Context::Autocomplete
            | Context::NotificationLog
            | Context::QuitConfirm
            | Context::Decision
            | Context::Login => &[],
            // The decision frames: the parent owns Esc (defer), Ctrl+C (back), Ctrl+Q
            // (quit), `c` (copy) and PgUp/PgDn (scroll).
            Context::Permission
            | Context::Consent
            | Context::QuestionIdle
            | Context::SkillTrust
            | Context::SkillTrustInspect
            | Context::PlanApproval
            | Context::PlanDecision
            | Context::SkipCascade
            | Context::Delegation
            | Context::SpawnGate
            | Context::DecisionConfirm
            | Context::Forget
            | Context::Rewind
            | Context::ExportOverwrite => &[Context::Decision],
            Context::MarkdownBody => &[Context::InspectorBody],
            Context::BlockViewer => &[Context::MarkdownBody],
            Context::ToolInspector | Context::ThreadInspector => &[Context::MarkdownBody],
            Context::MissionControl => &[Context::InspectorBody],
            // The artifact window: the Markdown body rows (`v`) and the RU-25 scroll rows.
            Context::ArtifactWindow => &[Context::MarkdownBody],
            // The block viewport's chain excludes the thread rows: `l` moves columns there.
            Context::BodyViewport => &[Context::MarkdownBody],
            // The reply field is a caret field of its own, never the composer.
            Context::ThreadReply => &[Context::CaretField],
            // The 6-E decision frames: the parent owns Esc, Ctrl+C, Ctrl+Q, `c` and scroll.
            Context::TeamProposal
            | Context::RecipientDraft
            | Context::Ticket
            | Context::ReplyArrived => &[Context::Decision],
            Context::DecisionCompose => &[Context::CaretField],
            // The answer field is a decision field plus its own `Ctrl+D` skip.
            Context::Question => &[Context::DecisionCompose],
            Context::TextEntry | Context::Chat | Context::Sidebar => &[Context::Global],
            Context::Input => &[Context::TextEntry],
            Context::CaretField => &[Context::TextEntry],
            Context::SidebarHistory
            | Context::SidebarAgents
            | Context::SidebarAgentInspector
            | Context::SidebarTasks => &[Context::Sidebar],
            Context::TasksReorder => &[Context::SidebarTasks],
            // Only the non-character keys of Chat are inherited (see `resolve_in`).
            Context::ChatDrill | Context::ChatWave | Context::ChatReorder => &[Context::Chat],
            Context::CommandPalette
            | Context::SearchTyping
            | Context::SearchNavigating
            | Context::CrossSearch
            | Context::InspectorSearch
            | Context::ReverseSearch => &[Context::CaretField],
            // The remaining fields retain their append-only editor.
            Context::ModelSelector => &[Context::FieldEdit],
        }
    }

    /// Contexts whose arms lower-case the key (the Leader follow-up letters, the
    /// decision pane, the model selector's `H J K L Y N`, the profile preview's
    /// `Y N`); every other context is case-sensitive (A2).
    pub const fn folds_case(self) -> bool {
        matches!(
            self,
            Context::Leader
                | Context::Decision
                | Context::ModelSelector
                | Context::ProfileSwitcher
                | Context::ExportOverwrite
        )
    }
}

/// The declared parent graph (one entry per context).
pub type Graph<'a> = &'a [(Context, &'a [Context])];

/// The shipped graph, spelled out from `Context::parents` for `validate`.
pub fn shipped_graph() -> Vec<(Context, &'static [Context])> {
    Context::ALL.iter().map(|c| (*c, c.parents())).collect()
}

// ── hosts, availability ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Host {
    Local,
    Attached,
}

/// Which hosts a row applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosts {
    Both,
    Local,
    Attached,
}

impl Hosts {
    pub const fn includes(self, host: Host) -> bool {
        matches!(
            (self, host),
            (Hosts::Both, _) | (Hosts::Local, Host::Local) | (Hosts::Attached, Host::Attached)
        )
    }
}

/// Facts a row's availability depends on (A5, A10).
#[derive(Debug, Clone, Copy)]
pub struct AvailCtx {
    pub host: Host,
    pub width: u16,
    pub density: DensityMode,
    pub read_only: bool,
    /// False while a confirmation owns the keyboard: `Ctrl+X` is unavailable.
    pub leader_available: bool,
    /// A frame lies beneath this route; still-legacy targets cannot take ownership.
    pub frame_origin: bool,
    /// The local Chat Stage has a render-focused, resolvable tool target.
    pub stage_target: bool,
}

impl AvailCtx {
    pub const fn local(width: u16, density: DensityMode) -> Self {
        Self {
            host: Host::Local,
            width,
            density,
            read_only: false,
            leader_available: true,
            frame_origin: false,
            stage_target: false,
        }
    }

    pub const fn attached(width: u16, read_only: bool) -> Self {
        Self {
            host: Host::Attached,
            width,
            density: DensityMode::Focus,
            read_only,
            leader_available: true,
            frame_origin: false,
            stage_target: false,
        }
    }
}

/// Whether a row can run now; an unavailable row names the real remedy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(&'static str),
}

impl Availability {
    pub const fn is_available(self) -> bool {
        matches!(self, Availability::Available)
    }
}

pub type AvailFn = fn(&AvailCtx) -> Availability;

pub fn always(_: &AvailCtx) -> Availability {
    Availability::Available
}

// ── commands ──────────────────────────────────────────────────────────────

/// A typed command id; each context group owns its enum (see `table*.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    Global(GlobalCmd),
    Leader(LeaderCmd),
    Input(InputCmd),
    Chat(ChatCmd),
    Sidebar(SidebarCmd),
    Search(SearchCmd),
    Bookmark(BookmarkCmd),
    Permission(PermissionCmd),
    Field(FieldCmd),
    Panel(PanelCmd),
    Inspector(InspectorCmd),
    Overlay(OverlayCmd),
    Card(CardCmd),
    Viewer(ViewerCmd),
    Tool(ToolCmd),
    Thread(ThreadCmd),
    Mission(MissionCmd),
    Block(BlockCmd),
    Body(BodyCmd),
}

macro_rules! command_from {
    ($($variant:ident($ty:ty)),* $(,)?) => {$(
        impl From<$ty> for CommandId {
            fn from(cmd: $ty) -> Self {
                CommandId::$variant(cmd)
            }
        }
    )*};
}
command_from!(
    Global(GlobalCmd),
    Leader(LeaderCmd),
    Input(InputCmd),
    Chat(ChatCmd),
    Sidebar(SidebarCmd),
    Search(SearchCmd),
    Bookmark(BookmarkCmd),
    Permission(PermissionCmd),
    Field(FieldCmd),
    Panel(PanelCmd),
    Inspector(InspectorCmd),
    Overlay(OverlayCmd),
    Card(CardCmd),
    Viewer(ViewerCmd),
    Tool(ToolCmd),
    Thread(ThreadCmd),
    Mission(MissionCmd),
    Block(BlockCmd),
    Body(BodyCmd),
);

// ── rows ──────────────────────────────────────────────────────────────────

/// Help overlay category of a row; titles and order are the 10 shipped ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HelpCat {
    Navigation,
    ScrollMouse,
    VimFold,
    Input,
    Commands,
    Chords,
    Permissions,
    Clipboard,
    Search,
    General,
}

impl HelpCat {
    pub const ALL: [HelpCat; 10] = [
        HelpCat::Navigation,
        HelpCat::ScrollMouse,
        HelpCat::VimFold,
        HelpCat::Input,
        HelpCat::Commands,
        HelpCat::Chords,
        HelpCat::Permissions,
        HelpCat::Clipboard,
        HelpCat::Search,
        HelpCat::General,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            HelpCat::Navigation => "NAVIGATION",
            HelpCat::ScrollMouse => "SCROLL & MOUSE",
            HelpCat::VimFold => "VIM FOLD & MOTION",
            HelpCat::Input => "INPUT",
            HelpCat::Commands => "COMMANDS",
            HelpCat::Chords => "CHORDS (Ctrl+X prefix)",
            HelpCat::Permissions => "PERMISSIONS",
            HelpCat::Clipboard => "CLIPBOARD & IMAGES",
            HelpCat::Search => "Search, Bookmarks & Export",
            HelpCat::General => "GENERAL",
        }
    }
}

/// The help-overlay presentation of a row (`key` is the exact help key column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Help {
    pub cat: HelpCat,
    pub key: &'static str,
    pub desc: &'static str,
}

/// One binding: a key (or two-key sequence) in one context and what it does.
#[derive(Clone, Copy)]
pub struct Binding {
    pub context: Context,
    /// 1–2 keys. `Leader` rows hold only the follow-up (the prefix is implicit).
    pub keys: &'static [Key],
    pub command: CommandId,
    /// Short label for the menu, rails and tips ("" = never painted).
    pub label: &'static str,
    pub help: Option<Help>,
    pub reach: Reach,
    pub hosts: Hosts,
    pub avail: AvailFn,
    /// Eligible for a generated contextual tip (`hints.rs`).
    pub hint: bool,
    /// Slash command this row is the key route of (`""` = none); asserted
    /// against `builtin_commands::BUILTINS`.
    pub slash: &'static str,
    /// Portable alternative route of a non-portable row (A2): a slash command,
    /// palette entry or other chord, in words.
    pub alt: &'static str,
    /// Visible to descendants of `TextEntry` (A4): only Ctrl+X, Esc, Enter.
    pub text_entry_ok: bool,
    /// The row is the Copy command (RU-21: a bare `c` outside text fields).
    pub copy: bool,
    /// Native-smoke witness for portable rows: text that must appear on an
    /// 80x24 local screen after the row's PTY bytes (`""` = none; AC9).
    pub witness: &'static str,
    /// Plain-words description for the generated reference (`docs/keyboard.md`),
    /// used before the help text and the label (`""` = fall back to those).
    pub does: &'static str,
}

impl std::fmt::Debug for Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Binding")
            .field("context", &self.context)
            .field("keys", &self.keys)
            .field("command", &self.command)
            .field("label", &self.label)
            .finish()
    }
}

const fn derive_reach(keys: &[Key]) -> Reach {
    let mut reach = Reach::Portable;
    let mut i = 0;
    while i < keys.len() {
        match keys[i].reach() {
            Reach::AltMeta => return Reach::AltMeta,
            Reach::ExtendedKeyboard => reach = Reach::ExtendedKeyboard,
            _ => {}
        }
        i += 1;
    }
    reach
}

/// Start a row. Reach derives from the keys; override with `.reach(..)`.
pub const fn bind(
    context: Context,
    keys: &'static [Key],
    command: CommandId,
    label: &'static str,
) -> Binding {
    Binding {
        context,
        keys,
        command,
        label,
        help: None,
        reach: derive_reach(keys),
        hosts: Hosts::Both,
        avail: always,
        hint: false,
        slash: "",
        does: "",
        alt: "",
        text_entry_ok: false,
        copy: false,
        witness: "",
    }
}

impl Binding {
    pub const fn help(mut self, cat: HelpCat, key: &'static str, desc: &'static str) -> Self {
        self.help = Some(Help { cat, key, desc });
        self
    }
    pub const fn does(mut self, does: &'static str) -> Self {
        self.does = does;
        self
    }
    pub const fn reach(mut self, reach: Reach) -> Self {
        self.reach = reach;
        self
    }
    pub const fn local(mut self) -> Self {
        self.hosts = Hosts::Local;
        self
    }
    pub const fn attached(mut self) -> Self {
        self.hosts = Hosts::Attached;
        self
    }
    pub const fn avail(mut self, avail: AvailFn) -> Self {
        self.avail = avail;
        self
    }
    pub const fn hint(mut self) -> Self {
        self.hint = true;
        self
    }
    pub const fn slash(mut self, slash: &'static str) -> Self {
        self.slash = slash;
        self
    }
    pub const fn alt(mut self, alt: &'static str) -> Self {
        self.alt = alt;
        self
    }
    pub const fn te(mut self) -> Self {
        self.text_entry_ok = true;
        self
    }
    pub const fn copy(mut self) -> Self {
        self.copy = true;
        self
    }
    pub const fn witness(mut self, witness: &'static str) -> Self {
        self.witness = witness;
        self
    }

    /// Long label of the whole sequence (`Ctrl+X, M`, `Alt+M`, `z a`).
    pub fn long_label(&self) -> String {
        sequence_label(self.context, self.keys)
    }

    /// Compact label (`^X m`, `^T`) for the status row and rails.
    pub fn compact_label(&self) -> String {
        let body = self
            .keys
            .iter()
            .map(|key| key.compact())
            .collect::<Vec<_>>()
            .join(" ");
        if self.context == Context::Leader {
            format!("^X {body}")
        } else {
            body
        }
    }

    /// PTY bytes of the whole sequence, `0x18` prefixed for leader rows; `None`
    /// when any key has no canonical bytes.
    pub fn pty_bytes(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        if self.context == Context::Leader {
            bytes.push(0x18);
        }
        for key in self.keys {
            bytes.extend_from_slice(key.pty()?);
        }
        Some(bytes)
    }

    /// Availability of this row now.
    pub fn availability(&self, ctx: &AvailCtx) -> Availability {
        (self.avail)(ctx)
    }
}
