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
            EditorSuspendError::Io(e) => write!(f, "I/O error: {}", e),
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

    let toml_content =
        toml::to_string_pretty(plan).map_err(|e| EditorSuspendError::Parse(e.to_string()))?;
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

    let mut edited_plan: crate::domain::models::Plan = match toml::from_str(&edited_content) {
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

    edited_plan.id = plan.id.clone();
    edited_plan.host_message_id = plan.host_message_id.clone();
    edited_plan.created_at = plan.created_at;

    Ok(Some(edited_plan))
}
