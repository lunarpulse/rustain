// Re-export from infrastructure to maintain existing imports.
// The implementation was moved to infrastructure/terminal_info.rs (Story 2-4)
// to allow adapters/cli/doctor.rs to use it without adapter-to-adapter imports.
pub use crate::infrastructure::terminal_info::{ColorCapability, detect_color_capability};
