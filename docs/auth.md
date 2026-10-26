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
4. **Overwrite warning** — if an enabled API key already exists for the
   provider, a confirmation prompt is shown before replacing it.

### Key resolution precedence

The shared cascade resolves credentials in this order (highest wins):

1. **Runtime** — explicit candidate passed by the login validation probe.
2. **Stored OAuth** — first enabled OAuth row in file order.
3. **Login key** — enabled API key with `origin: "login"`.
4. **Environment variable** — trimmed, non-empty value.
5. **Stored key** — enabled API key with `origin: "stored"`.

Current paste-based `auth login` writes `stored` origin; migrated v1 keys also
have `stored` origin, so environment variables still override existing API keys.
The OAuth and login-origin stages prepare for future login engines; this release
does not create those rows through the CLI. Config stores env-var names, never
plaintext credentials. Anthropic status/list and legacy startup check
`ANTHROPIC_AUTH_TOKEN` before `ANTHROPIC_API_KEY`; config-driven construction
uses the configured env-var name.

Status, list and ACP availability include stale OAuth rows. Synchronous provider
construction cannot refresh: it skips stale rows (expiry within 60 seconds) and
tries the next row or stage. Thus stale-only OAuth reports configured with `⚠`,
but cannot construct a provider until refreshed. Disabled rows never serve.

### Store migration and downgrade safety

The v2 store keeps provider arrays with stable row IDs, revisions, origin,
validation timestamps and optional `disabled_cause`. OAuth access and refresh
tokens are secret-wrapped in memory. v1 reads migrate in memory without writing;
the first write persists v2. Unknown credential types survive unrelated writes
verbatim and never participate in selection. Files newer than v2 are rejected,
never overwritten; synchronous startup warns and ignores their stored credentials.
Older binaries cannot read a v2 store that holds credentials, and their
read-before-write guard prevents clobbering it. An empty v2 store (every
provider removed) is readable by an older binary, which rewrites it as v1; the
next v2 read migrates it again. Back up the store before downgrading; do not
expect an older binary to use v2 credentials.

OAuth refresh (live once login engines create OAuth rows) is lazy and elects one
refresher across processes: the refresher holds an advisory lock on
`auth.json.refresh.lock` while it re-reads the row, runs the bounded (10 s) token
exchange and writes the revision-checked result, so two rustain processes never
spend the same refresh token. The kernel releases the lock if its holder exits.

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
Anthropic        ✓      stored     2026-06-15T22:00:00+00:00
OpenAI           ⚠      env        —
```

Status markers:

- `✓` — stored credential was validated by `auth login`.
- `⚠` — no validation record, or the winning OAuth token needs refresh.
- `✗` — all available stored rows are disabled and no other cascade stage serves.
  SOURCE is `disabled`, with the cause included in JSON.

Human sources are `oauth`, `login`, `env`, and `stored`. JSON schema 2.0 uses
`oauth`, `login_key`, `env`, and `stored_key`; disabled rows use `disabled`
and add `disabled_cause`. Tokens are never included.

Use `--json` for scripts:

```bash
rustain auth status --json
# → {
#     \"schema_version\": \"2.0\",
#     \"providers\": [
#       {
#         \"provider\": \"anthropic\",
#         \"status\": \"authenticated\",
#         \"source\": \"stored_key\",
#         \"last_validated\": \"2026-06-15T22:00:00+00:00\",
#         \"requires_key\": true
#       }
#     ]
#   }
```

`auth status` lists configured providers and providers with only disabled rows.
Use `auth list` (below) for the full provider catalog.

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

Configured means the shared cascade finds a credential, including stale OAuth;
disabled-only providers are not configured. `auth status` reports provenance.

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
