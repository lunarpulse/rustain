use serde::{Deserialize, Serialize};

/// Kind of autocomplete trigger — determines source of suggestions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutocompleteKind {
    SlashCommand,
    FileMention,
}

/// A single autocomplete suggestion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutocompleteSuggestion {
    SlashCommand {
        name: String,
        description: String,
    },
    FilePath {
        path: String,
        is_dir: bool,
    },
}
