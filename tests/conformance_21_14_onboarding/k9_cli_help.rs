//! K9 — CLI help (AC15; D20). `rustain --help` and every subcommand help, at every depth, read as
//! user instructions: no internal identifier and no first person, checked through the same
//! function the K8 literal gate uses. The help is RENDERED (short and long) from the real clap
//! tree, so a doc line, an `about`, an `after_help` or a flag help all count.

use clap::{Command, CommandFactory};
use rustain::adapters::cli::commands::Cli;

use super::k8_voice_gate::text_violations;

/// `(path, command)` of the root and every subcommand at every depth, hidden ones included.
fn walk(root: &Command) -> Vec<(Vec<String>, Command)> {
    fn visit(command: &Command, path: &mut Vec<String>, out: &mut Vec<(Vec<String>, Command)>) {
        out.push((path.clone(), command.clone()));
        for sub in command.get_subcommands() {
            path.push(sub.get_name().to_owned());
            visit(sub, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    visit(root, &mut Vec::new(), &mut out);
    out
}

/// Voice violations of the short and long help of every command in `root`'s tree.
fn help_violations(root: &Command) -> Vec<String> {
    let mut root = root.clone();
    root.build();
    let bin = root.get_name().to_owned();
    let mut violations = Vec::new();
    for (path, mut command) in walk(&root) {
        let name = if path.is_empty() {
            bin.clone()
        } else {
            format!("{bin} {}", path.join(" "))
        };
        violations.extend(text_violations(
            &format!("{name} --help"),
            &command.render_help().to_string(),
        ));
        violations.extend(text_violations(
            &format!("{name} --help (long)"),
            &command.render_long_help().to_string(),
        ));
    }
    violations
}

/// Number of commands under `command`, counted by an independent recursion.
fn count_below(command: &Command) -> usize {
    command
        .get_subcommands()
        .map(|sub| 1 + count_below(sub))
        .sum()
}

fn depth(command: &Command) -> usize {
    command
        .get_subcommands()
        .map(|sub| 1 + depth(sub))
        .max()
        .unwrap_or(0)
}

/// M9's killing test: every command at every depth is rendered and clean, and the walk reaches
/// exactly the commands an independent count finds (a top-level-only walk visits fewer).
#[test]
fn every_subcommand_at_every_depth_is_clean() {
    let mut root = Cli::command();
    root.build();
    let visited = walk(&root);
    assert_eq!(
        visited.len(),
        1 + count_below(&root),
        "the walk visits every command"
    );
    assert!(depth(&root) >= 2, "the real tree has nested subcommands");
    let deepest = visited
        .iter()
        .map(|(path, _)| path.len())
        .max()
        .unwrap_or(0);
    assert_eq!(deepest, depth(&root), "the walk reaches the deepest level");
    let violations = help_violations(&Cli::command());
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

/// Positive control: a synthetic command whose help carries an identifier, a nested one and a
/// first-person one are all caught through the same checker, at the depth they sit at.
#[test]
fn a_synthetic_help_with_an_identifier_or_first_person_is_caught() {
    let tree = Command::new("demo")
        .about("Start it (Story 1.2, FR9)")
        .subcommand(
            Command::new("deep")
                .about("A clean middle command")
                .subcommand(Command::new("leaf").about("I will draft it for you")),
        )
        .subcommand(Command::new("clean").about("Does a plain thing"));
    let violations = help_violations(&tree);
    let text = violations.join("\n");
    assert!(text.contains("demo --help: identifier"), "{text}");
    assert!(
        text.contains("demo deep leaf --help: first person"),
        "{text}"
    );
    assert!(!text.contains("demo clean"), "{text}");
    // A parent's help lists its subcommands' descriptions, so the middle command is caught too.
    assert!(text.contains("demo deep --help: first person"), "{text}");
    let clean = Command::new("demo").about("Plain").subcommand(
        Command::new("run")
            .about("Does a thing")
            .subcommand(Command::new("now").about("Does it now")),
    );
    assert!(
        help_violations(&clean).is_empty(),
        "a clean tree reports nothing"
    );
    // A flag help and an after-help count too.
    let flags = Command::new("demo")
        .about("Plain")
        .arg(
            clap::Arg::new("x")
                .long("x")
                .help("Override it (ADR-09-02)"),
        )
        .after_help("See NFR12 for more");
    let text = help_violations(&flags).join("\n");
    assert!(
        text.contains("identifier") && text.matches("identifier").count() >= 2,
        "{text}"
    );
}

/// Golden check: the rewrite did not empty a help text. Every visible command keeps a
/// non-trivial about, and every parent's help still lists each of its visible subcommands.
#[test]
fn every_listed_subcommand_keeps_its_name_and_a_description_in_help() {
    let mut root = Cli::command();
    root.build();
    for (path, mut command) in walk(&root) {
        let name = path.join(" ");
        let help = command.render_help().to_string();
        for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
            assert!(
                help.contains(sub.get_name()),
                "`rustain {name}` help no longer names `{}`",
                sub.get_name()
            );
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            assert!(
                about.trim().len() >= 12,
                "`{}` lost its description: {about:?}",
                sub.get_name()
            );
        }
        if !path.is_empty() {
            assert!(help.trim().len() >= 40, "`rustain {name}` help is empty");
        }
    }
    for known in ["ask", "daemon", "config", "session", "team", "completions"] {
        assert!(
            root.get_subcommands().any(|s| s.get_name() == known),
            "`{known}` is a top-level command"
        );
    }
}
