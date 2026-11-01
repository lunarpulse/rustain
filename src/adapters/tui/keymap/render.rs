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

/// The key a hint names for `command`: the long label of its first row, `?` when the
/// command has no row. Every key a footer, notice or title paints comes from here, so no
/// hint can name a key the table does not bind (KG-1).
pub fn key_text(command: impl Into<CommandId>) -> String {
    shortcut_for(command.into()).unwrap_or_else(|| "?".to_owned())
}

/// Long label of the `index`-th row bound to `command` (`d`, then `Del` for the bookmark
/// delete): a hint that names two keys of one command.
pub fn key_text_at(command: impl Into<CommandId>, index: usize) -> String {
    let command = command.into();
    table()
        .iter()
        .filter(|row| row.command == command)
        .nth(index)
        .map_or_else(|| "?".to_owned(), |row| row.long_label())
}

/// A shortcut that is a leader chord followed by a panel key (`Ctrl+X, T then x`):
/// the palette shortcut column of the Tasks panel commands (AC20c).
pub fn shortcut_then(first: CommandId, then: CommandId) -> Option<String> {
    Some(format!(
        "{} then {}",
        shortcut_for(first)?,
        shortcut_for(then)?
    ))
}

/// Long label of the first row that is the key route of the builtin slash
/// command `name` (without the slash): `team` -> `Ctrl+X, L`.
pub fn shortcut_for_slash(name: &str) -> Option<String> {
    table()
        .iter()
        .find(|row| row.slash.strip_prefix('/') == Some(name))
        .map(|row| row.long_label())
}

// ── decision cards (Story 21.3 AC15, AC16) ────────────────────────────────────

/// The key `context` binds to `command`, as a rail paints it inside brackets or alone:
/// `y`, `Esc`, `Ctrl+D`, `Space`. `?` when the context has no such row.
pub fn rail_key(context: Context, command: impl Into<CommandId>) -> String {
    let command = command.into();
    table()
        .iter()
        .find(|row| row.context == context && row.command == command)
        .map_or_else(|| "?".to_owned(), |row| row.long_label())
}

/// The bracketed key of a card rail (`[y]`, `[Ctrl+D]`), from the card's own row.
pub fn rail(context: Context, command: impl Into<CommandId>) -> String {
    format!("[{}]", rail_key(context, command))
}

/// Two keys of one rail item joined by a slash (`PgUp/PgDn`).
pub fn rail_pair(
    context: Context,
    first: impl Into<CommandId>,
    second: impl Into<CommandId>,
) -> String {
    format!("{}/{}", rail_key(context, first), rail_key(context, second))
}

/// One cue naming `items` (context, command pairs, painted as `[key] Label`): the text
/// `Esc` and `Ctrl+C` post on a card instead of deciding it (A8).
pub fn cue(items: &[(Context, CommandId)]) -> String {
    let (esc, ctrl_c) = (
        key_text_at(super::table_card::CardCmd::Cue, 0),
        key_text_at(super::table_card::CardCmd::Cue, 1),
    );
    let named: Vec<String> = items
        .iter()
        .filter_map(|(context, command)| {
            let row = table()
                .iter()
                .find(|row| row.context == *context && row.command == *command)?;
            Some(format!("[{}] {}", row.long_label(), row.label))
        })
        .collect();
    format!(
        "{esc} and {ctrl_c} never decide this card. Press {}",
        named.join("  ")
    )
}

/// The command that says no to a card (deny, decline, cancel, skip).
pub fn is_negative(command: CommandId) -> bool {
    use super::table_card::CardCmd;
    use super::table_permission::PermissionCmd;
    matches!(
        command,
        CommandId::Permission(PermissionCmd::Deny)
            | CommandId::Card(CardCmd::No | CardCmd::CancelPlan | CardCmd::Skip)
    )
}

/// Cells of a cue the toast shows at 80 columns (the body after the notice prefix).
pub const CUE_BUDGET_CELLS: usize = 74;

/// The cue of a card context in `host`: every labelled row of the context. The negative
/// key is named second (first when that still would not fit), so a toast clipped at 80
/// columns never loses it (R7).
pub fn card_cue(context: Context, host: Host) -> String {
    let mut items: Vec<(Context, CommandId)> = rows_of(context, host)
        .filter(|row| !row.label.is_empty())
        .map(|row| (context, row.command))
        .collect();
    if let Some(at) = items.iter().position(|(_, command)| is_negative(*command)) {
        let negative = items.remove(at);
        let slot = at.min(1);
        items.insert(slot, negative);
        if slot > 0 && UnicodeWidthStr::width(cue(&items[..=slot]).as_str()) > CUE_BUDGET_CELLS {
            let negative = items.remove(slot);
            items.insert(0, negative);
        }
    }
    cue(&items)
}
