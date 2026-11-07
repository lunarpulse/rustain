//! Bounded, subprocess-free reader of a workspace's git `HEAD` (Story 21.9 D17).
//!
//! The launchpad names the branch the operator is on. This reads at most two
//! small files (a `.git` pointer file and `HEAD`), never runs `git`, and never
//! trusts what it reads: the branch name is sanitized to one line and anything
//! it cannot place is `None`.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::domain::services::peer_text::sanitize_display_line;

/// How many directories above the workspace are searched for a `.git`.
const MAX_PARENTS: usize = 64;
/// `HEAD` and a `.git` pointer file are a line long; more is not git.
const MAX_READ_BYTES: usize = 4096;

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// `ref: refs/heads/<name>`.
    Branch(String),
    /// A bare object id; the first seven hex digits.
    Detached(String),
}

impl Head {
    /// The text the launchpad shows: the branch, or `detached <7 hex>`.
    pub fn label(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Detached(short) => format!("detached {short}"),
        }
    }
}

/// Read a small regular file. Opened non-blocking and checked to be a regular
/// file before any read, so a FIFO or device planted at `.git` or `HEAD` (an
/// ancestor directory, an extracted archive) can never block the draw thread.
fn read_bounded(path: &Path) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_READ_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The directory holding `HEAD` for the `.git` entry at `dot_git`: itself for a
/// directory, the target of its `gitdir:` line (followed once) for a worktree file.
fn git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let text = read_bounded(dot_git)?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}

fn parse_head(text: &str) -> Option<Head> {
    let line = text.lines().next()?.trim();
    if let Some(name) = line.strip_prefix("ref: refs/heads/") {
        let name = sanitize_display_line(name);
        return (!name.is_empty()).then_some(Head::Branch(name));
    }
    let hex = line.len() == 40 || line.len() == 64;
    (hex && line.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| Head::Detached(line[..7].to_ascii_lowercase()))
}

/// The `HEAD` of the repository containing `workspace`, if there is one.
pub fn read_branch(workspace: &Path) -> Option<Head> {
    let mut dir = Some(workspace);
    for _ in 0..=MAX_PARENTS {
        let current = dir?;
        let dot_git = current.join(".git");
        if dot_git.exists() {
            return parse_head(&read_bounded(&git_dir(&dot_git)?.join("HEAD"))?);
        }
        dir = current.parent();
    }
    None
}
