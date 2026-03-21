pub mod provider;
pub mod registry;
pub mod service;
// pub mod session;       // Session persistence (save/load .meta.json)
// pub mod config;        // Configuration loading (.claude/ directory, rustain-settings.json)
// pub mod permissions;   // Permission approval flow (blocklist, vault restriction, approval callback)
// pub mod markdown;      // pulldown-cmark → ratatui spans converter
// pub mod agents;        // Custom agent discovery (~/.claude/agents/ + workspace)
// pub mod commands;      // Slash command discovery and template expansion
//
// Capability providers (implement CapabilityProvider trait):
// pub mod skills_provider;  // Agent Skills standard (.agents/skills/, .claude/skills/)
// pub mod mcp_provider;     // MCP tool servers (.claude/mcp.json)
// pub mod a2a_provider;     // A2A agent protocol (.claude/a2a.json + spawn/despawn)
