use std::path::Path;

use crate::domain::models::project_context::ProjectContext;
use crate::domain::ports::PersonaPort;

/// PersonaPort adapter that returns assembled project context as system prompt.
pub struct PersonaAdapter {
    project_context: ProjectContext,
}

impl PersonaAdapter {
    pub fn new(project_context: ProjectContext) -> Self {
        Self { project_context }
    }

    /// Whether any project context files were loaded.
    pub fn has_context(&self) -> bool {
        !self.project_context.files.is_empty()
    }

    /// Number of context files loaded.
    #[allow(dead_code)]
    pub fn file_count(&self) -> usize {
        self.project_context.files.len()
    }

    /// Total characters across all context files.
    pub fn total_chars(&self) -> usize {
        self.project_context.total_chars
    }

    /// Whether any files were truncated or omitted due to budget.
    pub fn is_truncated(&self) -> bool {
        self.project_context.truncated
    }

    /// File paths of all loaded context files.
    pub fn file_paths(&self) -> Vec<&std::path::Path> {
        self.project_context
            .files
            .iter()
            .map(|f| f.path.as_path())
            .collect()
    }
}

impl PersonaPort for PersonaAdapter {
    fn system_prompt(&self, _workspace_path: &Path) -> String {
        self.project_context.assembled_prompt()
    }
}
