//! `rustain profile switch <name>` CLI subcommand — Story 8.4 AC-7 + Story 8.6a AC-5.
//!
//! Cross-process IPC for profile switching is the same DEFERRED follow-up as Story
//! 8.1 AC-9 config reload (DF-NNN — cross-process IPC). This stub prints the
//! user-facing message per the CLI not-yet-supported pattern and exits.

use anyhow::Result;

pub async fn run_profile_switch(name: String, start: bool) -> Result<()> {
    if start {
        println!(
            "To start rustain with profile '{}' active, run: rustain --profile {}",
            name, name
        );
        return Ok(());
    }

    println!(
        "No running rustain instance found. To switch profiles in the running TUI, \
         press Ctrl+X, P and pick '{}' from the modal, or type '> {}' in the command palette. \
         To inspect the profile without launching the TUI, run 'rustain profile show {}'.",
        name, name, name
    );
    Ok(())
}
