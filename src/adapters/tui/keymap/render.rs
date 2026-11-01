//! Registry-generated labels: the leader menu, its wrapping and the status
//! tail (Story 21.3 AC3, AC4). Pure; widths are terminal cells.

use unicode_width::UnicodeWidthStr;

use super::model::{AvailCtx, Availability, CommandId, Context, Host, Key};
use super::resolve::{rows_of, table};
use super::table::{GlobalCmd, LeaderCmd};

/// One painted leader-menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// Key as painted inside the brackets (`P`, `?`, `Enter`, `Esc`).
    pub key: String,
    /// Label painted after the key.
    pub name: String,
    /// False when the chord is painted but only answers its remedy.
    pub available: bool,
}

impl MenuItem {
    /// `[P] Profile` / `[P] Profile (n/a)`.
    pub fn text(&self) -> String {
        if self.available {
            format!("[{}] {}", self.key, self.name)
        } else {
            format!("[{}] {} (n/a)", self.key, self.name)
        }
    }
}

/// The complete leader menu for `avail.host`: every labelled Leader row of the
/// registry, qualified by its availability now (A5). Nothing is painted
/// without a route and no route is hidden.
pub fn leader_menu(avail: &AvailCtx) -> Vec<MenuItem> {
    rows_of(Context::Leader, avail.host)
        .filter(|row| !row.label.is_empty())
        .map(|row| MenuItem {
            key: match row.keys[0] {
                Key::Char(c) => c.to_ascii_uppercase().to_string(),
                other => other.label(),
            },
            name: row.label.to_string(),
            available: matches!(row.availability(avail), Availability::Available),
        })
        .collect()
}

/// Greedily wrap `items` into rows no wider than `width` terminal cells
/// (two-cell separators). An item wider than a row is kept whole on its own row.
pub fn wrap_rows(items: &[MenuItem], width: usize) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0usize;
    for (index, item) in items.iter().enumerate() {
        let w = item.text().width();
        match rows.last_mut() {
            Some(row) if used + 2 + w <= width => {
                row.push(index);
                used += 2 + w;
            }
            _ => {
                rows.push(vec![index]);
                used = w;
            }
        }
    }
    rows
}

/// The pending key-sequence prefix shown at the status row's tail (A10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPrefix {
    Leader,
    Z,
    BracketOpen,
    BracketClose,
}

/// What the status row's rightmost segment shows (A10). `Menu` is the default:
/// `^X menu` is always reserved and never faded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatusTail {
    /// `Ctrl+X` is unavailable in the current context: nothing is painted.
    Hidden,
    #[default]
    Menu,
    Pending(PendingPrefix),
}

impl StatusTail {
    /// The tail text, generated from the `Ctrl+X` row; `None` when hidden.
    pub fn text(self) -> Option<String> {
        match self {
            StatusTail::Hidden => None,
            StatusTail::Menu => Some(format!("{} menu", leader_compact())),
            StatusTail::Pending(PendingPrefix::Leader) => Some(format!("{}…", leader_compact())),
            StatusTail::Pending(PendingPrefix::Z) => Some("z…".to_string()),
            StatusTail::Pending(PendingPrefix::BracketOpen) => Some("[…".to_string()),
            StatusTail::Pending(PendingPrefix::BracketClose) => Some("]…".to_string()),
        }
    }
}

/// `^X`, from the registry row that opens the leader.
pub fn leader_compact() -> String {
    table()
        .iter()
        .find(|row| row.command == CommandId::Global(GlobalCmd::OpenLeader))
        .map(|row| row.compact_label())
        .unwrap_or_else(|| "^X".to_string())
}

/// Compact text of one leader follow-up (`^X q`), for rails and cues.
pub fn leader_follow_up_compact(command: LeaderCmd) -> String {
    table()
        .iter()
        .find(|row| row.command == CommandId::Leader(command))
        .map(|row| row.compact_label())
        .unwrap_or_default()
}

/// Long label of a leader follow-up (`Ctrl+X, Q`).
pub fn leader_follow_up_long(command: LeaderCmd) -> String {
    table()
        .iter()
        .find(|row| row.command == CommandId::Leader(command))
        .map(|row| row.long_label())
        .unwrap_or_default()
}

/// The context host-name used in notices: `Host` is only a discriminator here.
pub fn host_name(host: Host) -> &'static str {
    match host {
        Host::Local => "local",
        Host::Attached => "attached",
    }
}

/// Long label of the first row bound to `command` (`Ctrl+X, L`, `Alt+V`): the
/// palette shortcut column and every prose that names a key (AC3).
pub fn shortcut_for(command: CommandId) -> Option<String> {
    table()
        .iter()
        .find(|row| row.command == command)
        .map(|row| row.long_label())
}

/// Long label of the first row that is the key route of the builtin slash
/// command `name` (without the slash): `team` -> `Ctrl+X, L`.
pub fn shortcut_for_slash(name: &str) -> Option<String> {
    table()
        .iter()
        .find(|row| row.slash.strip_prefix('/') == Some(name))
        .map(|row| row.long_label())
}
