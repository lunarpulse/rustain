# Installing rustain on macOS

The macOS release assets (`aarch64-apple-darwin` for Apple silicon,
`x86_64-apple-darwin` for Intel Macs) are best-effort (P1) builds. They are not
signed with an Apple Developer ID and not notarized by Apple. Measured on the
v0.4.0 assets, the `aarch64-apple-darwin` one carries only the ad-hoc code
signature the linker adds and the `x86_64-apple-darwin` one carries no code
signature ([details](../RELEASING.md#macos-distribution-status)). macOS can
refuse to open a copy downloaded with a web browser, so download it with
`curl`, verify it, and install it into `~/.local/bin`.

## Download, verify and install

**1. Install the signature verifier.** If `minisign` is not already installed,
install it with [Homebrew](https://brew.sh):

```sh
brew install minisign
```

**2. Download, verify, and install.** `VERSION` is the newest release when this
page was last updated; for another release, change it (without a leading `v`).
A stale value installs an older, validly signed release; a value changed to a
release that is not published yet makes `curl` fail with 404 and installs
nothing. The release checklist updates it with each release.
`TARGET` is `aarch64-apple-darwin` for Apple silicon; on an Intel Mac, change it
to `x86_64-apple-darwin`. Paste the whole block into the same terminal, as shown:
do not add comments to its lines (an interactive zsh can run `#` as a command)
and do not append `&&` or `||` after the closing parenthesis: that can disable
`set -e`.

```sh
VERSION=0.4.1
TARGET=aarch64-apple-darwin

(
  set -eu
  if ! command -v minisign >/dev/null 2>&1; then
    printf '%s\n' 'minisign is required. Run: brew install minisign' >&2
    exit 1
  fi

  ASSET="rustain-${VERSION}-${TARGET}"
  URL="https://github.com/lunarpulse/rustain/releases/download/v${VERSION}"
  mkdir -p "$HOME/rustain-download"
  cd "$HOME/rustain-download"

  curl -fL -O "$URL/$ASSET"
  curl -fL -O "$URL/SHA256SUMS"
  curl -fL -O "$URL/SHA256SUMS.minisig"
  minisign -Vm SHA256SUMS \
    -P RWSUI8k3UrzEelNHSLUobmr5IbvKMx73rDg7gWVBy8vhID/OGxpiJKYF

  awk -v a="$ASSET" '
    length($1) == 64 &&
    $1 ~ /^[0-9a-f]+$/ &&
    substr($0, 65, 2) == "  " &&
    substr($0, 67) == a { n++; print }
    END { exit n != 1 }
  ' SHA256SUMS > "$ASSET.sha256"
  shasum -a 256 -c "$ASSET.sha256"

  chmod +x "$ASSET"
  mkdir -p "$HOME/.local/bin"
  mv "$ASSET" "$HOME/.local/bin/rustain"
  xattr -l "$HOME/.local/bin/rustain"
)
```

If you already downloaded the asset with a browser, place it in
`~/rustain-download` and omit only `curl -fL -O "$URL/$ASSET"` from the block.
Keep both manifest downloads and all verification steps.

The block stops at the first failed command. If it prints an error, do not run
the binary until you understand the failure. A failure before `mv` leaves the
new binary uninstalled; any existing `~/.local/bin/rustain` is unchanged.
The final `xattr` command can fail after installation.

`minisign` verifies the manifest with release key `PROD_KEY_1` from the
[Key Inventory](../RELEASING.md#key-inventory). The `awk` command requires
exactly one entry with the asset's complete filename; `shasum` must print
`rustain-<version>-<target>: OK`. These checks authenticate the published file;
they do not prove that it is free of malware.

**3. Check and run.** If `xattr -l` above does not list
`com.apple.quarantine`, check the installed executable:

```sh
"$HOME/.local/bin/rustain" --version
```

If quarantine is listed, follow [Downloaded with a browser?](#downloaded-with-a-browser)
before running it. Do not remove quarantine automatically.

If `~/.local/bin` is not on your `PATH`, enable it for the current terminal:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

For future terminals using macOS's default zsh, add that same `export` line to
`~/.zshrc` if needed. Then run:

```sh
rustain --help
```

You can delete `~/rustain-download` after successful installation.

## Downloaded with a browser?

A browser marks the file with the `com.apple.quarantine` attribute (`curl`
normally does not), and macOS checks a quarantined program the first time it
is opened. For software that is not notarized it may say that Apple cannot
check it for malicious software and offer **Move to Trash** or **Done**.
`xattr -l` in step 2 shows whether the installed file still has the attribute.
Before you remove it:

- Removing it skips that macOS check for this file. Do it only after the
  step 2 block succeeded.
- On a Mac managed by your employer, removing the attribute, or choosing
  **Open Anyway** in System Settings → Privacy & Security, may violate company policy,
  and security tools watch for it. Ask your IT team first.

```sh
xattr -d com.apple.quarantine ~/.local/bin/rustain
```

## Company-managed Macs

Endpoint-security or binary-authorization tools (an EDR agent, Santa) can block
or delete an unsigned program however it was downloaded. Ask IT to allow the
binary by its SHA-256 (your asset's line in the signed `SHA256SUMS`; every
release has a new hash), or build from source if your policy allows it.

## Build from source

The `rustain` executable is then built on your Mac instead of downloaded;
rustup and Cargo still download the toolchain and the dependencies. It needs
the Xcode Command Line Tools and rustup. Run it in a terminal where `VERSION`
is set as in step 2:

```sh
cargo install --locked --git https://github.com/lunarpulse/rustain --tag "v${VERSION}" rustain --bin rustain --features self-update,relay-server,a2a,p2p
```

`--features` adds to the default features, so this is the release feature set;
`--bin rustain` skips a test helper binary the package also defines. Releases
are built with the toolchain pinned in `rust-toolchain.toml` (1.98.1 at
v0.4.1); to use the same one, run `rustup toolchain install 1.98.1` and then
`cargo +1.98.1 install` with the same arguments. The build takes a while
(link-time optimization).

## If macOS still blocks it

The output of these read-only commands can help find out why. The output can
contain file paths and attribute values that may be sensitive; review it
before you paste it into an issue:

```sh
xattr -l ~/.local/bin/rustain
codesign -dv --verbose=2 ~/.local/bin/rustain 2>&1
spctl --assess --type execute -vv ~/.local/bin/rustain 2>&1
shasum -a 256 ~/.local/bin/rustain
```

## Sources and verification status

The macOS behaviour described here is from Apple's documentation, read 2026-11-14:
[Safely open apps on your Mac](https://support.apple.com/102445),
[Gatekeeper and runtime protection](https://support.apple.com/guide/security/sec5599b66df/web)
and [`LSFileQuarantineEnabled`](https://developer.apple.com/documentation/bundleresources/information-property-list/lsfilequarantineenabled);
for `curl` and the quarantine attribute, see
[MITRE ATT&CK T1553.001](https://attack.mitre.org/techniques/T1553/001/).
The project owner ran the step 2 block on an Apple-silicon Mac with release 0.4.1
on 2026-11-15: the signature and the checksum were verified and `--version`
printed 0.4.1; with `minisign` missing, the block printed the install command
and stopped before downloading. That block still had the `TARGET` explanation
as a comment, since moved into the text.
The Intel asset, the browser route, the source build and the diagnostics are not verified on macOS by the project.
