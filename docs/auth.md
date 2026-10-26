# Authentication (`rustain auth`)

Manage provider credentials with browser-based subscription login, masked API-key
entry, logout, and offline status inspection.

## `rustain auth login <provider>`

Anthropic supports Claude Pro/Max browser subscription login and API keys. Other
keyed providers use masked API-key entry with pre-storage validation.

### Usage

```bash
rustain auth login anthropic --method oauth     # Claude Pro/Max browser login
rustain auth login anthropic --method api-key   # Anthropic console key or subscription token
rustain auth login openai                      # store an OpenAI API key
rustain auth login ollama       # reports "no API key required"
```

With multiple methods, an interactive terminal offers a numbered picker. Without
a terminal, choose `--method oauth` explicitly; API-key entry still requires a TTY.
The browser flow prints its authorization URL to stderr, listens on localhost
port 54545 (falling back to a free port when necessary), and uses PKCE and a
separate CSRF state. In a terminal it also accepts the final redirect URL, a
`code=…&state=…` query (any parameter order, optional leading `?` or `#`),
`code#state`, or a bare code. A supplied wrong state is rejected and re-prompts;
a state-less or empty-state paste is accepted. The browser/paste flow times out
after 300 seconds; masked API-key entry has no deadline. Ctrl-C cancels the flow
and releases its listeners. Browser launch is best-effort; non-TTY login has no
paste prompt.

**Subscription policy risk:** Anthropic's
[authentication and credential-use terms](https://code.claude.com/docs/en/legal-and-compliance)
prohibit third-party applications from offering Claude.ai login or routing
Free/Pro/Max subscription credentials. Using this flow can put your account at
risk of provider action. Its availability here is not a claim of Anthropic
approval; use an Anthropic console API key if you do not accept that risk.

OAuth login stores an enabled subscription credential with the login instant,
account and organization identity when available. Re-login replaces the same
identity without prompting; another organization or account adds a separate row.
The success line identifies your account, never a token. API-key overwrite
confirmation applies only to enabled API-key rows.

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

Both OAuth login and API-key login now write `origin: "login"`: a new login key
beats environment variables. Migrated v1 keys and older paste logins retain
`stored` origin below env. Config stores env-var names, never plaintext secrets.
Anthropic status/list and legacy startup check `ANTHROPIC_AUTH_TOKEN` before
`ANTHROPIC_API_KEY`; config-driven construction uses the configured env name.

Every Anthropic request resolves its current credential through one shared
resolver per process. A stale-only subscription builds successfully and refreshes
on first use; status/list/ACP already consider that row configured. The engine
subtracts a 300-second expiry margin and the resolver applies another 60 seconds.
Disabled rows never serve. An unreadable or newer-version store warns and ignores
stored rows while runtime/env credentials remain usable.

### Anthropic gateways and subscription requests

The effective endpoint is config `base_url`, then `ANTHROPIC_BASE_URL`, then
`https://api.anthropic.com`. Stored OAuth credentials are sent **only** to the
official Anthropic API origin; an alternate origin skips that cascade stage.
An operator-supplied env/pasted key containing `sk-ant-oat` is sent where the
operator explicitly points it, with the same subscription request profile.

OAuth credentials and keys containing `sk-ant-oat` always use Bearer auth and
the Claude Code compatibility profile (headers, billing/identity system blocks,
tool-name mapping and account/session metadata). They are never rejected as
subscription tokens. Other keys use `x-api-key`, except the legacy
`ANTHROPIC_AUTH_TOKEN` env source, which uses Bearer. Set an explicit gateway
scheme when needed:

```toml
[provider.gateway]
provider_id = "gateway"
kind = "anthropic"
model_id = "claude-sonnet-4-6"
api_key_env = "GW_TOKEN"
base_url = "https://gateway.example"
auth_header = "bearer" # or "x-api-key"
```

The env-name `contains("API_KEY")` heuristic is gone. Config validation and the
factory warn about older Anthropic configs with non-`API_KEY` env names and no
explicit scheme (`ANTHROPIC_AUTH_TOKEN` keeps Bearer, so it is not warned about).
`auth_header` cannot override a subscription Bearer profile.
`RUSTAIN_CLAUDE_CODE_VERSION` pins the compatibility version; otherwise rustain
uses the version adopted from a provider rejection, falling back to `2.1.280`.
Before streaming content, a subscription-profile request rejected with
`claude_code_version_too_old` can adopt a strictly newer `major.minor.patch`
version named by the server and rebuild its headers, billing block and checksum.
Later requests in the process use that version. Each strictly newer version
earns another attempt; a repeated/older/malformed version stops the retry.
The env pin disables adoption, and plain API keys never adopt. Health checks
and connectivity probes do not perform version adoption. This is a request
fingerprint, not rustain's own version.

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

OAuth refresh is lazy and elects one refresher per store across processes:
the refresher holds an advisory lock on `auth.json.refresh.lock` while it
re-reads the row, runs the bounded (10 s) exchange and writes the revision-checked
result. Two rustain processes therefore never spend one rotating refresh token
concurrently. The lease wait is bounded at 15 s. A peer's already-fresh rotation
is adopted. Transient errors retain the row; definitive revocation disables it
with a sanitized cause. The kernel releases the lease if its holder exits.

Writes fsync the temporary file and parent directory around atomic rename.
If a rotated-token write fails with an I/O/lock error, it is retried once under
the lease. If persistence still fails, the fresh access token serves the current
request with a warning that re-login may be required. A timed-out exchange is
dropped and remains transient; no live-account success is implied. A request
abandoned mid-refresh (for example a cancelled turn) does not abandon the
exchange: it still completes and persists the rotated token for the next request.
Token exchange, refresh and identity-bootstrap requests never follow HTTP
redirects, so a code, verifier or refresh token is only ever sent to its policy
origin; a redirect fails the login or leaves the refresh transient.

For stored OAuth subscriptions only, an HTTP 401 before streaming content can
force a refresh of the **same row** and retry once with a different bearer.
Streaming completions, health checks and connectivity probes share this policy.
A second 401 ends the request; HTTP 403 never triggers refresh or account
rotation. Runtime/login/env/stored API keys (including pasted/env `sk-ant-oat`
tokens) do not receive this auth retry. A removed/disabled row, unchanged bearer,
or fresh token this process minted within five minutes is not retried. A peer's
newer fresh revision is adopted under the store lease rather than minted again.
Transient refresh failure surfaces as a connection failure without disabling
the row. Cancellation still lets an in-flight forced refresh persist its rotated
token. Auth recovery and strictly-newer version retries have independent budgets.

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
rustain auth login anthropic --method oauth --json
# → {"provider":"anthropic","status":"authenticated","method":"oauth","validated":true,"identity":{"email":"you@example.com"}}
```

API-key results use `"method":"api_key"`. Identity contains only fields actually
learned. Guidance and the credential-file path go to stderr; stdout is reserved
for the result line or JSON.

## `rustain auth logout <provider>`

```bash
rustain auth logout anthropic
rustain auth logout anthropic --json
```

Logout removes every stored row for that provider, including disabled tombstones,
without changing environment variables. If a provider env variable is still set,
the command says that variable will still be used. Repeating logout succeeds and
reports no stored credentials. Account-specific logout is not offered here.

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

Enabled Anthropic OAuth rows with a recorded login instant also carry a
grant-lifetime reminder. Anthropic grants last approximately 30 days after login,
regardless of access-token refresh. During the final seven days, human status
adds one indented line per account after the table, for example:

```text
  ⚠ you@example.com (Organization) — re-login within 5d12h (Anthropic expires OAuth grants ~30d after login)
  ⚠ old@example.com — grant is past Anthropic's ~30d lifetime; re-login now
```

These lines have no ANSI colour. Account labels follow the login success line
(email, else account ID; organization name, else organization ID), with terminal
escape sequences and control characters removed; a row with no identity uses the
provider display name. The reminder is a display heuristic:
it never disables a credential. JSON schema remains `2.0`; the Anthropic provider
adds `oauth_grants`, containing `who` and RFC 3339 `grant_expires_at` for **every**
enabled OAuth row with a login instant, even outside the warning window. The
field is omitted when empty. Other providers and API-key rows are unchanged.

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
Anthropic *    oauth, api-key ✓          https://console.anthropic.com/
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
- **AUTH METHODS** — `oauth, api-key` for Anthropic, `api-key` for other
  key-requiring providers, and `none (local)` for keyless providers like Ollama.
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
      "auth_methods": ["oauth", "api_key"],
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
