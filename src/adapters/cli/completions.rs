use std::io::{ErrorKind, Write};

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::aot::{Shell, generate};

use crate::adapters::cli::commands::Cli;

/// Valid characters for `--bin-name`: packaging wrappers/distros always use simple
/// identifiers; shell metacharacters would be embedded unescaped in generated scripts.
static BIN_NAME_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^[A-Za-z0-9._-]+\z").expect("valid bin-name regex")
});

fn validate_bin_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        BIN_NAME_RE.is_match(name),
        "--bin-name must contain only letters, digits, '.', '_', or '-'; got '{name}'"
    );
    Ok(())
}

/// Render a shell completion script for `shell` to stdout (Story 13.3b, FR104).
///
/// Pure: no network, no provider, no filesystem write, no config load.
/// No port abstraction: `generate(...)` is a pure CPU fold over in-process clap
/// data → bytes on stdout. There is no side-effecting collaborator to fake.
pub fn run_completions(shell: Shell, bin_name: Option<String>) -> Result<()> {
    let mut cmd = Cli::command();
    let name: String = match bin_name {
        Some(s) if !s.is_empty() => {
            validate_bin_name(&s).with_context(|| format!("invalid --bin-name '{s}'"))?;
            s
        }
        _ => cmd.get_name().to_string(),
    }; // AC4
    // Generate into a buffer first: clap_complete's generators .unwrap() their
    // writes, so writing straight to a closed pipe (`| head`) would panic (AC-BP).
    let mut buf = Vec::new();
    generate(shell, &mut cmd, name, &mut buf);
    match std::io::stdout().write_all(&buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}
