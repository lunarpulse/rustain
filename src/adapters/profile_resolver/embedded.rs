//! Embedded built-in profile catalog (compile-time `include_str!`).
//! Story 8.2 AC-12 — binary works with NO `~/.config/rustain/profiles/` directory.

use crate::domain::services::profile_loader::ProfileSource;

const BASE_TOML: &str = include_str!("../../../profiles/base.toml");
const CODING_TOML: &str = include_str!("../../../profiles/coding.toml");
const PERSONAL_ASSISTANT_TOML: &str = include_str!("../../../profiles/personal-assistant.toml");

pub struct EmbeddedProfileSource;

impl ProfileSource for EmbeddedProfileSource {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "base" => Some(BASE_TOML.to_string()),
            "coding" => Some(CODING_TOML.to_string()),
            "personal-assistant" => Some(PERSONAL_ASSISTANT_TOML.to_string()),
            _ => None,
        }
    }
}

pub fn embedded_names() -> &'static [&'static str] {
    &["base", "coding", "personal-assistant"]
}
