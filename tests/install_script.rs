//! rel-4: the published installer `dist/install.sh` against its sources of truth,
//! and `tests/install_script/harness.sh` (stubbed downloads, no network) under the
//! POSIX shells this host has. ⛔ Reads only files inside `rustain/` (CI has no
//! outer repository).

use std::path::Path;
use std::process::Command;

const SCRIPT: &str = include_str!("../dist/install.sh");
const PAGE: &str = include_str!("../docs/install-macos.md");
const README: &str = include_str!("../README.md");
const TRUST: &str = include_str!("../src/adapters/self_update/trust.rs");
const TYPES: &str = include_str!("../src/adapters/self_update/types.rs");
const ONE_LINER: &str = "curl --proto '=https' --tlsv1.2 -fsSL https://github.com/lunarpulse/rustain/releases/latest/download/install.sh | sh";
/// PASS lines one harness run prints. A harness that silently drops a case fails
/// here; adding a case means raising this number in the same change.
const HARNESS_CHECKS: usize = 126;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The value of the one `NAME=` line at column 0.
fn assignment<'a>(text: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name}=");
    let mut values = text
        .lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()));
    let value = values.next().unwrap_or_else(|| panic!("no `{name}=` line"));
    assert!(values.next().is_none(), "`{name}=` appears more than once");
    value
}

fn rust_str_const<'a>(source: &'a str, name: &str) -> &'a str {
    let prefix = format!("const {name}: &str = \"");
    source
        .lines()
        .find_map(|line| line.split_once(prefix.as_str())?.1.strip_suffix("\";"))
        .unwrap_or_else(|| panic!("no `{name}` const"))
}

/// The page's step 2 block: the ```sh block with a line that is exactly `(`.
fn page_block() -> Vec<&'static str> {
    let mut blocks = Vec::new();
    let mut current: Option<Vec<&str>> = None;
    for line in PAGE.lines() {
        match (&mut current, line) {
            (None, "```sh") => current = Some(Vec::new()),
            (Some(_), "```") => blocks.push(current.take().unwrap()),
            (Some(block), _) => block.push(line),
            (None, _) => {}
        }
    }
    let mut matching = blocks.into_iter().filter(|b| b.contains(&"("));
    let block = matching.next().expect("the step 2 block");
    assert!(matching.next().is_none(), "two step 2 blocks");
    block
}

/// The exact-record `awk` program, trimmed: from `awk -v a="$ASSET" '` up to (not
/// including) the line that starts with `' SHA256SUMS > "$ASSET.sha256"`.
fn awk_program<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let program: Vec<&str> = lines
        .into_iter()
        .map(str::trim)
        .skip_while(|line| *line != "awk -v a=\"$ASSET\" '")
        .collect();
    let end = program
        .iter()
        .position(|line| line.starts_with("' SHA256SUMS > \"$ASSET.sha256\""))
        .expect("end of the awk program");
    program[..end].to_vec()
}

#[test]
fn k1_default_version_and_page_version_are_the_crate_version() {
    assert_eq!(
        assignment(SCRIPT, "DEFAULT_VERSION"),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(
        assignment(&page_block().join("\n"), "VERSION"),
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn k2_script_and_page_pin_the_release_key_and_repository() {
    let key = rust_str_const(TRUST, "PROD_KEY_1");
    assert_eq!(assignment(SCRIPT, "PROD_KEY_1"), key);
    assert!(page_block().contains(&format!("    -P {key}").as_str()));
    let repo = format!(
        "https://github.com/{}",
        rust_str_const(TYPES, "GH_OWNER_REPO")
    );
    assert_eq!(assignment(SCRIPT, "REPO_URL"), repo);
    assert!(
        page_block()
            .contains(&format!("  URL=\"{repo}/releases/download/v${{VERSION}}\"").as_str())
    );
    assert!(SCRIPT.contains("  BASE_URL=$REPO_URL/releases/download/v$VERSION\n"));
}

#[test]
fn k3_record_selection_is_the_pages_awk_program() {
    let page = page_block();
    assert_eq!(
        awk_program(SCRIPT.lines()),
        awk_program(page.iter().copied())
    );
}

#[test]
fn k4_the_one_liner_is_documented_once_in_readme_page_and_script() {
    for (name, text) in [("README.md", README), ("docs/install-macos.md", PAGE)] {
        assert_eq!(
            text.lines().filter(|l| *l == ONE_LINER).count(),
            1,
            "{name}"
        );
    }
    assert!(SCRIPT.lines().any(|l| l == format!("#   {ONE_LINER}")));
}

/// Shape rules a parse check cannot see. The script runs from `curl | sh`: every
/// command lives in a function and the last line calls `main`, so a truncated
/// download defines functions and runs nothing.
#[test]
fn k5_script_shape_https_only_and_posix() {
    let code: Vec<&str> = SCRIPT
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(code.last(), Some(&"main \"$@\""));
    let mut depth = 0usize;
    for line in &code[..code.len() - 1] {
        if depth == 0 {
            let allowed = *line == "set -eu"
                || line.ends_with("() {")
                || line.starts_with("trap ")
                || (line.split_once('=').is_some_and(|(name, _)| {
                    !name.is_empty()
                        && name
                            .bytes()
                            .all(|b| b.is_ascii_uppercase() || b == b'_' || b.is_ascii_digit())
                }));
            assert!(allowed, "top-level command outside a function: {line}");
        }
        if line.ends_with("() {") {
            depth += 1;
        } else if *line == "}" {
            depth -= 1;
        }
    }
    let curls: Vec<&&str> = code
        .iter()
        .filter(|l| l.trim_start().starts_with("curl "))
        .collect();
    assert_eq!(curls.len(), 1, "one download call site: {curls:?}");
    assert!(
        curls[0].contains(
            "curl --proto '=https' --proto-redir '=https' --tlsv1.2 --max-redirs 5 -fsSL"
        )
    );
    assert_eq!(
        code.iter()
            .filter(|l| l.contains("mktemp -d \"$BIN_DIR/.rustain-install.XXXXXX\""))
            .count(),
        1
    );
    assert_eq!(
        code.iter()
            .filter(|l| l.trim_start().starts_with("mv "))
            .count(),
        1
    );
    for line in &code {
        assert!(
            !line.contains(">>"),
            "dist/install.sh appends to a file: {line}"
        );
        for word in ["eval ", "sudo "] {
            assert!(
                !line.trim_start().starts_with(word),
                "dist/install.sh: `{word}` in {line}"
            );
        }
    }
    let shell_files = std::iter::once(root().join("dist/install.sh"))
        .chain(std::iter::once(
            root().join("tests/install_script/harness.sh"),
        ))
        .chain(
            std::fs::read_dir(root().join("tests/install_script/stubs"))
                .expect("stubs")
                .map(|e| e.expect("stub").path()),
        );
    for path in shell_files {
        let text = std::fs::read_to_string(&path).expect("shell file");
        assert!(text.starts_with("#!/bin/sh\n"), "{}", path.display());
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let statement = line.trim_start();
            for bashism in [
                "[[ ", "$'", "<<<", "&>", "pipefail", "echo -e", "$RANDOM", "${!",
            ] {
                assert!(
                    !line.contains(bashism),
                    "{}: `{bashism}` in {line}",
                    path.display()
                );
            }
            for word in ["local ", "function ", "source ", "declare ", "typeset "] {
                assert!(
                    !statement.starts_with(word),
                    "{}: `{word}` in {line}",
                    path.display()
                );
            }
        }
    }
}

/// The harness, under every POSIX shell this host has. `sh` and `bash --posix`
/// are required; `dash` and `zsh` run when installed (the macOS CI job runs the
/// harness under macOS `/bin/sh` and `zsh` directly).
#[test]
#[cfg(unix)]
fn k6_harness_passes_under_each_shell() {
    let have = |name: &str| {
        Command::new("sh")
            .args(["-c", &format!("command -v {name}")])
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let mut shells = vec!["sh".to_owned(), "bash --posix".to_owned()];
    for optional in ["dash", "zsh"] {
        if have(optional) {
            shells.push(optional.to_owned());
        } else {
            eprintln!("install harness: {optional} not installed, not run");
        }
    }
    for shell in shells {
        let out = Command::new("sh")
            .arg("tests/install_script/harness.sh")
            .current_dir(root())
            .env("INSTALL_SH", &shell)
            .output()
            .expect("run harness");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let passes = stdout.lines().filter(|l| l.starts_with("PASS ")).count();
        assert!(
            out.status.success() && passes == HARNESS_CHECKS && !stdout.contains("\nFAIL "),
            "{shell}: {passes}/{HARNESS_CHECKS} PASS\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
