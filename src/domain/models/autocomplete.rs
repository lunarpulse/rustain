use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutocompleteKind {
    SlashCommand,
    FileMention,
    AgentMention,
    McpMention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolInfo {
    pub server: String,
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
}
