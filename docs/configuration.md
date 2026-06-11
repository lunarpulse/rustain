# Configuration

rustain merges configuration from 7 layers using [figment](https://crates.io/crates/figment). Each layer fills in values not set by higher-priority layers — field-level merge, not whole-file replacement.

## Layer priority (highest to lowest)

| # | Layer | Source | Format |
|---|-------|--------|--------|
| 1 | CLI flags | `--model`, `--log-level`, `--snapshot-retention`, `--config-file` | Parsed args |
| 2 | Environment | `RUSTAIN_*` prefixed env vars | Env |
| 3 | Local override | `<workspace>/.claude/rustain-settings.json` | JSON (camelCase) |
| 4 | Workspace config | `<workspace>/.rustain/config.toml` | TOML (snake_case) |
| 5 | User-global config | `~/.config/rustain/config.toml` | TOML (snake_case) |
| 6 | Profile defaults | `~/.config/rustain/profiles/<name>.toml` (Story 8.2+) | TOML |
| 7 | Built-in defaults | `AppConfig::default()` | In-memory |

The *highest* layer that defines a key wins. Lower layers fill in fields not touched by higher layers.

## Naming conventions

| Format | Canonical case | Examples |
|--------|---------------|----------|
| TOML (layers 4, 5, 6) | `snake_case` | `log_level`, `daily_limit_usd`, `cache_ttl_seconds` |
| JSON (layer 3) | `camelCase` | `logLevel`, `dailyLimitUsd`, `cacheTtlSeconds` |
| Env (layer 2) | `snake_case` with `__` nesting | `RUSTAIN_LOG_LEVEL=debug`, `RUSTAIN_BUDGET__DAILY_LIMIT_USD=5.00` |

## Configuration reloading

Reload config without restarting rustain:

- **Unix:** `kill -HUP <pid>` (SIGHUP)
- **In-TUI:** type `/config reload` in the command input
- **Cross-process CLI:** `rustain config reload` (prints instructions — cross-process reload deferred)

On reload, malformed config is skipped and the previous config remains active. A `ConfigReloaded` event with `success: false` is emitted to telemetry subscribers.

## Examples

### Override model via CLI

```sh
rustain --model claude-opus-4-7
```

### Override log level via env

```sh
RUSTAIN_LOG_LEVEL=debug rustain
```

### JSON local override (Claude Code compatible)

```json
// <workspace>/.claude/rustain-settings.json
{
  "model": "claude-sonnet-4-6",
  "logLevel": "debug"
}
```

### User-global TOML

```toml
# ~/.config/rustain/config.toml
model = "claude-sonnet-4-6"

[budget]
daily_limit_usd = 10.00

[provider.openrouter]
enabled = true
discover_models = true
```

### Workspace TOML (override with `--config-file`)

```toml
# <workspace>/.rustain/config.toml
log_level = "trace"

[provider.openai]
enabled = true
```

```sh
rustain --config-file /path/to/custom.toml
```

### Field-level merge

Setting `[pricing."my-model"]` in user TOML *adds* to the built-in pricing catalog — it does not replace it. The same holds for `[provider.X]` and `[router.step_tiers]`.
