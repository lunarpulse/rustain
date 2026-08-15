use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

pub use crate::domain::models::P2pConfigState;
use crate::domain::models::{P2pPeerSpec, PeerId, PinnedKey};
use crate::domain::services::peer_dial::{PeerDialVerdict, peer_dial_verdict};
use crate::infrastructure::paths::workspace_p2p_config_path;

// Unlike the A2A parser, this transport allowlist is deliberately fail-closed:
// reject unknown fields so a typo cannot silently disable operator policy.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRoot {
    #[serde(default)]
    listen: bool,
    #[serde(default)]
    agents: UniqueAgents,
}

#[derive(Debug, Default)]
struct UniqueAgents(BTreeMap<String, PeerInput>);

impl<'de> serde::Deserialize<'de> for UniqueAgents {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueAgentsVisitor;

        impl<'de> Visitor<'de> for UniqueAgentsVisitor {
            type Value = UniqueAgents;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an agents object with unique aliases")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut agents = BTreeMap::new();
                while let Some((alias, input)) = map.next_entry::<String, PeerInput>()? {
                    if agents.insert(alias.clone(), input).is_some() {
                        return Err(<A::Error as serde::de::Error>::custom(format!(
                            "duplicate transport peer alias {alias:?}"
                        )));
                    }
                }
                Ok(UniqueAgents(agents))
            }
        }

        deserializer.deserialize_map(UniqueAgentsVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerInput {
    #[serde(default, rename = "pinnedKey", alias = "pinned_key")]
    pinned_key: Option<PinnedKeyInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedKeyInput {
    alg: String,
    x: String,
    #[serde(default)]
    kid: Option<String>,
}

/// Read the listener trigger from `.rustain/p2p.json`.
///
/// This stays outside the `p2p` feature gate so a binary that cannot honor an
/// enabled listener fails loudly instead of silently ignoring operator intent.
pub fn p2p_listener_requested(path: &Path) -> Result<bool, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    let root: WorkspaceRoot = serde_json::from_str(&content)
        .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
    Ok(root.listen)
}

/// Load the transport-specific allowlist without a feature gate.
///
/// A build without the `p2p` adapter must still reject malformed operator
/// configuration loudly instead of silently treating it as an empty list.
pub fn load_workspace_p2p_config(path: &Path) -> P2pConfigState {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return P2pConfigState::Absent;
        }
        Err(error) => {
            return P2pConfigState::Malformed {
                reason: format!("failed to read {}: {error}", path.display()),
            };
        }
    };
    let root: WorkspaceRoot = match serde_json::from_str(&content) {
        Ok(root) => root,
        Err(error) => {
            return P2pConfigState::Malformed {
                reason: format!("invalid JSON in {}: {error}", path.display()),
            };
        }
    };
    match peers_from_root(root) {
        Ok(peers) => P2pConfigState::Present(peers),
        Err(reason) => P2pConfigState::Malformed { reason },
    }
}

fn peers_from_root(root: WorkspaceRoot) -> Result<Vec<P2pPeerSpec>, String> {
    let mut peers = Vec::with_capacity(root.agents.0.len());
    let mut aliases_by_identity = BTreeMap::<String, String>::new();
    for (id, input) in root.agents.0 {
        if id.trim().is_empty() {
            return Err("transport peer id must not be empty or whitespace".to_owned());
        }
        let pinned_key = match input.pinned_key {
            Some(pin) => Some(
                PinnedKey::parse(&pin.alg, pin.x, pin.kid)
                    .map_err(|error| format!("invalid transport peer {id:?}: {error}"))?,
            ),
            None => None,
        };
        if let Some(pin) = pinned_key.as_ref() {
            let peer_id = pin
                .peer_id()
                .map_err(|error| format!("invalid transport peer {id:?}: {error}"))?;
            if let Some(first_alias) =
                aliases_by_identity.insert(peer_id.as_str().to_owned(), id.clone())
            {
                return Err(format!(
                    "transport peers {first_alias:?} and {id:?} pin the same identity; one peer \
                     must have one alias"
                ));
            }
        }
        peers.push(P2pPeerSpec::new(id, pinned_key));
    }
    Ok(peers)
}

/// Re-read the transport list and apply the pure verdict core.
///
/// This seam intentionally reads only `.rustain/p2p.json`; the independent
/// `.rustain/a2a.json` HTTP admission policy cannot relax transport access.
pub fn peer_dial_verdict_from_workspace(workspace: &Path, peer_id: &PeerId) -> PeerDialVerdict {
    let config = load_workspace_p2p_config(&workspace_p2p_config_path(workspace));
    peer_dial_verdict(peer_id, &config)
}

// ---------------------------------------------------------------------------
// The writer (Story 18.4b, AC3) — the FIRST writer on this path.
// ---------------------------------------------------------------------------
//
// # Why this is a read-modify-write over raw JSON, never over `WorkspaceRoot`
//
// A typed round-trip through the structs above is itself the lossy rewrite AC3
// forbids, in three separate ways:
//
// * `PeerInput::pinned_key` carries `rename = "pinnedKey", alias =
//   "pinned_key"`, so re-serializing silently renames an operator's
//   `pinned_key` to `pinnedKey`.
// * `listen` is `#[serde(default)]`, so re-serializing injects a
//   security-relevant `"listen": false` the operator never wrote.
// * `agents` is a `BTreeMap`, so hand-authored alias order is re-sorted.
//
// `deny_unknown_fields` already means an unknown field cannot survive a read at
// all — such a file is `Malformed` and this writer refuses it rather than
// clobbering it. So preserving unknown fields is not the hazard; preserving
// **spelling, defaults and order** is, and that is what [`Ordered`] plus
// `RawValue` deliver: every key keeps its source position, every untouched
// value keeps its source text, and only the one entry being changed is
// re-emitted. Indentation is normalized to two spaces; no field, spelling,
// default or ordering is lost.

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize as _, Deserializer};
use serde_json::value::RawValue;

/// A JSON object deserialized into its **source key order**.
///
/// `serde_json::Map` is a `BTreeMap` in this build (`preserve_order` is off),
/// so parsing to `serde_json::Value` would re-sort every object. This keeps the
/// operator's order.
struct Ordered<V>(Vec<(String, V)>);

impl<'de, V: serde::Deserialize<'de>> serde::Deserialize<'de> for Ordered<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OrderedVisitor<V>(std::marker::PhantomData<V>);
        impl<'de, V: serde::Deserialize<'de>> Visitor<'de> for OrderedVisitor<V> {
            type Value = Ordered<V>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered<V>, A::Error> {
                let mut out = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    out.push((key, value));
                }
                Ok(Ordered(out))
            }
        }
        deserializer.deserialize_map(OrderedVisitor(std::marker::PhantomData))
    }
}

/// The pinned-key value of one entry, kept in the operator's own key order.
enum PinValue {
    /// A JSON object: its leaf fields as `(key, raw JSON text)` in source order.
    Object(Vec<(String, String)>),
    /// Anything else the operator wrote there (`null`, most plausibly), kept
    /// verbatim so a rewrite does not silently drop it.
    Raw(String),
}

/// One `agents` entry, carrying enough to re-emit it faithfully.
struct AgentEntry {
    alias: String,
    /// The operator's spelling of the pinned-key field. `None` for an entry that
    /// carried none; a new field then uses [`DEFAULT_PIN_FIELD`].
    pin_field: Option<String>,
    /// The pinned-key value, read from the file or set by this writer.
    pin: Option<PinValue>,
}

impl AgentEntry {
    /// Replace this entry's pinned key, keeping the operator's field spelling.
    fn set_pin(&mut self, key: &PinnedKey) {
        let mut fields = vec![
            ("alg".to_owned(), json_string("EdDSA")),
            ("x".to_owned(), json_string(&key.x)),
        ];
        if let Some(kid) = key.kid.as_deref() {
            fields.push(("kid".to_owned(), json_string(kid)));
        }
        self.pin = Some(PinValue::Object(fields));
    }
}

/// The canonical spelling used for a **new** entry. An existing entry keeps the
/// operator's spelling.
const DEFAULT_PIN_FIELD: &str = "pinnedKey";

/// Cross-process guard for one complete read-modify-write transaction.
///
/// `pub(crate)` because Story 18.4d's sibling reach store needs exactly this
/// transaction discipline. Reusing it is the point: a second hand-rolled
/// `flock` + temp + rename would be a second chance to get one of the six steps
/// wrong.
pub(crate) struct P2pConfigWriteLock {
    file: Option<std::fs::File>,
    #[cfg(not(unix))]
    path: std::path::PathBuf,
}

impl P2pConfigWriteLock {
    pub(crate) fn acquire(path: &Path) -> Result<Self, String> {
        let parent = path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        let lock_path = path.with_extension("json.lock");

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            use std::os::unix::fs::OpenOptionsExt as _;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|error| format!("opening config lock {}: {error}", lock_path.display()))?;
            // SAFETY: `file` owns this descriptor for the guard's lifetime.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(format!(
                    "locking config {}: {}",
                    lock_path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self { file: Some(file) })
        }

        #[cfg(not(unix))]
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .map_err(|error| {
                    format!(
                        "config {} is already being updated or its lock is stale: {error}",
                        path.display()
                    )
                })?;
            Ok(Self {
                file: Some(file),
                path: lock_path,
            })
        }
    }
}

impl Drop for P2pConfigWriteLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            if let Some(file) = self.file.as_ref() {
                // SAFETY: `file` still owns this descriptor.
                unsafe {
                    libc::flock(file.as_raw_fd(), libc::LOCK_UN);
                }
            }
        }
        #[cfg(not(unix))]
        {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Pin `key` under `alias` in `.rustain/p2p.json`, creating the file if absent.
///
/// Refuses without writing when the existing file cannot be parsed: a file
/// whose contents are unknown is not a file to overwrite.
pub fn pin_peer_in_workspace_config(
    path: &Path,
    alias: &str,
    key: &PinnedKey,
) -> Result<(), String> {
    if alias.trim().is_empty() {
        return Err("transport peer alias must not be empty or whitespace".to_owned());
    }
    let offered_id = key
        .peer_id()
        .map_err(|error| format!("invalid offered transport key: {error}"))?;
    rewrite_p2p_config(path, |agents, current| {
        if let Some(existing) = current.iter().find(|peer| peer.id == alias) {
            match existing.pinned_identity() {
                Some(on_file) if on_file == offered_id => return Ok(false),
                Some(on_file) => {
                    return Err(format!(
                        "alias {alias:?} now pins {on_file}; refusing to replace it with \
                         {offered_id}"
                    ));
                }
                None => {}
            }
        }
        if let Some(existing) = current
            .iter()
            .find(|peer| peer.id != alias && peer.pinned_identity().as_ref() == Some(&offered_id))
        {
            return Err(format!(
                "identity {offered_id} is already pinned as {:?}; one peer must have one alias",
                existing.id
            ));
        }
        match agents.iter_mut().find(|entry| entry.alias == alias) {
            Some(entry) => entry.set_pin(key),
            None => {
                let mut entry = AgentEntry {
                    alias: alias.to_owned(),
                    pin_field: None,
                    pin: None,
                };
                entry.set_pin(key);
                agents.push(entry);
            }
        }
        Ok(true)
    })
}

/// Remove `alias` from `.rustain/p2p.json`.
pub fn remove_peer_from_workspace_config(path: &Path, alias: &str) -> Result<(), String> {
    rewrite_p2p_config(path, |agents, current| {
        if !current.iter().any(|peer| peer.id == alias) {
            return Ok(false);
        }
        agents.retain(|entry| entry.alias != alias);
        Ok(true)
    })
}

/// Parse, mutate the ordered `agents` list, then re-emit and rename into place.
fn rewrite_p2p_config(
    path: &Path,
    mutate: impl FnOnce(&mut Vec<AgentEntry>, &[P2pPeerSpec]) -> Result<bool, String>,
) -> Result<(), String> {
    let _lock = P2pConfigWriteLock::acquire(path)?;
    let content = match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };

    let current = match content.as_deref() {
        Some(content) => {
            let typed: WorkspaceRoot = serde_json::from_str(content)
                .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
            peers_from_root(typed)
                .map_err(|reason| format!("invalid config in {}: {reason}", path.display()))?
        }
        None => Vec::new(),
    };
    let mut root_keys: Vec<(String, Option<String>)> = Vec::new();
    let mut agents: Vec<AgentEntry> = Vec::new();
    if let Some(content) = content.as_deref() {
        let root: Ordered<Box<RawValue>> = serde_json::from_str(content)
            .map_err(|error| format!("invalid JSON in {}: {error}", path.display()))?;
        for (field, raw) in root.0 {
            if field == "agents" {
                let listed: Ordered<Box<RawValue>> =
                    serde_json::from_str(raw.get()).map_err(|error| {
                        format!("invalid \"agents\" object in {}: {error}", path.display())
                    })?;
                for (alias, entry_raw) in listed.0 {
                    let fields: Ordered<Box<RawValue>> = serde_json::from_str(entry_raw.get())
                        .map_err(|error| {
                            format!("invalid entry {alias:?} in {}: {error}", path.display())
                        })?;
                    let mut pin_field = None;
                    let mut pin = None;
                    for (name, value) in fields.0 {
                        if name != "pinnedKey" && name != "pinned_key" {
                            return Err(format!(
                                "entry {alias:?} in {} carries an unrecognised field {name:?}; \
                                 refusing to rewrite it",
                                path.display()
                            ));
                        }
                        pin_field = Some(name);
                        pin = Some(
                            match serde_json::from_str::<Ordered<Box<RawValue>>>(value.get()) {
                                Ok(leaves) => PinValue::Object(
                                    leaves
                                        .0
                                        .into_iter()
                                        .map(|(key, leaf)| (key, leaf.get().to_owned()))
                                        .collect(),
                                ),
                                Err(_) => PinValue::Raw(value.get().trim().to_owned()),
                            },
                        );
                    }
                    agents.push(AgentEntry {
                        alias,
                        pin_field,
                        pin,
                    });
                }
                root_keys.push((field, None));
            } else {
                root_keys.push((field, Some(raw.get().to_owned())));
            }
        }
    }
    if !root_keys.iter().any(|(field, _)| field == "agents") {
        root_keys.push(("agents".to_owned(), None));
    }
    if !mutate(&mut agents, &current)? {
        return Ok(());
    }

    let body = emit_p2p_config(&root_keys, &agents)?;
    write_config_atomically(path, ".p2p-config-", &body)
}

/// Re-emit the document.
///
/// Preserves the operator's top-level key order, alias order, pinned-key field
/// spelling and pinned-key leaf order, and omits any key they never wrote —
/// notably `listen`, whose absence is not the same claim as `"listen": false`.
/// Indentation is normalized to two spaces; nothing else about the file changes.
fn emit_p2p_config(
    root_keys: &[(String, Option<String>)],
    agents: &[AgentEntry],
) -> Result<String, String> {
    let mut out = String::from("{\n");
    for (index, (field, raw)) in root_keys.iter().enumerate() {
        out.push_str("  ");
        out.push_str(&json_string(field));
        out.push_str(": ");
        match raw {
            // Every non-`agents` key `deny_unknown_fields` permits is the scalar
            // `listen`, so its raw text is a single line and travels verbatim.
            Some(raw) => out.push_str(raw.trim()),
            None => out.push_str(&emit_agents(agents, 1)),
        }
        if index + 1 < root_keys.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    Ok(out)
}

fn emit_agents(agents: &[AgentEntry], depth: usize) -> String {
    if agents.is_empty() {
        return "{}".to_owned();
    }
    let pad = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    for (index, entry) in agents.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&json_string(&entry.alias));
        out.push_str(": ");
        out.push_str(&emit_entry(entry, depth + 1));
        if index + 1 < agents.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&"  ".repeat(depth));
    out.push('}');
    out
}

fn emit_entry(entry: &AgentEntry, depth: usize) -> String {
    let Some(pin) = entry.pin.as_ref() else {
        return "{}".to_owned();
    };
    let field = entry.pin_field.as_deref().unwrap_or(DEFAULT_PIN_FIELD);
    let pad = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    out.push_str(&pad);
    out.push_str(&json_string(field));
    out.push_str(": ");
    match pin {
        PinValue::Raw(raw) => out.push_str(raw),
        PinValue::Object(leaves) => out.push_str(&emit_leaves(leaves, depth + 1)),
    }
    out.push('\n');
    out.push_str(&"  ".repeat(depth));
    out.push('}');
    out
}

fn emit_leaves(leaves: &[(String, String)], depth: usize) -> String {
    if leaves.is_empty() {
        return "{}".to_owned();
    }
    let pad = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    for (index, (key, raw)) in leaves.iter().enumerate() {
        out.push_str(&pad);
        out.push_str(&json_string(key));
        out.push_str(": ");
        out.push_str(raw.trim());
        if index + 1 < leaves.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(&"  ".repeat(depth));
    out.push('}');
    out
}

fn json_string(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

/// Temp-then-rename with mode `0o600` before the rename, so the final file is
/// never world-readable even momentarily (`auth_store` and `key_store`
/// precedents). An interrupted write leaves the previous file intact.
///
/// `prefix` names the temporary file so a directory listing during a write says
/// which store is being replaced.
pub(crate) fn write_config_atomically(path: &Path, prefix: &str, body: &str) -> Result<(), String> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;

    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix).suffix(".tmp").rand_bytes(16);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut temporary = builder
        .tempfile_in(parent)
        .map_err(|error| format!("creating a temporary file: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("chmod 0600: {error}"))?;
    }
    temporary
        .write_all(body.as_bytes())
        .map_err(|error| format!("writing a temporary file: {error}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| format!("fsync: {error}"))?;
    temporary
        .persist(path)
        .map_err(|error| format!("rename: {}", error.error))?;
    #[cfg(unix)]
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("syncing config directory: {error}"))?;
    Ok(())
}
