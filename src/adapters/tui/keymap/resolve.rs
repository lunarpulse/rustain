//! Resolution and governance of the keymap table (Story 21.3 AC1, AC2).

use std::collections::HashMap;
use std::sync::LazyLock;

use super::model::{
    AvailCtx, Availability, Binding, CommandId, Context, Graph, Host, Key, Reach, shipped_graph,
};
use super::table::LeaderCmd;

/// The shipped table (validated in debug builds at first use, AC1).
pub fn table() -> &'static [Binding] {
    static TABLE: LazyLock<Vec<Binding>> = LazyLock::new(|| {
        let rows = super::table::all_rows();
        #[cfg(debug_assertions)]
        {
            let graph = shipped_graph();
            if let Err(errors) = validate(&rows, &graph, &[]) {
                panic!("keymap table violates governance: {errors:?}");
            }
        }
        rows
    });
    &TABLE
}

/// Result of resolving a key sequence in a context.
#[derive(Debug, Clone, Copy)]
pub enum Resolved {
    Hit(&'static Binding),
    /// A proper prefix of at least one row (`z` before `a`).
    Pending,
    Miss,
}

/// The context followed by its ancestors, nearest first; the second value says
/// whether the chain passes through `TextEntry` (restricted inheritance, A4).
fn chain(context: Context) -> Vec<Context> {
    let mut order = vec![context];
    let mut i = 0;
    while i < order.len() {
        for parent in order[i].parents() {
            if !order.contains(parent) {
                order.push(*parent);
            }
        }
        i += 1;
    }
    order
}

/// Resolve `seq` in `context` for `host`: the context's own rows first, then
/// its declared parents (AD-2103). Below `TextEntry` only rows marked
/// `text_entry_ok` are inherited (A4); a printable character therefore never
/// reaches a Chat or Global command from a text field.
pub fn resolve(context: Context, host: Host, seq: &[Key]) -> Resolved {
    resolve_in(table(), context, host, seq)
}

pub fn resolve_in(rows: &'static [Binding], context: Context, host: Host, seq: &[Key]) -> Resolved {
    let chain = chain(context);
    let in_text_entry = chain.contains(&Context::TextEntry);
    let mut pending = false;
    for ctx in chain {
        let inherited_through_text_entry =
            in_text_entry && ctx != context && !matches!(ctx, Context::TextEntry);
        let fold = ctx.folds_case();
        let wanted: Vec<Key> = seq.iter().map(|k| k.normalized(fold)).collect();
        for row in rows
            .iter()
            .filter(|r| r.context == ctx && r.hosts.includes(host))
        {
            if inherited_through_text_entry && !row.text_entry_ok {
                continue;
            }
            let keys: Vec<Key> = row.keys.iter().map(|k| k.normalized(fold)).collect();
            if keys == wanted {
                return Resolved::Hit(row);
            }
            if keys.len() > wanted.len() && keys[..wanted.len()] == wanted[..] {
                pending = true;
            }
        }
    }
    if pending {
        Resolved::Pending
    } else {
        Resolved::Miss
    }
}

/// Rows of one context visible to `host`, table order.
pub fn rows_of(context: Context, host: Host) -> impl Iterator<Item = &'static Binding> {
    table()
        .iter()
        .filter(move |row| row.context == context && row.hosts.includes(host))
}

// ── leader ────────────────────────────────────────────────────────────────

/// What a `Ctrl+X` follow-up means (both hosts call this one function).
#[derive(Debug, Clone)]
pub enum LeaderStep {
    /// Run this row's command.
    Run(&'static Binding),
    /// The row exists but cannot run now: answer with the real remedy.
    Unavailable(&'static Binding, &'static str),
    /// `Esc` / `Ctrl+C`: close the menu silently.
    Cancel,
    /// No row: post this info text (never forwarded, never silent).
    NoBinding(String),
}

/// The text of the unknown-follow-up notice (UX `EXPERIENCE.md:105`).
pub fn no_binding_text(key: Key, context_name: &str) -> String {
    format!("No binding for Ctrl+X {} in {context_name}", key.label())
}

/// Resolve one follow-up key. `origin_name` names the context the leader was
/// opened from (`Input`, `Chat`, …) for the no-binding notice.
pub fn leader_step(host: Host, key: Key, avail: &AvailCtx, origin_name: &str) -> LeaderStep {
    match resolve(Context::Leader, host, &[key]) {
        Resolved::Hit(row) => match row.command {
            CommandId::Leader(LeaderCmd::Cancel) => LeaderStep::Cancel,
            _ => match row.availability(avail) {
                Availability::Available => LeaderStep::Run(row),
                Availability::Unavailable(remedy) => LeaderStep::Unavailable(row, remedy),
            },
        },
        Resolved::Pending | Resolved::Miss => {
            LeaderStep::NoBinding(no_binding_text(key, origin_name))
        }
    }
}

// ── governance ────────────────────────────────────────────────────────────

/// One governance defect found by `validate` (K1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    DuplicateKey { context: Context, keys: Vec<Key> },
    UnknownParent { context: Context },
    CyclicParent { context: Context },
    MissingReach { context: Context, keys: Vec<Key> },
    NonPortableOnlyRoute { command: CommandId },
    PortableWithoutBytes { context: Context, keys: Vec<Key> },
    UnregisteredCommandDisplayed { command: CommandId },
    BareCNotCopy { context: Context },
    EmptyKeys { context: Context },
}

/// Contexts whose bare `c` is a named exception to RU-21 (PC-14). Empty in
/// 3-A: the spawn gate and skip-cascade (owner 21-6) and the agent inspector
/// (owner 21-7) join the registry with their contexts.
const BARE_C_EXCEPTIONS: &[Context] = &[];

/// Governance of a table: duplicate normalized keys per context (host-aware),
/// the parent graph, declared reachability, portable alternatives, PTY bytes,
/// displayed commands and the bare-`c` rule (KG-1, KG-3, RU-21, RU-22, RU-34).
pub fn validate(
    rows: &[Binding],
    graph: Graph<'_>,
    displayed: &[CommandId],
) -> Result<(), Vec<Violation>> {
    let mut errors = Vec::new();
    let parents_of: HashMap<Context, &[Context]> = graph.iter().copied().collect();

    // Parent graph: every used or referenced context declared; no cycle.
    for row in rows {
        if !parents_of.contains_key(&row.context) {
            push_unique(
                &mut errors,
                Violation::UnknownParent {
                    context: row.context,
                },
            );
        }
    }
    for (context, parents) in graph {
        for parent in *parents {
            if !parents_of.contains_key(parent) {
                push_unique(&mut errors, Violation::UnknownParent { context: *context });
            }
        }
        if reaches_itself(*context, &parents_of) {
            push_unique(&mut errors, Violation::CyclicParent { context: *context });
        }
    }

    for (index, row) in rows.iter().enumerate() {
        if row.keys.is_empty() {
            errors.push(Violation::EmptyKeys {
                context: row.context,
            });
            continue;
        }
        let fold = row.context.folds_case();
        let normalized: Vec<Key> = row.keys.iter().map(|k| k.normalized(fold)).collect();
        for other in &rows[index + 1..] {
            if other.context != row.context || !hosts_overlap(row, other) {
                continue;
            }
            let other_keys: Vec<Key> = other.keys.iter().map(|k| k.normalized(fold)).collect();
            if other_keys == normalized {
                push_unique(
                    &mut errors,
                    Violation::DuplicateKey {
                        context: row.context,
                        keys: normalized.clone(),
                    },
                );
            }
        }
        if row.reach == Reach::Unspecified {
            errors.push(Violation::MissingReach {
                context: row.context,
                keys: normalized.clone(),
            });
        }
        if row.reach == Reach::Portable && row.pty_bytes().is_none() {
            errors.push(Violation::PortableWithoutBytes {
                context: row.context,
                keys: normalized.clone(),
            });
        }
        let text_field = matches!(
            row.context,
            Context::TextEntry | Context::Input | Context::SearchTyping
        );
        if row.keys == [Key::Char('c')]
            && !text_field
            && !row.copy
            && !BARE_C_EXCEPTIONS.contains(&row.context)
        {
            push_unique(
                &mut errors,
                Violation::BareCNotCopy {
                    context: row.context,
                },
            );
        }
    }

    // A command whose only routes are non-portable needs a declared alternative.
    for row in rows {
        if row.reach == Reach::Portable || !row.alt.is_empty() {
            continue;
        }
        let has_portable = rows
            .iter()
            .any(|r| r.command == row.command && r.reach == Reach::Portable);
        if !has_portable {
            push_unique(
                &mut errors,
                Violation::NonPortableOnlyRoute {
                    command: row.command,
                },
            );
        }
    }

    // Every command shown by a renderer is a registered row.
    for command in displayed {
        if !rows.iter().any(|r| r.command == *command) {
            push_unique(
                &mut errors,
                Violation::UnregisteredCommandDisplayed { command: *command },
            );
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn push_unique(errors: &mut Vec<Violation>, violation: Violation) {
    if !errors.contains(&violation) {
        errors.push(violation);
    }
}

fn hosts_overlap(a: &Binding, b: &Binding) -> bool {
    [Host::Local, Host::Attached]
        .iter()
        .any(|h| a.hosts.includes(*h) && b.hosts.includes(*h))
}

fn reaches_itself(start: Context, parents_of: &HashMap<Context, &[Context]>) -> bool {
    let mut stack: Vec<Context> = parents_of
        .get(&start)
        .map(|p| p.to_vec())
        .unwrap_or_default();
    let mut seen: Vec<Context> = Vec::new();
    while let Some(next) = stack.pop() {
        if next == start {
            return true;
        }
        if seen.contains(&next) {
            continue;
        }
        seen.push(next);
        if let Some(parents) = parents_of.get(&next) {
            stack.extend(parents.iter().copied());
        }
    }
    false
}
