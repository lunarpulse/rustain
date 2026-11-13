#[derive(Debug)]
pub enum EditorSuspendError {
    Io(std::io::Error),
    EditorNonZeroExit,
    Spawn(std::io::Error),
    Parse(String),
}

impl std::fmt::Display for EditorSuspendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditorSuspendError::Io(e) => write!(f, "Could not run the editor: {}", e),
            EditorSuspendError::EditorNonZeroExit => {
                write!(f, "Editor exited with non-zero status")
            }
            EditorSuspendError::Spawn(e) => write!(f, "Failed to spawn editor: {}", e),
            EditorSuspendError::Parse(s) => write!(f, "Parse error: {}", s),
        }
    }
}

impl std::error::Error for EditorSuspendError {}

pub fn suspend_terminal<F: FnOnce() -> std::io::Result<std::process::ExitStatus>>(
    f: F,
) -> Result<std::process::ExitStatus, EditorSuspendError> {
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(
        out,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    );
    let _ = crossterm::terminal::disable_raw_mode();

    let _restore = scopeguard::guard((), |_| {
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        );
    });

    let result = f();

    drop(_restore);

    result.map_err(EditorSuspendError::Spawn)
}

/// Spawn an editor on a file path and return its exit status.
/// Shared guts for both 6-0d's `[e]` Revise and 6-1a's `[e]` Edit.
pub fn run_editor_on_path(
    path: &std::path::Path,
) -> Result<std::process::ExitStatus, EditorSuspendError> {
    suspend_terminal(|| {
        let editor = crate::infrastructure::utils::env_var_trimmed("EDITOR")
            .unwrap_or_else(|| "vi".to_string());
        let parts = shell_words::split(&editor).unwrap_or_else(|_| vec![editor.clone()]);
        if parts.is_empty() {
            return std::process::Command::new("vi").arg(path).status();
        }
        let mut cmd = std::process::Command::new(&parts[0]);
        for arg in &parts[1..] {
            cmd.arg(arg);
        }
        cmd.arg(path).status()
    })
}

pub async fn run_editor_on_plan(
    plan: &crate::domain::models::Plan,
) -> Result<Option<crate::domain::models::Plan>, EditorSuspendError> {
    let temp_dir = std::env::temp_dir();
    let temp_path = temp_dir.join(format!("rustain-plan-{}.toml", plan.id));

    let toml_content = toml::to_string_pretty(&editor_view(plan))
        .map_err(|e| EditorSuspendError::Parse(e.to_string()))?;
    tokio::fs::write(&temp_path, &toml_content)
        .await
        .map_err(EditorSuspendError::Io)?;

    let path_clone = temp_path.clone();
    let exit_result = run_editor_on_path(&path_clone);

    match exit_result {
        Ok(status) => {
            if !status.success() {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return Err(EditorSuspendError::EditorNonZeroExit);
            }
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e);
        }
    }

    let edited_content = match tokio::fs::read_to_string(&temp_path).await {
        Ok(c) => c,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(EditorSuspendError::Io(e));
        }
    };

    let _ = tokio::fs::remove_file(&temp_path).await;

    let edited_plan: crate::domain::models::Plan = match toml::from_str(&edited_content) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("Plan edit parse error: {}", e);
            return Ok(None);
        }
    };

    if let Err(e) = crate::domain::services::plan_parser::validate_plan(&edited_plan) {
        tracing::warn!("Plan edit validation error: {}", e);
        return Ok(None);
    }

    Ok(Some(merge_edited_plan(plan, edited_plan)))
}

/// The plan as the editor sees it: the proposing call's restrictions are
/// never shown, so nothing typed in the editor can reach them.
pub(crate) fn editor_view(plan: &crate::domain::models::Plan) -> crate::domain::models::Plan {
    crate::domain::models::Plan {
        origin_restrictions: None,
        ..plan.clone()
    }
}

/// Rebuild the identity of an edited plan from the original. The edited TOML
/// can neither drop nor widen `origin_restrictions` (Story 19.29, R11): the
/// original's value always wins.
pub(crate) fn merge_edited_plan(
    original: &crate::domain::models::Plan,
    mut edited: crate::domain::models::Plan,
) -> crate::domain::models::Plan {
    edited.id = original.id.clone();
    edited.host_message_id = original.host_message_id.clone();
    edited.created_at = original.created_at;
    edited.origin_restrictions = original.origin_restrictions.clone();
    edited
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_14_an_editor_io_failure_says_the_editor_could_not_run() {
        let error = EditorSuspendError::Io(std::io::Error::other("disk full"));
        assert_eq!(error.to_string(), "Could not run the editor: disk full");
    }

    fn plan(
        origin: Option<crate::domain::models::DelegationRestrictions>,
    ) -> crate::domain::models::Plan {
        use crate::domain::models::{PlanStatus, plan::Plan};
        Plan {
            id: "plan-1".into(),
            title: "T".into(),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Editing,
            created_at: 7,
            resolved_at: None,
            host_message_id: Some("host".into()),
            origin_restrictions: origin,
        }
    }

    fn restrictions(items: &[&str]) -> crate::domain::models::DelegationRestrictions {
        crate::domain::models::DelegationRestrictions::new(
            None,
            Some(crate::domain::models::SkillToolRestriction {
                skill_names: vec!["kube".into()],
                items: items.iter().map(|item| (*item).to_string()).collect(),
                inherited: false,
            }),
        )
    }

    /// K-d (Story 19.29, R11): the proposing call's restrictions are never
    /// shown to the editor, and nothing the operator types can drop or widen
    /// them.
    #[test]
    fn story_19_29_editing_a_plan_can_neither_drop_nor_widen_its_origin_restrictions() {
        let original = plan(Some(restrictions(&["Read"])));

        let toml = toml::to_string_pretty(&editor_view(&original)).unwrap();
        assert!(
            !toml.contains("origin"),
            "the editor TOML must not carry the restrictions:\n{toml}"
        );

        // The operator's edit omits the field entirely.
        let edited: crate::domain::models::Plan = toml::from_str(&toml).unwrap();
        assert_eq!(edited.origin_restrictions, None);
        assert_eq!(
            merge_edited_plan(&original, edited).origin_restrictions,
            original.origin_restrictions
        );

        // …or tries to widen it.
        let widened = plan(Some(restrictions(&["Bash", "Read", "Write"])));
        let merged = merge_edited_plan(&original, widened);
        assert_eq!(merged.origin_restrictions, original.origin_restrictions);
        assert_eq!(merged.id, "plan-1");
        assert_eq!(merged.host_message_id.as_deref(), Some("host"));
        assert_eq!(merged.created_at, 7);
    }
}
