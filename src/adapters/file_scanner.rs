use std::path::Path;

/// A file suggestion from workspace scanning.
#[derive(Debug, Clone)]
pub struct FileSuggestion {
    pub relative_path: String,
    pub is_dir: bool,
}

/// Scan workspace files matching a prefix filter.
/// Respects common exclusion patterns and limits depth/count for performance.
pub fn scan_workspace_files(workspace: &Path, prefix: &str, limit: usize) -> Vec<FileSuggestion> {
    let mut results = Vec::new();
    let lower_prefix = prefix.to_lowercase();
    scan_dir(
        workspace,
        workspace,
        &lower_prefix,
        limit,
        0,
        4,
        &mut results,
    );
    results
}

/// Directories to always exclude from file scanning.
const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".rustain",
    ".venv",
    "dist",
    "build",
];

fn scan_dir(
    root: &Path,
    dir: &Path,
    prefix: &str,
    limit: usize,
    depth: usize,
    max_depth: usize,
    results: &mut Vec<FileSuggestion>,
) {
    if depth > max_depth || results.len() >= limit {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    let mut dirs_to_recurse = Vec::new();

    for entry in entries.flatten() {
        if results.len() >= limit {
            return;
        }

        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files/dirs (except when prefix starts with .)
        if file_name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let is_dir = path.is_dir();

        if is_dir {
            // Skip excluded directories
            if EXCLUDED_DIRS.contains(&file_name.as_str()) {
                continue;
            }

            // Add dir if it matches prefix
            if prefix.is_empty() || relative.to_lowercase().contains(prefix) {
                results.push(FileSuggestion {
                    relative_path: format!("{}/", relative),
                    is_dir: true,
                });
            }

            dirs_to_recurse.push(path);
        } else {
            if prefix.is_empty() || relative.to_lowercase().contains(prefix) {
                results.push(FileSuggestion {
                    relative_path: relative,
                    is_dir: false,
                });
            }
        }
    }

    // Recurse into subdirectories
    for sub_dir in dirs_to_recurse {
        if results.len() >= limit {
            return;
        }
        scan_dir(root, &sub_dir, prefix, limit, depth + 1, max_depth, results);
    }
}
