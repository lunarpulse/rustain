//! Slash grammar through the standalone production submission and Enter paths.
use rustain::adapters::tui::app::{InputAction, handle_input, submit_message_for_test};
use rustain::adapters::tui::state::{ResolvedMention, TuiState};
use rustain::domain::events::{DomainInputEvent, DomainKey};
use rustain::domain::models::{FileContextProvenance, FocusState};

fn invocation_forms<'a>(name: &str, arg: &'a str) -> [(String, Option<&'a str>); 5] {
    [
        (format!("/{name} {arg}"), Some(arg)),
        (format!("  /{name}  {arg} "), Some(arg)),
        (format!("/{name}\t{arg}"), Some(arg)),
        (format!("/{name}"), None),
        (format!("/{name}  "), None),
    ]
}

#[test]
fn every_builtin_route_uses_the_shared_boundary() {
    for (name, arg) in [
        ("export", "résumé notes.md"),
        ("login", "openai"),
        ("models", "openai"),
        ("logout", "openai"),
        ("accounts", "openai"),
        ("mode", "plan"),
        ("plan", "on"),
        ("compact", "summarize"),
        ("deactivate", "review-skill"),
        ("context", "show"),
        ("memory", "consolidate"),
        ("fanout", "3 inspect the module"),
        ("team", "log"),
        ("room", "arg"),
        ("artifacts", "arg"),
        ("artifact", "arg"),
        ("peer", "arg"),
        ("new", "ignored"),
        ("clear", "ignored"),
        ("ml", "ignored"),
    ] {
        for (input, args) in invocation_forms(name, arg) {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = input.clone();
            assert_eq!(
                submit_message_for_test(&mut state),
                InputAction::ExecuteCommand {
                    name: name.into(),
                    args: if matches!(name, "new" | "clear" | "ml") {
                        None
                    } else {
                        args.map(str::to_owned)
                    },
                },
                "input: {input:?}"
            );
        }
    }
}

#[test]
fn discovered_skill_and_custom_routes_preserve_suffix_bytes() {
    let rich = "3  inspect\t模块\nthen \"a  b\" \\x";
    for (name, execute) in [
        ("fanout", true),
        ("review-skill", true),
        ("deploy:staging", false),
        ("éclair", false),
        ("Logout", false),
    ] {
        let mut forms = invocation_forms(name, "inspect the module").to_vec();
        forms.push((format!(" \u{2003}/{name}\u{2003}{rich} \t"), Some(rich)));
        for (input, args) in forms {
            let mut state = TuiState::new(80, 24);
            state.skill_name_cache.insert("review-skill".into());
            state.resolved_mentions.push(mention());
            state.input_buffer = input.clone();
            let expected = if execute {
                InputAction::ExecuteCommand {
                    name: name.into(),
                    args: args.map(str::to_owned),
                }
            } else {
                InputAction::SubmitWithContext {
                    text: String::new(),
                    command: Some(name.into()),
                    command_args: args.map(str::to_owned),
                }
            };
            assert_eq!(
                submit_message_for_test(&mut state),
                expected,
                "input: {input:?}"
            );
            if !execute {
                assert!(state.resolved_mentions.is_empty(), "input: {input:?}");
            }
        }
    }
}

fn mention() -> ResolvedMention {
    ResolvedMention {
        path: "Cargo.toml".into(),
        source: FileContextProvenance::UserProvided,
    }
}

#[test]
fn malformed_slashes_preserve_literal_and_resolved_mention_fallbacks() {
    for input in [
        "/ logout openai",
        "/\tlogin anthropic",
        "/\nlogout openai",
        "/\u{2003}accounts openai",
        "/",
        "/  ",
        "say /logout openai",
    ] {
        for with_mention in [false, true] {
            let mut state = TuiState::new(80, 24);
            state.input_buffer = input.into();
            if with_mention {
                state.resolved_mentions.push(mention());
            }
            let expected = if with_mention {
                InputAction::SubmitWithContext {
                    text: input.into(),
                    command: None,
                    command_args: None,
                }
            } else {
                InputAction::SubmitMessage(input.into())
            };
            assert_eq!(
                submit_message_for_test(&mut state),
                expected,
                "input: {input:?}"
            );
            if with_mention {
                assert_eq!(state.resolved_mentions.len(), 1, "input: {input:?}");
                assert_eq!(
                    state.resolved_mentions[0].path, "Cargo.toml",
                    "input: {input:?}"
                );
                assert_eq!(
                    state.resolved_mentions[0].source,
                    FileContextProvenance::UserProvided,
                    "input: {input:?}"
                );
            }
        }
    }
}

#[test]
fn submission_resets_composer_but_keeps_original_history() {
    for input in ["  /logout openai  ", "/ logout openai", "  ordinary text  "] {
        let mut state = TuiState::new(80, 24);
        state.input_history.push("previous".into());
        state.input_history.navigate_up("draft");
        state.input_buffer = input.into();
        state.cursor_position = input.chars().count();
        state.input_scroll_offset = 2;
        state.autocomplete.active = true;
        state.needs_redraw = false;
        submit_message_for_test(&mut state);
        assert!(state.input_buffer.is_empty(), "input: {input:?}");
        assert_eq!(state.cursor_position, 0, "input: {input:?}");
        assert_eq!(state.input_scroll_offset, 0, "input: {input:?}");
        assert!(!state.autocomplete.active, "input: {input:?}");
        assert!(!state.input_history.is_navigating(), "input: {input:?}");
        assert!(state.needs_redraw, "input: {input:?}");
        assert_eq!(
            state.input_history.navigate_up(""),
            Some(input),
            "input: {input:?}"
        );
    }
}

#[test]
fn actual_enter_classifies_pre_slash_whitespace_without_confirming_retract() {
    for (input, name, args) in [
        ("/team retract peer ri_x", "team", "retract peer ri_x"),
        ("  /team retract peer ri_x", "team", "retract peer ri_x"),
        ("\t/team retract peer ri_x", "team", "retract peer ri_x"),
        (
            "\u{2003}/team retract peer ri_x",
            "team",
            "retract peer ri_x",
        ),
        ("  /logout openai", "logout", "openai"),
        ("  / team retract peer ri_x", "", ""),
    ] {
        let mut state = TuiState::new(80, 24);
        state.focus = FocusState::Input;
        state.multiline_mode = false;
        state.autocomplete.dismiss();
        state.input_buffer = input.into();
        let expected = if name.is_empty() {
            InputAction::SubmitMessage(input.into())
        } else {
            InputAction::ExecuteCommand {
                name: name.into(),
                args: Some(args.into()),
            }
        };
        assert_eq!(
            handle_input(&mut state, &DomainInputEvent::SpecialKey(DomainKey::Enter)),
            expected,
            "input: {input:?}"
        );
        assert!(state.pending_team_retract.is_none(), "input: {input:?}");
        assert_eq!(state.focus, FocusState::Input, "input: {input:?}");
        assert!(state.feedback_blocks.is_empty(), "input: {input:?}");
    }
}
