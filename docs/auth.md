# Authentication (`rustain auth`)

Stories 13.4a/13.4b introduce the `auth` subcommand family for managing and
inspecting provider credentials from the command line.

## `rustain auth login <provider>`

Configure API credentials for an AI provider via interactive masked entry with
pre-storage validation.

### Usage

```bash
rustain auth login anthropic    # store an Anthropic API key
rustain auth login openai       # store an OpenAI API key
rustain auth login ollama       # reports "no API key required"
```

### How it works

1. **Masked entry** — the API key is read with no terminal echo (via `rpassword`).
2. **Validation** — the key is validated against the provider's `/models` endpoint
   before storage. An invalid key is never written to disk.
3. **Storage** — credentials are persisted to `~/.rustain/auth.json` (mode `0600`,
   advisory-locked writes, atomic temp+rename) via the `AuthStorePort`.
4. **Overwrite warning** — if a credential already exists for the provider, a
   confirmation prompt is shown before replacing it.

### Key resolution precedence

When constructing a provider, the key is resolved in this order (highest wins):

1. **Environment variable** (e.g. `ANTHROPIC_API_KEY`) — always takes precedence.
   Anthropic also accepts `ANTHROPIC_AUTH_TOKEN`; either env credential wins.
2. **`auth.json`** — the stored credential from `rustain auth login`.
3. **Config** — reserved for future auth sources; current config stores env var
   names, not plaintext keys.

This means `auth.json` is a convenient alternative to env vars, but an env var
will always override it (AC7, backward compatible). `auth status` reports the
same winning source the provider construction path uses.

### Supported providers

| Provider   | Env var              | Key required |
|------------|----------------------|:------------:|
| anthropic  | `ANTHROPIC_API_KEY`  | ✓            |
| openai     | `OPENAI_API_KEY`     | ✓            |
| openrouter | `OPENROUTER_API_KEY` | ✓            |
| google     | `GOOGLE_API_KEY`     | ✓            |
| deepseek   | `DEEPSEEK_API_KEY`   | ✓            |
| moonshot   | `MOONSHOT_API_KEY`   | ✓            |
| ollama     | —                    | ✗            |

### Security

- Credentials are **never** logged, printed, or displayed in plaintext.
- `auth.json` is stored with file mode `0600` (owner-only read/write).
- Writes use advisory locking (`flock`) and atomic temp+rename.
- The `Credential` type masks its contents in `Debug`, `Display`, and `Serialize`.


### JSON output

Use `--json` for machine-readable login output:

```bash
rustain auth login anthropic --json
# → {"provider":"anthropic","status":"authenticated","validated":true}
```

## `rustain auth status`

Show configured credential sources without validating keys or making network
calls. The command is read-only and exits successfully when no credentials are
configured.

```bash
rustain auth status
```

Example:

```text
PROVIDER         STATUS SOURCE     LAST VALIDATED
Anthropic        ✓      auth.json  2026-06-15T22:00:00+00:00
OpenAI           ⚠      env        —
```

Status markers:

- `✓` — stored credential was validated by `auth login`.
- `⚠` — credential is present but rustain has no validation record, usually
  because it came from an env var.
- `✗` — reserved for invalid credentials; `status` does not probe, so it does
  not produce this today.

Use `--json` for scripts:

```bash
rustain auth status --json
# → {
#     \"schema_version\": \"1.0\",
#     \"providers\": [
#       {
#         \"provider\": \"anthropic\",
#         \"status\": \"authenticated\",
#         \"source\": \"auth_json\",
#         \"last_validated\": \"2026-06-15T22:00:00+00:00\",
#         \"requires_key\": true
#       }
#     ]
#   }
```

`auth status` lists configured providers only. Use `auth list` (below) for the
full provider catalog.

## `rustain auth list`

List **all** supported providers — including unconfigured and keyless ones — with
auth methods, configured status, signup URLs, and the active default marker.
The command is read-only, offline-safe, and never builds a provider.

```bash
rustain auth list
```

Example:

```text
PROVIDER       AUTH METHODS  CONFIGURED  SIGNUP URL
Anthropic *    api-key       ✓           https://console.anthropic.com/
OpenAI         api-key       ✓           https://platform.openai.com/api-keys
OpenRouter     api-key                   https://openrouter.ai/keys
Google AI      api-key                   https://aistudio.google.com/apikey
DeepSeek       api-key                   https://platform.deepseek.com/api_keys
Moonshot AI    api-key                   https://platform.moonshot.cn/console/api-keys
Ollama         none (local)  n/a         https://ollama.com/

* = active default provider
Run `rustain auth status` for credential sources, or `rustain auth login <provider>` to add one.
```

### Column legend

- **PROVIDER** — display name; `*` suffix marks the active default provider
  (the first enabled `[provider]` entry in config, else `anthropic`).
- **AUTH METHODS** — `api-key` for key-requiring providers; `none (local)` for
  keyless providers like Ollama.
- **CONFIGURED** — `✓` if a credential is found (env var or `auth.json`);
  blank if absent; `n/a` for keyless providers (no credential concept).
- **SIGNUP URL** — where to create/manage API keys for this provider.

The env-var credential always wins over `auth.json` (same precedence as
`auth status` and the runtime provider construction).

### JSON output

Use `--json` for machine-readable output:

```bash
rustain auth list --json
```

```json
{
  "schema_version": "1.0",
  "providers": [
    {
      "provider": "anthropic",
      "display_name": "Anthropic",
      "auth_methods": ["api_key"],
      "signup_url": "https://console.anthropic.com/",
      "requires_key": true,
      "configured": true,
      "is_default": true
    },
    {
      "provider": "ollama",
      "display_name": "Ollama",
      "auth_methods": ["none"],
      "signup_url": "https://ollama.com/",
      "requires_key": false,
      "configured": null,
      "is_default": false
    }
  ]
}
```

Notes:
- `configured` is `true`/`false` for key-requiring providers, `null` for keyless
  (`null` ≠ `false` — keyless providers have no credential concept).
- `is_default` marks the configured default provider (config-intent, not
  build-verified).
- `auth_methods` tokens are snake_case (`api_key`, `none`).
- No `source` field — use `auth status --json` for credential provenance.
