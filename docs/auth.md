# Authentication (`rustain auth`)

Manage provider credentials with browser-based subscription login, masked API-key
entry, logout, and offline status inspection.

## Provider availability

The built-in Anthropic, ChatGPT Codex, OpenAI, OpenRouter, Google AI, DeepSeek, Moonshot,
xAI and xAI Grok OAuth
catalogues are visible without adding provider configuration. A home
`auth.json` credential or a non-empty provider environment variable unlocks its
provider from any working directory. Workspace configuration remains an explicit
override; credentials are not tied to the current project.

Providers without usable credentials appear **not signed in** in the model
selector. Their public catalogue and sign-in hint remain visible, but Enter
does not select a locked model or close the selector. For example:
``Set OPENAI_API_KEY or run `rustain auth login openai` ``.
Stored Anthropic OAuth counts only when the effective endpoint has the official
API origin, just as it does for request construction. A gateway uses its
configured key environment name, not an unrelated Anthropic environment key.

Ollama remains explicitly configured and keyless. An `openai-compatible` local
server is keyless **only** when `api_key_env = ""`; naming an unset variable
locks it instead of silently sending unauthenticated requests.

Discovery distinguishes a rejected credential (HTTP 401/403, **sign in again**)
from an unavailable endpoint or network failure; health checks classify 401/403
the same way, and a gateway without `/models` (404/405) posts no notice. A
matching cached catalogue survives fetch failures; rejected credentials mark
served models as stale. Successful live discovery prunes models the account no
longer exposes, and stays in effect for the session even if the cache write
fails. A successful empty list stays empty for that cycle and does not
overwrite the last-good cache.

The metadata order is bundled → provider-aware models.dev → credential-matching
cache → live, merged per model and field. Sparse live records keep useful names,
context limits and capabilities from earlier layers. Cache identities are
opaque credential/endpoint-scoped hashes stamped with the credential a
discovery request actually sent (a stored OAuth login by its account identity,
a key by its value), and re-checked against the credential requests would use now; replacing
a credential refetches without waiting for the TTL, and removing it never
exposes an old account's private roster in a locked pane. Discovery cache v2 is
rebuilt once as v3;
bundled seed and existing pricing-cache compatibility are unchanged.

After login actually changes stored rows, rustain immediately discovers that
provider again when it supports live discovery and reports its result on stderr.
Codex is included: a successful login prints, for example,
`Discovered N models for ChatGPT Plus/Pro (Codex Subscription).`
models.dev currently has no Codex provider source;
unrelated OpenAI API models are not promoted into the subscription catalogue.
Login stdout and success exit status are unchanged even if discovery fails.

An unreadable or future-version `auth.json` produces a warning and falls back to
environment credentials in startup, `auth status`, `auth list`, and ACP.
Both CLI commands still exit successfully; their JSON adds top-level
`store_error` only when the read failed. Parse warnings never include credential
file contents. ACP returns the normal authentication-required error when
neither stored nor environment credentials can serve.

## `rustain auth login <provider>`

Anthropic supports Claude Pro/Max browser subscription login and API keys.
ChatGPT Codex supports browser and device-code subscription login. xAI Grok OAuth
(`xai-oauth`) supports device-code subscription login only. GitHub Copilot
(`github-copilot`) supports device login only, for github.com or a GitHub
Enterprise domain. Google Cloud Code Assist (`google-gemini-cli`) supports browser
subscription login only. OpenRouter supports browser key minting and API keys.
Other keyed providers, including `xai` and `google`, use masked API-key entry with
pre-storage validation.

### Usage

```bash
rustain auth login anthropic --method oauth     # Claude Pro/Max browser login
rustain auth login anthropic --method api-key   # Anthropic console key or subscription token
rustain auth login openai                      # store an OpenAI API key
rustain auth login openai-codex --method oauth  # ChatGPT Plus/Pro browser login
rustain auth login openai-codex --method device # ChatGPT Plus/Pro device code (headless)
rustain auth login xai-oauth                    # SuperGrok / X Premium+ device code
rustain auth login xai                          # store an xAI API key
rustain auth login github-copilot               # GitHub Copilot device login (github.com or Enterprise)
rustain auth login google-gemini-cli            # Gemini CLI (Cloud Code Assist) browser login
rustain auth login openrouter --method oauth    # mint an OpenRouter key in the browser
rustain auth login openrouter --method api-key  # store an existing sk-or- key
rustain auth login ollama       # reports "no API key required"
```

With multiple methods, an interactive terminal offers a numbered picker. Without
a terminal, choose `--method oauth` or `--method device` explicitly (the error names
the provider's choices); API-key entry still requires a TTY. Device-code login needs
no TTY.
The Anthropic browser flow prints its authorization URL to stderr, listens on localhost
port 54545 (falling back to a free port when necessary), and uses PKCE and a
separate CSRF state. In a terminal it also accepts the final redirect URL, a
`code=…&state=…` query (any parameter order, optional leading `?` or `#`),
`code#state`, or a bare code. A supplied wrong state is rejected and re-prompts;
a state-less or empty-state paste is accepted. The browser/paste wait times out
after 300 seconds; the token exchange and any post-login provisioning that follow
it run under their own per-request timeouts, and masked API-key entry has no
deadline. Ctrl-C cancels the flow
and releases its listeners. Browser launch is best-effort; non-TTY login has no
paste prompt.

**Subscription policy risk:** Anthropic's
[authentication and credential-use terms](https://code.claude.com/docs/en/legal-and-compliance)
prohibit third-party applications from offering Claude.ai login or routing
Free/Pro/Max subscription credentials. Using this flow can put your account at
risk of provider action. Its availability here is not a claim of Anthropic
approval; use an Anthropic console API key if you do not accept that risk.

**Codex subscription policy risk:** This third-party client uses ChatGPT
subscription credentials with an omp-compatible request profile; its availability
does not imply OpenAI approval. Provider policies or backend changes may restrict
access or put your account at risk. rustain does not refuse subscription
tokens. Use the separate `openai` API-key provider if you do not
accept subscription-client risk.

**xAI subscription policy risk:** This third-party client uses SuperGrok / X
Premium+ subscription credentials obtained through xAI's device login, with an
omp-compatible request profile (`User-Agent: omp/18.12.6`); its availability does
not imply xAI approval. Provider policies or backend changes may restrict access
or put your account at risk. rustain does not refuse subscription tokens. Use the
separate `xai` API-key provider if you do not accept subscription-client risk.

**GitHub Copilot subscription policy risk:** This third-party client uses GitHub
Copilot subscription credentials obtained through GitHub's device login, with an
omp-compatible wire identity (`copilot/1.0.82`, `copilot-developer-action/0.0.1` and
the OpenCode / Copilot CLI OAuth client ids); its availability does not imply GitHub
approval. Provider policies, organization settings or backend changes may restrict
access or put your account at risk. rustain does not refuse subscription tokens.

**Gemini CLI subscription policy risk:** This third-party client signs in with
Google's Gemini CLI installed-app OAuth client and sends requests with the Gemini
CLI wire identity (`User-Agent: GeminiCLI/0.46.0/<model> (<platform>; <arch>;
terminal)` and Gemini CLI `Client-Metadata`), as oh-my-pi does; model discovery
also sends the access token to Google's Antigravity hosts
(`daily-cloudcode-pa.googleapis.com` and `daily-cloudcode-pa.sandbox.googleapis.com`)
with the Antigravity identity (`antigravity/hub/2.19.1 (aidev_client;
os_type=darwin; arch=arm64; cl=963137146)`). Its availability does not imply Google
approval. Provider policies or backend changes may restrict access or put your
account at risk. rustain does not refuse subscription tokens. Use the separate
`google` API-key provider if you do not accept subscription-client risk.

### OpenRouter

`rustain auth login openrouter --method oauth` creates a durable OpenRouter API
key through OpenRouter's browser PKCE flow, as oh-my-pi does: the authorization
URL (`https://openrouter.ai/auth`) carries only `callback_url`, an S256
`code_challenge` and `code_challenge_method` (no client id, scope or state). The
callback listens on `http://localhost:54549/callback`, falling back to a free port
(`Preferred port 54549 unavailable, using port N`); the advertised `callback_url`
always names the port actually bound. In a terminal you may instead paste the
redirect URL, the authorization code, or an existing OpenRouter key: a pasted
value starting with `sk-or-` skips the exchange and is validated like
`--method api-key`. The code is exchanged once at
`https://openrouter.ai/api/v1/auth/keys` (JSON `code`, `code_verifier`,
`code_challenge_method`, 30 s); the returned key is stored as an ordinary API-key
row (method `browser_key` in `auth login --json`), and the success line is
`✓ Logged in to OpenRouter.`. Failures store nothing and never echo the response:
`OpenRouter key exchange failed (HTTP <status>)`, `OpenRouter key exchange
returned no key`, `OpenRouter key exchange failed: transport unavailable`, or
`OAuth authorization was denied`.

Both `--method api-key` and a pasted key are validated with one
`GET <base>/auth/key` (bearer only, 5 s); `/models` answers 200 to any bearer and
never validates a key. A rejected key is not stored (`✗ Invalid API key for
OpenRouter.`); a base without `/auth/key` or an unreachable endpoint stores
nothing either. Startup health uses the same request, so an invalid
`OPENROUTER_API_KEY` shows as unauthenticated.

Inference uses OpenRouter's Responses API (`<base>/responses`) by default and
Chat Completions (`<base>/chat/completions`) when `RUSTAIN_OPENROUTER_RESPONSES=0`;
no error ever switches transports. Both send rustain's attribution
(`User-Agent: rustain/<version>`, `HTTP-Referer: https://github.com/lunarpulse/rustain`,
`X-OpenRouter-Title: rustain`, `X-OpenRouter-Categories: cli-agent`) and enable
OpenRouter response caching (`X-OpenRouter-Cache: true`,
`X-OpenRouter-Cache-TTL: 3600`). With a credential, discovery lists only
tool-capable models from `<base>/models`; without one, OpenRouter stays locked
with the bundled catalogue. Several keys are account rows (see
[Multiple accounts](#multiple-accounts)).

### ChatGPT Plus/Pro (Codex Subscription)

Run `rustain auth login openai-codex` and finish the authorization. Codex offers
two methods: `--method oauth` (browser) and `--method device` (device code, see
below); an interactive terminal shows the picker when no `--method` is given.
The browser PKCE callback is `http://localhost:1455/auth/callback`: port **1455 is
pinned**. If it is busy, release the port or use `--method device`; rustain never
substitutes a random callback port. The shared state checking, paste fallback and
cancellation rules apply to the browser method.

**Device code (`--method device`).** The device method suits headless and remote
machines: it opens no local listener,
so port 1455 is never used. rustain requests a user code from
`https://auth.openai.com/api/accounts/deviceauth/usercode` and prints, on stderr,
`Enter code: <code>` followed by `https://auth.openai.com/codex/device`; open that
page on any device, sign in and enter the code. In an interactive terminal the page
is also opened in a browser (best-effort). It then polls: the first poll waits at
most 5 seconds, later polls wait the server's interval (default 5 s) plus 3 seconds,
and a pending authorization (HTTP 403/404) keeps polling. After **120 polls** the
login fails with `Device authorization timed out — user did not complete login in
time`; the browser method's 300-second deadline does not apply. Ctrl-C cancels the
login (`Login cancelled`) and no further poll is sent. Any other polling status
fails the login.

On approval, the server-issued authorization code and verifier are exchanged
through the browser method's token endpoint, so the stored row has the same shape,
identity and refresh rules as a browser login and is served by the same Codex
transport. The success line names `ChatGPT Plus/Pro (Codex Subscription)`.

Codex uses the Responses SSE backend, not OpenAI Chat Completions. Stored OAuth
tokens refresh lazily and can recover once after a rejected request. Alternatively,
set `OPENAI_CODEX_OAUTH_TOKEN` to an OAuth access token; this is a static bearer,
not an API key, and is not refreshed after a 401. Access-token claims supply the
request's account identity. With neither source configured, the model selector
keeps the public catalogue locked and shows
``Set OPENAI_CODEX_OAUTH_TOKEN or run `rustain auth login openai-codex` ``.
`rustain auth logout openai-codex` removes Codex's stored credentials only;
an environment token remains effective until you unset it.

Conversation IDs keep prompt-cache and turn-state affinity stable across restarts;
title and compaction requests use separate conversation-scoped sessions. Codex and
Claude Code share the private, locked `install-id` file in rustain's data directory.
The wire compatibility identity is pinned to omp (`originator: omp`,
`User-Agent: omp/18.12.6`), not rustain branding.

Codex discovers its live roster from `GET <codex>/models`, falling back to
`<root>/models` after a non-auth failure. Bundled models keep their bundled
names; `gpt-5.6-luna`, `gpt-5.6-sol` and `gpt-5.6-terra` get a 1,000,000-token
context floor, and rows marked hidden are excluded. The discovery cache is
scoped to the credential actually sent, so switching accounts never serves the
previous account's roster. A stored login's 401 refreshes once; a remaining
401/403 is final, with no fallback request: the picker marks the provider
`⚠ sign in again` and a rejected-credential notice names the login command.
models.dev has no Codex provider source today, so that layer is empty; health checks
inspect only the models endpoint's status.

Encrypted reasoning items are captured privately from each response that reaches
its terminal completion event, persisted with the session (local sessions,
`rustain --session <id> ask` resume and the daemon), and replayed only to the same
provider kind, model and account. Unlike omp, an item is bound to the account that
produced it: a stored login's account identity (or, for a login without one, that
login across its token refreshes) or the API key itself. After `auth use` another
account sends none of them; switching back replays them; items saved before
account binding are not replayed. They are
never shown in transcripts, observers, ACP, `ask` output or logs. A failed or
cancelled attempt discards its items, so it replays nothing.
Upstream lists a 128,000-token maximum output, but rustain has no descriptor field
or runtime consumer for that metadata: it does not send an output-token limit.


OAuth login stores an enabled subscription credential with the login instant,
account and organization identity when available. Re-login replaces the same
identity without prompting; another organization or account adds a separate row
and becomes the active account (see [Multiple accounts](#multiple-accounts)).
The success line identifies your account, never a token.

### xAI Grok OAuth (SuperGrok or X Premium+)

Run `rustain auth login xai-oauth`. It uses the RFC 8628 device authorization
grant, the provider's only method, so no picker or `--method` is needed and no TTY
or local listener is required. rustain requests a code from
`https://auth.x.ai/oauth2/device/code` (scope `openid profile email offline_access
grok-cli:access api:access`) and prints `Enter code: <code>` and xAI's
verification URL on stderr (the complete URL with the code embedded when xAI
returns one). An interactive terminal also opens it (best-effort).

The token endpoint is discovered, not hard-coded: once per login (after the code
is shown, before the first poll) and again for every refresh, rustain reads
`https://auth.x.ai/.well-known/openid-configuration` and accepts its
`token_endpoint` only when it is an `https` URL on `x.ai` or a `*.x.ai` host.
Anything else fails with `Invalid xAI token_endpoint: …`; a failed or malformed
discovery fails the login. The result is never cached.

Polling starts immediately and then waits the server's `interval` (default 5 s,
never below 1 s); each `slow_down` adds 5 seconds. Polling ends at the device
code's `expires_in` with `Device flow timed out` (after any `slow_down`, the
message adds a clock-drift hint for WSL/VM hosts). `expired_token` and
`access_denied` end the login with a restart/denied message; other failures name
only the HTTP status and a recognised OAuth error id, never the response body.
Ctrl-C cancels the login. The browser flow's 300-second deadline does not apply.

The account identity comes from the access token's JWT `sub`, then OIDC userinfo
(`https://auth.x.ai/oauth2/userinfo`: `email`, `sub`); a userinfo failure keeps
what the token supplied and does not fail the login. xAI identities are never
organization-scoped. The success line is `Logged in to xAI Grok OAuth (SuperGrok or
X Premium+)` plus the account. The stored expiry is the token's `expires_in` minus
300 seconds. Stored rows refresh lazily through the rediscovered token endpoint
(userinfo runs again; fields it omits keep their stored values) and a stored
row's HTTP 401 forces one refresh-and-retry, as for the other subscriptions.

Alternatively, set `XAI_OAUTH_TOKEN` to an OAuth access token: a static bearer,
not an API key, never refreshed. With neither source the selector shows
``Set XAI_OAUTH_TOKEN or run `rustain auth login xai-oauth` ``. Stored OAuth rows
are sent only to the official `https://api.x.ai` origin; `xai-oauth` ignores
`XAI_BASE_URL`, and a configured `base_url` on another origin makes stored rows
ineligible (an explicitly set `XAI_OAUTH_TOKEN` is still sent there).
`xai-oauth` has no API-key method; use `xai` for keys.

### xAI API key

Run `rustain auth login xai` for masked key entry (TTY required) or set
`XAI_API_KEY`; without either the selector shows
``Set XAI_API_KEY or run `rustain auth login xai` ``. Validation sends only the
candidate key (never env or stored keys) to `GET <base>/models`, by default
`https://api.x.ai/v1/models`. HTTP 401/403 rejects the key; a base without
`/models` (HTTP 404/405) stores it unvalidated with
`⚠  Could not confirm key — xAI credentials stored unvalidated.`;
offline or other failures store nothing. The `xai` base is a non-default configured
`base_url`, else `XAI_BASE_URL`, else `https://api.x.ai/v1`, for validation,
requests and discovery alike.

### xAI requests and model discovery

Both kinds stream from the xAI Responses API (`POST <base>/responses`) with Bearer
auth and the omp wire identity `User-Agent: omp/18.12.6`. The requested output
limit is clamped to the model's bundled maximum (when it has one) and to 64,000
tokens.

Both kinds discover live from `GET <base>/models`; the bundled default model is
`grok-4.6` for each. `xai` bundles 32 chat models and `xai-oauth` 10 curated ones.
The live roster keeps every bundled model in bundled order (bundled rows are not
pruned), then appends live IDs the bundle lacks, sorted by ID, with their live
names and context lengths; an ID no layer describes gets a 128,000-token context.
Image, video, speech and voice models (`grok-imagine-image`,
`grok-imagine-image-quality`, `grok-imagine-image-2.0`, `grok-imagine-video*`,
`grok-tts`, `grok-stt-*`, `grok-voice-*`; case-insensitive) are never listed.
`model_filter` applies.
A stored row's 401 refreshes once; a remaining 401/403 marks the provider
`⚠ sign in again`. Health checks and login probes classify 404/405 as an
unsupported endpoint. A login that changes stored rows triggers the post-login
discovery described above.

### GitHub Copilot (Individual, Business, Enterprise)

Run `rustain auth login github-copilot`. It is a device login (no TTY, browser or
local listener required). rustain first asks
`GitHub Enterprise URL/domain (blank for github.com) (company.ghe.com): ` on one
plain line: press Enter for github.com, or type a GitHub Enterprise Cloud domain or
URL (`company.ghe.com`, `https://company.ghe.com/...`). For headless Enterprise
logins pipe the answer on stdin (`printf 'company.ghe.com\n' | rustain auth login
github-copilot`); end of input counts as blank. An answer that is not a host fails
with `Invalid GitHub Enterprise URL/domain` before any request. github.com uses the
minimal-grant OpenCode OAuth app; Enterprise domains use the GitHub Copilot CLI app.

rustain then requests a device code from `https://<host>/login/device/code` (scope
`read:user`) and prints `Enter code: <code>` and GitHub's verification URL on
stderr. Polling follows omp's cadence: each poll waits 1.2 × the interval first;
a `slow_down` adds 5 s to the interval (or replaces it with the one GitHub sends)
and switches to 1.4 ×, until the code's
`expires_in`, then fails with `Device flow timed out` (with a clock-drift hint after
a `slow_down`). A rejected authorization ends with `Device flow failed: <error>`;
other failures name only the HTTP status and a recognised OAuth error id. Ctrl-C
cancels the login and nothing is stored.

After approval rustain asks `https://api.github.com/copilot_internal/user` (always
the public API, also for Enterprise accounts) for the account's Copilot API
endpoint, then prints `Enabling models...` and enables every bundled Copilot model
for the account (`POST <endpoint>/models/<id>/policy`, five at a time). Both steps
are best-effort: a failed probe or policy request never fails the login. The
GitHub token is stored as is (there is no token exchange) with the Enterprise
domain and the API endpoint; its "refresh" sends no HTTP and re-serves the same
token. A second `rustain auth login github-copilot` replaces every stored Copilot
login instead of adding a row, and the next request uses the new token.

Alternatively, set `COPILOT_GITHUB_TOKEN` to a GitHub token, or to a JSON envelope
`{"token":"…","enterpriseUrl":"company.ghe.com","apiEndpoint":"https://api.<plan>.githubcopilot.com"}`;
only its `token` is ever sent. With neither source the selector shows
``Set COPILOT_GITHUB_TOKEN or run `rustain auth login github-copilot` ``.

**Requests.** Each request picks omp's wire API by model: `claude-haiku-*`,
`claude-sonnet-*`, `claude-opus-*`, `claude-fable-*` and `claude-mythos-*` use
Anthropic Messages (`<root>/v1/messages`, Bearer auth, no Claude Code betas),
GPT-5/6, Grok 4.x, `oswe` and MAI models use Responses (`<root>/responses`), and
every other model (older Claude ids such as `claude-3-5-sonnet` included) uses
Chat Completions (`<root>/chat/completions`). The root is
the stored or envelope API endpoint, else the Enterprise root
(`https://copilot-api.<domain>`), else the endpoint re-probed by discovery, else
`https://api.githubcopilot.com`; a configured `base_url` off `githubcopilot.com`
is used verbatim (see `docs/configuration.md`). Requests carry omp's Copilot
headers (`Copilot-Integration-Id`, `X-Initiator`, `Openai-Intent`,
`X-GitHub-Api-Version`, `Copilot-Vision-Request` for image turns). The
integration id is `copilot-chat` for github.com accounts and
`copilot-developer-cli` for Enterprise accounts; set `COPILOT_INTEGRATION_ID` to
pin another. When an unpinned request is denied (HTTP 403, or 400
`model_not_supported`) rustain retries it once with the other identity and
remembers the identity that worked for that credential and endpoint (in memory).

**401 vs 403.** HTTP 401 means the GitHub token was revoked: the request fails
with an authentication error and nothing can repair it except a new login. HTTP
403 is a plan, model-policy or organization denial: the token stays valid, the
login is never disabled, and the error explains the Copilot plan, model policy and
integration-id options.

Copilot discovers its live roster from `GET <root>/models` with the Copilot CLI
identity (`copilot-developer-cli`, as omp; listing is not policy-gated); rows whose
`capabilities.type` is not `chat` (embeddings) are dropped, and models with a
long-context tier also get an opt-in `<id>-1m` sibling. Unlike other providers, a
Copilot listing 403 keeps the cached roster (a plan or policy denial, not a
rejected credential); only a 401 asks to sign in again.

### Google Cloud Code Assist (Gemini CLI)

Run `rustain auth login google-gemini-cli` (one method, no picker). It is Google's
installed-app authorization-code flow without PKCE: stderr shows
`Complete the sign-in in your browser.`, the authorization URL and
`Local browser launch: http://127.0.0.1:<port>/launch`. The callback listens on
`127.0.0.1` only, port 8085, falling back to a free port
(`Preferred port 8085 unavailable, using port <n>`); the redirect is
`http://127.0.0.1:<port>/oauth2callback`. State checking, paste (redirect URL,
query, `code#state` or bare code), denial and Ctrl-C follow the shared browser rules,
and the 300-second deadline covers only the browser/paste wait. The success line
is `Logged in to Google Cloud Code Assist (Gemini CLI)` plus the account email.

The exchange posts the authorization code with the client id and client secret
to `https://oauth2.googleapis.com/token`; the stored expiry is `expires_in` minus
300 seconds. The account email comes from Google userinfo (best-effort, 15 s; a
failure logs in without an email). A token response without a refresh token fails
with `No refresh token received. Please try again.`

The login succeeds only after a Cloud Code Assist project is found or provisioned
(`Checking for existing Cloud Code Assist project...`, `loadCodeAssist`). An
account with a current tier uses its project, else `GOOGLE_CLOUD_PROJECT` (or
`GOOGLE_CLOUD_PROJECT_ID` when the first is unset or blank). Otherwise the default
allowed tier is onboarded (`Provisioning Cloud Code Assist project (this may take a
moment)...`); a non-free tier needs one of those variables first. A pending
provisioning operation is polled at most 24 times, 5 seconds apart (`Waiting for
project provisioning (attempt n/24)...`), then fails with `Project provisioning did
not complete after 24 attempts`. Each request is bounded to 30 seconds. An account
Google must verify fails with `Account verification required[ for <email>]. Visit
<url> to continue, then sign in again.`; only an `https:` verification URL is shown.
Any failure stores nothing. The project variables are read at login only: the
project is stored with the credential and every request sends the project of the
same row as its bearer.

Stored rows refresh lazily with the client secret and keep their project, email and
refresh token; refresh never re-runs userinfo or provisioning. A row without a
project never refreshes: it fails before any token request with
`google-gemini-cli credentials are missing projectId; sign in again` (the row stays
enabled). A request with it fails before it is sent with ``Missing token or
projectId in Google Cloud credentials. Run `rustain auth login google-gemini-cli`
to re-authenticate.`` Run the login again in either case. There is no
environment-variable credential: without a login the selector shows
``Run `rustain auth login google-gemini-cli` ``. `rustain auth logout
google-gemini-cli` removes the rows. Stored rows are sent only to
`https://cloudcode-pa.googleapis.com` and, for model discovery, to the two
`daily-cloudcode-pa` Antigravity hosts; a configured `base_url` on another origin
makes them ineligible.

**Client secret provenance.** The installed-app client id and secret are the
Base64 literals oh-my-pi ships for this login (decoded only into the token request
body, never printed, logged or stored). Base64 is obfuscation, not secrecy: the
secret is an installed-app registration value that grants nothing without your
consent. The inspected source shows where rustain obtained the values; it does not
establish who at Google registered or published them.

Requests use Cloud Code Assist's native
`POST <base>/v1internal:streamGenerateContent?alt=sse` (never Chat Completions),
with the Gemini CLI headers above and omp's envelope, roles, images, tool schemas,
retries (4 attempts; server retry hints up to 5 minutes) and error texts. Gemini
thought signatures are kept privately per response and replayed only to the same
provider, model and account (never another account's, not even as a demoted
other-model rebuild) within a tool round and across text turns; they never appear in
transcripts, observers, ACP, `ask` output or logs.

**Model discovery.** A signed Gemini CLI entry discovers its models live, as
oh-my-pi does. It first asks Antigravity (`POST /v1internal:fetchAvailableModels`
with body `{}` on `daily-cloudcode-pa.googleapis.com`, then the sandbox host;
10 seconds each). Any failure there — including HTTP 401/403, which only means
the account lacks Antigravity access — falls back to the account's own quota list
(`POST <base>/v1internal:retrieveUserQuota` with the Gemini CLI headers and body
`{"project":"<stored project>"}`). A quota 401 refreshes the login once and
resends with the new token and the same row's project; a remaining 401/403 marks
the provider signed out with ``Run `rustain auth login google-gemini-cli` ``. Other
quota failures keep the last served catalogue. Effort-tier variants collapse into
one logical model (for example `gemini-3.1-pro-low`, `gemini-pro-agent` and
`gemini-3.1-pro-high` become `gemini-3.1-pro`; `gemini-3.7-flash-low`… become
`gemini-3.7-flash`), and on the quota path a live `X`/`X-thinking` twin becomes `X`.
The picker lists the 7 bundled models first (default `gemini-3.1-pro-preview`),
then the newly discovered ones by name. A logical model is sent without an effort
as oh-my-pi sends it: `gemini-3.1-pro` as `gemini-3.1-pro-low` with
`thinkingConfig {"includeThoughts":false,"thinkingLevel":"MINIMAL"}`,
`gemini-<rev>-flash` (3.6 and later) as `gemini-<rev>-flash-low` with
`{"includeThoughts":true,"thinkingLevel":"LOW"}`, and a twin `X` as `X` without a
thinking config; bundled ids are sent unchanged. When the account does not list
the member a route names, the request goes to its first listed member instead (for
example `gemini-3.1-pro` as `gemini-pro-agent` when `gemini-3.1-pro-low` is not
listed), and an upstream id that only looks like a logical id is sent as itself.
The roster is cached per account (it survives token refreshes, never another
account), refreshed at startup, every
4 hours and after login: `rustain auth login google-gemini-cli` ends with
`Discovered N models for Google Cloud Code Assist (Gemini CLI).` The Antigravity
client version is pinned (no update-manifest lookup).

### How it works

1. **Masked entry** — the API key is read with no terminal echo (via `rpassword`).
2. **Validation** — the key is validated against the provider's `/models` endpoint
   before storage. An invalid key is never written to disk.
3. **Storage** — credentials are persisted to `~/.rustain/auth.json` (mode `0600`,
   advisory-locked writes, atomic temp+rename) via the `AuthStorePort`.
4. **Accounts** — the same key again updates its row in place; a different key
   is stored as another account and becomes the active one. Nothing is asked
   before the key itself.

### Multiple accounts

Every provider can hold several stored accounts: distinct API keys, OAuth
logins of different accounts or organizations, or both. Each login makes the
account it wrote the provider's **active account**, and when the provider then
holds more than one enabled account the login adds, on stderr:

```text
Active OpenAI account: API key #2.
Switch with `rustain auth use openai --id <N>`; remove one with `rustain auth logout openai --id <N>`.
```

```bash
rustain auth accounts              # every provider with stored accounts
rustain auth accounts openai       # one provider (also --json)
rustain auth use openai --id 1     # make account #1 active
rustain auth logout openai --id 2  # remove account #2 only
```

OpenRouter keys, browser-minted or pasted, are ordinary API-key account rows:
minting the same key again updates its row, and each new key becomes the active
account.

`auth accounts` prints one header per provider — `OpenAI (openai): serving API key #1`,
`serving OPENAI_API_KEY`, or `no usable credential` — and one line per stored
account: `#<id>  <label>  <oauth|api_key>  <login|stored>  <state>`, where state is
`active` (selected and served), `selected` (selected but not served, for example a
migrated key below a set environment variable), `serving` (served by the default
order), or `disabled: <cause>`. Labels are `API key #<id>`, or for OAuth the
account's email, account or project, else `OAuth credential #<id>`, with the
organization appended when it differs; key bytes are never shown. `--json` prints
`{"schema_version":"1.0","providers":[{"provider","display_name","served","accounts":[…]}]}`,
where `served` is `{"source":"oauth"|"login_key"|"stored_key","id":N}`,
`{"source":"env","var":…}` or `null`.

The choice is durable: it is stored in `auth.json`, survives restarts, and a
running TUI or daemon serves it on its next request. Account ids are never reused
within one `auth.json` (a deleted or recreated store starts again at `#1`), so a
stale id is refused (`No stored credential #N for openai. Run \`rustain auth
accounts openai\`.`) and never names another account. A disabled account cannot be
selected (`Credential #N for openai is disabled (<cause>); log in again.`).
`auth use` and `auth logout --id` refresh the provider's live model list when a
credential still serves it, printing the same `Discovered N models` line as login.

Google Cloud Code Assist logins whose best-effort email lookup failed are stored
by their project; a later login of the same project that recovers the email
updates that row instead of adding a second one. The reverse never merges: a
login without an email never overwrites an identified account.

### Key resolution precedence

The shared cascade resolves credentials in this order (highest wins):

1. **Runtime** — explicit candidate passed by the login validation probe.
2. **Active account** — the provider's selected row, when it is an enabled OAuth
   grant the endpoint may receive, or an enabled login key.
3. **Stored OAuth** — first enabled OAuth row, the active row first, then file order.
4. **Login key** — enabled API key with `origin: "login"`.
5. **Environment variable** — trimmed, non-empty value.
6. **Stored key** — enabled API key with `origin: "stored"` (an active one first).

A selected login key therefore beats an unselected OAuth row, but a selected
migrated (`stored`) key never outranks the provider's environment variable. A
disabled, removed, refresh-failed or unrecognised selection falls back to the
default order; removing the active account clears the choice and never selects a
neighbour.

Both OAuth login and API-key login now write `origin: "login"`: a new login key
beats environment variables. Migrated v1 keys and older paste logins retain
`stored` origin below env. Config stores env-var names, never plaintext secrets.
Bundled Anthropic status/list and startup check `ANTHROPIC_AUTH_TOKEN` before
`ANTHROPIC_API_KEY`; config-driven availability and construction use the configured env name.

A stored OAuth row leaves only for an endpoint that both the entry's `kind` and
the row's own provider accept: `[provider.openai-codex] kind = "openai"` with
another `base_url` is locked rather than sending the Codex grant there. A
`google-gemini-cli` entry serves its stored login only — never an API key or
environment variable, under any entry name.

Every Anthropic, Codex and xAI request resolves its current credential through one shared
resolver per process. A stale-only subscription builds successfully and refreshes
on first use; status/list/ACP already consider that row configured. The engine
subtracts a 300-second expiry margin for Anthropic and xAI OAuth (zero for Codex),
and the resolver applies another 60 seconds.
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

The v2 store keeps provider arrays with row IDs, revisions, an opaque grant
generation, origin, validation timestamps and optional `disabled_cause`. A login
creates a new generation even when it updates an existing row for the same
identity. Refresh preserves the generation and advances the revision. OAuth access
and refresh tokens are secret-wrapped in memory. Legacy v2 rows without a generation
remain readable without writing: their initial generation is derived from the
provider and original row, then persisted on the next write. v1 reads likewise
migrate in memory; the first write persists v2. Unknown credential types survive
unrelated writes verbatim and never participate in selection. Files newer than v2
are rejected, never overwritten; synchronous startup warns and ignores their
stored credentials.
Binaries predating the v2 store cannot read it when it holds credentials; their
read-before-write guard prevents clobbering it. An empty v2 store (every
provider removed) is readable by an older binary, which rewrites it as v1; the
next v2 read migrates it again. Back up the store before downgrading; do not
expect an older binary to use v2 credentials.

The active-account choice is an optional top-level `selected` map in the same
version 2 file. A v2 binary that predates it ignores the key and drops it on its
next write; its provider then serves the default order. No row changes.

Do not run pre-generation v2 writers alongside this version. They can drop the
`generation` field, causing an in-flight fenced refresh to discard its stale
write and potentially require signing in again. Stop older rustain processes
before upgrading. Mixed-version writers are unsupported; this version fails
closed rather than switching accounts.

OAuth refresh is lazy and elects one refresher per store across processes:
the refresher holds an advisory lock on `auth.json.refresh.lock` while it
re-reads the exact grant, runs the bounded (10 s) exchange and writes the
generation- and revision-checked result. Two rustain processes therefore never
spend one rotating refresh token concurrently. The lease wait is bounded at 15 s.
A peer's already-fresh rotation of the same grant is adopted. Transient errors
retain the row; definitive revocation disables it
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
force a refresh of the **same grant** and retry once with a different bearer.
Streaming completions, health checks, connectivity probes and discovery share this policy.
A second 401 ends the request; HTTP 403 never triggers refresh or account
rotation. Runtime/login/env/stored API keys (including pasted/env `sk-ant-oat`
tokens) do not receive this auth retry. A removed/disabled row, unchanged bearer,
or fresh token this process minted within five minutes is not retried. A peer's
newer fresh revision of the same generation is adopted under the store lease
rather than minted again. Re-login or deleting/recreating `auth.json` cannot make
an already-running request switch grants, even if row ID, revision or identity
are reused. A replacement grant is neither refreshed on that request's behalf nor
overwritten by an older in-flight refresh; start a new request after login.
Transient refresh failure surfaces as a connection failure without disabling
the row. Cancellation still lets an in-flight forced refresh persist its rotated
token. Auth recovery and strictly-newer version retries have independent budgets.
For providers that support live model discovery, the post-login refresh also
compares the full grant stamp: recreating the store cannot suppress rediscovery
merely because a row ID and revision repeat. Codex live discovery follows the
same post-login path, as described above.

### Supported providers

| Provider   | Env var              | Key required |
|------------|----------------------|:------------:|
| anthropic  | `ANTHROPIC_API_KEY`  | ✓            |
| openai     | `OPENAI_API_KEY`     | ✓            |
| openrouter | `OPENROUTER_API_KEY` | ✓            |
| google     | `GOOGLE_API_KEY`     | ✓            |
| deepseek   | `DEEPSEEK_API_KEY`   | ✓            |
| moonshot   | `MOONSHOT_API_KEY`   | ✓            |
| xai        | `XAI_API_KEY`        | ✓            |
| xai-oauth  | `XAI_OAUTH_TOKEN`    | ✓            |
| github-copilot | `COPILOT_GITHUB_TOKEN` | ✓       |
| google-gemini-cli | —             | ✓ (login)    |
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
# → {"provider":"anthropic","status":"authenticated","method":"oauth","validated":true,"identity":{"email":"you@example.com"},"id":1}
```

API-key results use `"method":"api_key"`; device-code logins store an OAuth row and
report `"method":"oauth"`. Identity contains only fields actually
learned; `id` is the stored account the login wrote. Guidance and the
credential-file path go to stderr; stdout is reserved for the result line or JSON.

## `rustain auth logout <provider>`

```bash
rustain auth logout anthropic
rustain auth logout anthropic --json
rustain auth logout anthropic --id 3
```

Logout removes every stored row for that provider, including disabled tombstones,
and its active-account choice, without changing environment variables. If a
provider env variable is still set, the command says that variable will still be
used. Repeating logout succeeds and reports no stored credentials.
With `--id`, only that account is removed (`Logged out of Anthropic account
me@example.com (Org).`), and stderr names what now serves the provider
(`Anthropic now uses <account (#N)>.`, the environment variable, `No Anthropic
credentials remain.`, or `Anthropic has no usable credential.` when only disabled
accounts or accounts this endpoint may not receive remain). Other accounts,
providers, environment and configuration are untouched. `--json` prints
`{"provider","status":"logged_out","removed":1,"id","remaining","served"}`, or
`{"provider","status":"not_found","id"}` with a non-zero exit for an unknown id.
`xai` and `xai-oauth` are separate providers: `rustain auth logout xai-oauth`
removes only the device-login rows (`XAI_OAUTH_TOKEN` stays effective until
unset), and `rustain auth logout xai` removes only stored xAI API keys.

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
PROVIDER                                  AUTH METHODS    CONFIGURED  SIGNUP URL
Anthropic *                               oauth, api-key  ✓           https://console.anthropic.com/
ChatGPT Plus/Pro (Codex Subscription)     oauth, device               https://chatgpt.com/
OpenAI                                    api-key         ✓           https://platform.openai.com/api-keys
OpenRouter                                oauth, api-key              https://openrouter.ai/keys
Google AI                                 api-key                     https://aistudio.google.com/apikey
DeepSeek                                  api-key                     https://platform.deepseek.com/api_keys
Moonshot AI                               api-key                     https://platform.moonshot.cn/console/api-keys
xAI                                       api-key                     https://console.x.ai/team/default/api-keys
xAI Grok OAuth (SuperGrok or X Premium+)  device                      https://grok.com/
Ollama                                    none (local)    n/a         https://ollama.com/

* = active default provider
Run `rustain auth status` for credential sources, or `rustain auth login <provider>` to add one.
```

### Column legend

- **PROVIDER** — display name; `*` suffix marks the active default provider
  (the first enabled `[provider]` entry in config, else `anthropic`).
- **AUTH METHODS** — `oauth, api-key` for Anthropic, `oauth, device` for ChatGPT
  Codex, `device` for xAI Grok OAuth, `api-key` for other keyed providers, and
  `none (local)` for keyless providers. These are the `--method` values.
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
- `auth_methods` tokens are snake_case (`oauth`, `api_key`, `device_code`, `none`).
- No `source` field — use `auth status --json` for credential provenance.
