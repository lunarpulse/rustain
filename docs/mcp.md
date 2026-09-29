# MCP (Model Context Protocol) Integration

Story 9.1 ships the foundational MCP infrastructure: configuration parsing, client lifecycle, and composite toolset adapter.

For per-turn tool exposure (how many of your N MCP tools the model sees on each request), see [docs/profiles.md §`[tools]` table](./profiles.md#tools-table--top-level-config-story-94) and ADR-09-01 v2.2.

## Supported Transports

| Transport | Status | Notes |
|---|---|---|
| **stdio** | supported | Story 9.1. The server is a child process; `tasks/*` (durable MCP tasks) work only here. |
| **Streamable HTTP** | supported | Story 9.9. `transport = "http"` / `"type": "http"` plus a `url`. |
| **SSE** (legacy) | ⛔ rejected, permanently | ADR-06-08. The MCP spec deprecated it on 2025-03-26. Servers show `Unsupported` with guidance to use a proxy such as `mcp-proxy`, or to update the server to Streamable HTTP. |

⚑ **Streamable HTTP's SSE *response stream* is not the rejected SSE *transport*.**
Streamable HTTP replies over `text/event-stream` by specification, and rustain's
dependency tree contains an SSE parser for exactly that reason. The transport
ADR-06-08 rejects is the **legacy SSE transport** — a separate endpoint pair
(`GET /sse` for events plus a distinct POST endpoint). Enabling Streamable HTTP
does not resurrect it, and `transport = "sse"` still lands in `Unsupported`.

### HTTP servers

`url` supports `$VAR` / `${VAR}` expansion exactly like `command`, `args` and
`env`. `command` is **optional** for an `http` entry.

A static bearer token can be supplied through the environment (no OAuth — the
client sends the token verbatim and never negotiates):

- `RUSTAIN_MCP_HTTP_AUTH_TOKEN_<SERVER>` — per server, where `<SERVER>` is the
  server id upper-cased with every non-alphanumeric character replaced by `_`
  (`remote-ci` → `RUSTAIN_MCP_HTTP_AUTH_TOKEN_REMOTE_CI`).
- `RUSTAIN_MCP_HTTP_AUTH_TOKEN` — process-wide fallback.

Supply the bare credential: rustain sends it as `Authorization: Bearer <token>`.

A plaintext `http://` URL pointed at a **non-loopback** host warns once per
server and then connects anyway (warn-and-allow): the traffic and any auth token
are unencrypted. Loopback URLs and `https://` URLs are silent.

⚠ **Trust anchors differ by subsystem, and this is worth knowing before you
deploy behind a corporate CA.** MCP HTTPS validates against the **operating
system trust store** (via `rustls-platform-verifier`, already in the shipped
binary through `iroh`), while LLM-provider traffic validates against the pinned
`webpki-roots` bundle. An OS-level intercepting proxy therefore sees MCP traffic
and not provider traffic. Tracked as `DF-9-9-TRUST-ANCHOR-SPLIT`.

⛔ **MCP tasks require stdio.** A server that answers `tools/call` with a
task-shaped result over HTTP is refused with a named boundary rather than
silently mis-decoded: the byte-level Tasks shim (`task_transport.rs`) is a stdio
construct, and over HTTP a task record would meet rmcp's untagged result union
raw. Run such a server over stdio.

## Configuration Layers

MCP servers are configured in two places (workspace wins on name collision):

1. **Workspace** — `.claude/mcp.json` (Claude Code format). ⚠ The top-level key
   is `mcpServers`, not `servers`:
   ```json
   {
     "mcpServers": {
       "postgres": {
         "command": "mcp-server-postgres",
         "args": ["--connection-string", "$DATABASE_URL"]
       },
       "remote": {
         "type": "http",
         "url": "https://mcp.example.com/mcp"
       }
     }
   }
   ```
   The `remote` entry carries **no `command`**, which is the shape Claude Code
   writes and is legal. ⚑ Before Story 9.9 that entry made the *whole file* fail
   to parse (`missing field \`command\``) and the error was swallowed into
   `~/.rustain/rustain.log` — so a single HTTP entry silently removed every MCP
   server in the file, healthy stdio ones included. A malformed entry now
   degrades on its own: its siblings load, and the bad entry's failure shows up
   in the adapter status panel when the connection is attempted. A file that
   cannot be parsed at all yields zero MCP servers **plus a warning notice in
   the UI**, and never stops rustain from starting.

2. **Profile TOML** — `~/.config/rustain/<profile>.toml`:
   ```toml
   [tools]
   adapter = "composite"

   [tools.config.mcp.postgres]
   transport = "stdio"
   command = "mcp-server-postgres"
   args = ["--connection-string", "$DATABASE_URL"]
   persistent = false

   [tools.config.mcp.remote]
   transport = "http"
   url = "${MCP_REMOTE_URL}"
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

- **"connection failed after 5 attempts"** — stdio: check the server command is
  in `$PATH` and executable. HTTP: check the server is running and the `url` is
  right. The retry envelope is five attempts at 1s, 2s, 4s, 8s and 16s; there is
  no mid-session reconnect for any transport (`DF-9-9-NO-LIVE-DISCONNECT-DETECT`).
- **"unsupported transport"** — SSE only. Use `mcp-proxy`, or update the server
  to Streamable HTTP and set `transport = "http"` with a `url`. ⛔ Do not upgrade
  a server that already speaks Streamable HTTP — that is supported.
- **"MCP server configuration error"** — the entry contradicts itself:
  `transport = "http"` with no `url` (or an unparseable one), or
  `transport = "stdio"` with no `command`. Only that entry is affected.
- **"MCP HTTP transport failed (auth required)"** — the server answered `401`.
  Set `RUSTAIN_MCP_HTTP_AUTH_TOKEN_<SERVER>` (see *HTTP servers* above); the
  message quotes the server's own `WWW-Authenticate` challenge.
- **"MCP tasks require the stdio transport"** — the server replied to
  `tools/call` with a durable task. Tasks are stdio-only by design (ADR-06-08
  amendment D1).
- **"composite adapter but no MCP servers"** — The profile was auto-rewritten to `builtin-full`. Add `[tools.config.mcp.*]` tables or create `.claude/mcp.json`.

## Related

- [Adapter Composition](adapter-composition.md#capability-registry) — Capability Registry and CPA trait integration (Story 9.3a)
