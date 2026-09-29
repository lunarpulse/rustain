# A2A AgentCard discovery

Rustain discovers allowlisted A2A peers and registers their AgentCard skills in the internal capability inventory (Story 17.4a), and serves its own signed card plus a task lifecycle to allowlisted callers (`--serve-a2a`, and the daemon's inbound path). A2A skills still do not appear in the LLM-facing `@` dropdown; § Task lifecycle below is the authority on what a caller can invoke.

A2A is off by default for `cargo install`. **Published release assets up to and including `v0.1.3` do not contain it** — it is on in every release built after Story 19.5; source builds need `--features a2a`:

```bash
cargo build --features a2a
```

## Workspace configuration

Create `.rustain/a2a.json` in the workspace:

```json
{
  "agents": {
    "security-peer": {
      "url": "https://agent.example"
    },
    "verified-ci": {
      "url": "https://ci.example",
      "pinnedKey": {
        "alg": "EdDSA",
        "x": "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w",
        "kid": "ci-key-2026"
      }
    },
    "remote-reviewer": {
      "url": "https://reviewer.example",
      "auth": "RUSTAIN_REVIEWER_API_KEY",
      "caCert": "certs/reviewer-ca.pem"
    }
  }
}
```

Workspace entries override profile entries with the same peer ID. Peer IDs must be non-empty and cannot contain `__`, which is reserved by the `a2a::<peer>::<skill>` capability name.

Profile configuration uses the active profile's tools config:

```toml
[tools.config.a2a.security-peer]
url = "https://agent.example"

[tools.config.a2a.verified-ci]
url = "https://ci.example"

[tools.config.a2a.verified-ci.pinned_key]
alg = "EdDSA"
x = "Pii06SUCwAi0D_BTTOeCsD5XSSrjqFqw0nXF8STr14w"
kid = "ci-key-2026"

[tools.config.a2a.remote-reviewer]
url = "https://reviewer.example"
auth = "RUSTAIN_REVIEWER_API_KEY"
ca_cert = "certs/reviewer-ca.pem"
```

If peers are configured but the binary was built without `a2a`, startup fails loudly instead of silently omitting them.

## Reaching a credentialed peer across a network boundary

Two optional roster fields let a rustain process — not `curl` — reach a peer that
demands TLS and an API key. They are independent: either may be used alone.

| Key | Value | Meaning |
|---|---|---|
| `auth` | env var **name** | The variable holding this peer's API key. The key itself never lives in the file, and the roster is safe to commit |
| `caCert` / `ca_cert` | path to a PEM file | The trust anchor this peer's server certificate must chain to |

### `auth` — the client credential

The value is a **variable name**, exactly like the server block's `apiKeyEnv`. The
variable is read **on every outbound JSON-RPC call**, so exporting it after
rustain starts works, and rotating it takes effect on the next send without a
restart.

The key is sent as `x-api-key`, on `POST` only. The AgentCard fetch is
**unauthenticated** — the served card is the document that explains how to get
past the gate, so gating it would gate the instructions.

**The credential is sent only to the origin your roster `url` names** — scheme,
host and port. An AgentCard decides where the JSON-RPC request goes, and a card
is written by the peer; without this bound, a card naming another host would
collect your key. If a peer's card advertises a different origin, the send is
refused and nothing is sent. So a credentialed peer's server must advertise an
authority equal to your roster `url` (the card's endpoint is built from the
server's `advertisedHost`).

### `caCert` — the trust anchor

A relative path is resolved against the directory that `.rustain/a2a.json` was
found in — **not** against `.rustain/` itself and not against a profile's
directory, so `"certs/ca.pem"` means `<workspace>/certs/ca.pem` for workspace and
profile peers alike. An absolute path is used as given.

Parsing never touches the file — a roster loads whether or not the anchor
exists. The client then reads it **once, at startup**, when the peer is
composed: a missing or unparseable anchor is kept as that peer's refusal, and
every send to it fails with the `<alias>'s pinned anchor could not be loaded: …`
form. Repairing or replacing the file therefore takes effect **only after a
restart**, like any other roster change.

`caCert` requires an `https` roster `url`. A plain-HTTP (loopback) peer never
performs a TLS handshake, so an anchor there could never be consulted — the
roster refuses to load that combination and says so, rather than silently
ignoring your trust setting.

An anchored peer's client trusts **only** that anchor. The platform root store is
switched off for it, so a public CA mis-issuance cannot impersonate that peer. A
peer without `caCert` is unchanged and keeps the system trust store.

This is **anchor validation**, not certificate fingerprint pinning: rustain
installs the file's certificates as roots and validates the presented chain
against them. Two certificate shapes therefore work, and one that looks like it
should does not.

**Form 1 — a private CA that issues the server's certificate.** Commands as run
on OpenSSL 3.5.5:

```bash
# The anchor you put in caCert
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout ca.key -out ca.pem -days 3650 -subj /CN=rustain-peer-ca \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

# The certificate the peer's server presents
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout server.key -out server.csr -subj /CN=peer.example
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:peer.example\n' > ext.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -out server.pem \
  -days 825 -extfile ext.cnf
openssl verify -CAfile ca.pem server.pem      # -> server.pem: OK
```

**Form 2 — a non-CA self-signed certificate, pinned as its own anchor.** The
`-addext` line is the load-bearing part:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -keyout leaf.key -out leaf.pem -days 365 -subj /CN=localhost \
  -addext "basicConstraints=critical,CA:FALSE" \
  -addext "subjectAltName=DNS:localhost" \
  -addext "extendedKeyUsage=serverAuth"
```

⚠ **`openssl req -x509` without that line emits `CA:TRUE`** (verified on OpenSSL
3.5.5), and a `CA:TRUE` certificate presented as a server's own certificate is
**refused** — rustain reports that the peer presents a CA certificate as its
server certificate. Add `basicConstraints=critical,CA:FALSE`, or use form 1.

⚠ **An expired anchor is still trusted.** A trust anchor's own validity period is
not checked (RFC 5280 §6.1.1 leaves it optional and the verifier here omits it),
so an expired CA keeps validating leaves it issued. The server's *own*
certificate is checked normally. Rotate anchors on a calendar, not on an error
message.

⚠ **A certificate fixed on the server needs a rustain restart.** The anchor
decision is made during the one AgentCard fetch at startup and retained, so a
peer whose certificate was wrong when rustain started stays refused until the
next start even after the server is repaired.

### When it refuses

Each failure names one trust decision, because each has a different fix:

| Rendered | What to change |
|---|---|
| `no credential for <alias>: set the env var named in its auth field` | export the variable your `auth` field names |
| `no credential configured for <alias>: add an auth field naming the env var that holds its key` | the peer wants a key and your roster entry has no `auth` field |
| `<alias> rejected this credential` | either side — a wrong key here, or a revoked grant there |
| `<alias>'s card sends requests to another host; its credential is only sent to <origin>` | the peer's `advertisedHost`, or your roster `url` |
| `<alias>'s certificate does not match the pinned anchor` | the wrong file in `caCert`, or a certificate not issued under it |
| `<alias>'s certificate has expired or is not yet valid` | the peer's certificate, or a clock on either host |
| `<alias>'s certificate is not valid for its roster address` | the certificate's SAN, or the host in your roster `url` |
| `<alias> presents a CA certificate as its server certificate` | the peer's certificate shape — see form 2 above |
| `<alias>'s pinned anchor could not be loaded: <reason>` | the `caCert` path or its contents; the reason names both |

There is still **no** mutual TLS: rustain presents a bearer secret, never a client
certificate (`DF-18-1-MTLS`).

## Sending to configured peers from the TUI

Peer entries are loaded once when rustain starts. After adding, removing, or changing an entry in `.rustain/a2a.json` or the active profile, restart rustain; `/team send` does not re-read configuration or discover a second source of truth.

Address one or more roster IDs as a comma list, followed by the verbatim message body:

```text
/team send security-peer,remote-reviewer review the authentication changes
```

Every listed ID must be a configured, distinct, non-empty roster ID. An empty segment, duplicate, unknown ID, or missing body refuses the whole action before network I/O and writes no journal row. A roster ID containing `,` is therefore not addressable by this verb.

The action creates one stable, keyless `Info` block with one row per alias in the order typed. It has no header or aggregate result. Each recipient settles independently at its **first** `message/send` answer: `● delivered` means that answer carried a non-empty recipient item ID; `✗ declined` is a send-time rejection; `⚠ unreachable` names a send that did not land, with a named preflight cause on its indented line when available. Tokenless rows name `sending…`, `awaiting their approval` (`auth-required` without an item), `accepted — this peer keeps no item id`, `no usable answer — '/team board' shows whether it arrived`, `asked a question this verb cannot answer … — multi-turn arrives with 19.18`, or `not sent — this host could not record the attempt`. The recipient's `server.admission` defaults to `deny`, so an unconfigured recipient declines a team message.

There is exactly one `message/send` attempt per recipient and no automatic retry or queue. To send again, re-type `/team send` with the recipients you choose; the block has no retry control. A recipient with an item ID is delivered even when its first answer is `auth-required`: this rail does not poll or cancel that task. An item-less `input-required` triggers a single-turn `tasks/cancel`; if that cleanup fails or its answer is not `canceled`, the row says `no usable answer` rather than claiming cancellation. A peer that answers with an A2A Message instead of a task, and no item ID, is `accepted — this peer keeps no item id`.

Each recipient is journaled independently. A request that may leave the host records `dispatched` before its POST; a delivered or accepted answer adds `accepted`; a proved rejection, a connection that never opened, or a post-dispatch credential/trust-anchor refusal adds `refused`; and an approval-needed or no-usable-answer row (including a timeout, HTTP or JSON-RPC error, or unreadable answer after dispatch) remains dispatch-only. A pre-I/O refusal writes one outbound `refused` row and no `dispatched` row. If durable dispatch recording fails, nothing is sent and no row is written. An accepted answer contributes a content hash, not stored reply text; a decline stores its sanitized reason. `/team send` never renders a peer reply and never materializes an `a2a-peer` node in the Agents panel. FR169 replies are the later answer channel.

`/team send` exists only in the interactive, non-attached TUI. On the attached rail it refuses aloud: `'/team send' needs this session's own A2A egress — run it in a non-attached session.` There is no headless `rustain team send` command. A roster entry supplies client credentials for a non-loopback peer through its `auth` and `caCert` fields — see [Reaching a credentialed peer across a network boundary](#reaching-a-credentialed-peer-across-a-network-boundary).

## The `log: N` reminder

When the transparency log holds rows newer than your last visit, the right edge of the status bar shows a muted `log: N` (`log: 99+` above 99). Your client reads its own workspace journal about once a second; nothing is pushed to it, and no peer, item, task or event name ever appears in the segment. It counts every transparency row of every kind and direction — a teammate's retract of an item you received is one such row — so the count is a reason to look, not a description of what happened.

- **How to clear it.** Open the log unfiltered: `Ctrl+X, L` or `/team log` in the standalone TUI; `/team log` in a daemon-attached client (there `Ctrl+X` retracts this host's auto-sent message, and a read-only attach may still read the log). The reminder clears only once the view is actually drawn on screen. A filtered command (`--filter=…`), a panel with an active search, a failed read, a result scrolled off screen, a panel export, an unrelated key, a restart, or the offline `rustain team log` command never clears it. `--json` and `--export` clear it like the plain view, because the rows were shown — whether or not the export file was written.
- **What clearing means.** It records that this client *presented* that snapshot. It is a local visit reminder: ⛔ not `/team ack`, not `/team remove`, not `/team retract`, not an acknowledgement or a receipt to anyone, and not proof that you read a row. The in-chat `/team log` shows at most 20 rows and says so; presenting it clears the reminder for the whole snapshot it read, while the older rows remain in `rustain team log` and the panel. Clearing does not mean those older rows were read.
- **What it counts.** Rows newer than the durable boundary your last presented visit recorded; rows that arrive after a view was read stay counted until the next visit. On first use every existing row counts. The boundary lives in `.rustain/transparency-seen.json`, is shared by the standalone and attached clients of this workspace, survives restarts, and is never written by polling.
- **`log: ?`** means the reminder cannot currently be trusted — the journal or the preference could not be read, a visit could not be saved, or the journal's head fell below the saved boundary (a reset, which the client confirms and then counts from zero). It is never shown as an apparently current zero; open the log for details.

## Trust tiers

Trust comes only from configuration; an AgentCard cannot promote itself.

| Configuration | Registry trust | Behavior |
|---|---|---|
| No `pinnedKey` | `Unverified` | Fetch over HTTPS, validate required fields, register inventory stamped `Unverified` |
| Ed25519 `pinnedKey` | `Verified` | Require a valid EdDSA JWS over the raw card before caching or registration |
| Unsupported or unusable pin | none | Fail startup; never silently degrade to `Unverified` |

An unverified card can surface in the internal inventory because the operator explicitly allowlisted its origin. It still cannot enter the `@` dropdown. A pinned peer with a missing, forged, tampered, wrong-key, wrong-algorithm, or wrong-`kid` signature never surfaces: removing `signatures` is treated as a downgrade attempt.

Revocation is removal of the peer from `.rustain/a2a.json` or the active profile. The A2A specification supplies no implementable key-expiry or revocation mechanism for this flow.

## Pinning an Ed25519 peer

1. Obtain the peer's public key through an operator-trusted channel. Do not trust a `jku` URL supplied by the card itself; rustain never fetches it.
2. From the peer's JWK, require `kty: "OKP"`, `crv: "Ed25519"`, and `alg: "EdDSA"`.
3. Copy the JWK's base64url `x` value into `pinnedKey.x`.
4. If the JWK has a `kid`, copy it into `pinnedKey.kid`. A configured `kid` must match the protected JWS header.
5. Restart rustain. An invalid base64 value, wrong key length, or unsupported algorithm is a boot error.

Do not paste a full JWK and do not configure `jku`; the allowlist and pin are the trusted inputs.

## Fetch and validation policy

Rustain requests exactly `<peer-base>/.well-known/agent-card.json` with a 30-second timeout, a five-redirect cap, per-hop URL validation, JSON content-type enforcement, and a 1 MiB body cap. HTTPS is required except for loopback HTTP used by local/manual tests.

The decoder accepts unknown vendor fields and both measured v0.3/v1.0 card shapes. It explicitly requires card `name`, a present `skills` array, and each skill's `id` and `name`. Signature verification canonicalizes the raw JSON value using RFC 8785 JCS; it never verifies a typed-struct round trip.

## Offline verification

Pinned fixtures and provenance live under `tests/fixtures/a2a/`. Re-run real captured signatures and controlled mutants without network access:

```bash
./tests/fixtures/a2a/REVERIFY_REAL_SIGNATURES.sh
./tests/fixtures/a2a/REPRODUCE_TEST_SIGNATURE.sh
```

The deterministic Ed25519 seed in that directory is marked TEST-only and must never be used in production.

## Serving A2A (`--serve-a2a`)

Rustain can also *be* an A2A agent: `--serve-a2a=ADDR` exposes a signed AgentCard
and a JSON-RPC endpoint. Two shapes, and the difference is not cosmetic:

```bash
# Discovery only. No execution core, so every inbound task is refused with a
# policy verdict that says so.
rustain --serve-a2a=127.0.0.1:8080

# Full: the listener runs inside the daemon lifecycle, sharing its node tree,
# core and event bus. Inbound tasks execute as local peer nodes.
rustain --serve-a2a=127.0.0.1:8080 daemon start
```

`--serve-a2a` may be combined only with the daemon actions that start its
lifecycle (`daemon start`, including its internal `__run` child), and with
nothing else. It is refused for `stop`, `status`, `attach`, `install`, and
`uninstall` rather than silently discarding the listener request.

For `daemon start`, the PID readiness marker is written only after the A2A
listener has passed configuration, any required TLS, and bind startup. If the
listener cannot start, daemon startup fails instead of reporting a ready daemon
with no listener.
After that handshake, an unexpected listener exit is recorded as a daemon error.

Standalone discovery-only refusals are also appended through the workspace's
canonical room journal. They do not use an inert or separate transparency sink.

### Server configuration

The `server` block of `.rustain/a2a.json`:

```json
{
  "server": {
    "admission": "ask",
    "apiKeyEnv": "RUSTAIN_A2A_API_KEY",
    "apiKeys": ["RUSTAIN_A2A_API_KEY_NEXT"],
    "advertisedHost": "a2a.example.com:8443",
    "tls": { "cert": "certs/server.pem", "key": "certs/server.key" }
}
```

| Key | Values | Meaning |
|---|---|---|
| `admission` | `deny` (default), `ask`, `allow` | What to do with a task from a remote agent |
| `apiKeyEnv` / `api_key_env` | env var **name** | Legacy primary API-key variable. It remains honored. The key itself never lives in the file |
| `apiKeys` / `api_keys` | array of env var **names** | Additional API keys. The effective set is the union with `apiKeyEnv`; each configured key is a distinct submitting principal |
| `advertisedHost` / `advertised_host` | public `host[:port]` authority | Authority published in the AgentCard. Required for `0.0.0.0` or `::` binds |
| `tls.cert` / `tls.key` | workspace-relative PEM paths | Certificate chain and private key |

`admission` defaults to `deny`. An endpoint that starts executing strangers'
instructions the moment it is reachable is a footgun, so enabling execution is
an explicit act. `ask` prompts the operator; `allow` does not.

Note that `admission` is deliberately **not** `[subagents] auto_approve`. That
knob governs subagents you launched; inheriting it here would silently hand a
network peer the auto-approval you granted to your own local work.

### Loopback vs the network

Loopback (`127.0.0.0/8`, `::1`, `localhost`) serves plaintext and
unauthenticated: an attacker who can reach it already runs code on your machine.

**Any other address requires TLS *and* at least one API key *and* a signed
identity — together.** Configure only some of them and rustain refuses to bind,
naming what is missing. There is no flag to serve a non-loopback address in the
clear.

Wildcard binds (`0.0.0.0` or `::`) additionally require `advertisedHost`. Set it
to the reachable authority clients should use, such as
`a2a.example.com:8443`; it is published in the AgentCard instead of the
unroutable wildcard. Use a hostname covered by the TLS certificate.

Present one configured key as `x-api-key: <key>`. An `Authorization: Bearer …`
token is treated as *no credential* rather than a wrong one: OAuth2 is not an
accepted scheme yet (`DF-18-1-OAUTH2`), and telling a client its key is invalid
when it never sent one is a lie. The served AgentCard publishes
`securitySchemes` and `security` describing exactly what the server enforces, and
the card itself stays reachable unauthenticated — gating it would gate the
document that explains how to get past the gate.

### Task lifecycle

| Method | Behaviour |
|---|---|
| `message/send` | Admits the task and answers immediately. Never blocks on a human |
| `tasks/get` | Current state, scoped to the submitting credential |
| `tasks/cancel` | Cancels the running turn, scoped to the submitting credential |

Under `admission: "ask"`, `message/send` answers `auth-required` and the client
polls `tasks/get`; the operator's decision arrives out of band. The request is
never held open across a keypress — it would hit the 30-second request deadline
and each retry would queue another prompt.

`tasks/get` and `tasks/cancel` for a task belonging to another credential return
exactly what a task that does not exist returns. That is deliberate: telling
"not yours" apart from "not found" would let a peer enumerate task ids and map
the host's federation for free.

If the host restarts mid-task, the task resolves to `failed` with a restart
reason — never a zombie `working`. Durable resumption is not implemented
(`DF-18-1-HOSTRECONCILE`).

### JSON-RPC profile (narrow, and stated)

- A **notification** (a request object with no `id` member) receives `204 No
  Content`.
- An explicit `"id": null` is rejected with `-32600`.
- **Batch arrays are not supported**; send one request object per HTTP POST.

### What a remote peer can see

Served payloads carry capability-scoped projections only: no filesystem path, no
workspace root, no system prompt, no internal node/room/journal state. This is
enforced by the *type* of the projection, not by a redaction pass, so it holds
even against a modified requesting client.
