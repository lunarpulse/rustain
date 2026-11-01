//! K1 — registry governance (AC1, KG-1, KG-3, RU-21, RU-22, RU-34). Every
//! negative has a positive control; each injected defect fails with its own
//! reason, so a validator that checks labels instead of normalized keys, loops
//! on a cycle, or lets a bare `c` through is caught.

use rustain::adapters::builtin_commands::CommandHost;
use rustain::adapters::command_registry::CommandRegistry;
use rustain::adapters::palette_registry::PaletteRegistry;
use rustain::adapters::tui::keymap::govern::{RowDefect, validate_rows};
use rustain::adapters::tui::keymap::model::{Graph, shipped_graph};
use rustain::adapters::tui::keymap::render::leader_menu;
use rustain::adapters::tui::keymap::render_help::help_groups;
use rustain::adapters::tui::keymap::table::{GlobalCmd, LeaderCmd};
use rustain::adapters::tui::keymap::{
    AvailCtx, Binding, CommandId, Context, Key, Reach, Violation, bind, table, validate,
};
use rustain::domain::models::visual::DensityMode;

fn quit() -> CommandId {
    CommandId::Global(GlobalCmd::QuitRequest)
}

fn other() -> CommandId {
    CommandId::Global(GlobalCmd::OpenLeader)
}

fn row(ctx: Context, keys: &'static [Key], cmd: CommandId) -> Binding {
    bind(ctx, keys, cmd, "x")
}

fn shipped() -> Vec<(Context, &'static [Context])> {
    shipped_graph()
}

fn errors(rows: &[Binding], graph: Graph<'_>) -> Vec<Violation> {
    validate(rows, graph, &[]).expect_err("the injected defect must be rejected")
}

#[test]
fn shipped_table_passes_governance() {
    let graph = shipped();
    validate(table(), &graph, &[]).expect("the shipped table is governed");
    assert!(table().len() > 20, "the shipped table is not empty");
}

#[test]
fn duplicate_key_in_one_context_is_rejected() {
    let graph = shipped();
    let rows = [
        row(Context::Chat, &[Key::Char('j')], quit()),
        row(Context::Chat, &[Key::Char('j')], other()),
    ];
    let found = errors(&rows, &graph);
    assert!(
        found.contains(&Violation::DuplicateKey {
            context: Context::Chat,
            keys: vec![Key::Char('j')]
        }),
        "{found:?}"
    );
    // Positive controls: the same key in another context, a different key, and
    // the same key for different hosts are all legal.
    let ok = [
        row(Context::Chat, &[Key::Char('j')], quit()),
        row(Context::Sidebar, &[Key::Char('j')], other()),
        row(Context::Chat, &[Key::Char('k')], other()),
        row(Context::Chat, &[Key::Char('x')], quit()).local(),
        row(Context::Chat, &[Key::Char('x')], other()).attached(),
    ];
    validate(&ok, &graph, &[]).expect("distinct (context, key, host) rows are legal");
}

#[test]
fn duplicates_compare_normalized_keys_not_labels() {
    let graph = shipped();
    // Leader follow-ups are case-insensitive: `E` and `e` are one key there...
    let leader = [
        row(Context::Leader, &[Key::Char('e')], quit()),
        bind(Context::Leader, &[Key::Char('E')], other(), "other label"),
    ];
    assert!(errors(&leader, &graph).iter().any(|v| matches!(
        v,
        Violation::DuplicateKey {
            context: Context::Leader,
            ..
        }
    )));
    // ...while `J` and `j` in Chat are two keys (case-sensitive context).
    let chat = [
        row(Context::Chat, &[Key::Char('j')], quit()),
        row(Context::Chat, &[Key::Char('J')], other()),
    ];
    validate(&chat, &graph, &[]).expect("J and j are different keys in Chat");
    // Ctrl letters normalize to lower case.
    let ctrl = [
        row(Context::Chat, &[Key::Ctrl('d')], quit()),
        row(Context::Chat, &[Key::Ctrl('D')], other()),
    ];
    assert!(errors(&ctrl, &graph).contains(&Violation::DuplicateKey {
        context: Context::Chat,
        keys: vec![Key::Ctrl('d')]
    }));
}

#[test]
fn unknown_and_cyclic_parents_are_rejected() {
    let rows = [row(Context::Chat, &[Key::Char('a')], quit())];
    // A used context missing from the graph.
    let missing: Vec<(Context, &[Context])> = vec![(Context::Global, &[])];
    assert!(errors(&rows, &missing).contains(&Violation::UnknownParent {
        context: Context::Chat
    }));
    // A parent that is not declared.
    let undeclared: Vec<(Context, &[Context])> =
        vec![(Context::Global, &[]), (Context::Chat, &[Context::Sidebar])];
    assert!(
        errors(&rows, &undeclared).contains(&Violation::UnknownParent {
            context: Context::Chat
        })
    );
    // A cycle terminates and is reported.
    let cycle: Vec<(Context, &[Context])> = vec![
        (Context::Global, &[]),
        (Context::Chat, &[Context::Sidebar]),
        (Context::Sidebar, &[Context::Chat]),
    ];
    let found = errors(&rows, &cycle);
    assert!(
        found.contains(&Violation::CyclicParent {
            context: Context::Chat
        }),
        "{found:?}"
    );
    // Positive control: the shipped graph is acyclic.
    validate(&rows, &shipped(), &[]).expect("shipped graph accepts the row");
}

#[test]
fn row_without_reachability_is_rejected() {
    let graph = shipped();
    let rows = [row(Context::Chat, &[Key::Char('a')], quit()).reach(Reach::Unspecified)];
    assert!(errors(&rows, &graph).contains(&Violation::MissingReach {
        context: Context::Chat,
        keys: vec![Key::Char('a')]
    }));
    let ok = [row(Context::Chat, &[Key::Char('a')], quit())];
    validate(&ok, &graph, &[]).expect("derived reachability is accepted");
}

#[test]
fn non_portable_only_route_needs_a_declared_alternative() {
    let graph = shipped();
    let alt_only = [row(Context::Input, &[Key::Alt('m')], quit())];
    assert!(
        errors(&alt_only, &graph).contains(&Violation::NonPortableOnlyRoute { command: quit() })
    );
    // Controls: a declared alternative, or another Portable row of the command.
    let declared = [row(Context::Input, &[Key::Alt('m')], quit()).alt("/ml")];
    validate(&declared, &graph, &[]).expect("declared alternative is a portable route");
    let paired = [
        row(Context::Input, &[Key::Alt('m')], quit()),
        row(Context::Chat, &[Key::Char('m')], quit()),
    ];
    validate(&paired, &graph, &[]).expect("a portable row of the same command is a route");
}

#[test]
fn portable_row_without_pty_bytes_is_rejected() {
    let graph = shipped();
    // Ctrl+Enter has no canonical bytes: declaring it Portable is a defect.
    let rows = [row(Context::Input, &[Key::CtrlEnter], quit())
        .reach(Reach::Portable)
        .alt("Ctrl+X Enter")];
    assert!(errors(&rows, &graph).iter().any(|v| matches!(
        v,
        Violation::PortableWithoutBytes {
            context: Context::Input,
            ..
        }
    )));
    // Control: declared extended keyboard with an alternative passes.
    let ok = [row(Context::Input, &[Key::CtrlEnter], quit()).alt("Ctrl+X Enter")];
    validate(&ok, &graph, &[]).expect("extended-keyboard row with an alternative");
    assert_eq!(ok[0].reach, Reach::ExtendedKeyboard);
}

#[test]
fn displayed_command_without_a_row_is_rejected() {
    let graph = shipped();
    let rows = [row(Context::Chat, &[Key::Char('a')], quit())];
    let ghost = CommandId::Leader(LeaderCmd::Profile);
    let found = validate(&rows, &graph, &[ghost]).expect_err("ghost command displayed");
    assert!(found.contains(&Violation::UnregisteredCommandDisplayed { command: ghost }));
    validate(&rows, &graph, &[quit()]).expect("a registered command may be displayed");
}

#[test]
fn bare_c_outside_text_fields_must_be_copy() {
    let graph = shipped();
    let rows = [row(Context::Chat, &[Key::Char('c')], quit())];
    assert!(errors(&rows, &graph).contains(&Violation::BareCNotCopy {
        context: Context::Chat
    }));
    // Controls: the Copy row, and a text field where `c` is just a letter.
    let copy = [row(Context::Chat, &[Key::Char('c')], quit()).copy()];
    validate(&copy, &graph, &[]).expect("Copy owns a bare c");
    let text = [row(Context::Input, &[Key::Char('c')], quit())];
    validate(&text, &graph, &[]).expect("c is a letter in a text field");
}

/// A shorter row hides a longer sequence it prefixes (`g` before `g g`): the
/// sequence could never complete.
#[test]
fn a_row_shadowing_a_sequence_is_rejected() {
    let g: &'static [Key] = &[Key::Char('g')];
    let gg: &'static [Key] = &[Key::Char('g'), Key::Char('g')];
    let shadowed = [
        row(Context::Chat, g, quit()),
        row(Context::Chat, gg, other()),
    ];
    assert_eq!(
        validate_rows(&shadowed),
        Err(vec![RowDefect::ShadowedSequence {
            context: Context::Chat,
            shorter: g.to_vec(),
            longer: gg.to_vec()
        }])
    );
    // Controls: another context, or a host the shorter row does not reach.
    for ok in [
        [
            row(Context::Chat, g, quit()),
            row(Context::Sidebar, gg, other()),
        ],
        [
            row(Context::Chat, g, quit()).local(),
            row(Context::Chat, gg, other()).attached(),
        ],
    ] {
        assert_eq!(validate_rows(&ok), Ok(()));
    }
    assert_eq!(
        validate_rows(table()),
        Ok(()),
        "the shipped table shadows nothing"
    );
}

/// UnregisteredCommandDisplayed is wired to production: every command the
/// leader menu, the palette shortcut column and the help overlay print resolves
/// to a registered row.
#[test]
fn every_command_a_surface_displays_is_a_registered_row() {
    let find = |pred: &dyn Fn(&Binding) -> bool| -> Vec<CommandId> {
        table()
            .iter()
            .filter(|r| pred(r))
            .map(|r| r.command)
            .collect()
    };
    let mut displayed: Vec<CommandId> = Vec::new();
    for avail in [
        AvailCtx::local(130, DensityMode::Focus),
        AvailCtx::attached(130, false),
    ] {
        for item in leader_menu(&avail) {
            let found = find(&|r| r.context == Context::Leader && r.label == item.name);
            assert!(!found.is_empty(), "menu entry {item:?} has no row");
            displayed.extend(found);
        }
    }
    for host in [CommandHost::Local, CommandHost::Attached] {
        let mut palette = PaletteRegistry::for_host(host);
        palette.populate_from_command_registry(&CommandRegistry::new());
        for entry in palette.all_entries() {
            if let Some(shortcut) = &entry.shortcut {
                let found = find(&|r| &r.long_label() == shortcut);
                assert!(!found.is_empty(), "palette shortcut {shortcut} has no row");
                displayed.extend(found);
            }
        }
    }
    for group in help_groups() {
        for help in group.rows {
            displayed.extend(find(&|r| r.help.is_some_and(|h| h.key == help.key)));
        }
    }
    assert!(
        displayed.len() > 60,
        "the surfaces display {} commands",
        displayed.len()
    );
    validate(table(), &shipped(), &displayed).expect("every displayed command is registered");
}
