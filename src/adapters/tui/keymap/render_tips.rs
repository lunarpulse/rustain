//! Generated contextual tips (Story 21.3 A10): the key text of every status-row
//! tip is read from registry rows (`hint()` rows of the context, or the named
//! leader rows), never typed. Fading by session count stays in `hints.rs`.

use super::model::{AvailCtx, Binding, CommandId, Context, Host, Key};
use super::resolve::rows_of;
use super::table::LeaderCmd;
use super::table_input::InputCmd;

fn row(context: Context, host: Host, command: impl Into<CommandId>) -> Option<&'static Binding> {
    let command = command.into();
    rows_of(context, host).find(|row| row.command == command)
}

/// A row of `context` that is part of its generated tip (`hint()`).
fn hint_row(
    context: Context,
    host: Host,
    command: impl Into<CommandId>,
) -> Option<&'static Binding> {
    let command = command.into();
    rows_of(context, host).find(|row| row.hint && row.command == command)
}

/// `[^X Enter] Send`, from the leader's Send row.
fn send_tip(host: Host) -> Option<String> {
    let send = row(Context::Leader, host, LeaderCmd::Send)?;
    Some(format!("[{}] {}", send.compact_label(), send.label))
}

/// The focused local tool's compact discovery cue. Both key and command text
/// come from its registry row; the status painter can fit this whole cue.
pub(crate) fn inspect_tip() -> Option<String> {
    let inspect = hint_row(
        Context::Chat,
        Host::Local,
        super::table_chat::ChatCmd::Inspect,
    )?;
    Some(format!("[{}] {}", inspect.compact_label(), inspect.label))
}

/// The tip of `context` for `host`: the Input tip (`is_vscode` selects the VS
/// Code variant), the Chat tip, or, for `Leader`, the which-key tip. `None`
/// for contexts without a tip.
pub fn tip(context: Context, host: Host, is_vscode: bool, stage_target: bool) -> Option<String> {
    match context {
        Context::Input if is_vscode => {
            let newline = hint_row(Context::Input, host, InputCmd::InsertNewline)?;
            let multiline = hint_row(Context::Input, host, InputCmd::ToggleMultiline)?;
            Some(format!(
                "VS Code detected. {} for newlines, {} for multi-line mode, {}",
                newline.long_label(),
                multiline.long_label(),
                send_tip(host)?
            ))
        }
        Context::Input => {
            let help = row(Context::Leader, host, LeaderCmd::Help)?;
            let palette = hint_row(Context::Input, host, InputCmd::OpenPalette)?;
            Some(format!(
                "Tip: {} for help, {} for commands, {}, /mode to switch permission mode",
                help.long_label().replace(", ", " "),
                palette.long_label(),
                send_tip(host)?
            ))
        }
        Context::Chat if host == Host::Local && stage_target => inspect_tip(),
        Context::Chat => {
            let items = chat_items(host, stage_target);
            (!items.is_empty()).then(|| format!("Tip: {}", items.join(", ")))
        }
        Context::Leader => {
            let cancel = rows_of(Context::Leader, host).find(|row| row.keys == [Key::Esc])?;
            Some(format!(
                "Tip: Press a key to execute, {} to cancel",
                cancel.keys[0].label()
            ))
        }
        _ => None,
    }
}

/// The command-palette tip.
pub fn palette_tip() -> String {
    format!("Tip: Type to filter, {} to select", Key::Enter.label())
}

/// The key texts of the launchpad (Story 21.9 D15), each read from a registry
/// row that is available for this host and width. A missing field means the row
/// is absent or unavailable here, so the launchpad does not name it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchpadKeys {
    /// Leader Sign in (`^X i`).
    pub sign_in: Option<String>,
    /// Input mention popup (`@`; local only).
    pub mention: Option<String>,
    /// Input slash popup (`/`).
    pub browse: Option<String>,
    /// Input command palette.
    pub palette: Option<String>,
    /// Key-line items `<key> <label>`: Help, palette, Model, History, in that
    /// order, only when available.
    pub items: Vec<String>,
}

/// Generate the launchpad's keys for `host` under `avail`.
pub fn launchpad_keys(host: Host, avail: &AvailCtx) -> LaunchpadKeys {
    let available = |context: Context, command: CommandId| {
        row(context, host, command).filter(|row| row.availability(avail).is_available())
    };
    let leader = |cmd: LeaderCmd| available(Context::Leader, cmd.into());
    let input = |cmd: InputCmd| available(Context::Input, cmd.into());
    let item = |row: Option<&'static Binding>| {
        row.map(|row| format!("{} {}", row.compact_label(), row.label))
    };
    let palette = input(InputCmd::OpenPalette);
    let items = [
        item(leader(LeaderCmd::Help)),
        item(palette),
        item(leader(LeaderCmd::Model)),
        item(leader(LeaderCmd::History)),
    ]
    .into_iter()
    .flatten()
    .collect();
    LaunchpadKeys {
        sign_in: leader(LeaderCmd::SignIn).map(Binding::compact_label),
        mention: input(InputCmd::OpenMentionPopup).map(Binding::compact_label),
        browse: input(InputCmd::OpenSlashPopup).map(Binding::compact_label),
        palette: palette.map(Binding::compact_label),
        items,
    }
}

/// The `hint()` rows of Chat as `key label` items. Consecutive letters of one
/// case (`j`,`k` / `J`,`K`) share one item (`j/k scroll`).
fn chat_items(host: Host, stage_target: bool) -> Vec<String> {
    let mut avail = match host {
        Host::Local => AvailCtx::local(0),
        Host::Attached => AvailCtx::attached(0, false),
    };
    avail.stage_target = stage_target && host == Host::Local;
    let mut items: Vec<(String, String)> = Vec::new();
    let mut last: Option<char> = None;
    for row in rows_of(Context::Chat, host)
        .filter(|row| row.hint && row.availability(&avail).is_available())
    {
        let key = row.keys[0];
        let pair = match (key, last, items.last_mut()) {
            (Key::Char(c), Some(p), Some(item))
                if p.is_alphabetic() && c.is_alphabetic() && p as u32 + 1 == c as u32 =>
            {
                item.0.push('/');
                item.0.push(c);
                if item.1 != row.label {
                    item.1 = format!("{}/{}", item.1, row.label);
                }
                true
            }
            _ => false,
        };
        if !pair {
            items.push((key.label(), row.label.to_string()));
        }
        last = match key {
            Key::Char(c) => Some(c),
            _ => None,
        };
    }
    items
        .into_iter()
        .map(|(key, label)| {
            if label.is_empty() {
                key
            } else {
                format!("{key} {label}")
            }
        })
        .collect()
}

/// The attached client's status hint: `^X d detach` (the Detach leader row),
/// the Chat tip while Chat has focus (names `i`, how to type), and
/// `[^X Enter] Send` while the composer is multi-line. The `^X menu` tail is
/// painted by the status row itself and is not repeated here.
pub fn attached_status_tip(chat_focus: bool, multiline: bool) -> Option<String> {
    let host = Host::Attached;
    let mut parts: Vec<String> = Vec::new();
    if let Some(detach) = row(Context::Leader, host, LeaderCmd::Detach) {
        parts.push(format!(
            "{} {}",
            detach.compact_label(),
            detach.label.to_lowercase()
        ));
    }
    if chat_focus {
        parts.extend(chat_items(host, false));
    }
    if multiline {
        parts.extend(send_tip(host));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_3_input_tips_are_generated_and_keep_their_visible_text() {
        assert_eq!(
            tip(Context::Input, Host::Local, false, false).as_deref(),
            Some(
                "Tip: Ctrl+X ? for help, Ctrl+P for commands, [^X Enter] Send, /mode to switch permission mode"
            )
        );
        assert_eq!(
            tip(Context::Input, Host::Local, true, false).as_deref(),
            Some(
                "VS Code detected. Alt+Enter for newlines, Alt+M for multi-line mode, [^X Enter] Send"
            )
        );
    }

    #[test]
    fn story21_3_chat_tip_names_the_scroll_jump_cycle_type_and_help_keys() {
        let local = tip(Context::Chat, Host::Local, false, false).unwrap();
        for key in ["j/k", "J/K", "Tab", "i ", "?"] {
            assert!(local.contains(key), "{key}: {local}");
        }
        // The attached client has no j/k rows: only the keys it answers.
        let attached = tip(Context::Chat, Host::Attached, false, false).unwrap();
        assert!(
            attached.contains("i ") && attached.contains('?'),
            "{attached}"
        );
        assert!(!attached.contains("j/k"), "{attached}");
    }

    #[test]
    fn story21_3_which_key_and_palette_tips_keep_their_pinned_substrings() {
        let which_key = tip(Context::Leader, Host::Local, false, false).unwrap();
        assert!(
            which_key.contains("Press a key") && which_key.contains("Esc"),
            "{which_key}"
        );
        let palette = palette_tip();
        assert!(
            palette.contains("Tip:") && palette.contains("filter"),
            "{palette}"
        );
        assert_eq!(tip(Context::Global, Host::Local, false, false), None);
    }

    #[test]
    fn story21_3_attached_status_tip_comes_from_the_detach_and_chat_rows() {
        assert_eq!(
            attached_status_tip(false, false).as_deref(),
            Some("^X d detach")
        );
        let multiline = attached_status_tip(false, true).unwrap();
        assert_eq!(multiline, "^X d detach · [^X Enter] Send");
        let chat = attached_status_tip(true, false).unwrap();
        assert!(chat.starts_with("^X d detach · i "), "{chat}");
        assert!(!chat.contains("menu"), "the tail paints ^X menu: {chat}");
        assert!(!chat.contains("Esc") && !chat.contains("Ctrl+C"), "{chat}");
    }
}
