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

## Domain Catalog Shape

For the domain catalog shape (`ToolDescriptor`) and delta semantics (`CatalogDelta`), see [docs/adapter-composition.md §Capability Registry](./adapter-composition.md#capability-registry).

## Invoking MCP Tools

MCP tools are surfaced to the LLM with canonical `mcp__<server>__<tool>` naming per ADR-06-08. For example, `mcp__postgres__query` invokes the `query` tool on the `postgres` server. The display layer renders this as `[postgres] query` but the canonical form is used in the conversation log, LLM context, and permission chain.

Server-side input validation is authoritative (epics.md:3644). rustain forwards the LLM's `tool_use` input to the MCP server verbatim — the server is the source of truth for schema validation. Arguments are propagated as-is; users should be aware that sensitive data in MCP tool calls is visible to the MCP server.

Non-text content blocks (images, embedded resources, audio) are rendered as bracketed placeholders (`[image: <mime>]`, `[resource: <uri>]`) in v0. Full multi-modal rendering is deferred.

## Discovering Tools with `@MCP/`

Type `@MCP/` in the input box to see all available MCP tools grouped by server. The dropdown:
- Groups tools by server (in profile declaration order)
- Shows `[server] tool-name` for each entry
- Filters case-insensitively by tool name or description as you type after `@MCP/`
- Inserts the canonical `mcp__<server>__<tool>` form on selection

Type `@` then `MCP/` to activate. Press `Tab` or `Enter` to select, `Esc` to dismiss.

## Permissions for MCP Tools

MCP tool permission gating works through the same `permission_chain` as built-in tools:

- **Workspace restriction does NOT apply** to MCP tools (epics.md:3640) — the file-path extractor only matches built-in `Read`/`Write`/`Edit`.
- **`read_only_hint` controls Plan mode eligibility** (ADR-06-08 + ADR-06-10): MCP tools with `annotations.read_only_hint == true` are classified as `Safe` risk, which Plan mode auto-allows. Tools without the hint (or with `read_only_hint == false`) are `Elevated` and denied in Plan mode.
- **`Always for [server]` scope** binds to the canonical `mcp__<server>` identifier (not the bare server name), preventing future skill or built-in name collisions.

## Excluding Built-in Tools

Set `include_builtin = false` in `[tools.config]` to expose only MCP tools to the LLM:

```toml
[tools]
adapter = "composite"

[tools.config]
include_builtin = false

[tools.config.mcp.postgres]
transport = "stdio"
command = "mcp-server-postgres"
```

When zero MCP servers are connected and `include_builtin = false`, the tool catalog is empty. If no MCP servers are configured at all, the profile resolver falls back to `builtin-full` (the `include_builtin` flag is ignored in the fallback path).

## Refreshing the Catalog

MCP servers that support `notifications/tools/list_changed` (announced during `initialize`) trigger an automatic catalog refresh when their tool list changes. The refresh re-fetches `tools/list` and emits an `McpCatalogChanged` event, which updates the autocomplete dropdown and status panel on the next render tick.

For servers that don't emit the notification, tool lists are cached at connection time and can be refreshed by restarting the server session (future: slash-command refresh per DG 2.6).

## Troubleshooting

- **"connection failed after 5 attempts"** — Check the server command is in `$PATH` and the server binary is executable.
- **"unsupported transport"** — SSE is not supported. Use `mcp-proxy` or update the server to stdio/Streamable HTTP.
- **"composite adapter but no MCP servers"** — The profile was auto-rewritten to `builtin-full`. Add `[tools.config.mcp.*]` tables or create `.claude/mcp.json`.

## Related

- [Adapter Composition](adapter-composition.md#capability-registry) — Capability Registry and CPA trait integration (Story 9.3a)
