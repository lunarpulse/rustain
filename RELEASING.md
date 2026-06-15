# Releasing Rustain

This document describes the release process for `rustain`, including the frozen artifact contract, signing infrastructure, key management, and both automated (CI) and manual release procedures.

## Frozen Artifact Contract (Co-Owned with 13-3a)

Every release publishes the following assets, with names and formats that are **FROZEN** — Story 13-3a's `verify_release` computes them byte-for-byte:

- **Binary asset**: `rustain-{version}-{target-triple}[.exe]`  
  Example: `rustain-0.2.0-x86_64-unknown-linux-gnu`
- **Manifest**: `SHA256SUMS` — one line per asset in GNU coreutils format:  
  `<sha256-hex>  <asset-filename>`
- **Signature**: `SHA256SUMS.minisig` — detached minisign signature of the manifest (prehashed `ED` algorithm)

The manifest is signed, not each binary individually. One signed manifest binds every asset's hash.

## Platform Support Matrix (NFR33)

| Tier | Platform | Target Triple | Status |
|------|----------|---------------|--------|
| P0 (required) | Linux x86_64 | `x86_64-unknown-linux-gnu` | Required — release fails if this target fails |
| P1 (best-effort) | macOS aarch64 | `aarch64-apple-darwin` | Allowed to fail without blocking release |
| P1 (best-effort) | macOS x86_64 | `x86_64-apple-darwin` | Allowed to fail without blocking release |
| P2 (future) | Windows x86_64 | `x86_64-pc-windows-msvc` | Allowed to fail without blocking release |

P1/P2 targets may be added incrementally, but the asset-naming convention must accommodate all of them from day one so that 13-3a's target-triple selection never needs a contract change.

**Note on Windows (P2):** The rustain codebase currently uses Unix-specific APIs (`tokio::signal::unix`, `std::os::unix::fs`, `tokio::net::UnixListener`/`UnixStream`, `libc` for PID files and daemon lifecycle) without `#[cfg(unix)]` gates in all paths. Windows builds will fail at compile time. The release workflow is configured with `fail-fast: false` and the sign job uses `if: always() \u0026\u0026 !cancelled()` so that Windows failures do not block signing and publishing of Linux/macOS artifacts. Full Windows support requires a dedicated Epic (not currently scheduled).

The release pipeline asserts each published binary is **< 30 MB** (PRD aspirational: < 20 MB). The size check lives in CI, not runtime. The existing release profile (`Cargo.toml` `[profile.release]`) already uses `lto = true`, `strip = true`, `codegen-units = 1`, `panic = "abort"` to minimize size.

## Signing Scheme: minisign

We use [minisign](https://jedisct1.github.io/minisign/) (Ed25519, prehashed `ED` algorithm) for release signing. The runtime verification in 13-3a uses `minisign-verify` (pure-Rust, zero-dependencies, no network at verify time).

**Why minisign over cosign/Sigstore/GPG:**
- Pure-Rust verify-only client (`minisign-verify`) — tiny, no network at verify time
- Ed25519, trivial in GitHub Actions
- Fits NFR9 (small binary) and NFR53 (minimal dependencies)
- Trade-off accepted: long-lived secret key managed via protected-environment custody + rotation ladder, rather than PKI complexity

### Trust Model

The **public key is pinned in the binary** at `self_update/trust.rs` (13-3a `TRUSTED_KEYS`). The runtime trusts only the embedded `const` — a fetched key is theater. Publishing the public key as a release asset for human convenience is fine, but the runtime does not use it.

## Key Generation (Human-Gated — Do This Once)

**Generate the keypair offline on a trusted machine using C minisign (NOT rsign2).** CI uses C minisign 0.12 (`apt-get install minisign`); rsign2-generated passwordless keys are read-incompatible with C minisign's passwordless marker and trigger `Password: get_password()` failures in CI.

```bash
# Install C minisign (Arch/Manjaro: pacman -S minisign; Debian/Ubuntu: apt install minisign; macOS: brew install minisign)
minisign -G -W -p ~/.minisign/minisign.pub -s ~/.minisign/minisign.key
```

`-W` = passwordless (see rationale below). This produces:
- `~/.minisign/minisign.key` — **SECRET KEY** (never commit, never echo)
- `~/.minisign/minisign.pub` — **PUBLIC KEY** (embed in 13-3a `trust.rs`)

**Verify the key is CI-compatible before storing it as a secret:**
```bash
echo test > /tmp/t.txt
minisign -S -H -W -m /tmp/t.txt -s ~/.minisign/minisign.key -x /tmp/t.sig -t test
minisign -V -m /tmp/t.txt -x /tmp/t.sig -p ~/.minisign/minisign.pub
# Both commands must succeed with NO password prompt
```

### Why passwordless (`-W`)?

Both minisign (C) and rsign2 read the key password from `/dev/tty` via `getpass()`. There is **no env var, no stdin pipe** that works in GitHub Actions — the TTY is absent. A password on the key would block CI signing. The key's protection instead comes from:

1. GitHub encrypted secret (encrypted at rest)
2. Protected `release` environment with required reviewers (human gate on every use)
3. Branch protection on `main` (no unauthorized workflow changes)

This is defense-in-depth equivalent to a password on a key that lives on a single-purpose CI runner.

## Secret Key Custody (Human-Gated — Do This Once)

Store the secret key as a GitHub Actions encrypted secret in a **protected `release` environment with required reviewers**:

1. Go to Settings → Environments → New environment → Name: `release`
2. Enable **Required reviewers** — add at least one maintainer
3. Add secrets:
   - `MINISIGN_SECRET_KEY` — the full content of the secret key file (both lines: comment + base64 key)
   - `MINISIGN_PUBLIC_KEY` — the full content of the public key file (used for fail-closed in-workflow verification with `-P`)
4. The release workflow (`release.yml`) runs only from this protected environment — a single leaked token cannot mint a release

> **Note:** The runner needs `minisign` installed. The workflow includes an `Install minisign` step (`apt-get install -y minisign`) since it is not pre-installed on `ubuntu-latest`.

## Key Rotation Ladder

When rotating the signing key:

1. **Generate new keypair** (offline, trusted machine)
2. **Add new pubkey to 13-3a's `TRUSTED_KEYS`** — ship a release with the updated binary
3. **Dual-sign during overlap window** — sign `SHA256SUMS` with BOTH old and new keys (two `.minisig` files, or concatenate signatures in one file per minisign multi-sig convention). In-field binaries with the old key still trust the release; binaries with the new key also trust it.
4. **Drop old key after deprecation window** — remove from `TRUSTED_KEYS`, stop signing with old key

## No-Revocation Limitation

**minisign has NO online revocation mechanism.** If the secret key leaks:

- Already-installed binaries trust an attacker-signed payload until reinstalled out-of-band
- The mitigation is **prevention**, not revocation: protected environment + required reviewers means a leaked key alone cannot publish a release
- Incident response: rotate the CI secret, audit Actions release runs, publish a security advisory, burn the key forward via rotation ladder

## Automated Release Procedure (CI)

1. Ensure all changes are on `main` and tests pass
2. Update `Cargo.toml` version if needed
3. Push a tag: `git tag v0.2.0 && git push origin v0.2.0`
4. The `release.yml` workflow triggers automatically:
   - Builds per-platform binaries (matrix: Linux P0, macOS P1, Windows P2)
   - Size-guard asserts < 30 MB
   - Generates `SHA256SUMS`
   - Signs manifest in the protected `release` environment
   - Verifies signature fail-closed before publishing
   - Creates GitHub release with all assets

## Manual Release Procedure (CI-Down Fallback)

If CI is unavailable, a maintainer can cut a release by hand:

```bash
# 1. Ensure clean tree on main
git status

# 2. Set version
VERSION="0.2.0"
TARGET="x86_64-unknown-linux-gnu"

# 3. Build release binary
cargo build --release --target "$TARGET"

# 4. Stage asset
ASSET="rustain-${VERSION}-${TARGET}"
cp "target/${TARGET}/release/rustain" "$ASSET"

# 5. Size check
SIZE=$(stat -c%s "$ASSET")
if [ "$SIZE" -gt "$((30 * 1024 * 1024))" ]; then
    echo "ERROR: Binary exceeds 30 MB limit"
    exit 1
fi

# 6. Generate manifest
sha256sum "$ASSET" > SHA256SUMS

# 7. Sign manifest (prehashed ED, passwordless)
minisign -S -H -W -m SHA256SUMS -s /path/to/minisign.key -x SHA256SUMS.minisig -t "rustain v${VERSION}"

# 8. Verify signature fail-closed (-P pubkey as pinned base64 string)
PUBKEY=$(grep -v '^untrusted' /path/to/minisign.pub)
minisign -V -m SHA256SUMS -x SHA256SUMS.minisig -P "$PUBKEY"

# 9. Create GitHub release and upload: rustain-*, SHA256SUMS, SHA256SUMS.minisig
#    (Use GitHub web UI or gh CLI: gh release create "v${VERSION}" ...)
```

For macOS/Windows targets, repeat steps 3-8 on the appropriate host or cross-compile.

## First Release Checklist

- [ ] Generate minisign keypair offline (trusted machine)
- [ ] Store secret key in GitHub `release` environment (protected + required reviewers)
- [ ] Add `MINISIGN_PUBLIC_KEY` secret to `release` environment
- [ ] Hand the public key to 13-3a for embedding in `trust.rs`
- [ ] Verify branch protection on `main` (no force-push, required status checks)
- [ ] Push tag `v0.1.0` (or first version) to trigger release.yml
- [ ] Verify assets published: binary, SHA256SUMS, SHA256SUMS.minisig
- [ ] Verify signature validates: `minisign -V -m SHA256SUMS -x SHA256SUMS.minisig -p minisign.pub`
- [ ] Verify 13-3a can update from this release (end-to-end test)
