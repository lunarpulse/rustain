# MCP (Model Context Protocol) Integration

Story 9.1 ships the foundational MCP infrastructure: configuration parsing, client lifecycle, and composite toolset adapter.

## Supported Transports

- **stdio v0.5** — fully supported (Story 9.1)
- **Streamable HTTP** — deferred to a later Epic 9 story
- **SSE** — rejected per ADR-06-08; servers show `Unsupported` state with guidance to use a proxy

## Configuration Layers

MCP servers are configured in two places (workspace wins on name collision):

1. **Workspace** — `.claude/mcp.json` (Claude Code format):
   ```json
   {
     "mcpServers": {
       "postgres": {
         "command": "mcp-server-postgres",
         "args": ["--connection-string", "$DATABASE_URL"]
       }
     }
   }
   ```

2. **Profile TOML** — `~/.config/rustain/<profile>.toml`:
   ```toml
   [tools]
   adapter = "composite"

   [tools.config.mcp.postgres]
   transport = "stdio"
   command = "mcp-server-postgres"
   args = ["--connection-string", "$DATABASE_URL"]
   persistent = false
   ```

## Environment Variable Interpolation

Both `$VAR` and `${VAR}` are expanded from the rustain process environment at spawn time. Unknown variables are preserved literally and a warning is logged.

## Security Considerations

MCP child processes inherit rustain's working directory and environment. They can write to any file rustain can write to. Sandboxing (Landlock on Linux) lands in Story 9.5.

## Adapter Status Panel

Press `Ctrl+X, A` to view MCP server health. Connected servers show tool counts; failed servers display error reasons.

## Troubleshooting

- **"connection failed after 5 attempts"** — Check the server command is in `$PATH` and the server binary is executable.
- **"unsupported transport"** — SSE is not supported. Use `mcp-proxy` or update the server to stdio/Streamable HTTP.
- **"composite adapter but no MCP servers"** — The profile was auto-rewritten to `builtin-full`. Add `[tools.config.mcp.*]` tables or create `.claude/mcp.json`.
