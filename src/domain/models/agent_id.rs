use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Agent identifier syntax failure.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum AgentIdError {
    #[error("agent id must not be empty")]
    Empty,
    #[error("agent id segment must not be empty")]
    EmptySegment,
    #[error("agent id segment must not contain '/'")]
    EmbeddedSeparator,
    #[error("agent id segment must not be reserved root sentinel")]
    ReservedRoot,
    #[error("peer agent id path must include peer id and at least one child segment")]
    PeerPathTooShort,
}

/// Newtype for an agent identifier.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub struct AgentId(String);

impl Default for AgentId {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentId {
    /// Generate a fresh 12-character URL-safe ID.
    pub fn new() -> Self {
        let id = nanoid::nanoid!(12);
        debug_assert!(!id.contains('/'), "nanoid must not contain path separator");
        Self(id)
    }

    /// Sentinel for the root agent (used in PermissionChain recursion-guard comparisons).
    pub fn root() -> Self {
        Self(String::from("root"))
    }

    /// Build an id from an already-domain-owned string.
    ///
    /// Crate-private: this constructor PANICS on malformed input. It exists for
    /// trusted, domain-owned call sites where the string is known valid by
    /// construction. Public callers must use the fallible [`AgentId::parse`] or
    /// [`TryFrom<String>`] so untrusted input can never reach a panic.
    pub(crate) fn from_validated(id: impl Into<String>) -> Self {
        let id = id.into();
        Self::validate_path(&id, false).expect("AgentId must be syntactically valid");
        Self(id)
    }

    /// Build a peer-scoped path: `<peer_id>/<segment>[/segment...]`.
    pub fn from_peer_path(path: &str) -> Result<Self, AgentIdError> {
        Self::validate_path(path, true)?;
        Ok(Self(path.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Split the id into its `'/'`-separated path segments.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    /// Returns `true` when this id has no path separator (a single local segment).
    pub fn is_local(&self) -> bool {
        !self.0.contains('/')
    }

    /// Build an id by joining `segs` with `'/'`.
    ///
    /// Crate-private and panicking on malformed segments (non-empty, no embedded
    /// `/`, none equal to the `"root"` sentinel). Public callers must validate
    /// via [`AgentId::parse`] or [`AgentId::from_peer_path`].
    pub(crate) fn from_segments(segs: &[&str]) -> Self {
        let id = segs.join("/");
        Self::validate_path(&id, false).expect("AgentId segments must be syntactically valid");
        Self(id)
    }

    /// Parse an arbitrary string into a validated `AgentId` (non-panicking).
    ///
    /// This is the public, fallible constructor for untrusted input. It rejects
    /// empty ids, empty segments, embedded separators, and the reserved `"root"`
    /// sentinel. For trusted domain-owned strings, internal code uses the
    /// crate-private `from_validated`; for the root sentinel use [`AgentId::root`].
    pub fn parse(s: &str) -> Result<Self, AgentIdError> {
        Self::validate_path(s, false)?;
        Ok(Self(s.to_string()))
    }

    fn validate_path(path: &str, peer_path: bool) -> Result<(), AgentIdError> {
        if path.is_empty() {
            return Err(AgentIdError::Empty);
        }
        let mut count = 0usize;
        for segment in path.split('/') {
            count += 1;
            validate_segment(segment)?;
        }
        if peer_path && count < 2 {
            return Err(AgentIdError::PeerPathTooShort);
        }
        Ok(())
    }
}

impl TryFrom<String> for AgentId {
    type Error = AgentIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::validate_path(&value, false)?;
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for AgentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // The "root" sentinel (only constructable via AgentId::root) must
        // survive a serde round-trip; everything else runs full path validation
        // so malformed ids cannot enter the domain over the wire.
        if s != "root" {
            Self::validate_path(&s, false).map_err(serde::de::Error::custom)?;
        }
        Ok(Self(s))
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn validate_segment(segment: &str) -> Result<(), AgentIdError> {
    if segment.is_empty() {
        return Err(AgentIdError::EmptySegment);
    }
    if segment.contains('/') {
        return Err(AgentIdError::EmbeddedSeparator);
    }
    if segment == "root" {
        return Err(AgentIdError::ReservedRoot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AgentId, AgentIdError};

    #[test]
    fn segments_single_local_id() {
        let id = AgentId::from_validated(String::from("abc"));
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["abc"]);
    }

    #[test]
    fn segments_multiple_path_id() {
        let id = AgentId::from_validated(String::from("peer1/sub/child"));
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["peer1", "sub", "child"]);
    }

    #[test]
    fn is_local_true_for_local_id() {
        let id = AgentId::from_validated(String::from("abc"));
        assert!(id.is_local());
    }

    #[test]
    fn is_local_false_for_path_id() {
        let id = AgentId::from_validated(String::from("peer1/sub"));
        assert!(!id.is_local());
    }

    #[test]
    fn from_segments_roundtrip() {
        let id = AgentId::from_segments(&["peer1", "sub", "child"]);
        assert_eq!(id.as_str(), "peer1/sub/child");
        let segs: Vec<&str> = id.segments().collect();
        assert_eq!(segs, vec!["peer1", "sub", "child"]);
    }

    #[test]
    fn root_sentinel_is_preserved_and_local() {
        let root = AgentId::root();
        assert_eq!(root.as_str(), "root");
        assert!(root.is_local());
    }

    #[test]
    fn new_is_always_local() {
        for _ in 0..100 {
            assert!(AgentId::new().is_local());
        }
    }

    #[test]
    fn from_peer_path_release_rejects_malformed_paths() {
        assert!(matches!(
            AgentId::from_peer_path("peer"),
            Err(AgentIdError::PeerPathTooShort)
        ));
        assert!(matches!(
            AgentId::from_peer_path("peer//child"),
            Err(AgentIdError::EmptySegment)
        ));
        assert!(matches!(
            AgentId::from_peer_path("peer/root"),
            Err(AgentIdError::ReservedRoot)
        ));
    }

    #[test]
    fn from_peer_path_accepts_well_formed_path() {
        let id = AgentId::from_peer_path("peer123/child").expect("valid peer path");
        assert_eq!(id.as_str(), "peer123/child");
    }

    #[test]
    fn parse_accepts_well_formed_and_rejects_malformed() {
        assert_eq!(AgentId::parse("abc").unwrap().as_str(), "abc");
        assert_eq!(
            AgentId::parse("peer1/sub/child").unwrap().as_str(),
            "peer1/sub/child"
        );
        assert!(matches!(AgentId::parse(""), Err(AgentIdError::Empty)));
        assert!(matches!(
            AgentId::parse("a//b"),
            Err(AgentIdError::EmptySegment)
        ));
        assert!(matches!(
            AgentId::parse("root"),
            Err(AgentIdError::ReservedRoot)
        ));
    }

    #[test]
    fn serde_rejects_malformed_agent_id_over_the_wire() {
        // A well-formed id round-trips.
        let id = AgentId::parse("peer/child").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let back: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);

        // The root sentinel must survive a serde round-trip.
        let root_json = serde_json::to_string(&AgentId::root()).unwrap();
        let root_back: AgentId = serde_json::from_str(&root_json).unwrap();
        assert_eq!(root_back, AgentId::root());

        // Malformed ids are rejected at deserialization — they cannot enter the
        // domain over the wire (DD-4 / AC1).
        assert!(serde_json::from_str::<AgentId>("\"\"").is_err());
        assert!(serde_json::from_str::<AgentId>("\"a//b\"").is_err());
        assert!(serde_json::from_str::<AgentId>("\"has space\"").is_ok()); // space is not illegal syntax
        // Embedded leading/trailing separators and empty segments rejected.
        assert!(serde_json::from_str::<AgentId>("\"/leading\"").is_err());
        assert!(serde_json::from_str::<AgentId>("\"trailing/\"").is_err());
    }
}
