//! Story C-12 (AC2): where `rustain init`'s channel step may write, and the
//! bounded writes themselves.
//!
//! Three things are kept apart: the *requested* profile name (startup
//! precedence, taken before init writes anything), the resolver's *actual
//! origin* for that name (user file → community file → embedded preset,
//! mirroring `FileSystemProfileSource`, where an unreadable or oversize file
//! falls through) and the *authorized destination*. Only two destinations
//! exist: a verified writable, non-preview user profile file — only its
//! channel table changes — or, for an embedded preset, a separately named new
//! user profile that materializes the preset's effective baseline with only
//! the channel replaced. Everything else is refused before any prompt, and no
//! fallback (startup's `coding` for a missing name, a community file, a
//! bypassed file) ever becomes a write target.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::adapters::profile_resolver::embedded::{EmbeddedProfileSource, embedded_names};
use crate::adapters::profile_resolver::toml_resolver::{
    FileSystemProfileSource, MAX_PROFILE_SIZE, TomlProfileResolver,
};
use crate::domain::errors::ProfileError;
use crate::domain::models::{PortDimension, ProfileDefinition, ResolvedProfile};
use crate::domain::ports::ProfileResolver;
use crate::domain::services::adapter_catalog::AdapterCatalog;
use crate::domain::services::profile_loader::{ProfileLoader, ProfileSource};
use crate::domain::services::profile_serializer::overrides_to_toml;

use super::{Allowlist, ChannelChoice, CurrentChannel};

/// The seven profile ports in file order.
const PORTS: [(PortDimension, &str); 7] = [
    (PortDimension::Persona, "persona"),
    (PortDimension::Memory, "memory"),
    (PortDimension::Session, "session"),
    (PortDimension::Tools, "tools"),
    (PortDimension::Channels, "channels"),
    (PortDimension::Scheduler, "scheduler"),
    (PortDimension::Context, "context"),
];

/// The requested profile, what it resolves to now, and where a channel edit
/// for it may go.
#[derive(Debug, Clone)]
pub struct Target {
    /// The name startup selected, before init wrote anything.
    pub requested: String,
    /// The channel the profile resolves to now (`terminal` is no channel).
    pub current: CurrentChannel,
    pub destination: Destination,
}

#[derive(Debug, Clone)]
pub enum Destination {
    /// A verified writable, non-preview user profile: edit its channel table.
    User(UserEdit),
    /// An embedded preset: only a separately named new user profile.
    Derived(DerivedProfile),
    /// No write is possible; the step explains why instead of a menu.
    Refused(Refusal),
}

/// An in-place edit of one user profile file.
#[derive(Debug, Clone)]
pub struct UserEdit {
    pub path: PathBuf,
    /// The bytes the edit is based on; a commit publishes only over these.
    original: String,
    /// The effective channel config (the file's own, or inherited).
    current_config: toml::Table,
}

/// A new user profile materialized from an embedded preset.
#[derive(Debug, Clone)]
pub struct DerivedProfile {
    pub preset: String,
    pub name: String,
    pub path: PathBuf,
    profiles_dir: PathBuf,
    /// What the preset resolves to in this build (preview fallbacks applied).
    baseline: Box<ResolvedProfile>,
}

/// Why the step will not write for this profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No user, community or embedded profile has this name.
    NotFound,
    /// A community profile: installed content, never edited by init.
    Community {
        path: PathBuf,
    },
    /// A user or community file exists but the resolver skips it (unreadable,
    /// not UTF-8, over the size limit, a dangling link).
    Bypassed {
        path: PathBuf,
    },
    /// A symbolic link or a non-regular file or directory on the write path.
    Unsafe {
        path: PathBuf,
    },
    /// A user-authored preview profile (own or inherited `preview = true`).
    Preview {
        path: PathBuf,
    },
    ReadOnly {
        path: PathBuf,
    },
    /// The file holds an inline channel credential: never rewritten.
    InlineCredential {
        path: PathBuf,
        adapter: String,
        key: String,
    },
    /// The profile does not load.
    Invalid,
    /// The derived profile's name is already used.
    NameTaken {
        name: String,
    },
    /// Deriving from the preset would copy a credential.
    BaselineCredential {
        name: String,
    },
}

/// A failed commit. Nothing about a failure is written over the existing
/// profile, and no temporary file is left behind.
#[derive(Debug)]
pub enum SaveError {
    /// The file changed after init read it.
    Changed,
    Unsafe(PathBuf),
    Taken(String),
    /// The edit did not read back exactly as intended.
    Unverified,
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => write!(f, "the profile file changed while init was running"),
            Self::Unsafe(path) => write!(
                f,
                "{} is a link or not a regular file or directory",
                path.display()
            ),
            Self::Taken(name) => write!(f, "profile '{name}' was created meanwhile"),
            Self::Unverified => write!(f, "the profile could not be edited safely"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<std::io::Error> for SaveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Resolve `requested` the way startup and the daemon do and decide where a
/// channel edit for it may go. Reads only; never writes.
pub fn inspect(profiles_dir: &Path, requested: &str) -> Target {
    let source = FileSystemProfileSource::new(profiles_dir.to_path_buf());
    let loaded =
        ProfileLoader::new(&AdapterCatalog, &source).load_ignoring_channel_feature(requested);
    let current = loaded.as_ref().map_or(CurrentChannel::Terminal, |profile| {
        CurrentChannel::of(
            profile
                .selection
                .dimensions
                .get(&PortDimension::Channels)
                .map(|reference| reference.adapter.as_str()),
        )
    });
    Target {
        requested: requested.to_string(),
        current,
        destination: destination(profiles_dir, requested, &loaded),
    }
}

/// Write `list` to the target's destination. The caller has the operator's
/// explicit confirmation for exactly this edit.
pub fn save(target: &Target, list: &Allowlist) -> Result<(), SaveError> {
    match &target.destination {
        Destination::User(edit) => save_user(edit, &target.current, list),
        Destination::Derived(derived) => save_derived(derived, list),
        Destination::Refused(_) => Err(SaveError::Unverified),
    }
}

impl UserEdit {
    /// Fields the new channel table keeps besides the allowlist: the effective
    /// config of the same adapter, nothing when the adapter changes. `Err`
    /// names a credential key that keeping them would copy.
    pub fn siblings(
        &self,
        current: &CurrentChannel,
        choice: ChannelChoice,
    ) -> Result<toml::Table, String> {
        if *current != CurrentChannel::Known(choice) {
            return Ok(toml::Table::new());
        }
        let mut kept = self.current_config.clone();
        kept.remove(choice.allowlist_key());
        match credential_key(&toml::Value::Table(kept.clone())) {
            Some(key) => Err(key),
            None => Ok(kept),
        }
    }
}

/// What the resolver does with one candidate file.
enum Read {
    /// No directory entry: the resolver looks further.
    Absent,
    /// The resolver uses this content.
    Used(String),
    /// An entry exists but the resolver skips it.
    Bypassed,
}

/// Mirror `FileSystemProfileSource::get` for one path.
fn read_like_resolver(path: &Path) -> Read {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Read::Absent,
        Err(_) => return Read::Bypassed,
        Ok(_) => {}
    }
    if !path.exists() {
        return Read::Bypassed;
    }
    match std::fs::read_to_string(path) {
        Ok(content) if content.len() <= MAX_PROFILE_SIZE => Read::Used(content),
        _ => Read::Bypassed,
    }
}

fn destination(
    profiles_dir: &Path,
    requested: &str,
    loaded: &Result<ResolvedProfile, ProfileError>,
) -> Destination {
    if requested.is_empty() || requested.contains(['/', '\\']) {
        return Destination::Refused(Refusal::NotFound);
    }
    let user_path = profiles_dir.join(format!("{requested}.toml"));
    match read_like_resolver(&user_path) {
        Read::Used(content) => return user_destination(profiles_dir, user_path, content, loaded),
        Read::Bypassed => return Destination::Refused(Refusal::Bypassed { path: user_path }),
        Read::Absent => {}
    }
    let community_path = profiles_dir
        .join("community")
        .join(format!("{requested}.toml"));
    match read_like_resolver(&community_path) {
        Read::Used(_) => {
            return Destination::Refused(Refusal::Community {
                path: community_path,
            });
        }
        Read::Bypassed => {
            return Destination::Refused(Refusal::Bypassed {
                path: community_path,
            });
        }
        Read::Absent => {}
    }
    if EmbeddedProfileSource.get(requested).is_none() {
        return Destination::Refused(Refusal::NotFound);
    }
    derived_destination(profiles_dir, requested, loaded)
}

fn user_destination(
    profiles_dir: &Path,
    path: PathBuf,
    content: String,
    loaded: &Result<ResolvedProfile, ProfileError>,
) -> Destination {
    if !is_real_dir(profiles_dir) {
        return Destination::Refused(Refusal::Unsafe {
            path: profiles_dir.to_path_buf(),
        });
    }
    if !is_regular_file(&path) {
        return Destination::Refused(Refusal::Unsafe { path });
    }
    let Ok(profile) = loaded else {
        return Destination::Refused(Refusal::Invalid);
    };
    if profile.preview {
        return Destination::Refused(Refusal::Preview { path });
    }
    let writable = std::fs::metadata(&path).is_ok_and(|meta| !meta.permissions().readonly());
    if !writable {
        return Destination::Refused(Refusal::ReadOnly { path });
    }
    let Ok(own) = toml::from_str::<toml::Table>(&content) else {
        return Destination::Refused(Refusal::Invalid);
    };
    let own_channels = own.get("channels");
    if let Some(key) = own_channels
        .and_then(|channels| channels.get("config"))
        .and_then(credential_key)
    {
        let adapter = own_channels
            .and_then(|channels| channels.get("adapter"))
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Destination::Refused(Refusal::InlineCredential { path, adapter, key });
    }
    let current_config = profile
        .selection
        .dimensions
        .get(&PortDimension::Channels)
        .and_then(|reference| reference._config.as_ref())
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    Destination::User(UserEdit {
        path,
        original: content,
        current_config,
    })
}

fn derived_destination(
    profiles_dir: &Path,
    preset: &str,
    loaded: &Result<ResolvedProfile, ProfileError>,
) -> Destination {
    if loaded.is_err() {
        return Destination::Refused(Refusal::Invalid);
    }
    if std::fs::symlink_metadata(profiles_dir).is_ok_and(|meta| !meta.file_type().is_dir()) {
        return Destination::Refused(Refusal::Unsafe {
            path: profiles_dir.to_path_buf(),
        });
    }
    let name = format!("{preset}-channel");
    if name_taken(profiles_dir, &name) {
        return Destination::Refused(Refusal::NameTaken { name });
    }
    // The real resolver: the preset as startup resolves it in this build.
    let Some(baseline) = TomlProfileResolver::new(preset, profiles_dir.to_path_buf())
        .ok()
        .and_then(|resolver| resolver.resolve_active())
    else {
        return Destination::Refused(Refusal::Invalid);
    };
    if baseline_credential(&baseline) {
        return Destination::Refused(Refusal::BaselineCredential { name });
    }
    Destination::Derived(DerivedProfile {
        preset: preset.to_string(),
        path: profiles_dir.join(format!("{name}.toml")),
        name,
        profiles_dir: profiles_dir.to_path_buf(),
        baseline: Box::new(baseline),
    })
}

/// Any directory entry (a link included) for `name` in the user or community
/// directory, or a built-in name.
fn name_taken(profiles_dir: &Path, name: &str) -> bool {
    let file = format!("{name}.toml");
    embedded_names().contains(&name)
        || has_entry(&profiles_dir.join(&file))
        || has_entry(&profiles_dir.join("community").join(&file))
}

fn has_entry(path: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir())
}

fn is_regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

/// The first key under `value` that names a credential. Only names are
/// inspected: no value is read, compared or shown.
pub fn credential_key(value: &toml::Value) -> Option<String> {
    match value {
        toml::Value::Table(table) => table.iter().find_map(|(key, value)| {
            if looks_like_credential(key) {
                Some(key.clone())
            } else {
                credential_key(value)
            }
        }),
        toml::Value::Array(items) => items.iter().find_map(credential_key),
        _ => None,
    }
}

fn looks_like_credential(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "secret", "password", "api_key", "apikey"]
        .iter()
        .any(|word| key.contains(word))
}

/// Whether materializing the preset would copy a credential (any non-channel
/// port config or the overrides).
fn baseline_credential(baseline: &ResolvedProfile) -> bool {
    let ports = baseline
        .selection
        .dimensions
        .iter()
        .filter(|(port, _)| **port != PortDimension::Channels)
        .filter_map(|(_, reference)| reference._config.as_ref());
    let overrides = baseline
        .overrides
        .as_ref()
        .and_then(|overrides| overrides_to_toml(overrides).ok());
    ports
        .chain(overrides.as_ref())
        .any(|value| credential_key(value).is_some())
}

/// The new `[channels]` table: adapter, then config = siblings + allowlist.
fn channel_table(list: &Allowlist, siblings: toml::Table) -> toml::Table {
    let choice = list.choice();
    let mut config = siblings;
    config.insert(choice.allowlist_key().to_string(), list.to_toml());
    let mut channels = toml::Table::new();
    channels.insert(
        "adapter".to_string(),
        toml::Value::String(choice.adapter().to_string()),
    );
    channels.insert("config".to_string(), toml::Value::Table(config));
    channels
}

fn save_user(edit: &UserEdit, current: &CurrentChannel, list: &Allowlist) -> Result<(), SaveError> {
    let siblings = edit
        .siblings(current, list.choice())
        .map_err(|_| SaveError::Unverified)?;
    let text = splice_channels(&edit.original, &channel_table(list, siblings))?;
    replace_file(&edit.path, edit.original.as_bytes(), text.as_bytes())
}

/// The header name of a `[table]` or `[[array]]` line, dotted parts trimmed.
fn table_header(line: &str) -> Option<String> {
    let inner = line.trim_start().strip_prefix('[')?;
    let inner = inner.strip_prefix('[').unwrap_or(inner);
    let end = inner.find(']')?;
    Some(
        inner[..end]
            .split('.')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("."),
    )
}

/// Replace only the `[channels]` / `[channels.*]` tables of `original`,
/// keeping every other byte. The result must read back with exactly the new
/// channel table and every other value unchanged; a layout this cannot edit
/// safely (dotted or inline `channels` keys, a header inside a multi-line
/// string) fails that check and nothing is written.
pub(super) fn splice_channels(original: &str, channels: &toml::Table) -> Result<String, SaveError> {
    let before: toml::Table = toml::from_str(original).map_err(|_| SaveError::Unverified)?;
    let mut block_root = toml::Table::new();
    block_root.insert("channels".to_string(), toml::Value::Table(channels.clone()));
    let block = toml::to_string(&block_root).map_err(|_| SaveError::Unverified)?;

    let mut out = String::with_capacity(original.len() + block.len() + 2);
    let mut insert_at = None;
    let mut in_channels = false;
    for line in original.split_inclusive('\n') {
        if let Some(header) = table_header(line) {
            in_channels = header == "channels" || header.starts_with("channels.");
            if in_channels && insert_at.is_none() {
                insert_at = Some(out.len());
            }
        }
        if !in_channels {
            out.push_str(line);
        }
    }
    match insert_at {
        Some(at) => out.insert_str(at, &format!("{block}\n")),
        None => {
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push('\n');
            }
            out.push_str(&block);
        }
    }

    let mut after: toml::Table = toml::from_str(&out).map_err(|_| SaveError::Unverified)?;
    let written = after.remove("channels");
    let mut rest = before;
    rest.remove("channels");
    if written != Some(toml::Value::Table(channels.clone())) || after != rest {
        return Err(SaveError::Unverified);
    }
    toml::from_str::<ProfileDefinition>(&out).map_err(|_| SaveError::Unverified)?;
    Ok(out)
}

fn regular_file(path: &Path) -> Result<std::fs::Metadata, SaveError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_file() {
        Ok(meta)
    } else {
        Err(SaveError::Unsafe(path.to_path_buf()))
    }
}

/// A hidden sibling that no profile scan picks up (`.tmp`, not `.toml`).
fn temp_path(dir: &Path, target: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let file = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    dir.join(format!(".{file}.{}.{nanos}.tmp", std::process::id()))
}

/// Create `temp` exclusively (never through an existing entry) and sync it.
fn write_temp(temp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Best effort: the rename or link already happened.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// Atomically replace `path`, which must still hold exactly `expected`.
fn replace_file(path: &Path, expected: &[u8], new: &[u8]) -> Result<(), SaveError> {
    let dir = path
        .parent()
        .ok_or_else(|| SaveError::Unsafe(path.to_path_buf()))?;
    let meta = regular_file(path)?;
    let temp = temp_path(dir, path);
    let result = (|| -> Result<(), SaveError> {
        write_temp(&temp, new)?;
        std::fs::set_permissions(&temp, meta.permissions())?;
        // The edit is based on `expected`: publish only over those bytes.
        regular_file(path)?;
        if std::fs::read(path)? != expected {
            return Err(SaveError::Changed);
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    } else {
        sync_dir(dir);
    }
    result
}

fn save_derived(derived: &DerivedProfile, list: &Allowlist) -> Result<(), SaveError> {
    let text = render_derived(derived, list)?;
    verify_derived(&text, derived, list)?;
    ensure_profiles_dir(&derived.profiles_dir)?;
    if name_taken(&derived.profiles_dir, &derived.name) {
        return Err(SaveError::Taken(derived.name.clone()));
    }
    publish_new(
        &derived.profiles_dir,
        &derived.path,
        &derived.name,
        text.as_bytes(),
    )
}

/// The profiles directory as a real directory, created if absent.
fn ensure_profiles_dir(dir: &Path) -> Result<(), SaveError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => return Ok(()),
        Ok(_) => return Err(SaveError::Unsafe(dir.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    match std::fs::create_dir(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    if is_real_dir(dir) {
        Ok(())
    } else {
        Err(SaveError::Unsafe(dir.to_path_buf()))
    }
}

/// Publish `bytes` at `dest` only if nothing is there: write a synced
/// temporary file, then hard-link it into place (`link(2)` never replaces an
/// existing entry, a link included).
fn publish_new(dir: &Path, dest: &Path, name: &str, bytes: &[u8]) -> Result<(), SaveError> {
    let temp = temp_path(dir, dest);
    let result = (|| -> Result<(), SaveError> {
        write_temp(&temp, bytes)?;
        match std::fs::hard_link(&temp, dest) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(SaveError::Taken(name.to_string()))
            }
            Err(error) => Err(error.into()),
        }
    })();
    let _ = std::fs::remove_file(&temp);
    if result.is_ok() {
        sync_dir(dir);
    }
    result
}

/// The derived profile: every port of the preset's effective baseline with its
/// config, the new channel, and the effective overrides. No `extends` and no
/// `preview`: inheriting a preview would OR it back on and rewrite the
/// feature-gated adapters, and the raw preset would bring back its dormant
/// scheduler.
pub(super) fn render_derived(
    derived: &DerivedProfile,
    list: &Allowlist,
) -> Result<String, SaveError> {
    let mut root = toml::Table::new();
    root.insert(
        "name".to_string(),
        toml::Value::String(derived.name.clone()),
    );
    root.insert(
        "description".to_string(),
        toml::Value::String(format!(
            "Built-in '{}' with a {} channel, created by rustain init.",
            derived.preset,
            list.choice().label()
        )),
    );
    for (port, key) in PORTS {
        let table = if port == PortDimension::Channels {
            channel_table(list, toml::Table::new())
        } else {
            let reference = derived
                .baseline
                .selection
                .dimensions
                .get(&port)
                .ok_or(SaveError::Unverified)?;
            let mut table = toml::Table::new();
            table.insert(
                "adapter".to_string(),
                toml::Value::String(reference.adapter.clone()),
            );
            if let Some(config) = &reference._config {
                table.insert("config".to_string(), config.clone());
            }
            table
        };
        root.insert(key.to_string(), toml::Value::Table(table));
    }
    if let Some(overrides) = &derived.baseline.overrides {
        match overrides_to_toml(overrides).map_err(|_| SaveError::Unverified)? {
            toml::Value::Table(table) if table.is_empty() => {}
            value @ toml::Value::Table(_) => {
                root.insert("overrides".to_string(), value);
            }
            _ => return Err(SaveError::Unverified),
        }
    }
    let body = toml::to_string(&root).map_err(|_| SaveError::Unverified)?;
    Ok(format!(
        "# Created by `rustain init` from the built-in '{preset}' profile: the same\n\
         # settings with a new channel. '{preset}' itself is unchanged. Use it with:\n\
         #   rustain --profile {name} daemon start\n\
         {body}",
        preset = derived.preset,
        name = derived.name,
    ))
}

/// One in-memory profile, for checking a rendered derivative.
struct OneProfile<'a> {
    name: &'a str,
    content: &'a str,
}

impl ProfileSource for OneProfile<'_> {
    fn get(&self, name: &str) -> Option<String> {
        (name == self.name).then(|| self.content.to_string())
    }
}

fn overrides_json(overrides: Option<&figment::value::Value>) -> serde_json::Value {
    let value = overrides.map_or(serde_json::Value::Null, |overrides| {
        serde_json::to_value(overrides).unwrap_or(serde_json::Value::Null)
    });
    match value {
        serde_json::Value::Object(map) if map.is_empty() => serde_json::Value::Null,
        value => value,
    }
}

/// The rendered derivative must load on its own with exactly the baseline's
/// non-channel ports and overrides and the new channel.
fn verify_derived(text: &str, derived: &DerivedProfile, list: &Allowlist) -> Result<(), SaveError> {
    let definition: ProfileDefinition = toml::from_str(text).map_err(|_| SaveError::Unverified)?;
    if definition.extends.is_some() || definition.preview {
        return Err(SaveError::Unverified);
    }
    let source = OneProfile {
        name: &derived.name,
        content: text,
    };
    let read = ProfileLoader::new(&AdapterCatalog, &source)
        .load_ignoring_channel_feature(&derived.name)
        .map_err(|_| SaveError::Unverified)?;
    let choice = list.choice();
    for (port, _) in PORTS {
        let got = read.selection.dimensions.get(&port);
        let same = if port == PortDimension::Channels {
            got.is_some_and(|reference| {
                reference.adapter == choice.adapter()
                    && reference
                        ._config
                        .as_ref()
                        .and_then(|config| config.get(choice.allowlist_key()))
                        == Some(&list.to_toml())
            })
        } else {
            match (got, derived.baseline.selection.dimensions.get(&port)) {
                (Some(got), Some(want)) => {
                    got.adapter == want.adapter && got._config == want._config
                }
                _ => false,
            }
        };
        if !same {
            return Err(SaveError::Unverified);
        }
    }
    if overrides_json(read.overrides.as_ref())
        != overrides_json(derived.baseline.overrides.as_ref())
    {
        return Err(SaveError::Unverified);
    }
    Ok(())
}
