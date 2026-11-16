use std::path::Path;
use std::sync::Arc;

use crate::domain::models::project_context::ProjectContext;
use crate::domain::ports::PersonaPort;

/// PersonaPort adapter: an optional persona body (Story C-2: the profile's
/// persona file or an embedded built-in) followed by the assembled project
/// context. With no body the prompt is the project context alone.
pub struct PersonaAdapter {
    body: Option<Arc<str>>,
    project_context: ProjectContext,
}

impl PersonaAdapter {
    pub fn new(project_context: ProjectContext) -> Self {
        Self::with_body(None, project_context)
    }

    pub fn with_body(body: Option<Arc<str>>, project_context: ProjectContext) -> Self {
        Self {
            body,
            project_context,
        }
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
        let context = self.project_context.assembled_prompt();
        match &self.body {
            None => context,
            Some(body) if context.is_empty() => body.to_string(),
            Some(body) => format!("{body}\n\n{context}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::project_context::{ContextFileType, ProjectContextFile};

    fn one_file_context() -> ProjectContext {
        let content = "C2-CTX-LINE".to_string();
        ProjectContext {
            total_chars: content.len(),
            files: vec![ProjectContextFile {
                path: "/ws/CLAUDE.md".into(),
                content,
                priority: 1,
                source_type: ContextFileType::ClaudeMd,
            }],
            truncated: false,
        }
    }

    fn prompt(adapter: &PersonaAdapter) -> String {
        adapter.system_prompt(Path::new("/ws"))
    }

    #[test]
    fn no_body_is_byte_identical_to_the_project_context() {
        let ctx = one_file_context();
        assert_eq!(
            prompt(&PersonaAdapter::new(ctx.clone())),
            ctx.assembled_prompt()
        );
        assert_eq!(
            prompt(&PersonaAdapter::with_body(None, ctx.clone())),
            ctx.assembled_prompt()
        );
        assert_eq!(prompt(&PersonaAdapter::new(ProjectContext::empty())), "");
    }

    #[test]
    fn body_alone_when_the_context_is_empty() {
        let adapter = PersonaAdapter::with_body(Some(Arc::from("BODY")), ProjectContext::empty());
        assert_eq!(prompt(&adapter), "BODY");
    }

    #[test]
    fn body_then_blank_line_then_context() {
        let ctx = one_file_context();
        let adapter = PersonaAdapter::with_body(Some(Arc::from("BODY")), ctx.clone());
        assert_eq!(
            prompt(&adapter),
            format!("BODY\n\n{}", ctx.assembled_prompt())
        );
    }
}
