# Configuration

rustain merges configuration from 8 possible layers using [figment](https://crates.io/crates/figment). Each layer fills in values not set by higher-priority layers — field-level merge, not whole-file replacement.

## Layer priority (highest to lowest)

| # | Layer | Source | Format |
|---|-------|--------|--------|
| 0 | `-c` / `--set` overrides | `-c key=value`, `--set router.threshold_tokens=100000` | Parsed scalar key/value |
| 1 | CLI flags | `--model`, `--log-level`, `--snapshot-retention`, `--config-file` | Parsed args |
| 2 | Environment | `RUSTAIN_*` prefixed env vars | Env |
| 3 | Local override | `<workspace>/.claude/rustain-settings.json` | JSON (camelCase) |
| 4 | Workspace config | `<workspace>/.rustain/config.toml` | TOML (snake_case) |
| 5 | User-global config | `~/.config/rustain/config.toml` | TOML (snake_case) |
| 6 | Profile defaults | `~/.config/rustain/profiles/<name>.toml` (Story 8.2+) | TOML |
| 7 | Built-in defaults | `AppConfig::default()` | In-memory |

The *highest* layer that defines a key wins. Lower layers fill in fields not touched by higher layers.

`RUSTAIN_CONFIG_DIR` replaces the user-global configuration directory for both
application startup and `rustain config show`. Its `config.toml` is used from
every working directory; a workspace configuration remains an explicit higher
priority override.

## Naming conventions

| Format | Canonical case | Examples |
|--------|---------------|----------|
| TOML (layers 4, 5, 6) | `snake_case` | `log_level`, `daily_limit_usd`, `cache_ttl_seconds` |
| JSON (layer 3) | `camelCase` | `logLevel`, `dailyLimitUsd`, `cacheTtlSeconds` |
| Env (layer 2) | `snake_case` with `__` nesting | `RUSTAIN_LOG_LEVEL=debug`, `RUSTAIN_BUDGET__DAILY_LIMIT_USD=5.00` |
| `-c` / `--set` (layer 0) | dot-path `snake_case` | `-c model=gpt-4o`, `-c router.threshold_tokens=100000` |

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

### Override one invocation with `-c` / `--set`

```sh
rustain ask \"summarize this\" -c model=gpt-4o -c router.threshold_tokens=100000
```

`-c` is ephemeral and is not written to any config file. Dot-path keys map to nested config sections, and values are scalar JSON leaves: numbers, booleans, `null`, or strings. Array/map replacement is intentionally out of scope; use a config file for those.

Credential-like keys are rejected because command-line arguments can leak through shell history or process listings. Use `rustain auth login <provider>` for API keys and tokens.

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

Provider availability is independent of the current directory: built-in keyed
providers unlock from home credentials or environment variables without a
`[provider.*]` entry. A configured entry replaces a built-in row wholesale when
its key matches, or when an enabled entry has the same `kind`. The runtime does
not write these catalogue defaults into files created by `rustain init`.

Entries must include `provider_id`, `model_id` and `api_key_env`. To hide a
built-in provider entirely, supply a complete entry with `enabled = false`;
an incomplete `[provider.openai] enabled = false` is not a valid configuration.


```toml
# ~/.config/rustain/config.toml
model = "claude-sonnet-4-6"

[budget]
daily_limit_usd = 10.00

[provider.openrouter]
provider_id = "openrouter"
kind = "openrouter"
model_id = "anthropic/claude-sonnet-4.6"
api_key_env = "OPENROUTER_API_KEY"
enabled = true
discover_models = true
```

### Workspace TOML (override with `--config-file`)

```toml
# <workspace>/.rustain/config.toml
log_level = "trace"

[provider.openai]
provider_id = "openai"
kind = "openai"
model_id = "gpt-5.5"
api_key_env = "OPENAI_API_KEY"
enabled = true
```

```sh
rustain --config-file /path/to/custom.toml
```

### Field-level merge

Setting `[pricing."my-model"]` in user TOML *adds* to the built-in pricing catalog — it does not replace it. The same holds for `[provider.X]` and `[router.step_tiers]`.

### Discovery and local endpoints

Set `discover_models = true` to refresh the provider catalogue; built-in keyed
rows enable it automatically. `model_filter` globs apply to every merged layer.
Metadata is merged bundled → models.dev → matching cache → live, per ID and
field; live IDs are authoritative. Empty success is not an outage, and does not
replace the last-good disk cache. Cache writes are advisory-locked across
processes, so a login CLI and long-lived sessions retain each other's provider
entries. The cache is scoped to the effective endpoint and credential.

The model selector retains empty providers and distinguishes **connected**,
**sign in again** (401/403), **using cached model list**, **unavailable**, and
**empty discovery**. Missing credentials instead show a locked public catalogue
with an actionable sign-in hint. The locked catalogue never uses account cache.

For an unauthenticated local OpenAI-compatible server:

```toml
[provider.local]
provider_id = "local"
kind = "openai-compatible"
model_id = "my-local-model"
api_key_env = "" # Explicitly keyless; a named-but-unset variable locks the provider.
base_url = "http://127.0.0.1:8080/v1"
discover_models = true
```

To hide a built-in without falling back to its bundled entry:

```toml
[provider.openai]
provider_id = "openai"
model_id = "gpt-5.5"
api_key_env = "OPENAI_API_KEY"
enabled = false
```
