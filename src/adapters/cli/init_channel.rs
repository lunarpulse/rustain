//! Story C-12 (AC1, AC2): the optional Messaging channel step of `rustain init`.
//!
//! Line-oriented, after the setup questions and before the completion summary;
//! provider sign-in stays the separate last step. The step never reads a
//! channel credential, contacts a service or touches a daemon: it records an
//! allowlist in a profile and names the environment variables the daemon
//! needs. A write needs its own explicit confirmation (default No) — the
//! global overwrite answer is never channel consent — and goes only where
//! [`destination`] authorizes. Choice numbers never shift with the build's
//! features.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;

use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::app_credentials::{
    SLACK_APP_TOKEN_ENV, SLACK_BOT_TOKEN_ENV, TELEGRAM_BOT_TOKEN_ENV,
};
use crate::domain::services::channel_allowlist::{
    AllowlistEntryError, AllowlistError, parse_slack_user_ids, parse_telegram_chat_ids,
};

mod destination;

use destination::{DerivedProfile, Destination, Refusal};
pub use destination::{Target, inspect};

/// One external channel. Its menu number is fixed (AC1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelChoice {
    Telegram,
    Slack,
}

impl ChannelChoice {
    /// The `[channels] adapter` name.
    pub const fn adapter(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Slack => "slack",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Slack => "Slack",
        }
    }

    /// The daemon environment variables this channel reads — names only.
    pub const fn env_names(self) -> &'static [&'static str] {
        match self {
            Self::Telegram => &[TELEGRAM_BOT_TOKEN_ENV],
            Self::Slack => &[SLACK_APP_TOKEN_ENV, SLACK_BOT_TOKEN_ENV],
        }
    }

    const fn number(self) -> u8 {
        match self {
            Self::Telegram => 1,
            Self::Slack => 2,
        }
    }

    /// The cargo feature, as `AdapterCatalog` names it.
    const fn feature(self) -> &'static str {
        self.adapter()
    }

    /// The `[channels.config]` key the daemon reads the allowlist from.
    const fn allowlist_key(self) -> &'static str {
        match self {
            Self::Telegram => "allowed_chat_ids",
            Self::Slack => "allowed_user_ids",
        }
    }

    const fn senders(self) -> &'static str {
        match self {
            Self::Telegram => "DM chat IDs",
            Self::Slack => "app DM user IDs",
        }
    }

    const fn conversations(self) -> &'static str {
        match self {
            Self::Telegram => "DMs",
            Self::Slack => "app DMs",
        }
    }

    const fn transport(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram (bot DMs)",
            Self::Slack => "Slack (app DMs over Socket Mode)",
        }
    }

    const fn id_prompt(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram chat IDs (comma-separated; Enter to go back): ",
            Self::Slack => "Slack user IDs (comma-separated; Enter to go back): ",
        }
    }

    const fn counted(self) -> &'static str {
        match self {
            Self::Telegram => "Allowed Telegram chats",
            Self::Slack => "Allowed Slack users",
        }
    }

    fn from_adapter(name: &str) -> Option<Self> {
        [Self::Telegram, Self::Slack]
            .into_iter()
            .find(|choice| choice.adapter() == name)
    }

    fn env_list(self) -> String {
        self.env_names().join(" and ")
    }
}

/// Which channel features the build has. Injectable so every menu variant is
/// exercised in every build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelFeatures {
    pub telegram: bool,
    pub slack: bool,
}

impl ChannelFeatures {
    /// The features compiled into this binary.
    pub fn compiled() -> Self {
        Self {
            telegram: AdapterCatalog::is_feature_compiled(ChannelChoice::Telegram.feature()),
            slack: AdapterCatalog::is_feature_compiled(ChannelChoice::Slack.feature()),
        }
    }

    const fn has(self, choice: ChannelChoice) -> bool {
        match choice {
            ChannelChoice::Telegram => self.telegram,
            ChannelChoice::Slack => self.slack,
        }
    }
}

/// The channel a profile resolves to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentChannel {
    /// `terminal`: no external channel.
    Terminal,
    Known(ChannelChoice),
    /// A catalogued adapter this step does not set up.
    Other(String),
}

impl CurrentChannel {
    fn of(adapter: Option<&str>) -> Self {
        match adapter {
            None | Some("terminal") => Self::Terminal,
            Some(name) => ChannelChoice::from_adapter(name)
                .map_or_else(|| Self::Other(name.to_string()), Self::Known),
        }
    }

    fn label(&self, features: ChannelFeatures) -> String {
        match self {
            Self::Terminal => "none".to_string(),
            Self::Known(choice) if features.has(*choice) => choice.label().to_string(),
            Self::Known(choice) => format!("{} (unavailable in this build)", choice.label()),
            Self::Other(name) => name.clone(),
        }
    }
}

/// A validated, deduplicated allowlist of one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Allowlist {
    /// Positive decimal chat IDs, saved as TOML integers.
    Telegram(Vec<i64>),
    /// Canonical sender user IDs, case preserved, saved as strings.
    Slack(Vec<String>),
}

impl Allowlist {
    /// `Ok(None)` for a blank line (go back).
    fn parse(choice: ChannelChoice, line: &str) -> Result<Option<Self>, AllowlistError> {
        let list = match choice {
            ChannelChoice::Telegram => Self::Telegram(parse_telegram_chat_ids(line)?),
            ChannelChoice::Slack => Self::Slack(parse_slack_user_ids(line)?),
        };
        Ok((list.count() > 0).then_some(list))
    }

    const fn choice(&self) -> ChannelChoice {
        match self {
            Self::Telegram(_) => ChannelChoice::Telegram,
            Self::Slack(_) => ChannelChoice::Slack,
        }
    }

    fn count(&self) -> usize {
        match self {
            Self::Telegram(ids) => ids.len(),
            Self::Slack(ids) => ids.len(),
        }
    }

    fn to_toml(&self) -> toml::Value {
        match self {
            Self::Telegram(ids) => {
                toml::Value::Array(ids.iter().copied().map(toml::Value::Integer).collect())
            }
            Self::Slack(ids) => {
                toml::Value::Array(ids.iter().cloned().map(toml::Value::String).collect())
            }
        }
    }
}

/// What the step did, for the completion summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOutcome {
    /// No external channel before, none now.
    Skipped,
    /// The existing channel (its label) is unchanged.
    Kept(String),
    /// Written; nothing connected.
    Saved {
        choice: ChannelChoice,
        profile: String,
        path: PathBuf,
    },
    /// The confirmed write failed; the profile is unchanged.
    Failed { reason: String },
}

impl ChannelOutcome {
    fn unchanged(current: &CurrentChannel) -> Self {
        match current {
            CurrentChannel::Terminal => Self::Skipped,
            CurrentChannel::Known(choice) => Self::Kept(choice.label().to_string()),
            CurrentChannel::Other(name) => Self::Kept(name.clone()),
        }
    }

    /// The summary's "Messaging channel:" lines. Environment variables are
    /// named, never read or shown.
    pub fn summary_lines(&self) -> Vec<String> {
        match self {
            Self::Skipped => vec!["  Channel setup skipped.".to_string()],
            Self::Kept(label) => vec![format!("  Existing {label} configuration kept.")],
            Self::Saved {
                choice,
                profile,
                path,
            } => vec![
                format!("  {} settings saved (not connected).", choice.label()),
                format!("  Profile: {profile}"),
                format!("  File: {}", path.display()),
                format!("  Before starting the daemon, set {} in", choice.env_list()),
                "  its environment, then run:".to_string(),
                format!("    rustain --profile {profile} daemon start"),
                "  A supervised daemon needs these variables in its own environment.".to_string(),
                "  A daemon that is already running keeps its old settings until you".to_string(),
                "  restart it with this profile.".to_string(),
            ],
            Self::Failed { reason } => vec![
                "  Channel change not saved (save failed).".to_string(),
                format!("  {reason}"),
                "  The profile's existing channel settings are unchanged.".to_string(),
            ],
        }
    }
}

/// Stdout is not a terminal: skip only this step, without reading (AC1).
pub fn skip_without_terminal<W: Write>(target: &Target, output: &mut W) -> Result<ChannelOutcome> {
    writeln!(
        output,
        "Messaging channel setup skipped (output is not a terminal)."
    )?;
    Ok(ChannelOutcome::unchanged(&target.current))
}

/// Run the step on a terminal: the target profile and its current channel,
/// the fixed menu, ID entry and an explicit confirmation (default No) before
/// the one bounded write. Back, decline, invalid input and end of input before
/// that confirmation leave every profile byte unchanged.
pub fn run_channel_step<R: BufRead, W: Write>(
    target: &Target,
    features: ChannelFeatures,
    input: &mut R,
    output: &mut W,
) -> Result<ChannelOutcome> {
    let unchanged = ChannelOutcome::unchanged(&target.current);
    writeln!(output)?;
    writeln!(
        output,
        "Messaging channel (optional) — profile: {}",
        target.requested
    )?;
    writeln!(
        output,
        "Current channel: {}",
        target.current.label(features)
    )?;
    let (profile, path) = match &target.destination {
        Destination::User(edit) => (target.requested.clone(), edit.path.clone()),
        Destination::Derived(derived) => (derived.name.clone(), derived.path.clone()),
        Destination::Refused(refusal) => {
            write_lines(output, &refusal_lines(refusal, &target.requested))?;
            return Ok(unchanged);
        }
    };
    if !features.telegram && !features.slack {
        write_lines(
            output,
            &[
                "Telegram and Slack are unavailable in this build (they need the",
                "telegram or slack feature). Channel settings were not changed.",
            ],
        )?;
        return Ok(unchanged);
    }

    let mut new_profile_accepted = false;
    loop {
        write_lines(output, &menu_lines(&target.current, features))?;
        let Some(choice) = select(input, output)? else {
            return Ok(unchanged);
        };
        if !features.has(choice) {
            write_lines(output, &unavailable_lines(choice))?;
            continue;
        }
        if let Destination::User(edit) = &target.destination
            && let Err(key) = edit.siblings(&target.current, choice)
        {
            write_lines(output, &inherited_credential_lines(choice, &key))?;
            continue;
        }
        if let Destination::Derived(derived) = &target.destination
            && !new_profile_accepted
        {
            write_lines(output, &derived_lines(derived))?;
            let question = format!("Use new profile '{}' for this channel?", derived.name);
            if !confirm(&question, input, output)? {
                writeln!(output, "Channel settings were not changed.")?;
                return Ok(unchanged);
            }
            new_profile_accepted = true;
        }
        let list = match read_allowlist(choice, input, output)? {
            Entry::List(list) => list,
            Entry::Back => continue,
            Entry::End => return Ok(unchanged),
        };
        writeln!(output)?;
        writeln!(output, "Review:")?;
        writeln!(output, "  Adapter: {}", choice.transport())?;
        match &target.destination {
            Destination::Derived(_) => writeln!(output, "  Profile: {profile} (new)")?,
            _ => writeln!(output, "  Profile: {profile}")?,
        }
        writeln!(output, "  File: {}", path.display())?;
        writeln!(output, "  {}: {}", choice.counted(), list.count())?;
        let question = match &target.destination {
            Destination::Derived(_) => format!("Create '{profile}' with these channel settings?"),
            _ => "Save, replacing this profile's channel settings?".to_string(),
        };
        if !confirm(&question, input, output)? {
            writeln!(output, "Channel settings were not changed.")?;
            return Ok(unchanged);
        }
        return Ok(match destination::save(target, &list) {
            Ok(()) => ChannelOutcome::Saved {
                choice,
                profile,
                path,
            },
            Err(error) => ChannelOutcome::Failed {
                reason: error.to_string(),
            },
        });
    }
}

fn write_lines<W: Write, S: AsRef<str>>(output: &mut W, lines: &[S]) -> std::io::Result<()> {
    for line in lines {
        writeln!(output, "{}", line.as_ref())?;
    }
    Ok(())
}

/// The three fixed rows: 1 Telegram, 2 Slack, 3 skip or keep (Enter).
fn menu_lines(current: &CurrentChannel, features: ChannelFeatures) -> Vec<String> {
    let row = |choice: ChannelChoice| {
        let number = choice.number();
        let label = choice.label();
        if !features.has(choice) {
            return format!(
                "  {number}. {label} — unavailable in this build (requires the {} feature)",
                choice.feature()
            );
        }
        match current {
            CurrentChannel::Known(existing) if *existing == choice => {
                format!("  {number}. {label} — edit allowed {}", choice.senders())
            }
            CurrentChannel::Known(existing) => format!(
                "  {number}. {label} — replace {} with {label} {}",
                existing.label(),
                choice.conversations()
            ),
            _ => format!("  {number}. {label} — allowed {}", choice.senders()),
        }
    };
    let last = match current {
        CurrentChannel::Terminal => "  3. Skip [Enter]",
        _ => "  3. Keep existing [Enter]",
    };
    vec![
        row(ChannelChoice::Telegram),
        row(ChannelChoice::Slack),
        last.to_string(),
    ]
}

fn read_answer<R: BufRead>(input: &mut R) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    Ok((input.read_line(&mut line)? > 0).then_some(line))
}

/// `None`: Enter, `3` or end of input.
fn select<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> Result<Option<ChannelChoice>> {
    loop {
        write!(output, "Selection [3]: ")?;
        output.flush()?;
        let Some(line) = read_answer(input)? else {
            return Ok(None);
        };
        match line.trim() {
            "" | "3" => return Ok(None),
            "1" => return Ok(Some(ChannelChoice::Telegram)),
            "2" => return Ok(Some(ChannelChoice::Slack)),
            _ => writeln!(output, "Choose 1, 2, or 3.")?,
        }
    }
}

/// Yes only for an answer starting with `y`; Enter and end of input are No.
fn confirm<R: BufRead, W: Write>(question: &str, input: &mut R, output: &mut W) -> Result<bool> {
    write!(output, "{question} [y/N] ")?;
    output.flush()?;
    Ok(read_answer(input)?.is_some_and(|answer| answer.trim().starts_with(['y', 'Y'])))
}

enum Entry {
    List(Allowlist),
    Back,
    End,
}

fn read_allowlist<R: BufRead, W: Write>(
    choice: ChannelChoice,
    input: &mut R,
    output: &mut W,
) -> Result<Entry> {
    loop {
        write!(output, "{}", choice.id_prompt())?;
        output.flush()?;
        let Some(line) = read_answer(input)? else {
            return Ok(Entry::End);
        };
        match Allowlist::parse(choice, &line) {
            Ok(Some(list)) => return Ok(Entry::List(list)),
            Ok(None) => return Ok(Entry::Back),
            Err(error) => writeln!(output, "{}", entry_error(choice, error))?,
        }
    }
}

/// A corrective message naming only the entry's position, never its text.
fn entry_error(choice: ChannelChoice, error: AllowlistError) -> String {
    let n = error.index;
    match (choice, error.kind) {
        (_, AllowlistEntryError::Empty) => format!("Entry {n} is empty; remove the extra comma."),
        (_, AllowlistEntryError::Wildcard) => {
            format!("Entry {n} is a wildcard; list each ID instead.")
        }
        (ChannelChoice::Slack, AllowlistEntryError::ConversationId) => {
            format!("Entry {n} must be a Slack user ID, not a channel ID.")
        }
        (ChannelChoice::Slack, _) => {
            format!("Entry {n} must be a Slack user ID (U or W, then capitals and digits).")
        }
        (ChannelChoice::Telegram, AllowlistEntryError::OutOfRange) => {
            format!("Entry {n} must be a positive chat ID; group IDs are not accepted.")
        }
        (ChannelChoice::Telegram, _) => format!("Entry {n} must be a numeric Telegram chat ID."),
    }
}

fn unavailable_lines(choice: ChannelChoice) -> Vec<String> {
    vec![
        format!(
            "{} is unavailable in this build: it needs rustain built with the",
            choice.label()
        ),
        format!(
            "{feature} feature (cargo build --features {feature}). Nothing changed.",
            feature = choice.feature()
        ),
    ]
}

fn inherited_credential_lines(choice: ChannelChoice, key: &str) -> Vec<String> {
    vec![
        format!(
            "This profile inherits {} settings that hold a token ({key}).",
            choice.label()
        ),
        format!(
            "Init will not copy it. Put the token in {} in the daemon's",
            choice.env_list()
        ),
        format!("environment, remove {key} from the parent profile, then run init again."),
    ]
}

fn derived_lines(derived: &DerivedProfile) -> Vec<String> {
    vec![
        format!(
            "'{}' is a built-in profile; init does not change it.",
            derived.preset
        ),
        "Channel settings can go to a new profile with its other settings,".to_string(),
        "used only when selected with --profile (your default stays the same):".to_string(),
        format!("  {}", derived.name),
    ]
}

/// Why nothing will be written, with the action on its own line(s) so a long
/// wrapped path never hides it.
fn refusal_lines(refusal: &Refusal, requested: &str) -> Vec<String> {
    let retry = "Then run: rustain --profile <name> init".to_string();
    match refusal {
        Refusal::NotFound => vec![
            format!("Profile '{requested}' was not found; no channel was set up."),
            "Create the profile first, or choose another one.".to_string(),
            retry,
        ],
        Refusal::Community { path } => vec![
            format!("Profile '{requested}' is a community profile; init does not edit it:"),
            format!("  {}", path.display()),
            "Copy it to a user profile with another name to change its channel.".to_string(),
            retry,
        ],
        Refusal::Bypassed { path } => vec![
            "This profile file is skipped when profiles load, so init will".to_string(),
            "not change it (unreadable, over 1 MB, or a broken link):".to_string(),
            format!("  {}", path.display()),
            "Fix or remove the file, then run init again.".to_string(),
        ],
        Refusal::Unsafe { path } => vec![
            "Init does not write through a link or to a non-regular file:".to_string(),
            format!("  {}", path.display()),
            "Channel settings were not changed.".to_string(),
        ],
        Refusal::Preview { path } => vec![
            format!("Profile '{requested}' is a preview profile; init does not change it:"),
            format!("  {}", path.display()),
            "Use a non-preview profile for a messaging channel.".to_string(),
            retry,
        ],
        Refusal::ReadOnly { path } => vec![
            "This profile file is read-only, so init will not change it:".to_string(),
            format!("  {}", path.display()),
        ],
        Refusal::InlineCredential { path, adapter, key } => {
            let variables = ChannelChoice::from_adapter(adapter).map_or_else(
                || "the channel's environment variable".to_string(),
                ChannelChoice::env_list,
            );
            vec![
                format!("This profile keeps a channel token inline ({key}) in:"),
                format!("  {}", path.display()),
                format!("Init will not rewrite it. Put the token in {variables} in the"),
                format!(
                    "daemon's environment, delete the {key} line by hand, then run init again."
                ),
            ]
        }
        Refusal::Invalid => {
            vec![format!(
                "Profile '{requested}' did not load; channel setup skipped."
            )]
        }
        Refusal::NameTaken { name } => vec![
            format!("Profile '{name}' already exists, so init will not create it."),
            format!("To change its channel, run: rustain --profile {name} init"),
        ],
        Refusal::BaselineCredential { name } => vec![
            format!("Creating '{name}' would copy a credential from '{requested}',"),
            "so init will not create it. Channel settings were not changed.".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests;
