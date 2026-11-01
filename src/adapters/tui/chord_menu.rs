//! Retained `Ctrl+X` chord inventory shared by the local and attached hosts
//! (Story 21.2 AC6/AC13, A5).
//!
//! The menu is derived from the real dispatch table (`WhichKeyState::chord_map`)
//! plus the one portable special follow-up (`Enter` send). A chord that the
//! attached host cannot execute is still *painted* — qualified before selection
//! — and answers with an explicit capability-qualified result and its real
//! remedy when chosen (never a Noop or a silent omission). This is deliberately
//! not the 21-3 keymap registry: it is the bounded truth table for the retained
//! chord homes.

use crate::adapters::tui::state::WhichKeyState;

/// Which client paints / dispatches the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordHost {
    /// The local TUI (`event_loop::run`).
    Local,
    /// The attached client (`attach_loop::run_attached`).
    Attached,
}

/// How the attached client answers a retained chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachedRoute {
    /// Same presentation as the local host (source available in both).
    Same,
    /// Backed by an attached-side source with the same logical command
    /// (e.g. the transparency log read).
    Source,
    /// Explicit unavailable result with the real remedy text.
    Unavailable(&'static str),
    /// Attached-only meaning: cleanly detach, never cancelling daemon work.
    Detach,
    /// Opens S-LOG, the Notification Log (Story 21.1): same store and widget.
    NotificationLog,
    /// Archives every visible toast (Story 21.1); sources are untouched.
    ClearToasts,
}

/// One retained chord.
#[derive(Debug, Clone, Copy)]
pub struct ChordSpec {
    pub key: char,
    pub name: &'static str,
    pub attached: AttachedRoute,
}

/// Attached refusal for sign-in/model chords: the existing CLI remedy.
pub const ATTACHED_SIGN_IN: &str = "Sign-in is not available in an attached session. Run `rustain auth login <provider>` in a terminal; restart the daemon to unlock newly signed-in providers.";
const ATTACHED_PROFILE: &str = "Profile switching is not available in an attached session. Run `rustain profile list` or `rustain profile show <name>` in a terminal.";
const ATTACHED_PANEL: &str = "This panel is local to a standalone session and is not projected to an attached client. Detach (Ctrl+X d) and run `rustain` for the panels, or use `/team status`, `/team board` and `/team log` here.";
const ATTACHED_DENSITY: &str =
    "Density modes are local to a standalone session; the attached view has a single layout.";

/// Retained chord homes, in display order. `d` is Dashboard locally and
/// Detach in the attached client only (RU-3/RU-4); final homes belong to 21-3/21-7.
pub const RETAINED_CHORDS: &[ChordSpec] = &[
    ChordSpec {
        key: 'p',
        name: "Profile",
        attached: AttachedRoute::Unavailable(ATTACHED_PROFILE),
    },
    ChordSpec {
        key: 'm',
        name: "Model",
        attached: AttachedRoute::Unavailable(ATTACHED_SIGN_IN),
    },
    ChordSpec {
        key: 'i',
        name: "Sign in",
        attached: AttachedRoute::Unavailable(ATTACHED_SIGN_IN),
    },
    ChordSpec {
        key: 'a',
        name: "Adapters",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 's',
        name: "Agents",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 'l',
        name: "Log",
        attached: AttachedRoute::Source,
    },
    ChordSpec {
        key: 'r',
        name: "Room",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 'e',
        name: "Evidence",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 't',
        name: "Tasks",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 'u',
        name: "Usage",
        attached: AttachedRoute::Unavailable(ATTACHED_PANEL),
    },
    ChordSpec {
        key: 'w',
        name: "Watch",
        attached: AttachedRoute::Unavailable(ATTACHED_DENSITY),
    },
    ChordSpec {
        key: 'f',
        name: "Focus",
        attached: AttachedRoute::Unavailable(ATTACHED_DENSITY),
    },
    ChordSpec {
        key: 'd',
        name: "Dashboard",
        attached: AttachedRoute::Detach,
    },
    ChordSpec {
        key: '?',
        name: "Help",
        attached: AttachedRoute::Same,
    },
    ChordSpec {
        key: 'n',
        name: "Notifications",
        attached: AttachedRoute::NotificationLog,
    },
    ChordSpec {
        key: 'x',
        name: "Clear toasts",
        attached: AttachedRoute::ClearToasts,
    },
];

/// Look up the retained spec for a chord character (case-insensitive).
pub fn spec_for(key: char) -> Option<&'static ChordSpec> {
    let key = key.to_ascii_lowercase();
    RETAINED_CHORDS.iter().find(|spec| spec.key == key)
}

/// One painted menu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChordMenuItem {
    /// Key as painted inside the brackets (`P`, `?`, `Enter`, `Esc`).
    pub key: String,
    /// Label painted after the key.
    pub name: String,
    /// False when the chord is painted but only answers an unavailable result.
    pub available: bool,
}

impl ChordMenuItem {
    /// Display text: `[P] Profile` / `[P] Profile (n/a)`.
    pub fn text(&self) -> String {
        if self.available {
            format!("[{}] {}", self.key, self.name)
        } else {
            format!("[{}] {} (n/a)", self.key, self.name)
        }
    }
}

/// The complete menu for `host`, derived from the live dispatch table: every
/// retained chord that `which_key` can actually dispatch, then the portable
/// `Enter` send and the `Esc` cancel. Nothing is painted without a route and
/// no route is hidden.
pub fn menu_items(which_key: &WhichKeyState, host: ChordHost) -> Vec<ChordMenuItem> {
    let mut items = Vec::with_capacity(RETAINED_CHORDS.len() + 2);
    for spec in RETAINED_CHORDS {
        if !which_key.chord_map.contains_key(&spec.key) {
            continue;
        }
        let (name, available) = match (host, spec.attached) {
            (ChordHost::Local, _) => (spec.name, true),
            (ChordHost::Attached, AttachedRoute::Detach) => ("Detach", true),
            (ChordHost::Attached, AttachedRoute::Unavailable(_)) => (spec.name, false),
            (ChordHost::Attached, _) => (spec.name, true),
        };
        items.push(ChordMenuItem {
            key: spec.key.to_ascii_uppercase().to_string(),
            name: name.to_string(),
            available,
        });
    }
    items.push(ChordMenuItem {
        key: "Enter".into(),
        name: "Send".into(),
        available: true,
    });
    items.push(ChordMenuItem {
        key: "Esc".into(),
        name: "Cancel".into(),
        available: true,
    });
    items
}

/// Greedily wrap `items` into rows no wider than `width` terminal cells
/// (two-cell separators). An item wider than a row is kept whole on its own row.
pub fn wrap_rows(items: &[ChordMenuItem], width: usize) -> Vec<Vec<usize>> {
    use unicode_width::UnicodeWidthStr;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_2_chord_menu_covers_every_dispatchable_chord_in_both_hosts() {
        let wk = WhichKeyState::new();
        for host in [ChordHost::Local, ChordHost::Attached] {
            let items = menu_items(&wk, host);
            for key in wk.chord_map.keys() {
                let painted = key.to_ascii_uppercase().to_string();
                assert!(
                    items.iter().any(|item| item.key == painted),
                    "{host:?} menu hides chord {key:?}"
                );
            }
            assert!(items.iter().any(|i| i.key == "Enter" && i.name == "Send"));
            assert!(items.iter().any(|i| i.key == "Esc"));
        }
    }

    #[test]
    fn story21_2_attached_d_is_detach_and_unavailable_chords_are_qualified() {
        let wk = WhichKeyState::new();
        let attached = menu_items(&wk, ChordHost::Attached);
        let d = attached.iter().find(|i| i.key == "D").unwrap();
        assert_eq!(d.name, "Detach");
        assert!(d.available);
        let tasks = attached.iter().find(|i| i.key == "T").unwrap();
        assert!(!tasks.available);
        assert!(tasks.text().ends_with("(n/a)"));
        let local = menu_items(&wk, ChordHost::Local);
        assert_eq!(
            local.iter().find(|i| i.key == "D").unwrap().name,
            "Dashboard"
        );
        assert!(local.iter().all(|i| i.available));
    }

    #[test]
    fn story21_2_wrap_rows_never_drop_or_clip_labels() {
        let wk = WhichKeyState::new();
        let items = menu_items(&wk, ChordHost::Attached);
        for width in [56usize, 78, 128] {
            let rows = wrap_rows(&items, width);
            let flat: Vec<usize> = rows.iter().flatten().copied().collect();
            assert_eq!(flat, (0..items.len()).collect::<Vec<_>>());
            for row in &rows {
                if row.len() > 1 {
                    use unicode_width::UnicodeWidthStr;
                    let total: usize = row.iter().map(|&i| items[i].text().width()).sum::<usize>()
                        + 2 * (row.len() - 1);
                    assert!(total <= width, "row of {total} cells exceeds {width}");
                }
            }
        }
    }

    #[test]
    fn story21_1_notification_chords_are_retained_rows_in_both_hosts() {
        let wk = WhichKeyState::new();
        for host in [ChordHost::Local, ChordHost::Attached] {
            let items = menu_items(&wk, host);
            let n = items.iter().find(|i| i.key == "N").unwrap();
            assert_eq!(n.name, "Notifications", "{host:?}");
            assert!(n.available, "{host:?}");
            let x = items.iter().find(|i| i.key == "X").unwrap();
            assert_eq!(x.name, "Clear toasts", "{host:?}");
            assert!(x.available, "{host:?}");
        }
        let notifications = RETAINED_CHORDS.iter().find(|c| c.key == 'n').unwrap();
        assert!(matches!(
            notifications.attached,
            AttachedRoute::NotificationLog
        ));
        let clear = RETAINED_CHORDS.iter().find(|c| c.key == 'x').unwrap();
        assert!(matches!(clear.attached, AttachedRoute::ClearToasts));
    }
}
