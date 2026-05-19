//! Shared in-memory ProfileSource for validation of imported/installed profiles.
//! Resolves the target name from the in-memory content; falls back to EmbeddedProfileSource
//! for extends = "base" resolution. Reused by import.rs (local path / stdin) and install.rs
//! (community profile fetched from gh:user/repo).

use crate::adapters::profile_resolver::embedded::EmbeddedProfileSource;
use crate::domain::services::profile_loader::ProfileSource as LoaderProfileSource;

pub(super) struct SinglePathSource {
    pub(super) name: String,
    pub(super) content: String,
    pub(super) fallback: EmbeddedProfileSource,
}

impl LoaderProfileSource for SinglePathSource {
    fn get(&self, name: &str) -> Option<String> {
        if name == self.name {
            Some(self.content.clone())
        } else {
            self.fallback.get(name)
        }
    }
}
