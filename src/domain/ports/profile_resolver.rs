//! Profile resolver port — implemented by adapters that load profile definitions
//! and produce ResolvedProfile values (adapter selection + AppConfig overrides).

use crate::domain::models::ResolvedProfile;

pub trait ProfileResolver: Send + Sync {
    /// Story 8.2 — returns the fully-resolved active profile, or None if no profile is active.
    fn resolve_active(&self) -> Option<ResolvedProfile> {
        None
    }

    /// Story 8.1 back-compat — returns the active profile's AppConfig overrides as a figment value.
    /// Default delegates to resolve_active().overrides; adapters implementing only
    /// resolve_active() get this for free.
    fn resolve_active_profile_defaults(&self) -> Option<figment::value::Value> {
        self.resolve_active().and_then(|r| r.overrides)
    }
}
