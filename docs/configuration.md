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

### ChatGPT Codex subscription

Codex is separate from the `openai` API-key provider:

```toml
model = "gpt-5.6-luna"

[provider.openai-codex]
provider_id = "openai-codex"
kind = "openai-codex"
model_id = "gpt-5.6-luna"
api_key_env = "OPENAI_CODEX_OAUTH_TOKEN"
enabled = true
```

Authenticate with `rustain auth login openai-codex`, or supply a static OAuth
bearer through the named environment variable (not an OpenAI API key).
The top-level `model` selects the default model for turns; each provider's
`model_id` configures its entry and does not replace that global turn default.
The default base URL is `https://chatgpt.com/backend-api`. A `base_url` ending in
`/codex/responses` is used unchanged; one ending in `/codex` gains `/responses`;
any other base gains `/codex/responses` (trailing slashes are stripped first).
Stored OAuth credentials are eligible only on the official ChatGPT origin;
changing the origin never forwards a stored subscription token to a gateway.
An explicitly supplied environment bearer is operator-directed.

Configured aliases may use another map key with `kind = "openai-codex"` and
`provider_id = "openai-codex"`: the map key identifies the runtime entry while
`provider_id` selects the login credential rows. Codex seeds eight bundled
models and models.dev currently has no Codex source (OpenAI API-only models are
not promoted). With `discover_models = true` a signed Codex entry discovers its
live roster (`/codex/models`, then `/models`) and encrypted reasoning items are
replayed privately to the same provider, model and login generation. Refresh
preserves replay even when it first learns identity; same-account re-login
invalidates old-login items. Unscoped/old identity-scoped items load without
replay, preserving ordinary history; see `docs/auth.md`.
Upstream's 128,000-token maximum-output metadata has no rustain descriptor
consumer and is not a transmitted limit.

### xAI (API key and Grok subscription)

xAI has two built-in providers that need no configuration: `xai` (API key) and
`xai-oauth` (SuperGrok / X Premium+ device login). Both default to `grok-4.6`.
An explicit entry looks like:

```toml
model = "grok-4.6"

[provider.xai]
provider_id = "xai"
kind = "xai"
model_id = "grok-4.6"
api_key_env = "XAI_API_KEY"
enabled = true

[provider.xai-oauth]
provider_id = "xai-oauth"
kind = "xai-oauth"
model_id = "grok-4.6"
api_key_env = "XAI_OAUTH_TOKEN"
enabled = true
```

Authenticate with `rustain auth login xai` (key) or `rustain auth login xai-oauth`
(device code); `XAI_OAUTH_TOKEN` is a static OAuth bearer, not an API key.
Requests go to `<base>/responses`; discovery and key validation use
`<base>/models`. The default base is `https://api.x.ai/v1`; trailing slashes are
stripped, and a `base_url` equal to the default counts as unset. The base is
resolved per kind:

- `kind = "xai"`: a non-default `base_url`, else `XAI_BASE_URL`, else the default.
- `kind = "xai-oauth"`: a non-default `base_url`, else the default.
  `XAI_BASE_URL` is never read, so a shell override cannot redirect subscription
  tokens.

Stored `xai-oauth` credentials are eligible only when the effective base has the
official `https://api.x.ai` origin; another origin never receives a stored
subscription token (an explicitly set `XAI_OAUTH_TOKEN` is operator-directed and
is still sent). `xai` keys have no origin rule. For `xai-oauth`, `provider_id`
selects the login credential rows, so an alias map key with
`provider_id = "xai-oauth"` uses the same login.

### GitHub Copilot

`github-copilot` is a built-in provider that needs no configuration; it defaults
to `gpt-5.5` and bundles GitHub Copilot's 55 models. An explicit entry looks like:

```toml
[provider.copilot]
provider_id = "github-copilot"
kind = "github-copilot"
model_id = "claude-sonnet-4.5"
api_key_env = "COPILOT_GITHUB_TOKEN"
enabled = true
```

Authenticate with `rustain auth login github-copilot` (device code, see
`docs/auth.md`) or set `COPILOT_GITHUB_TOKEN`. Each request picks its wire API
from the model id (Messages for `claude-haiku-*`, `claude-sonnet-*`,
`claude-opus-*`, `claude-fable-*` and `claude-mythos-*`; Responses for GPT-5/6,
Grok 4.x, `oswe` and MAI; Chat Completions otherwise, including older Claude ids
such as `claude-3-5-sonnet`). Leave `base_url` unset: requests then go to the
account's API endpoint learned at login (or its Enterprise root, or
`https://api.githubcopilot.com`). A `base_url` on `githubcopilot.com` or a
`*.githubcopilot.com` host still yields to the account endpoint; any other
`base_url` (a proxy) is used verbatim. The origin guard applies to the configured
`base_url`: a stored Copilot login is eligible only when it is unset or an
`https` host on `githubcopilot.com`; another `base_url` makes the stored login
ineligible, while an explicitly set `COPILOT_GITHUB_TOKEN` is still sent there.
The account endpoint reported by GitHub at login (and an Enterprise
`copilot-api.<domain>` root) is trusted as is, as in omp.
`COPILOT_INTEGRATION_ID` pins the `Copilot-Integration-Id` header.

### Google Cloud Code Assist (Gemini CLI subscription)

`google-gemini-cli` is a built-in provider that needs no configuration; its
bundled default is `gemini-3.1-pro-preview`. An explicit entry looks like:

```toml
model = "gemini-3.1-pro-preview"

[provider.google-gemini-cli]
provider_id = "google-gemini-cli"
kind = "google-gemini-cli"
model_id = "gemini-3.1-pro-preview"
api_key_env = "" # Required field; this provider has no environment credential.
enabled = true
```

Authenticate with `rustain auth login google-gemini-cli`; there is no API-key or
environment credential, so keep `api_key_env = ""`. Requests go to
`<base>/v1internal:streamGenerateContent?alt=sse`. The base is `base_url` with
trailing slashes stripped, else `https://cloudcode-pa.googleapis.com`. Stored
credentials are eligible only when that base has the official
`https://cloudcode-pa.googleapis.com` origin: a `base_url` on any other origin
leaves the provider locked and never receives the token. `provider_id` selects the
login rows, so an alias map key with `provider_id = "google-gemini-cli"` uses the
same login. This provider is unrelated to the `google` API-key provider.

### OpenRouter

`openrouter` is a built-in provider; its bundled default is `openai/gpt-5.5`. An
explicit entry looks like:

```toml
[provider.openrouter]
provider_id = "openrouter"
kind = "openrouter"
model_id = "openai/gpt-5.5"
api_key_env = "OPENROUTER_API_KEY"
enabled = true
```

Authenticate with `rustain auth login openrouter` (browser key minting or a
pasted `sk-or-` key, see `docs/auth.md`) or set `OPENROUTER_API_KEY`. Requests go
to OpenRouter's Responses API, `<base>/responses`; set
`RUSTAIN_OPENROUTER_RESPONSES=0` to use Chat Completions
(`<base>/chat/completions`) instead. The base is `base_url` with trailing slashes
stripped, else `https://openrouter.ai/api/v1`. Every inference request carries
`User-Agent: rustain/<version>`, `HTTP-Referer: https://github.com/lunarpulse/rustain`,
`X-OpenRouter-Title: rustain`, `X-OpenRouter-Categories: cli-agent` and enables
response caching (`X-OpenRouter-Cache: true`, `X-OpenRouter-Cache-TTL: 3600`).
No output cap is sent for the default token budget; `anthropic/*` models get an
ephemeral prompt-cache breakpoint. A model id with a routing suffix such as
`:nitro` is sent verbatim and keeps its base model's reasoning and image support.
Chat Completions resends a 408, 429, 5xx or transport failure at open up to five
times, as omp does.

### Notifications (`[notifications]`)

Transient notices (copy confirmations, model switches, warnings, errors) paint
as short toasts at the bottom of the chat pane and then expire; each one stays
reviewable in the Notification Log (`Ctrl+X n` or `/notifications`) for the
session. Lifetimes are whole seconds and are clamped to `[1, 60]`; a value
above that range (or `0`) is clamped and logged as a warning, whereas a
negative or non-integer value invalidates the config. The 60 s ceiling is not
configurable, so an error never outlives a minute on screen.

```toml
[notifications]
info_ttl_secs = 4      # default 4
success_ttl_secs = 4   # default 4
warning_ttl_secs = 10  # default 10
error_ttl_secs = 60    # default 60
```

Notifications are session-local and are never written to disk.

### Code blocks (`[markdown]`)

On the local Stage a fenced code block of an assistant reply that is longer than
`code_max_rows` rows shows its first `code_max_rows` rows and a muted
`… N more lines` row inside the box (`… 1 more line` for one); the rest of the
block is one key away: focus it (the `▸` marker and `[Enter] Inspect` appear on
its first row) and press `Enter` to read all of it in the block viewer, where
`#` adds line numbers, `w` wraps, `v` shows the source and `c` copies the
exact source. The value is a whole number of rows, clamped to `[1, 1000]`: a
value outside that range (or `0` or a negative number) is clamped and logged as
a warning, whereas a non-integer value invalidates the config. It can be set per
profile, and `/config reload` applies a new value to every conversation
without a restart.

```toml
[markdown]
code_max_rows = 30   # default 30, range 1-1000
```

The cap applies to the local Stage only and to completed replies: code in the
reply that is still streaming, in reasoning, in system messages and in an
attached session always shows every row.

### Thinking (`[markdown] reasoning`)

The model's thinking collapses to one muted row on the local Stage, in the
place where it happened: `◇ Thinking · 4.2s · 1.3k tokens`. The duration runs
from the first thinking chunk to the answer. The token count is the
provider's own figure only when the provider reports reasoning tokens before
the response ends (today the Responses API and Gemini CLI) and the response
had exactly one thinking run; otherwise (OpenAI chat completions, which
report usage after the end of the turn, and Anthropic, which reports none) it
is an estimate marked `~` (bytes ÷ 4). While the model is still thinking
the row reads `◇ Thinking… · 3.1s · ~120 tokens`.

Focus the row with `Tab` (the `▸` marker and `[Enter] Inspect` appear) and
press `Enter` to read the whole text in a window: `v` shows the source,
`/` searches, `c` copies the exact original text. `[markdown] reasoning =
"expanded"` keeps the text on the Stage under the header row instead.

```toml
[markdown]
reasoning = "folded"   # default; "expanded" shows the text under the header
```

The value is the lowercase string `"folded"` or `"expanded"`. Any other string
(including `"Expanded"`) is logged as a warning and folds, and the rest of the
config is kept; a value that is not a string at all (for example `reasoning =
3`) is a type error that invalidates the config like any other mistyped key.
`/config reload` applies a change to every conversation without a restart. An
attached session always shows its thinking expanded.

### Decisions that need an answer

A few prompts are decisions, not notices, so they never expire: the startup
recovery prompt, a context-window warning (`Compact` / `Start fresh`), the
daily budget limit, carrying a summary into a fresh conversation, attaching a
large image, and deleting conversations. They share one decision pane at the
bottom of the chat pane and the answer keys are painted in it (for example
`[y] Delete  [n] Cancel  [Esc] Cancel`). An automatically raised decision never
takes the keyboard while you are typing (a non-empty composer, or a printable key within the last
second), while an overlay or the Notification Log is open, or within 60 s of a
`Later`; it then waits behind a `Decision waiting — Ctrl+X q` toast and a
`Needs you (N) ^X q` cue in the status row. `Ctrl+X q` focuses the oldest
waiting decision. Answer keys are ignored for the first 500 ms after a decision
first paints and are never replayed. `Esc` defers a decision (`Later`), except
that it continues on the recovery prompt and cancels a delete. In the
Notification Log, `K` on a context-warning record raises the Compact decision
and `n` on a context-warning or compaction-failure record raises the
carry-over decision. The old keyed action chips no longer exist.

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

OpenAI-family Chat Completions providers (`openai`, `openrouter`, `google`,
`deepseek`, `moonshot`, `openai-compatible`) resolve their key for every request,
so a key changed with `rustain auth login` applies without a restart, and they now
send the system prompt as the leading `system` message (it was previously dropped).

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
