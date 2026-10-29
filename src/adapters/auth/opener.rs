//! Best-effort system browser launch shared by the CLI and TUI login hosts.

/// Hands `url` to the platform opener with null stdio; a spawn failure is
/// ignored (the URL is always shown for manual use).
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
