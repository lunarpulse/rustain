use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutocompleteKind {
    SlashCommand,
    FileMention,
    AgentMention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutocompleteSuggestion {
    SlashCommand { name: String, description: String },
    FilePath { path: String, is_dir: bool },
    Skill { name: String, description: String },
    AgentMention { name: String, description: String },
}
