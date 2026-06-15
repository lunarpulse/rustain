# Self-update trust fixtures (Story 13-3a)

Golden byte-vectors captured from the **real** `lunarpulse/rustain` `v0.1.0` release
(`https://github.com/lunarpulse/rustain/releases/tag/v0.1.0`). These anchor the
signature-verification tests to the exact wire format the release pipeline (13-3c)
produces — C minisign `0.12`, prehashed `ED` algorithm — so the hermetic suite cannot
pass on a signature shape production never emits.

| File | What it is |
|---|---|
| `SHA256SUMS` | GNU-coreutils checksum manifest, one `<sha256-hex>␣␣<asset>` line per binary |
| `SHA256SUMS.minisig` | detached minisign signature over `SHA256SUMS` (prehashed `ED`, 4-line: untrusted-comment + sig + trusted-comment + global-sig) |
| `minisign.pub` | the pinned PROD_KEY_1 public key (`RWSUI8k3…`) — same value as `self_update/trust.rs::TRUSTED_KEYS[0]` |

## Proven facts (de-risking spike, 2026-06-15)

`minisign-verify` 0.2 (pure-Rust, verify-only) verifies these bytes with
`PublicKey::from_base64("RWSUI8k3…").verify(&SHA256SUMS, &sig, /*allow_legacy=*/ false)` → `Ok`.
Signature algorithm bytes = `0x45 0x44` (`ED`, prehashed). The superseded stray key
`RWS76R3n…` does **not** verify (key-id mismatch). A 1-byte manifest tamper fails closed.

## What these fixtures DO and DO NOT cover

- **DO (PR suite, no network):** signature authenticity — real sig verifies against the
  pinned root; stray-key → `UntrustedKey`; 1-byte `.minisig`/manifest tamper → `BadSignature`;
  manifest parses and contains the expected per-triple hash strings; sig algo tag == `ED`.
- **DO NOT:** full artifact hash-binding (`N3` artifact-flip → `ChecksumMismatch`) and the
  `P` end-to-end binary binding need a **real 16 MB binary**, which is intentionally NOT
  committed. Those run (a) hermetically against a self-signed ephemeral-key manifest + tiny
  fake artifact in the PR suite, and (b) against the real downloaded asset in the **nightly
  Claim-A lane**. See Story 13-3a Testing Requirements P0 #4 / real-network split.

## REGEN RUNBOOK (do this whenever the signing key rotates)

These fixtures are pinned to `v0.1.0`'s key. When `PROD_KEY_1` rotates (rotation ladder,
13-3c AC6) the golden-`P` test will false-RED until regenerated:

```sh
REL=https://github.com/lunarpulse/rustain/releases/download/<new-tag>
curl -fsSL -o SHA256SUMS         $REL/SHA256SUMS
curl -fsSL -o SHA256SUMS.minisig $REL/SHA256SUMS.minisig
# update minisign.pub line 2 to the new TRUSTED_KEYS[0] base64, then confirm:
minisign -Vm SHA256SUMS -P "$(sed -n 2p minisign.pub)"   # must print "Signature and comment signature verified"
```

Commit the new bytes; the `.gitattributes` `-text` rule keeps line endings byte-stable.
