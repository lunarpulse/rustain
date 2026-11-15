#!/bin/sh
# rel-4 install harness: runs dist/install.sh and the step 2 block of
# docs/install-macos.md against stub curl/minisign/uname/id/sysctl/xattr and
# hashing wrappers (tests/install_script/stubs), with no network.
#
#   sh tests/install_script/harness.sh            # from the repository root
#   INSTALL_SH='bash --posix' sh tests/install_script/harness.sh
#
# INSTALL_SH is the shell that runs the installer and the page block (default
# sh). Prints one PASS/FAIL line per check and exits non-zero on any failure.

set -eu

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
STUBS=$ROOT/tests/install_script/stubs
SCRIPT=$ROOT/dist/install.sh
PAGE=$ROOT/docs/install-macos.md

# shellcheck disable=SC2086 # INSTALL_SH may carry options, e.g. 'bash --posix'
set -- ${INSTALL_SH:-sh}
SH_BIN=$(command -v "$1") || {
  printf 'harness: shell %s not found\n' "$1" >&2
  exit 2
}
shift
SH_ARGS=$*

REAL_PATH=$PATH
if command -v sha256sum >/dev/null 2>&1; then
  hash_of() { sha256sum "$1" | awk '{ print $1 }'; }
else
  hash_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }
fi

# Values under test come from the repository files, never from literals here.
VERSION_UNDER_TEST=$(awk -F= '/^DEFAULT_VERSION=/ { print $2; exit }' "$SCRIPT")
TRUSTED_KEY=$(awk -F'"' '/^const PROD_KEY_1: &str = "/ { print $2; exit }' "$ROOT/src/adapters/self_update/trust.rs")
PIN_VERSION=0.0.1
[ -n "$VERSION_UNDER_TEST" ] || { printf 'harness: no DEFAULT_VERSION in %s\n' "$SCRIPT" >&2; exit 2; }
[ -n "$TRUSTED_KEY" ] || { printf 'harness: no PROD_KEY_1 in trust.rs\n' >&2; exit 2; }

T=$(mktemp -d "${TMPDIR:-/tmp}/rel4-harness.XXXXXX")
trap 'rm -rf "$T"' EXIT
# Physical path: macOS TMPDIR is under the /var -> /private/var symlink, and the
# curl stub records the physical working directory.
T=$(cd "$T" && pwd -P)

# The installer's PATH: its declared tools, real, plus the stubs a case selects.
mkdir "$T/sys"
for tool in awk chmod mkdir mktemp mv rm; do
  ln -s "$(command -v "$tool")" "$T/sys/$tool"
done

passed=0
failed=0
check() { # check CASE DESCRIPTION COMMAND...
  c=$1
  d=$2
  shift 2
  if "$@"; then
    passed=$((passed + 1))
    printf 'PASS %s %s\n' "$c" "$d"
  else
    failed=$((failed + 1))
    printf 'FAIL %s %s\n' "$c" "$d"
    for f in out err; do
      if [ -s "$CASE/$f" ]; then
        printf '  %s: ' "$f"
        tr '\n' '|' <"$CASE/$f"
        printf '\n'
      fi
    done
  fi
}

# fixture_asset FILE VERSION TARGET: a stand-in binary that prints its version.
fixture_asset() {
  printf '#!/bin/sh\nprintf '"'"'rustain %s (%s)\\n'"'"'\n' "$2" "$3" >"$1"
  chmod 755 "$1"
}

# mirror_release DIR VERSION: release vVERSION with all three targets, a
# SHA256SUMS in the release format and a stub signature over it.
mirror_release() {
  d=$1/v$2
  mkdir -p "$d"
  for t in x86_64-unknown-linux-gnu aarch64-apple-darwin x86_64-apple-darwin; do
    fixture_asset "$d/rustain-$2-$t" "$2" "$t"
  done
  printf 'echo not an asset\n' >"$d/install.sh"
  (cd "$d" && for f in rustain-* install.sh; do printf '%s  %s\n' "$(hash_of "$f")" "$f"; done >SHA256SUMS)
  sign "$d"
}

sign() {
  printf 'stub-signature %s\n' "$(hash_of "$1/SHA256SUMS")" >"$1/SHA256SUMS.minisig"
}

# new_case NAME [STUB...]: fresh HOME, mirror, log and PATH. Default stubs:
# curl minisign uname id sysctl xattr sha256sum sudo.
new_case() {
  CASE=$T/$1
  shift
  CASE_HOME=$CASE/home
  mkdir -p "$CASE_HOME" "$CASE/log" "$CASE/bin" "$CASE/mirror"
  : >"$CASE/log/curl.log"
  if [ "$#" = 0 ]; then
    set -- curl minisign uname id sysctl xattr sha256sum sudo
  fi
  for s in "$@"; do
    ln -s "$STUBS/$s" "$CASE/bin/$s"
  done
  mirror_release "$CASE/mirror" "$VERSION_UNDER_TEST"
  BIN=$CASE_HOME/.local/bin
  DEST=$BIN/rustain
}

seed_old() {
  mkdir -p "$BIN"
  printf '#!/bin/sh\necho rustain old\n' >"$DEST"
  chmod 755 "$DEST"
  cp "$DEST" "$CASE/old"
}

# run_installer [VAR=VALUE...] [-- ARG...]: runs dist/install.sh in a clean
# environment; sets RC.
run_installer() {
  run_with "$SCRIPT" "$@"
}

run_with() {
  file=$1
  shift
  vars=
  while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
    vars="$vars $1"
    shift
  done
  if [ "$#" -gt 0 ]; then shift; fi
  set +e
  # shellcheck disable=SC2086 # vars and SH_ARGS are word lists by design
  env -i HOME="$CASE_HOME" PATH="$CASE/bin:$T/sys" \
    STUB_REAL_PATH="$REAL_PATH" STUB_LOG="$CASE/log" STUB_MIRROR="$CASE/mirror" \
    STUB_TRUSTED_KEY="$TRUSTED_KEY" STUB_CURL_REQUIRE_TLS=1 \
    STUB_UNAME_S=Linux STUB_UNAME_M=x86_64 $vars \
    "$SH_BIN" $SH_ARGS "$file" "$@" >"$CASE/out" 2>"$CASE/err" </dev/null
  RC=$?
  set -e
}

# Assertions (all read CASE, DEST, RC).
rc_is() { [ "$RC" = "$1" ]; }
rc_nonzero() { [ "$RC" != 0 ]; }
out_has() { grep -F -q -- "$1" "$CASE/out"; }
err_has() { grep -F -q -- "$1" "$CASE/err"; }
installed() { cmp -s "$DEST" "$CASE/mirror/v$1/rustain-$1-$2"; }
mode_755() { [ "$(ls -l "$DEST" | cut -c1-10)" = -rwxr-xr-x ]; }
old_kept() { cmp -s "$DEST" "$CASE/old"; }
nothing_installed() { [ ! -e "$DEST" ] && [ ! -L "$DEST" ]; }
no_bin_dir() { [ ! -e "$CASE/home/.local" ]; }
no_tmp() {
  for leftover in "$BIN"/.rustain-install.*; do
    [ ! -e "$leftover" ] || return 1
  done
}
only_rustain_in_bin() { [ "$(ls -A "$BIN")" = rustain ]; }
curls() { [ "$(wc -l <"$CASE/log/curl.log" | tr -d ' ')" = "$1" ]; }
no_curl() { curls 0; }
fetched() { grep -F -q -- "/releases/download/v$1/$2" "$CASE/log/curl.log"; }
not_fetched() { ! grep -F -q -- "/$1" "$CASE/log/curl.log"; }
minisign_argv() { [ "$(cat "$CASE/log/minisign.log")" = "-Vm SHA256SUMS -P $TRUSTED_KEY" ]; }
no_sudo() { [ ! -e "$CASE/log/sudo.log" ]; }
home_untouched() { [ "$(ls -A "$CASE/home")" = .local ]; }
downloads_beside_dest() { ! grep -v -F -q -- "$BIN/.rustain-install." "$CASE/log/curl.cwd"; }

L=x86_64-unknown-linux-gnu
A=aarch64-apple-darwin
I=x86_64-apple-darwin
V=$VERSION_UNDER_TEST

# c01 good path, Linux x86_64, minisign present.
new_case c01
run_installer
check c01 'exit 0' rc_is 0
check c01 'installs the Linux asset' installed "$V" "$L"
check c01 'installed mode is 0755' mode_755
check c01 'minisign -Vm SHA256SUMS -P <trust.rs PROD_KEY_1>' minisign_argv
check c01 'three HTTPS downloads from the release' curls 3
check c01 'manifest, signature and asset fetched' fetched "$V" "rustain-$V-$L"
check c01 'prints the installed --version' out_has "rustain $V ($L)"
check c01 'downloads into ~/.local/bin/.rustain-install.*' downloads_beside_dest
check c01 'no temporary directory left' no_tmp
check c01 'only rustain in ~/.local/bin' only_rustain_in_bin
check c01 'never calls sudo' no_sudo
check c01 'no shell startup file created' home_untouched
check c01 'PATH hint printed, not applied' out_has 'export PATH="$HOME/.local/bin:$PATH"'
check c01 'names rustain update' out_has 'rustain update'

# c02 macOS Apple silicon.
new_case c02
run_installer STUB_UNAME_S=Darwin STUB_UNAME_M=arm64
check c02 'exit 0' rc_is 0
check c02 'installs aarch64-apple-darwin' installed "$V" "$A"

# c03 Intel Mac, and Rosetta on Apple silicon.
new_case c03
run_installer STUB_UNAME_S=Darwin STUB_UNAME_M=x86_64
check c03 'Intel Mac installs x86_64-apple-darwin' installed "$V" "$I"
new_case c03r
run_installer STUB_UNAME_S=Darwin STUB_UNAME_M=x86_64 STUB_PROC_TRANSLATED=1
check c03r 'Rosetta shell installs aarch64-apple-darwin' installed "$V" "$A"

# c04 minisign not installed: checksum only, stated plainly.
new_case c04 curl uname id sysctl xattr sha256sum sudo
run_installer
check c04 'exit 0' rc_is 0
check c04 'installs the Linux asset' installed "$V" "$L"
check c04 'says the signature is NOT verified' err_has 'signature of SHA256SUMS is NOT verified'
check c04 'says how to install minisign' err_has 'brew install minisign'

# c05 checksum mismatch: asset tampered, manifest and signature intact.
new_case c05
seed_old
printf 'tampered\n' >>"$CASE/mirror/v$V/rustain-$V-$L"
run_installer
check c05 'exit non-zero' rc_nonzero
check c05 'reports the checksum mismatch' err_has 'checksum mismatch'
check c05 'existing install unchanged' old_kept
check c05 'no temporary directory left' no_tmp

# c06 signature failure: manifest replaced (attacker hash) without a valid signature.
new_case c06
seed_old
d=$CASE/mirror/v$V
printf 'tampered\n' >>"$d/rustain-$V-$L"
(cd "$d" && printf '%s  %s\n' "$(hash_of "rustain-$V-$L")" "rustain-$V-$L" >SHA256SUMS)
run_installer
check c06 'exit non-zero' rc_nonzero
check c06 'reports the signature failure' err_has 'did not verify'
check c06 'asset never downloaded' not_fetched "rustain-$V-$L"
check c06 'existing install unchanged' old_kept
check c06 'no temporary directory left' no_tmp

# c07 unsupported OS/arch: refused before any download or directory creation.
for p in 'Linux aarch64' 'Linux armv7l' 'FreeBSD amd64' 'Darwin ppc'; do
  new_case "c07-$(printf '%s' "$p" | tr ' ' '-')"
  set -- $p
  run_installer STUB_UNAME_S="$1" STUB_UNAME_M="$2"
  check "c07 $p" 'exit non-zero' rc_nonzero
  check "c07 $p" 'names the platform and the source build' err_has "no release asset for"
  check "c07 $p" 'no download' no_curl
  check "c07 $p" 'creates nothing' no_bin_dir
done

# c08 VERSION pin.
new_case c08
mirror_release "$CASE/mirror" "$PIN_VERSION"
run_installer VERSION="$PIN_VERSION"
check c08 'exit 0' rc_is 0
check c08 'installs the pinned release' installed "$PIN_VERSION" "$L"
check c08 'downloads only from the pinned tag' fetched "$PIN_VERSION" "rustain-$PIN_VERSION-$L"
check c08 'says the version is pinned' out_has 'pinned by VERSION'
n=0
for bad in "v$PIN_VERSION" '0.4' '../../x' '1.2.3;id' ''; do
  n=$((n + 1))
  new_case "c08-bad$n"
  if [ -z "$bad" ]; then
    run_installer VERSION=
    check "c08 VERSION=''" 'empty VERSION means the default' installed "$V" "$L"
  else
    run_installer VERSION="$bad"
    check "c08 VERSION=$bad" 'refused' rc_nonzero
    check "c08 VERSION=$bad" 'no download' no_curl
  fi
done
new_case c08-404
run_installer VERSION=9.9.9
check c08-404 'unpublished version fails' rc_nonzero
check c08-404 'says the download failed' err_has 'download failed'
check c08-404 'nothing installed' nothing_installed
check c08-404 'no temporary directory left' no_tmp

# c09 idempotent rerun: the second run downloads no asset and leaves the file alone.
new_case c09
run_installer
cp "$DEST" "$CASE/first"
ls -li "$DEST" | awk '{ print $1 }' >"$CASE/inode1"
: >"$CASE/log/curl.log"
run_installer
check c09 'second run exit 0' rc_is 0
check c09 'second run says nothing to do' out_has 'nothing to do'
check c09 'second run fetches manifest and signature only' curls 2
check c09 'second run downloads no asset' not_fetched "rustain-$V-$L"
check c09 'same file (inode) kept' [ "$(ls -li "$DEST" | awk '{ print $1 }')" = "$(cat "$CASE/inode1")" ]
check c09 'same bytes' cmp -s "$DEST" "$CASE/first"
check c09 'second run leaves no temporary directory' no_tmp
check c09 'prints --version' out_has "rustain $V ($L)"
new_case c09u
seed_old
run_installer
check c09u 'replaces a different existing binary' installed "$V" "$L"

# c09x matching bytes but not executable (mode 0644): rerun repairs it from a verified download.
new_case c09x
run_installer
chmod 644 "$DEST"
: >"$CASE/log/curl.log"
run_installer
check c09x 'rerun exit 0' rc_is 0
check c09x 'asset downloaded again' fetched "$V" "rustain-$V-$L"
check c09x 'installed mode is 0755 again' mode_755
check c09x 'no temporary directory left' no_tmp

# c09e HOME containing a backslash: sha256sum escapes the file name it prints;
# the idempotence check must still see the manifest digest.
new_case c09e
CASE_HOME="$CASE/h\\ome"
mkdir -p "$CASE_HOME"
BIN=$CASE_HOME/.local/bin
DEST=$BIN/rustain
run_installer
ls -li "$DEST" | awk '{ print $1 }' >"$CASE/inode1"
: >"$CASE/log/curl.log"
run_installer
check c09e 'second run exit 0' rc_is 0
check c09e 'second run says nothing to do' out_has 'nothing to do'
check c09e 'second run downloads no asset' not_fetched "rustain-$V-$L"
check c09e 'same file (inode) kept' [ "$(ls -li "$DEST" | awk '{ print $1 }')" = "$(cat "$CASE/inode1")" ]
check c09e 'installed mode is 0755' mode_755
check c09e 'second run leaves no temporary directory' no_tmp

# c10 root refusal.
new_case c10
run_installer STUB_UID=0
check c10 'refuses as root' rc_nonzero
check c10 'explains' err_has 'refusing to run as root'
check c10 'no download' no_curl
check c10 'creates nothing' no_bin_dir
new_case c10f
run_installer STUB_UID=0 -- --allow-root
check c10f '--allow-root installs' installed "$V" "$L"

# c11 partial download: no half-written binary, existing install unchanged.
new_case c11
seed_old
run_installer STUB_CURL_PARTIAL="rustain-$V-$L"
check c11 'exit non-zero' rc_nonzero
check c11 'existing install unchanged' old_kept
check c11 'no temporary directory left' no_tmp
check c11 'nothing else in ~/.local/bin' only_rustain_in_bin
new_case c11n
run_installer STUB_CURL_PARTIAL="rustain-$V-$L"
check c11n 'fresh install: nothing installed' nothing_installed
check c11n 'fresh install: no temporary directory left' no_tmp

# c12 interrupted download (SIGTERM to the installer mid-download).
new_case c12
seed_old
run_installer STUB_CURL_PARTIAL="rustain-$V-$L" STUB_CURL_KILL_PARENT=1
check c12 'exit non-zero' rc_nonzero
check c12 'existing install unchanged' old_kept
check c12 'no temporary directory left' no_tmp

# c13 redirect to an untrusted host.
new_case c13
run_installer STUB_CURL_FINAL_URL=https://evil.example/x.githubusercontent.com/SHA256SUMS
check c13 'refused' rc_nonzero
check c13 'names the host' err_has 'untrusted host (evil.example)'
check c13 'nothing installed' nothing_installed
new_case c13h
run_installer STUB_CURL_FINAL_URL=http://github.com/x
check c13h 'non-https end refused' rc_nonzero

# c14 the verified binary does not run here (e.g. glibc too old).
new_case c14
seed_old
d=$CASE/mirror/v$V
printf '#!/bin/sh\necho "version GLIBC_2.39 not found" >&2\nexit 1\n' >"$d/rustain-$V-$L"
(cd "$d" && for f in rustain-* install.sh; do printf '%s  %s\n' "$(hash_of "$f")" "$f"; done >SHA256SUMS)
sign "$d"
run_installer
check c14 'exit non-zero' rc_nonzero
check c14 'shows why' err_has 'GLIBC_2.39'
check c14 'existing install unchanged' old_kept

# c15 destination is a symlink or a directory: refused, untouched.
new_case c15
mkdir -p "$BIN"
ln -s /nonexistent/rustain "$DEST"
run_installer
check c15 'symlink destination refused' rc_nonzero
check c15 'symlink untouched' [ "$(readlink "$DEST")" = /nonexistent/rustain ]
check c15 'no download' no_curl

# c16 no sha256sum: shasum is used.
new_case c16 curl minisign uname id sysctl xattr shasum sudo
run_installer
check c16 'installs with shasum' installed "$V" "$L"
check c16 'shasum -a 256 -c ran' grep -F -q -- '-a 256 -c' "$CASE/log/shasum.log"

# c17 quarantined download on macOS: not run, not installed.
new_case c17
run_installer STUB_UNAME_S=Darwin STUB_UNAME_M=arm64 STUB_QUARANTINE=1
check c17 'refused' rc_nonzero
check c17 'nothing installed' nothing_installed
check c17 'names the quarantine' err_has 'com.apple.quarantine'

# c18 manifest without a single exact record for the asset.
new_case c18
d=$CASE/mirror/v$V
(cd "$d" && printf '%s  %s\n' "$(hash_of "rustain-$V-$L")" "rustain-$V-$L-extra" >SHA256SUMS)
sign "$d"
run_installer
check c18 'no exact record refused' rc_nonzero
check c18 'asset never downloaded' not_fetched "rustain-$V-$L"
new_case c18d
d=$CASE/mirror/v$V
(cd "$d" && h=$(hash_of "rustain-$V-$L") && printf '%s  %s\n%s  %s\n' "$h" "rustain-$V-$L" "$h" "rustain-$V-$L" >SHA256SUMS)
sign "$d"
run_installer
check c18d 'duplicate record refused' rc_nonzero

# c19 arguments.
new_case c19
run_installer -- --help
check c19 '--help exits 0' rc_is 0
check c19 '--help prints usage' out_has 'Usage: install.sh'
check c19 '--help downloads nothing' no_curl
run_installer -- --bogus
check c19 'unknown option exits 2' rc_is 2

# p01-p04: the step 2 block of docs/install-macos.md, extracted from the page,
# against the same stubs (macOS Apple silicon). Its VERSION= line is the page's.
PAGE_BLOCK=$T/page-block.sh
awk '
  /^```sh$/ { inblock = 1; n = 0; has = 0; next }
  /^```$/ && inblock { if (has) { for (i = 1; i <= n; i++) print buf[i]; exit } inblock = 0; next }
  inblock { buf[++n] = $0; if ($0 == "(") has = 1 }
' "$PAGE" >"$PAGE_BLOCK"
PAGE_VERSION=$(awk -F= '/^VERSION=/ { print $2; exit }' "$PAGE_BLOCK")
page_case() {
  new_case "$@"
  mirror_release "$CASE/mirror" "$PAGE_VERSION"
  BIN=$CASE/home/.local/bin
  DEST=$BIN/rustain
}
run_page() {
  run_with "$PAGE_BLOCK" STUB_CURL_REQUIRE_TLS=0 STUB_UNAME_S=Darwin STUB_UNAME_M=arm64 "$@"
}
check p00 'page block extracted' [ -s "$PAGE_BLOCK" ]
page_case p01 curl minisign xattr shasum sudo
run_page
check p01 'page block installs the aarch64 asset' installed "$PAGE_VERSION" "$A"
check p01 'page block uses the trusted key' minisign_argv
page_case p02 curl xattr shasum sudo
run_page
check p02 'page block without minisign fails' rc_nonzero
check p02 'and downloads nothing' no_curl
page_case p03 curl minisign xattr shasum sudo
printf 'tampered\n' >>"$CASE/mirror/v$PAGE_VERSION/rustain-$PAGE_VERSION-$A"
run_page
check p03 'page block rejects a tampered asset' nothing_installed
page_case p04 curl minisign xattr shasum sudo
printf '\n' >>"$CASE/mirror/v$PAGE_VERSION/SHA256SUMS"
run_page
check p04 'page block rejects a bad signature' nothing_installed

printf 'install harness (%s %s): %s passed, %s failed\n' "$SH_BIN" "$SH_ARGS" "$passed" "$failed"
[ "$failed" = 0 ]
