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
context   = default      # project-context.md only
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
context   = daily              # daily-context.md + recent-events injection
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
adapter = "default"

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

## Available Adapters by Port

| Port       | Available Adapters                          |
|------------|---------------------------------------------|
| Persona    | minimal, coding, personal-assistant         |
| Memory     | noop, project-scoped, daily-log             |
| Session    | basic, workspace                            |
| Tools      | builtin-only, builtin-full                  |
| Channels   | terminal, telegram (feature-gated)          |
| Scheduler  | none, cron (feature-gated)                  |
| Context    | default, daily                              |

## See Also

- [Configuration](configuration.md) — the full 7-layer config system
- `~/.config/rustain/profiles/` — your custom profile directory
