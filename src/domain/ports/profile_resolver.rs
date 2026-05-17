//! Profile resolver port — hook for Story 8.2's `TomlProfileResolver`.
//!
//! Story 8.1 ships `NoopProfileResolver` only. The trait lives in `domain/ports/`
//! per hexagonal layering (AC-5 + AC-14).

pub trait ProfileResolver: Send + Sync {
    /// Returns the active profile's defaults as a figment value, or `None` if
    /// no profile is active.
    ///
    /// Story 8.2 implements `TomlProfileResolver`; this story ships
    /// `NoopProfileResolver` only.
    fn resolve_active_profile_defaults(&self) -> Option<figment::value::Value>;
}
