#[cfg(feature = "meta-search")]
pub mod eval_corpus;
#[cfg(feature = "meta-search")]
pub mod eval_partition;
#[cfg(feature = "meta-search")]
pub mod eval_report_writer;
#[cfg(feature = "meta-search")]
pub mod eval_types;
pub mod stub_subagent;
#[allow(dead_code)] // used only by targets that spawn the fake-mcp-server
pub fn fake_mcp_binary() -> std::path::PathBuf {
    let binary_name = if cfg!(target_os = "windows") {
        "fake-mcp-server.exe"
    } else {
        "fake-mcp-server"
    };
    let exe_dir = std::env::current_exe()
        .expect("current test executable")
        .parent()
        .expect("test executable parent")
        .to_path_buf();
    for candidate in [
        exe_dir.join(binary_name),
        exe_dir.parent().expect("deps parent").join(binary_name),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "fake-mcp-server binary not found near {} — ensure this test lane passes `--features test-fake-mcp`",
        exe_dir.display()
    );
}

use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[allow(dead_code)]
pub fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .clone()
        .content()
        .iter()
        .map(|cell| cell.symbol().chars().next().unwrap_or(' '))
        .collect()
}

/// Story 17.4b (AC2) — await a domain event matching `pred` within `budget`,
/// draining non-matching events. This is the shared correlation-keyed await
/// helper the AC2 assertions require: NO fire-and-assert `sleep`. Returns the
/// matched event, or `None` on timeout / channel close.
#[allow(dead_code)]
pub async fn expect_event_matching<T>(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<T>,
    mut pred: impl FnMut(&T) -> bool,
    budget: std::time::Duration,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) if pred(&event) => return Some(event),
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return None,
        }
    }
}
