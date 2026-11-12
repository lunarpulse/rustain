//! K4 — key truth: every key the tour names is a registry row available in its context, and
//! `Context::Tour` is governed like every other context (AC7, D3, D8).
use rustain::adapters::builtin_commands::{self, CommandHost};
use rustain::adapters::command_registry::CommandRegistry;
use rustain::adapters::palette_registry::PaletteRegistry;
use rustain::adapters::tui::keymap::model::shipped_graph;
use rustain::adapters::tui::keymap::table::LeaderCmd;
use rustain::adapters::tui::keymap::table_chat::ChatCmd;
use rustain::adapters::tui::keymap::table_input::InputCmd;
use rustain::adapters::tui::keymap::table_tour::TourCmd;
use rustain::adapters::tui::keymap::{
    AvailCtx, Binding, CommandId, Context, HelpCat, Host, table, validate,
};
use rustain::adapters::tui::tour::{TourStep, copy};

/// The one row `context` binds to `command` that is available now (a focused tool line exists).
fn available_row(context: Context, command: impl Into<CommandId>) -> &'static Binding {
    let command = command.into();
    let mut avail = AvailCtx::local(80);
    avail.stage_target = true;
    table()
        .iter()
        .find(|row| {
            row.context == context
                && row.command == command
                && row.hosts.includes(Host::Local)
                && row.availability(&avail).is_available()
        })
        .unwrap_or_else(|| panic!("no available local row for {command:?} in {context:?}"))
}

fn body(step: TourStep, multiline: bool) -> String {
    copy::body(step, multiline).join(" ")
}

#[test]
fn every_key_a_card_names_is_an_available_registry_row() {
    let named = |step: TourStep, multiline: bool, rows: &[&Binding]| {
        let text = body(step, multiline);
        for row in rows {
            let key = row.long_label();
            assert_ne!(key, "?", "a missing key paints `?`");
            assert!(
                text.contains(&format!(" {key}")),
                "{step:?} names {key}: {text}"
            );
        }
    };
    let skip = available_row(Context::Tour, TourCmd::SkipStep);
    named(
        TourStep::Send,
        false,
        &[available_row(Context::Input, InputCmd::SubmitOrNewline)],
    );
    named(
        TourStep::Send,
        true,
        &[available_row(Context::Leader, LeaderCmd::Send)],
    );
    named(
        TourStep::Inspect,
        false,
        &[
            available_row(Context::Input, InputCmd::ReturnToChat),
            available_row(Context::Chat, ChatCmd::CycleInvocation),
            available_row(Context::Chat, ChatCmd::Inspect),
            skip,
        ],
    );
    named(
        TourStep::Decide,
        false,
        &[available_row(Context::Leader, LeaderCmd::Decisions), skip],
    );
    named(
        TourStep::Hub,
        false,
        &[
            available_row(Context::Leader, LeaderCmd::Agents),
            available_row(Context::Input, InputCmd::OpenPalette),
        ],
    );
    named(
        TourStep::Help,
        false,
        &[available_row(Context::Leader, LeaderCmd::Help)],
    );
    // The notices read the same rows and the built-in catalogue.
    assert_eq!(
        copy::closed_notice(),
        "Tour closed. Run /tour to take it again."
    );
    assert_eq!(
        copy::complete_notice(),
        format!(
            "Tour complete. Help is always one key away: {}.",
            available_row(Context::Leader, LeaderCmd::Help).long_label()
        )
    );
}

#[test]
fn the_rail_names_only_rows_that_can_act_on_the_card() {
    for step in [
        TourStep::Send,
        TourStep::Inspect,
        TourStep::Decide,
        TourStep::Hub,
        TourStep::Help,
    ] {
        let rail = copy::rail_items(step);
        assert_eq!(rail.len(), 3, "{step:?}");
        for (item, command) in
            rail.iter()
                .zip([TourCmd::Continue, TourCmd::SkipStep, TourCmd::Leave])
        {
            let row = available_row(Context::Tour, command);
            assert_eq!(
                item,
                &format!("[{}] {}", row.long_label(), row.label),
                "{step:?}"
            );
        }
    }
    let offer = copy::rail_items(TourStep::Offer);
    assert!(
        offer.iter().all(|item| !item.starts_with("[n]")),
        "{offer:?}"
    );
    assert_eq!(offer.len(), 2);
}

#[test]
fn the_tour_context_is_governed_local_and_in_general_help() {
    validate(table(), &shipped_graph(), &[]).expect("the shipped table is governed");
    assert!(Context::ALL.contains(&Context::Tour));
    assert_eq!(Context::Tour.parents(), &[] as &[Context]);
    let rows: Vec<&Binding> = table()
        .iter()
        .filter(|row| row.context == Context::Tour)
        .collect();
    assert_eq!(rows.len(), 7);
    assert!(
        rows.iter()
            .all(|row| row.hosts.includes(Host::Local) && !row.hosts.includes(Host::Attached))
    );
    for row in rows.iter().filter_map(|row| row.help) {
        assert_eq!(row.cat, HelpCat::General, "{}", row.key);
    }
    assert_eq!(HelpCat::ALL.len(), 10, "no new help category");
    // Every key of the card is portable: reachable on a plain terminal.
    assert!(rows.iter().all(|row| row.pty_bytes().is_some()));
}

#[test]
fn the_tour_command_is_local_only_in_every_discovery_surface() {
    assert!(builtin_commands::offered_in(CommandHost::Local, "tour"));
    assert!(!builtin_commands::offered_in(CommandHost::Attached, "tour"));
    let commands = CommandRegistry::new();
    let defs = commands.filter("tour");
    let def = defs
        .iter()
        .find(|def| def.name == "tour")
        .expect("the catalogue lists /tour");
    assert_eq!(def.description, "Take the two-minute tour");
    assert!(
        def.usage.iter().any(|row| row.1 == "/tour" && row.3),
        "help's slash rows list it"
    );
    let mut local = PaletteRegistry::for_host(CommandHost::Local);
    local.populate_from_command_registry(&commands);
    assert!(
        local
            .all_entries()
            .iter()
            .any(|entry| entry.name == "/tour")
    );
    let mut attached = PaletteRegistry::for_host(CommandHost::Attached);
    attached.populate_from_command_registry(&commands);
    assert!(
        attached
            .all_entries()
            .iter()
            .all(|entry| entry.name != "/tour")
    );
}
