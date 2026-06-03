# Profiles

Rustain profiles let you compose the agent's behavior by selecting one adapter
per port dimension (persona, memory, session, tools, channels, scheduler, context).
Switch profiles to change how rustain remembers, communicates, and acts — no code
required.

## Built-in Profiles

Three profiles ship with rustain and are embedded in the binary:

### `base` — Minimal Foundation
```toml
persona   = minimal      # no system-prompt specialization
memory    = noop         # no conversation persistence
session   = basic        # single-session, no persistence beyond log
tools     = builtin-only # Bash/Read/Write/Edit/Glob/Grep only
channels  = terminal     # TUI only
scheduler = none         # no scheduled tasks
context   = default      # memory + project-context injection (degrades to project-only when memory=noop)
```
**Use as:** a parent for custom profiles. Not intended for direct daily use.

### `coding` — Software Development (default)
```toml
extends = base
persona   = coding           # coding-focused system prompt (concise, code-style discipline)
memory    = project-scoped   # per-workspace memory, persists across sessions
session   = workspace        # workspace-aware session lifecycle
tools     = builtin-full     # Bash/Read/Write/Edit/Glob/Grep + MCP + Skills
# [overrides]
# default_plan_mode = false  # favors immediate execution
```
**Active by default** when no `--profile` flag, `RUSTAIN_PROFILE` env var, or
`active_profile` config field is set.

### `personal-assistant` — Preview
```toml
extends = base
preview = true
persona   = personal-assistant  # warm-tone, life-organization framing
memory    = daily-log          # append-only daily-log memory (date-indexed)
channels  = telegram           # feature-gated; falls back to terminal
scheduler = cron               # feature-gated; falls back to none
context   = daily              # alias of `default` — memory + project-context injection
```
**Preview status:** Telegram and cron adapters are not yet available (coming in Epic 12).
The profile loads gracefully with terminal and none as fallbacks.

## Profile Resolution Order

The active profile name is resolved in this order (highest precedence first):

1. `--profile <name>` CLI flag (e.g., `rustain --profile personal-assistant`)
2. `RUSTAIN_PROFILE` environment variable
3. `active_profile` field in your config file (`~/.config/rustain/config.toml`)
4. Default: `"coding"`

Profile name matching is **case-sensitive**.

## Profile File Locations

- **Custom profiles:** `~/.config/rustain/profiles/{name}.toml`
- **Built-in profiles:** embedded in the binary at compile time

Custom profiles with the same name as a built-in profile **override** the built-in.
For example, creating `~/.config/rustain/profiles/coding.toml` replaces the built-in
coding profile.

## Profile TOML Schema

A profile file has the following structure:

```toml
name = "my-profile"                  # REQUIRED — must match the file stem
description = "My custom profile"    # OPTIONAL — shown in profile switcher
extends = "base"                     # OPTIONAL — parent profile; chain depth ≤ 4
preview = false                      # OPTIONAL — enables graceful fallback for preview adapters

# Seven port-dimension tables (each OPTIONAL; missing → inherits from extends or falls back to noop)
[persona]
adapter = "coding"
[persona.config]                     # OPTIONAL — adapter-specific TOML
tone = "concise"

[memory]
adapter = "project-scoped"

[session]
adapter = "workspace"

[tools]
adapter = "builtin-full"

[channels]
adapter = "terminal"

[scheduler]
adapter = "none"

[context]
adapter = "default"                 # memory + project-context injection (Story 11.4)
# [context.config]                  # optional — adapter-local tuning (see below)
# recent_limit = 20
# search_limit = 10
# max_tokens = 2000
# daily_window_days = 2

# AppConfig overrides — field-level merge into the config chain
[overrides]
default_plan_mode = false
model = "claude-sonnet-4-6"
```

### `extends` — Profile Inheritance

- Child profiles inherit all undefined dimensions from their parent.
- Child's per-dimension entries **completely replace** the parent's (no field-level merge within dimensions).
- Child's `[overrides]` block **deep-merges** with the parent's at the field level.
- Extends chain depth is capped at **4 levels**.
- **Circular chains** (A → B → A) are detected and rejected with a clear error message.

### `[overrides]` — AppConfig Overrides

The `[overrides]` block contributes field-level overrides to the AppConfig chain.
It merges at **layer 6** of the 7-layer figment chain:

1. CLI flags (`--model`, `--profile`, etc.)
2. Environment variables (`RUSTAIN_*`)
3. Local JSON override (`.claude/rustain-settings.json`)
4. Workspace config (`.rustain/config.toml`)
5. User-global config (`~/.config/rustain/config.toml`)
**6. Active profile overrides (injected here)**
7. Built-in defaults

Any `AppConfig` field is valid in the `[overrides]` block.

### `[tools]` table — top-level config (Story 9.4)

The top-level `[tools]` table in `~/.config/rustain/config.toml` (Story 9.4)
carries the per-turn exposure strategy:

```toml
[tools]
exposure = "static-full"   # Phase A: only "static-full" is accepted.
                           # Phase B (Story 9.7): "meta-search" will be available.
```

This is DIFFERENT from the profile-layer `[tools]` table (Flag 2 wiring),
which selects the toolset adapter (`composite` / `builtin-only` / `builtin-full`)
per profile. To set per-profile exposure, override at the user-global or
workspace layer instead — exposure is session-wide v1.

CLI: `--tool-exposure static-full`
Env: `RUSTAIN_TOOLS__EXPOSURE=static-full` (note the double-underscore nest
separator per existing `RUSTAIN_*` env var convention).

### `preview` — Preview Profiles

Set `preview = true` to mark a profile as a preview. Preview profiles gracefully
fall back when they reference adapters with cargo features not yet compiled in:

- Feature-gated adapters are silently rewritten to their documented fallback.
- A single warning notice is emitted at startup.
- Non-preview profiles referencing missing feature-gated adapters abort with a
  `ProfileError::AdapterFeatureGated` error.

**Note:** The `preview` flag is intended for built-in profiles only. Custom profiles
that set `preview = true` will load successfully but emit a warning at startup.
Avoid using `preview = true` in custom profiles — it is a mechanism for the rustain
team to ship experimental built-in profiles (like `personal-assistant`) that reference
adapters not yet fully implemented.

## Creating a Custom Profile

1. Create `~/.config/rustain/profiles/my-profile.toml`:

   ```toml
   name = "my-profile"
   description = "My lightweight coding profile"
   extends = "coding"

   # Override the persona for a different tone
   [persona]
   adapter = "minimal"

   # Add custom config overrides
   [overrides]
   log_level = "debug"
   ```

2. Activate it:
   ```bash
   rustain --profile my-profile
   ```
   Or set it permanently in `~/.config/rustain/config.toml`:
   ```toml
   active_profile = "my-profile"
   ```

### `[memory]` — Embedding Providers (`vector-search`, feature-gated)

The `vector-search` memory adapter (build with `--features vector-search`) wraps the
`project-scoped` content source with semantic + hybrid keyword retrieval. The embedding
provider is selected by the per-dimension `[memory] config` table. The **default is the
local in-process model** (no config needed, fully offline):

```toml
[memory]
adapter = "vector-search"           # local BAAI/bge-small-en-v1.5 (384-dim), downloaded on first use
```

To use a remote OpenAI-compatible `/embeddings` endpoint, add a `[memory.config]` table.
Only the API-key env-var **name** is configured — the key value is read from the environment,
never stored in the profile:

```toml
[memory]
adapter = "vector-search"
[memory.config]
provider    = "openrouter"          # → base_url https://openrouter.ai/api/v1, api_key_env OPENROUTER_API_KEY
model       = "baai/bge-m3"         # 1024-dim, fixed (GATE-verified 2026-06-01)
# dimension = 1024                  # optional; auto-locked for known models
# api_key_env = "OPENROUTER_API_KEY"  # optional; defaults per provider
```

- **`provider`** — one of `local` (default), `openai`, `voyage`, `openrouter`, `deepinfra`,
  `together`, `openai-compatible`. Each maps to a default `base_url` + `*_API_KEY` env var.
  `openai-compatible` requires an explicit `base_url`.
- **`dimension`** is auto-resolved for known models (`baai/bge-m3` → 1024,
  `qwen/qwen3-embedding-8b` → 4096, OpenAI `text-embedding-3-*`); set it explicitly for any
  other model.
- **Switching providers** (or models with a different dimension) triggers a **guided reindex**:
  a notice is surfaced and the index is automatically rebuilt from your memory content.
- A misconfigured remote provider (missing key, unknown model/dimension) **falls back to the
  local model** with a warning notice — it never blocks startup.
- **Verified providers** (AC-11-3b-GATE reachability probe, 2026-06-01): **OpenRouter** with
  `baai/bge-m3` (1024-dim) and `qwen/qwen3-embedding-8b` (4096-dim, higher quality; ~6.4s
  first-request cold start). Other hosts use the same OpenAI-compatible client and are
  reachable by changing only `base_url`/`model`/`api_key_env`.

> Remote network embedding latency is excluded from the `<200ms` search bound (NFR56); the
> local embedding + index-search path is what that bound covers.

#### Removal-integrity — `/memory forget` (Story 11.4a / FR122)

A memory you remove is **gone, not just hidden** — it can never resurface through memory
search, now or after any reindex.

- **`/memory forget <text>`** fuzzy-matches your memory entries and shows a confirm card
  listing each match (with its stable key). Because there is no separate `/memory show`, this
  card doubles as the scoped "what's in memory" view. **Nothing is purged until you press
  `[y]`** (`[n]`/`Esc` cancels).
- On confirm, a durable **redaction tombstone** is written to `…/.rustain/memory/redactions.bin`
  (a sibling of `index.bin`, **never** inside your `MEMORY.md`/daily-log source), then the
  vector index and the BM25 keyword index are purged of that entry.
- **The guarantee survives a reindex.** The persistent index re-derives itself from an
  append-only source on every refresh; the tombstone gates that rebuild, skipping the redacted
  key at embed time. So even if the original source row is still present (daily-log is
  append-only), the removed fact is never re-embedded — and a full index rebuild from source
  still excludes it. The tombstone is the source of truth: it is written and persisted **first**,
  so "redacted ⇒ never retrievable" holds even if a purge is interrupted (the next refresh
  converges).
- **Scope.** `/memory forget` removes the entry from the *search index*. Strategy A does not
  edit your `MEMORY.md`/daily-log source files — silently honoring a manual `MEMORY.md`
  line-deletion is a deferred follow-up (it will funnel through the *same* tombstone, confirm
  on reload, and never silent-purge). Removal-integrity applies to the `vector-search` build;
  without a derived index there is nothing to purge.

> **Known issue (AC-R4, 2026-06-02).** Removal-integrity (`/memory forget` → tombstone →
> index purge) and the 11.2 "remove a fact" affordance must ship **together**: a build that
> exposes removal without 11.4a green could leave a removed fact searchable. They land in the
> same Epic-11 closure, so any honest "removed/forgotten" copy is now backed by an actual
> purge. (Low urgency: a single internal dogfood install with test data existed pre-11.4a.)

### `[context]` — Memory Context Injection (Story 11.4)

The `default` context adapter (`daily` is an alias) assembles relevant memory at the
**start of each turn** and injects it into the user message's invisible context prefix
(it never shows as a chat message). It pulls recent entries + semantic search hits from the
composed `[memory]` adapter, re-derives provenance, **deduplicates across sources**
(`MEMORY.md` wins over a duplicate daily-log row), prioritises (`MEMORY.md` > daily logs >
search results), and budget-truncates. Project context (`CLAUDE.md`) stays injected by the
persona (it is structural, not memory) — the context bundle only *references* it.

With `[memory] adapter = "noop"` (e.g. the `base` profile) the bundle is empty and nothing is
injected — the agent operates normally on project context alone. Use `noop` to disable the
adapter entirely.

Inspect/toggle at runtime:
- `/context show` — read-only view of the last-assembled context with per-source token counts.
- `/context off` / `/context on` — disable/enable memory injection **for the session** (project
  context keeps applying). When off, the status bar shows `mem: off` and zero memory reads occur.

Adapter-local config (`[context.config]`):

| Key | Default | Meaning |
|-----|---------|---------|
| `recent_limit` | `20` | Max recent (`MEMORY.md` + daily-log) rows pulled per turn. |
| `search_limit` | `10` | Max semantic search hits pulled per turn. |
| `max_tokens` | `2000` | Hard cap on injected memory tokens (effective budget = `min(call-site, this)`). |
| `daily_window_days` | `2` | Recency window: entries within N days are labelled `[daily-log: …]`, older as `[memory]`. |

## Available Adapters by Port

| Port       | Available Adapters                          |
|------------|---------------------------------------------|
| Persona    | minimal, coding, personal-assistant         |
| Memory     | noop, project-scoped, daily-log, long-term, vector-search (feature-gated) |
| Session    | basic, workspace                            |
| Tools      | builtin-only, builtin-full, composite        |
| Channels   | terminal, telegram (feature-gated)          |
| Scheduler  | none, cron (feature-gated)                  |
| Context    | default, daily (alias of default), noop      |

### MCP Servers

MCP servers can be configured per-profile via `[tools.config.mcp.<server-name>]` or workspace-wide via `.claude/mcp.json`. See [Adapter Composition](adapter-composition.md#composite-tools-adapter) for full details. For MCP tool invocation and discovery, see [Invoking MCP tools](mcp.md#invoking-mcp-tools).

## CLI: Profile Management Commands

Manage profiles without launching the TUI. All commands work in CI / non-TTY environments.

| Command | Description |
|---------|-------------|
| `rustain profile list` | Enumerate all profiles (builtin, user, community) with source, preview, and active marker |
| `rustain profile show <name>` | Display the fully resolved profile configuration (supports `--toml` and `--json`) |
| `rustain profile create` | Interactive wizard to build a new profile (TTY-only) |
| `rustain profile edit <name>` | Open the profile TOML in `$EDITOR` (TTY-only; `--no-validate` to skip post-save check) |
| `rustain profile switch <name>` | CLI stub for switching profiles (IPC requires a running TUI) |
| `rustain profile validate <name>` | Run all 5 validation passes (default `--all` checks every profile) |
| `rustain profile export <name>` | Flatten extends chain into a shareable self-contained TOML |
| `rustain profile import <path>` | Validate and install a profile TOML from a local path or stdin (`-`) |

## See Also

- [Configuration](configuration.md) — the full 7-layer config system
- `~/.config/rustain/profiles/` — your custom profile directory
