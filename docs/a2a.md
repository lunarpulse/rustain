# A2A AgentCard discovery

Rustain can discover allowlisted A2A peers and register their AgentCard skills in the internal capability inventory. Story 17.4a is discovery-only: A2A skills do not appear in the LLM-facing `@` dropdown, and invocation refuses until Story 17.4b adds task lifecycle translation.

A2A support is off by default. Build with:

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
```

If peers are configured but the binary was built without `a2a`, startup fails loudly instead of silently omitting them.

## Trust tiers

Trust comes only from configuration; an AgentCard cannot promote itself.

| Configuration | Registry trust | Behavior |
|---|---|---|
| No `pinnedKey` | `Unverified` | Fetch over HTTPS, validate required fields, register inventory stamped `Unverified` |
| Ed25519 `pinnedKey` | `Verified` | Require a valid EdDSA JWS over the raw card before caching or registration |
| Unsupported or unusable pin | none | Fail startup; never silently degrade to `Unverified` |

An unverified card can surface in the internal inventory because the operator explicitly allowlisted its origin. It still cannot enter the `@` dropdown or be invoked in 17.4a. A pinned peer with a missing, forged, tampered, wrong-key, wrong-algorithm, or wrong-`kid` signature never surfaces: removing `signatures` is treated as a downgrade attempt.

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
