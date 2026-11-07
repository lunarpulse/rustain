//! K9 (AC14; D10, D15, D19): the texts the story rewrote are true by value, the
//! launchpad's keys are read from registry rows and never typed, and the retired
//! shapes are gone from the tree. The scanner clauses with their positive
//! controls live in the shared cutover scanner (`conformance_21_4_modal_stack`
//! k6_gate); the literal-key gate (`conformance_21_3_keymap` k12_gate) and the
//! `docs/keyboard.md` equality (`k6_docs`) run unchanged and cover the new
//! files; this file pins what only this story can say.
use rustain::adapters::tui::keymap::{
    self, AvailCtx, Binding, CommandId, Context, Host, Key, render_tips::launchpad_keys,
    table::LeaderCmd, table_chat::ChatCmd, table_input::InputCmd,
};
use rustain::domain::models::visual::DensityMode;

fn row(context: Context, host: Host, command: impl Into<CommandId>) -> &'static Binding {
    let command = command.into();
    keymap::table()
        .iter()
        .find(|row| row.context == context && row.hosts.includes(host) && row.command == command)
        .expect("the registry row")
}

#[test]
fn tab_and_inspect_texts_name_thinking() {
    let tab = row(Context::Chat, Host::Local, ChatCmd::CycleInvocation);
    let help = tab.help.as_ref().expect("Tab help");
    assert_eq!(
        help.desc,
        "Cycle tools, thinking, code, tables (needs 2+); else sidebar focus, else next tab"
    );
    assert!(help.desc.chars().count() <= 83, "the help-row cap");
    let inspect = row(Context::Chat, Host::Local, ChatCmd::Inspect);
    assert_eq!(
        inspect.does,
        "Inspect the focused tool call, thinking, code block or table"
    );
}

#[test]
fn launchpad_keys_are_the_registry_rows_compact_labels() {
    let local = AvailCtx::local(130, DensityMode::Focus);
    let keys = launchpad_keys(Host::Local, &local);
    let compact = |context, host, command: CommandId| row(context, host, command).compact_label();
    assert_eq!(
        keys.sign_in.as_deref(),
        Some(compact(Context::Leader, Host::Local, LeaderCmd::SignIn.into()).as_str())
    );
    assert_eq!(
        keys.mention.as_deref(),
        Some(
            compact(
                Context::Input,
                Host::Local,
                InputCmd::OpenMentionPopup.into()
            )
            .as_str()
        )
    );
    assert_eq!(
        keys.browse.as_deref(),
        Some(compact(Context::Input, Host::Local, InputCmd::OpenSlashPopup.into()).as_str())
    );
    assert_eq!(
        keys.palette.as_deref(),
        Some(compact(Context::Input, Host::Local, InputCmd::OpenPalette.into()).as_str())
    );
    let item = |context, command: CommandId| {
        let binding = row(context, Host::Local, command);
        format!("{} {}", binding.compact_label(), binding.label)
    };
    assert_eq!(
        keys.items,
        [
            item(Context::Leader, LeaderCmd::Help.into()),
            item(Context::Input, InputCmd::OpenPalette.into()),
            item(Context::Leader, LeaderCmd::Model.into()),
            item(Context::Leader, LeaderCmd::History.into()),
        ]
    );
    // The spelled-out chords are the registry's own, not typed here.
    assert_eq!(keys.sign_in.as_deref(), Some("^X i"));
    assert_eq!(keys.mention.as_deref(), Some("@"));
    assert_eq!(Key::Char('@').label(), "@");
}

#[test]
fn launchpad_keys_follow_availability_by_host_and_width() {
    let narrow = launchpad_keys(Host::Local, &AvailCtx::local(80, DensityMode::Focus));
    assert_eq!(
        narrow.items.len(),
        3,
        "History needs the sidebar column: {narrow:?}"
    );
    assert!(narrow.items.iter().all(|item| !item.contains("History")));
    let attached = launchpad_keys(Host::Attached, &AvailCtx::attached(130, false));
    // `@` is local-only; the model selector and History are not attached keys.
    assert_eq!(attached.mention, None);
    assert_eq!(attached.sign_in, None);
    assert_eq!(attached.browse.as_deref(), Some("/"));
    assert_eq!(attached.items, ["^X ? Help", "^P Commands"]);
}

#[test]
fn the_receipt_bound_strings_are_verbatim_and_never_say_ready() {
    use rustain::adapters::tui::launchpad::{START, WELCOME};
    assert_eq!(WELCOME, "Welcome to Rustain.");
    assert_eq!(START, "Type a message to start.");
    assert!(!WELCOME.contains("Ready") && !START.contains("Ready"));
}

#[test]
fn the_retired_files_and_shapes_are_gone() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root
            .join("src/adapters/tui/widgets/empty_state.rs")
            .exists(),
        "the two-line welcome widget is deleted (D19)"
    );
    let block =
        std::fs::read_to_string(root.join("src/adapters/tui/widgets/tool_block.rs")).unwrap();
    for retired in ["fn tool_summary", "running...", " → "] {
        assert!(
            !block
                .split("#[cfg(test)]")
                .next()
                .unwrap()
                .contains(retired),
            "{retired:?} is retired from the tool block"
        );
    }
}
