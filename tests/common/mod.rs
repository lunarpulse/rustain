#[cfg(feature = "meta-search")]
pub mod eval_corpus;
#[cfg(feature = "meta-search")]
pub mod eval_partition;
#[cfg(feature = "meta-search")]
pub mod eval_report_writer;
#[cfg(feature = "meta-search")]
pub mod eval_types;
pub mod stub_subagent;

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
