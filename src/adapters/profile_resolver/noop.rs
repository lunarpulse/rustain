//! No-op profile resolver — returns `None` for the profile defaults layer.
//! Story 8.2 ships `TomlProfileResolver` that reads `~/.config/rustain/profiles/{name}.toml`.

use crate::domain::ports::ProfileResolver;

pub struct NoopProfileResolver;

impl ProfileResolver for NoopProfileResolver {
    fn resolve_active_profile_defaults(&self) -> Option<figment::value::Value> {
        None
    }
}
