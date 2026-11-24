#!/bin/sh
# rustain installer. Downloads a published release asset, verifies it against the
# release's SHA256SUMS (and that file's minisign signature when minisign is
# installed) and installs it as ~/.local/bin/rustain for the current user.
#
#   curl --proto '=https' --tlsv1.2 -fsSL https://github.com/lunarpulse/rustain/releases/latest/download/install.sh | sh
#
# Source: dist/install.sh in the rustain repository; published as the release
# asset install.sh and listed in the release's signed SHA256SUMS. It runs the
# checks of the step 2 block in docs/install-macos.md. Everything runs inside
# main(), called on the last line, so a truncated download runs nothing.

set -eu

DEFAULT_VERSION=0.5.0
REPO_URL=https://github.com/lunarpulse/rustain
PROD_KEY_1=RWSUI8k3UrzEelNHSLUobmr5IbvKMx73rDg7gWVBy8vhID/OGxpiJKYF

TMP_DIR=
BIN_DIR=

say() {
  printf 'rustain-install: %s\n' "$*"
}

warn() {
  printf 'rustain-install: %s\n' "$*" >&2
}

die() {
  warn "error: $*"
  exit 1
}

usage() {
  printf '%s\n' \
    'Usage: install.sh [--allow-root] [--help]' \
    '' \
    'Installs rustain into ~/.local/bin/rustain for the current user.' \
    '' \
    '  VERSION=X.Y.Z   install that release (no leading v); default: the release' \
    '                  this script was published with' \
    '  --allow-root    run even as root (installs into root'"'"'s ~/.local/bin)' \
    '  --help          print this help' \
    '' \
    'Never uses sudo and never edits shell startup files.' \
    'Update an installed rustain with: rustain update'
}

cleanup() {
  if [ -n "$TMP_DIR" ]; then
    case "$TMP_DIR" in
      "$BIN_DIR"/.rustain-install.*)
        cd / && rm -rf "$TMP_DIR" && TMP_DIR=
        ;;
    esac
  fi
}

have() {
  command -v "$1" >/dev/null 2>&1
}

sha256() {
  if have sha256sum; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

detect_target() {
  OS=$(uname -s) || die 'uname -s failed'
  arch=$(uname -m) || die 'uname -m failed'
  case "$OS" in
    Linux)
      case "$arch" in
        x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
        *) unsupported "Linux $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        arm64 | aarch64) TARGET=aarch64-apple-darwin ;;
        x86_64)
          # An x86_64 shell on Apple silicon runs under Rosetta; install the native asset.
          if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            TARGET=aarch64-apple-darwin
          else
            TARGET=x86_64-apple-darwin
          fi
          ;;
        *) unsupported "macOS $arch" ;;
      esac
      ;;
    *) unsupported "$OS $arch" ;;
  esac
}

unsupported() {
  die "no release asset for $1. Release assets exist for Linux x86_64 and macOS (Apple silicon, Intel); build from source instead: $REPO_URL#readme"
}

# fetch NAME: download $BASE_URL/NAME to ./NAME over HTTPS only, then require
# that the last URL (after redirects) is on github.com or *.githubusercontent.com.
fetch() {
  curl --proto '=https' --proto-redir '=https' --tlsv1.2 --max-redirs 5 -fsSL \
    -o "$1" -w '%{url_effective}\n' "$BASE_URL/$1" >"$1.url" ||
    die "download failed: $BASE_URL/$1 (is v$VERSION published for $TARGET?); nothing was installed"
  IFS= read -r effective <"$1.url" || effective=
  rest=${effective#https://}
  if [ "$rest" = "$effective" ]; then
    die "download of $1 did not end on an https URL; nothing was installed"
  fi
  host=${rest%%/*}
  host=${host##*@}
  host=${host%%:*}
  case "$host" in
    github.com | *.githubusercontent.com) ;;
    *) die "download of $1 ended on an untrusted host ($host); nothing was installed" ;;
  esac
}

main() {
  allow_root=0
  for arg in "$@"; do
    case "$arg" in
      --allow-root) allow_root=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
  done

  uid=$(id -u) || die 'id -u failed'
  if [ "$uid" = 0 ] && [ "$allow_root" != 1 ]; then
    die 'refusing to run as root: rustain installs per user into ~/.local/bin and never needs root. Run it as your own user, or pass --allow-root to install into root'"'"'s home.'
  fi

  detect_target

  for tool in curl awk mktemp chmod mv rm mkdir; do
    have "$tool" || die "$tool is required but not installed"
  done
  have sha256sum || have shasum || die 'sha256sum or shasum is required but not installed'

  if [ -n "${VERSION:-}" ]; then
    how='pinned by VERSION'
  else
    VERSION=$DEFAULT_VERSION
    how='default'
  fi
  case "$VERSION" in
    v*) die "VERSION must not start with v (for example VERSION=$DEFAULT_VERSION)" ;;
    *[!0-9A-Za-z.-]*) die 'VERSION may contain only letters, digits, dots and hyphens' ;;
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) die "VERSION must look like $DEFAULT_VERSION" ;;
  esac

  case "${HOME:-}" in
    /*) ;;
    *) die 'HOME must be set to an absolute path' ;;
  esac
  BIN_DIR=$HOME/.local/bin
  DEST=$BIN_DIR/rustain
  if [ -L "$DEST" ] || { [ -e "$DEST" ] && [ ! -f "$DEST" ]; }; then
    die "$DEST exists and is not a regular file; move it away and rerun"
  fi

  ASSET=rustain-$VERSION-$TARGET
  BASE_URL=$REPO_URL/releases/download/v$VERSION
  say "installing rustain $VERSION ($how) for $TARGET into $DEST"

  mkdir -p "$BIN_DIR" || die "cannot create $BIN_DIR"
  umask 077
  # Same directory as the destination, so the final mv is a rename.
  TMP_DIR=$(mktemp -d "$BIN_DIR/.rustain-install.XXXXXX") ||
    die "cannot create a temporary directory in $BIN_DIR"
  cd "$TMP_DIR" || die "cannot enter $TMP_DIR"

  fetch SHA256SUMS
  fetch SHA256SUMS.minisig
  if have minisign; then
    minisign -Vm SHA256SUMS -P "$PROD_KEY_1" ||
      die 'the minisign signature of SHA256SUMS did not verify with the rustain release key; nothing was installed'
    signature=verified
  else
    signature=skipped
    warn 'minisign is not installed: the signature of SHA256SUMS is NOT verified; only the SHA-256 checksum it lists is checked.'
    warn 'To verify the signature too, install minisign (macOS: brew install minisign; Debian/Ubuntu: apt install minisign; Fedora: dnf install minisign; Arch: pacman -S minisign) and rerun.'
  fi

  awk -v a="$ASSET" '
    length($1) == 64 &&
    $1 ~ /^[0-9a-f]+$/ &&
    substr($0, 65, 2) == "  " &&
    substr($0, 67) == a { n++; print }
    END { exit n != 1 }
  ' SHA256SUMS > "$ASSET.sha256" ||
    die "SHA256SUMS of v$VERSION has no single entry for $ASSET; nothing was installed"

  # Only a matching, executable destination is a no-op; a matching file that
  # cannot run falls through to the verified replacement. Hash stdin: sha256sum
  # prefixes a backslash to the file name it prints for paths with \ or newline.
  if [ -f "$DEST" ] && [ -x "$DEST" ]; then
    sha256 <"$DEST" >installed.sha256 || die "cannot hash $DEST"
    read -r installed_hash _ <installed.sha256 || installed_hash=
    read -r wanted_hash _ <"$ASSET.sha256" || wanted_hash=
    if [ "$installed_hash" = "$wanted_hash" ]; then
      say "$DEST is already rustain $VERSION for $TARGET; nothing to do"
      "$DEST" --version || die "$DEST --version failed"
      return 0
    fi
  fi

  fetch "$ASSET"
  sha256 -c "$ASSET.sha256" ||
    die "checksum mismatch for $ASSET; nothing was installed"
  chmod 0755 "$ASSET" || die "chmod failed"

  if [ "$OS" = Darwin ] && have xattr && xattr -p com.apple.quarantine "$ASSET" >/dev/null 2>&1; then
    die "the download is marked com.apple.quarantine; nothing was installed. See $REPO_URL/blob/main/docs/install-macos.md#downloaded-with-a-browser"
  fi
  if ! "./$ASSET" --version >staged.version 2>&1; then
    while IFS= read -r line; do
      warn "  $line"
    done <staged.version
    die "the downloaded rustain does not run on this system; nothing was installed"
  fi

  if [ -d "$DEST" ]; then
    die "$DEST is a directory; nothing was installed"
  fi
  mv -f "$ASSET" "$DEST" || die "cannot move the verified binary to $DEST"
  say "installed $DEST"
  "$DEST" --version || die "$DEST --version failed"

  case ":${PATH:-}:" in
    *":$BIN_DIR:"*) ;;
    *)
      say "$BIN_DIR is not on your PATH. For this terminal run:"
      say '  export PATH="$HOME/.local/bin:$PATH"'
      say 'and add that line to your shell startup file (~/.zshrc for zsh, ~/.bashrc for bash) yourself.'
      ;;
  esac
  say 'update later with: rustain update'
  if [ "$signature" = skipped ]; then
    warn 'reminder: the minisign signature was NOT verified (minisign is not installed).'
  fi
}

# Top level, not inside main(): zsh scopes an EXIT trap set in a function. The
# signal traps clean up themselves: zsh with errexit skips the EXIT trap after
# an exit from a signal trap.
trap cleanup EXIT
trap 'cleanup; exit 129' HUP
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM
main "$@"
