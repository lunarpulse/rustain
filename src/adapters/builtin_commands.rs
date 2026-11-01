//! Built-in slash command authority: which host offers and dispatches which
//! command (Story 21.2 AC1, RU-34).
//!
//! One small table is the single source for BOTH the catalogue (what discovery
//! offers in each host) and the dispatch classification (`app::submit_message`
//! routing a name to the real runtime dispatcher), so "offered but rejected"
//! cannot recur and equality is checked per host, never as a local∪attached
//! union. This is deliberately not the 21-3 keymap registry.

/// The client a command is offered in / dispatched by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandHost {
    /// The local TUI (`event_loop::run`).
    Local,
    /// The attached client (`attach_loop::run_attached`).
    Attached,
}

/// How the attached client treats a built-in name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachedRoute {
    /// Not handled by the attached client or daemon path: never offered, and a
    /// manual entry gets an honest "not available attached" notice instead of
    /// becoming model input.
    Unavailable,
    /// Offered and handled (client presentation or the daemon/team path).
    Offered,
    /// Typed entry answers the existing auth/account refusal with its CLI
    /// remedy; deliberately NOT advertised as an attached sign-in.
    TypedRefusal,
}

/// One built-in command.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinCommand {
    pub name: &'static str,
    /// Dispatched locally through `InputAction::ExecuteCommand`.
    pub local: bool,
    pub attached: AttachedRoute,
    /// The dispatcher receives the argument bytes (false: dispatched bare).
    pub takes_args: bool,
}

const fn cmd(
    name: &'static str,
    local: bool,
    attached: AttachedRoute,
    takes_args: bool,
) -> BuiltinCommand {
    BuiltinCommand {
        name,
        local,
        attached,
        takes_args,
    }
}

use AttachedRoute::{Offered, TypedRefusal, Unavailable};

/// Every built-in, in catalogue order.
pub const BUILTINS: &[BuiltinCommand] = &[
    cmd("new", true, Unavailable, false),
    cmd("clear", true, Offered, false),
    cmd("export", true, Unavailable, true),
    cmd("deactivate", true, Unavailable, true),
    cmd("mode", true, Unavailable, true),
    cmd("plan", true, Unavailable, true),
    cmd("compact", true, Unavailable, true),
    cmd("config", true, Unavailable, true),
    cmd("persona", true, Unavailable, true),
    cmd("memory", true, Unavailable, true),
    cmd("session", true, Unavailable, true),
    cmd("tools", true, Unavailable, true),
    cmd("channels", true, Unavailable, true),
    cmd("scheduler", true, Unavailable, true),
    cmd("context", true, Unavailable, true),
    cmd("fanout", true, Unavailable, true),
    cmd("team", true, Offered, true),
    cmd("room", true, Unavailable, true),
    cmd("peer", true, Unavailable, true),
    cmd("artifacts", true, Unavailable, true),
    cmd("artifact", true, Unavailable, true),
    cmd("login", true, TypedRefusal, true),
    cmd("models", true, TypedRefusal, true),
    cmd("logout", true, TypedRefusal, true),
    cmd("accounts", true, TypedRefusal, true),
    // Story 21.2: presentation commands now registered in the catalogue.
    cmd("ml", true, Offered, false),
    cmd("quit", true, Offered, false),
    // Story 21.1 AC5: the Notification Log, offered in both hosts (same store).
    cmd("notifications", true, Offered, false),
    // Attached-only: cleanly detach (never cancels daemon work).
    cmd("detach", false, Offered, false),
];

/// Look up a built-in by exact name.
pub fn find(name: &str) -> Option<&'static BuiltinCommand> {
    BUILTINS.iter().find(|builtin| builtin.name == name)
}

/// The local dispatch classification: `Some` when `name` is a built-in the
/// local runtime dispatcher handles.
pub fn local_dispatch(name: &str) -> Option<&'static BuiltinCommand> {
    find(name).filter(|builtin| builtin.local)
}

/// True when `host` offers `name` in discovery (autocomplete / palette).
pub fn offered_in(host: CommandHost, name: &str) -> bool {
    find(name).is_some_and(|builtin| match host {
        CommandHost::Local => builtin.local,
        CommandHost::Attached => builtin.attached == AttachedRoute::Offered,
    })
}

/// Names `host` offers, catalogue order.
pub fn offered_names(host: CommandHost) -> impl Iterator<Item = &'static str> {
    BUILTINS
        .iter()
        .filter(move |builtin| match host {
            CommandHost::Local => builtin.local,
            CommandHost::Attached => builtin.attached == AttachedRoute::Offered,
        })
        .map(|builtin| builtin.name)
}

/// The notice for a submitted slash command that resolved to nothing. A built-in
/// that exists but is not offered by `host` (local `/detach`) must not be called
/// "not a built-in".
pub fn unknown_command_notice(host: CommandHost, name: &str) -> String {
    match find(name) {
        Some(builtin) if !offered_in(host, builtin.name) => match host {
            CommandHost::Local => {
                format!("/{name} is only available in an attached client (rustain daemon attach).")
            }
            CommandHost::Attached => format!(
                "/{name} is not available in an attached session. Detach ({detach}) and use it from a standalone `rustain` session.",
                detach = crate::adapters::tui::keymap::render::leader_follow_up_long(
                    crate::adapters::tui::keymap::table::LeaderCmd::Detach
                )
            ),
        },
        _ => format!("Unknown command: /{name} — not a built-in or a custom command."),
    }
}

/// A known built-in that the attached client does NOT handle and does not
/// refuse via the auth/account route: manual entry must answer honestly and
/// never fall through to the model as input.
pub fn attached_unavailable(name: &str) -> bool {
    find(name).is_some_and(|builtin| builtin.attached == AttachedRoute::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_2_names_are_unique_and_attached_only_is_detach() {
        let mut seen = std::collections::HashSet::new();
        for builtin in BUILTINS {
            assert!(seen.insert(builtin.name), "duplicate {}", builtin.name);
        }
        let attached_only: Vec<_> = BUILTINS
            .iter()
            .filter(|b| !b.local && b.attached == AttachedRoute::Offered)
            .map(|b| b.name)
            .collect();
        assert_eq!(attached_only, ["detach"]);
    }

    #[test]
    fn story21_2_per_host_offering_is_not_the_union() {
        let local: Vec<_> = offered_names(CommandHost::Local).collect();
        let attached: Vec<_> = offered_names(CommandHost::Attached).collect();
        assert_eq!(
            attached,
            ["clear", "team", "ml", "quit", "notifications", "detach"]
        );
        assert!(local.contains(&"quit") && local.contains(&"ml"));
        assert!(!local.contains(&"detach"));
        // Typed auth/account refusal is preserved but never advertised attached.
        for name in ["login", "models", "logout", "accounts"] {
            assert!(!offered_in(CommandHost::Attached, name));
            assert_eq!(find(name).unwrap().attached, AttachedRoute::TypedRefusal);
        }
        assert!(attached_unavailable("mode") && !attached_unavailable("clear"));
    }
}

#[cfg(test)]
mod catalogue_parity {
    use super::*;
    use crate::adapters::command_registry::{CommandRegistry, CommandSource};

    /// K1 (catalogue half): registry built-ins == the authority table, and each
    /// host's discovery offers exactly the authority's names — never an
    /// offered-but-unhandled command, never the local∪attached union.
    #[test]
    fn story21_2_registry_builtins_equal_the_authority_and_filter_per_host() {
        let registry = CommandRegistry::new();
        let mut registered: Vec<_> = registry
            .filter_for_host("", CommandHost::Local)
            .into_iter()
            .chain(registry.filter_for_host("", CommandHost::Attached))
            .filter(|c| matches!(c.source, CommandSource::BuiltIn))
            .map(|c| c.name.clone())
            .collect();
        registered.sort();
        registered.dedup();
        let mut authority: Vec<_> = BUILTINS
            .iter()
            .filter(|b| b.local || b.attached == AttachedRoute::Offered)
            .map(|b| b.name.to_string())
            .collect();
        authority.sort();
        assert_eq!(registered, authority);
        for host in [CommandHost::Local, CommandHost::Attached] {
            let offered: Vec<_> = registry
                .filter_for_host("", host)
                .into_iter()
                .map(|c| c.name.clone())
                .collect();
            let expected: Vec<_> = offered_names(host).map(str::to_owned).collect();
            let mut a = offered.clone();
            let mut b = expected.clone();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{host:?} discovery must equal its authority set");
        }
        assert!(
            registry.filter("detach").is_empty(),
            "detach is attached-only"
        );
    }

    /// Scoped "offered but unhandled" check: every name the LOCAL host offers has
    /// a real dispatch arm in the event loop (its own `cmd_name == "…"` arm, or the
    /// adapter-overlay path for the port commands). Deleting an arm — say
    /// `cmd_name == "quit"` — fails here even though K1 stops at `ExecuteCommand`.
    #[test]
    fn story21_2_every_locally_offered_command_has_an_event_loop_dispatch_arm() {
        let event_loop = include_str!("../infrastructure/runtime/event_loop.rs");
        assert!(
            event_loop.contains("handle_apply_adapter_override("),
            "the adapter-overlay path exists for the port commands"
        );
        for name in offered_names(CommandHost::Local) {
            let own_arm = event_loop.contains(&format!("cmd_name == \"{name}\""));
            let port_arm =
                crate::domain::services::adapter_overlay::port_dimension_from_command_name(name)
                    .is_some();
            assert!(
                own_arm || port_arm,
                "`/{name}` is offered locally but event_loop.rs has no dispatch arm for it"
            );
        }
        // Positive control: the discriminating forms really are present.
        for name in ["quit", "ml", "login", "new", "export", "mode"] {
            assert!(
                event_loop.contains(&format!("cmd_name == \"{name}\"")),
                "{name}"
            );
        }
    }

    #[test]
    fn story21_2_unknown_command_notice_is_honest_about_host_only_built_ins() {
        // A built-in this host does not offer is NOT "not a built-in".
        let detach = unknown_command_notice(CommandHost::Local, "detach");
        assert!(
            detach.contains("only available in an attached client"),
            "{detach}"
        );
        assert!(!detach.contains("not a built-in"), "{detach}");
        // A genuinely unknown name keeps the original wording.
        assert_eq!(
            unknown_command_notice(CommandHost::Local, "nope"),
            "Unknown command: /nope — not a built-in or a custom command."
        );
        // A name the host offers never reaches this notice with a false claim either.
        assert!(!unknown_command_notice(CommandHost::Attached, "clear").contains("only available"),);
    }
}
