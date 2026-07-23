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
