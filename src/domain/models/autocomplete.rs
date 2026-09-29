use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutocompleteKind {
    SlashCommand,
    FileMention,
    AgentMention,
    McpMention,
    A2aMention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub server: String,
    pub name: String,
    pub description: String,
}

/// Cached A2A peer agent entry for `@A2A/` autocomplete (Story 19.13).
/// `name` is the raw skill ID, NOT the human skill title; `peer` is the
/// configured peer ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct A2aAgentInfo {
    pub peer: String,
    pub name: String,
    pub description: String,
}

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
    Skill {
        name: String,
        description: String,
    },
    AgentMention {
        name: String,
        description: String,
    },
    McpTool {
        server: String,
        name: String,
        description: String,
    },
    A2aAgent {
        peer: String,
        name: String,
        description: String,
    },
}
