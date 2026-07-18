# A2A AgentCard fixtures

Pinned on 2026-07-17 for Story 17.4a. Tests are offline-only: third-party hosts may change or disappear, so no test fetches these URLs.

## Provenance

- `FIXTURE_moltrust_v1.0_agent-card.json`: `https://registry.moltrust.com/.well-known/agent-card.json`
- `FIXTURE_moltrust_v1.0_jwks.json`: `https://registry.moltrust.com/.well-known/jwks.json`
- `FIXTURE_planets_v0.3_agent-card.json`: `https://planets-agent.ai/.well-known/agent.json`
- `FIXTURE_planets_v0.3_jwks.json`: key material captured from the card's published JWKS endpoint during the same field capture
- `CORPUS_141_live_cards_2026-07-17.json`: 141 parseable cards captured from the live `a2aregistry.org` population

Every artifact is checked against `manifest.json` before use. `REVERIFY_REAL_SIGNATURES.sh` reruns strict RFC 8785/EdDSA verification against both captured shapes.

## Controlled-mutant key

`TEST_ONLY_ed25519_seed.hex` is a deterministic, public test seed (`0x07` repeated 32 times). It is not a secret and MUST NEVER be used outside tests. `tests/a2a_jws.rs` generates the deterministic card/signature and all forged, tampered, stripped, wrong-key, wrong-algorithm, and rotation mutants from that seed. Run `REPRODUCE_TEST_SIGNATURE.sh` to reproduce those verdicts.
