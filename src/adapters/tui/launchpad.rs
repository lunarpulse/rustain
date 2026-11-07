//! The launchpad: the empty conversation's welcome screen (Story 21.9 D15-D18).
//!
//! Pure adapter code, no I/O. [`LaunchpadFacts`] is built once per
//! `(conversation id, model, provider, locked)` by
//! `infrastructure::runtime::launchpad_facts::refresh` and stored on the tab's
//! `TabRenderState`; [`plan`] turns the facts into the rows that fit a given
//! area, and `widgets::launchpad` paints them. Every key text comes from a
//! registry row (`keymap::render_tips::launchpad_keys`).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::adapters::builtin_commands::{self, CommandHost};
use crate::adapters::tui::keymap::render_tips::{LaunchpadKeys, launchpad_keys};
use crate::adapters::tui::keymap::{AvailCtx, Host};
use crate::adapters::tui::text_geometry as geom;
use crate::domain::models::visual::DensityMode;

/// The launchpad column never exceeds this many cells.
pub const MAX_COLUMN: usize = 72;
/// How many recent sessions the launchpad lists.
pub const MAX_RECENT: usize = 3;

/// `Welcome to Rustain.`: receipt-bound text, kept verbatim (RU-21-9-7).
pub const WELCOME: &str = "Welcome to Rustain.";
/// `Type a message to start.`: a canonical wait string in a receipt-bound
/// native driver; never reworded and never accompanied by the word `Ready`.
pub const START: &str = "Type a message to start.";
const ATTACHED_LINE: &str = "Attached to the workspace daemon";
const ASK: &str = "Ask about this project: \"explain this codebase\"";
const PLAN: &str = "/plan on \u{2014} plan before changing files";

/// Whether the active provider can be used right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthState {
    SignedIn,
    NoSignInNeeded,
    NotSignedIn,
    /// A provider the sign-in table does not know: no claim is made.
    Unknown,
}

impl AuthState {
    fn word(self) -> Option<&'static str> {
        match self {
            AuthState::SignedIn => Some("signed in"),
            AuthState::NoSignInNeeded => Some("no sign-in needed"),
            AuthState::NotSignedIn => Some("not signed in"),
            AuthState::Unknown => None,
        }
    }
}

/// One listed past session: a one-line title and its age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    pub title: String,
    pub age: String,
}

/// What a facts value was computed from; a draw compares it, never recomputes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchpadKey {
    pub conversation_id: String,
    pub model: String,
    pub provider_id: String,
    pub locked: bool,
}

impl LaunchpadKey {
    /// Allocation-free comparison with the live values.
    pub fn is(&self, conversation_id: &str, model: &str, provider_id: &str, locked: bool) -> bool {
        self.locked == locked
            && self.conversation_id == conversation_id
            && self.model == model
            && self.provider_id == provider_id
    }
}

/// Everything the launchpad shows besides keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchpadFacts {
    pub host: Host,
    pub workspace: String,
    pub branch: Option<String>,
    pub model: String,
    pub provider: String,
    pub auth: AuthState,
    pub recent: Vec<RecentSession>,
    /// The terminal's width, written every draw (an O(1) store) so the key line
    /// is generated under the width the keymap itself would use.
    pub terminal_width: u16,
    pub key: LaunchpadKey,
}

impl LaunchpadFacts {
    /// The attached host's variant: workspace and branch, the daemon line, no
    /// model, no recent sessions.
    pub fn attached(workspace: String, branch: Option<String>, terminal_width: u16) -> Self {
        Self {
            host: Host::Attached,
            workspace,
            branch,
            model: String::new(),
            provider: String::new(),
            auth: AuthState::Unknown,
            recent: Vec::new(),
            terminal_width,
            key: LaunchpadKey {
                conversation_id: String::new(),
                model: String::new(),
                provider_id: String::new(),
                locked: false,
            },
        }
    }
}

/// The local host's inputs to a refresh: the workspace and a live lock query.
#[derive(Clone)]
pub struct LaunchpadEnv {
    pub workspace: PathBuf,
    is_locked: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl LaunchpadEnv {
    pub fn new(
        workspace: PathBuf,
        is_locked: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            workspace,
            is_locked: Arc::new(is_locked),
        }
    }

    pub fn is_locked(&self, provider_id: &str) -> bool {
        (self.is_locked)(provider_id)
    }
}

impl fmt::Debug for LaunchpadEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchpadEnv")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

/// How a row is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Heading,
    Meta,
    Plain,
    Muted,
    Blank,
}

/// One row of the plan; `text` already fits the column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub kind: RowKind,
}

fn row(text: impl Into<String>, kind: RowKind) -> Row {
    Row {
        text: text.into(),
        kind,
    }
}

/// Fit `text` to `budget` cells, losing its head: `…tail`.
fn ellipsize_head(text: &str, budget: usize) -> String {
    if geom::cells(text) <= budget {
        return text.to_owned();
    }
    match budget {
        0 => String::new(),
        1 => "…".to_owned(),
        _ => format!("…{}", geom::clip_tail(text, budget - 1)),
    }
}

fn ellipsize_tail(text: &str, budget: usize) -> String {
    geom::ellipsize(text, budget, "…").into_owned()
}

/// `<workspace> · <branch>`; the workspace loses its head first.
fn workspace_row(facts: &LaunchpadFacts, column: usize) -> Row {
    let Some(branch) = facts.branch.as_deref() else {
        return row(ellipsize_head(&facts.workspace, column), RowKind::Meta);
    };
    let suffix = format!(" \u{b7} {branch}");
    let room = column.saturating_sub(geom::cells(&suffix));
    if room == 0 {
        return row(ellipsize_tail(branch, column), RowKind::Meta);
    }
    row(
        format!("{}{suffix}", ellipsize_head(&facts.workspace, room)),
        RowKind::Meta,
    )
}

/// `<model> · <provider> · <auth>`; the attached host says where it is attached.
fn model_row(facts: &LaunchpadFacts, column: usize) -> Option<Row> {
    let text = if facts.host == Host::Attached {
        ATTACHED_LINE.to_owned()
    } else {
        let parts: Vec<&str> = [
            Some(facts.model.as_str()),
            Some(facts.provider.as_str()),
            facts.auth.word(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect();
        if parts.is_empty() {
            return None;
        }
        parts.join(" \u{b7} ")
    };
    Some(row(ellipsize_tail(&text, column), RowKind::Meta))
}

/// The three starter actions valid for the host (D15 rows 6-8).
fn starters(facts: &LaunchpadFacts, keys: &LaunchpadKeys, column: usize) -> Vec<Row> {
    let host = match facts.host {
        Host::Local => CommandHost::Local,
        Host::Attached => CommandHost::Attached,
    };
    let first = match (facts.auth, keys.sign_in.as_deref()) {
        (AuthState::NotSignedIn, Some(key)) => Some(format!("{key} Sign in to {}", facts.provider)),
        _ => Some(ASK.to_owned()),
    };
    let second = match facts.host {
        Host::Local => keys
            .mention
            .as_deref()
            .map(|key| format!("{key} Mention a file to add it as context")),
        Host::Attached => keys
            .browse
            .as_deref()
            .map(|key| format!("{key} Browse commands")),
    };
    let third = if builtin_commands::offered_in(host, "plan") {
        Some(PLAN.to_owned())
    } else {
        keys.palette
            .as_deref()
            .map(|key| format!("{key} Open the command palette"))
    };
    [first, second, third]
        .into_iter()
        .flatten()
        .map(|text| {
            row(
                ellipsize_tail(&format!("  \u{2022} {text}"), column),
                RowKind::Plain,
            )
        })
        .collect()
}

fn recent_rows(facts: &LaunchpadFacts, column: usize) -> Vec<Row> {
    facts
        .recent
        .iter()
        .take(MAX_RECENT)
        .map(|session| {
            let suffix = format!(" \u{b7} {}", session.age);
            let room = column.saturating_sub(2 + geom::cells(&suffix));
            let text = if room == 0 {
                ellipsize_tail(&format!("  {}", session.title), column)
            } else {
                format!("  {}{suffix}", ellipsize_tail(&session.title, room))
            };
            row(text, RowKind::Muted)
        })
        .collect()
}

/// The key line: the items that fit whole, in order; the first alone, clipped,
/// when even it does not fit.
fn key_row(keys: &LaunchpadKeys, column: usize) -> Option<Row> {
    keys.items.first()?;
    let mut shown = keys.items.len();
    while shown > 1 && geom::cells(&keys.items[..shown].join(" \u{b7} ")) > column {
        shown -= 1;
    }
    let text = keys.items[..shown].join(" \u{b7} ");

    Some(row(ellipsize_tail(&text, column), RowKind::Muted))
}

/// The column the launchpad uses inside an area `cols` wide.
pub fn column_width(cols: usize) -> usize {
    cols.saturating_sub(2).min(MAX_COLUMN)
}

/// The rows the launchpad paints in an area of `rows` x `cols` cells.
///
/// Fit (D16): `Welcome…`, `Type a message to start.` and the key line are kept
/// first; then the model row, the workspace row, the starters in order, the
/// Recent block (heading with its first entry, then entries newest first); the
/// blank separators come last. Rows keep their D15 order. `facts == None`
/// (a caller that never refreshed) keeps the receipt-bound rows and a local
/// key line only.
pub fn plan(facts: Option<&LaunchpadFacts>, rows: usize, cols: usize) -> Vec<Row> {
    let column = column_width(cols);
    if rows == 0 || column == 0 {
        return Vec::new();
    }
    let (host, width) = match facts {
        Some(facts) => (
            facts.host,
            if facts.terminal_width == 0 {
                cols
            } else {
                usize::from(facts.terminal_width)
            },
        ),
        None => (Host::Local, cols),
    };
    let width = u16::try_from(width).unwrap_or(u16::MAX);
    let avail = match host {
        Host::Local => AvailCtx::local(width, DensityMode::Focus),
        Host::Attached => AvailCtx::attached(width, false),
    };
    let keys = launchpad_keys(host, &avail);

    // Verbatim whenever the column holds them; a terminal narrower than the
    // text ellipsizes like every other row.
    let welcome = row(ellipsize_tail(WELCOME, column), RowKind::Heading);
    let start = row(ellipsize_tail(START, column), RowKind::Meta);
    let key_line = key_row(&keys, column);
    let workspace = facts.map(|facts| workspace_row(facts, column));
    let model = facts.and_then(|facts| model_row(facts, column));
    let starters = facts.map_or_else(Vec::new, |facts| starters(facts, &keys, column));
    let recent = facts.map_or_else(Vec::new, |facts| recent_rows(facts, column));

    let mut left = rows;
    let mut take = |cost: usize| {
        let fits = cost <= left;
        if fits {
            left -= cost;
        }
        fits
    };
    let keep_welcome = take(1);
    let keep_start = take(1);
    let keep_key = key_line.is_some() && take(1);
    let keep_model = model.is_some() && take(1);
    let keep_workspace = workspace.is_some() && take(1);
    let mut kept_starters = 0;
    for _ in &starters {
        if !take(1) {
            break;
        }
        kept_starters += 1;
    }
    let mut kept_recent = 0;
    for index in 0..recent.len() {
        if !take(if index == 0 { 2 } else { 1 }) {
            break;
        }
        kept_recent += 1;
    }
    let blank_after_meta = take(1);
    let blank_before_key = keep_key && take(1);
    let blank_before_recent = kept_recent > 0 && take(1);

    let blank = || row("", RowKind::Blank);
    let mut out = Vec::with_capacity(rows.min(16));
    if keep_welcome {
        out.push(welcome);
    }
    out.extend(workspace.filter(|_| keep_workspace));
    out.extend(model.filter(|_| keep_model));
    if blank_after_meta {
        out.push(blank());
    }
    if keep_start {
        out.push(start);
    }
    out.extend(starters.into_iter().take(kept_starters));
    if blank_before_recent {
        out.push(blank());
    }
    if kept_recent > 0 {
        out.push(row("Recent", RowKind::Muted));
        out.extend(recent.into_iter().take(kept_recent));
    }
    if blank_before_key {
        out.push(blank());
    }
    out.extend(key_line.filter(|_| keep_key));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(items: &[&str]) -> LaunchpadKeys {
        LaunchpadKeys {
            items: items.iter().map(|item| (*item).to_owned()).collect(),
            ..LaunchpadKeys::default()
        }
    }

    #[test]
    fn story21_9_key_row_drops_whole_items_from_the_end() {
        let all = keys(&["^X ? Help", "^P Commands", "^X m Model"]);
        let row = |column| key_row(&all, column).map(|row| row.text);
        assert_eq!(
            row(60).as_deref(),
            Some("^X ? Help \u{b7} ^P Commands \u{b7} ^X m Model")
        );
        assert_eq!(row(30).as_deref(), Some("^X ? Help \u{b7} ^P Commands"));
        assert_eq!(row(12).as_deref(), Some("^X ? Help"));
        // Even the first item is clipped on a grapheme boundary when it must be.
        assert_eq!(row(5).as_deref(), Some("^X ?\u{2026}"));
        assert_eq!(key_row(&keys(&[]), 40), None);
    }

    #[test]
    fn story21_9_column_is_centred_and_capped() {
        assert_eq!(column_width(0), 0);
        assert_eq!(column_width(2), 0);
        assert_eq!(column_width(20), 18);
        assert_eq!(column_width(74), 72);
        assert_eq!(column_width(200), MAX_COLUMN);
    }

    #[test]
    fn story21_9_head_ellipsis_keeps_the_tail() {
        assert_eq!(ellipsize_head("/a/b/c/d", 20), "/a/b/c/d");
        assert_eq!(ellipsize_head("/a/b/c/d", 5), "\u{2026}/c/d");
        assert_eq!(ellipsize_head("/a/b/c/d", 1), "\u{2026}");
        assert_eq!(ellipsize_head("/a/b/c/d", 0), "");
    }

    #[test]
    fn story21_9_plan_without_facts_keeps_the_receipt_bound_rows() {
        let rows: Vec<String> = plan(None, 24, 80).into_iter().map(|row| row.text).collect();
        assert_eq!(rows[0], WELCOME);
        assert!(rows.iter().any(|row| row == START));
        assert!(plan(None, 0, 80).is_empty());
        assert!(plan(None, 24, 2).is_empty());
    }
}
