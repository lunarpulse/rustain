//! Profile resolver port — implemented by adapters that load profile definitions
//! and produce ResolvedProfile values (adapter selection + AppConfig overrides).

use crate::domain::models::{ProfileDescriptor, ResolvedProfile};

pub trait ProfileResolver: Send + Sync {
    fn resolve_active(&self) -> Option<ResolvedProfile> {
        None
    }
    fn resolve_active_profile_defaults(&self) -> Option<figment::value::Value> {
        self.resolve_active().and_then(|r| r.overrides)
    }
    fn list_profiles(&self) -> Vec<ProfileDescriptor> {
        Vec::new()
    }
}
