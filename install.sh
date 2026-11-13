#!/bin/sh
# Installs parasight on macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/wabarurse/parasight/main/install.sh | sh
#
# It downloads the latest release for this Mac and checks its sha256. If there is no
# release it can reach, it builds from source with cargo instead.
#
#   PARASIGHT_INSTALL_DIR  where the binary goes (default: ~/.local/bin)
#   PARASIGHT_BASE_URL     where to fetch the release files from (default: the latest
#                          GitHub release); mainly for testing
set -eu

REPO="wabarurse/parasight"
DIR="${PARASIGHT_INSTALL_DIR:-$HOME/.local/bin}"
BASE="${PARASIGHT_BASE_URL:-https://github.com/$REPO/releases/latest/download}"

say() { printf '%s\n' "$*"; }
fail() {
    printf 'parasight install: %s\n' "$*" >&2
    exit 1
}

[ "$(uname -s)" = "Darwin" ] || fail "parasight only runs on macOS"
case "$(uname -m)" in
    arm64) TARGET="aarch64-apple-darwin" ;;
    x86_64) TARGET="x86_64-apple-darwin" ;;
    *) fail "unsupported cpu: $(uname -m)" ;;
esac
ASSET="parasight-$TARGET.tar.gz"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT INT TERM

# 1. the release, straight from GitHub
fetch_release() {
    command -v curl >/dev/null 2>&1 || return 1
    curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET" 2>/dev/null || return 1
    curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256" 2>/dev/null || return 1
}

# 2. the same files through the gh cli, which can reach a private repository
fetch_with_gh() {
    [ -z "${PARASIGHT_BASE_URL:-}" ] || return 1
    command -v gh >/dev/null 2>&1 || return 1
    rm -f "$TMP/$ASSET" "$TMP/$ASSET.sha256"
    gh release download --repo "$REPO" --pattern "$ASSET" --pattern "$ASSET.sha256" --dir "$TMP" >/dev/null 2>&1
}

# 3. no release to be had: build it here
build_from_source() {
    [ -z "${PARASIGHT_BASE_URL:-}" ] || return 1
    command -v cargo >/dev/null 2>&1 || return 1
    say "no release found for $TARGET, building from source (this takes a minute)"
    # fetch with the git cli, so a private repository uses the credentials git already has
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --git "https://github.com/$REPO" --locked --root "$TMP/cargo" >/dev/null 2>"$TMP/cargo.log" || {
        tail -n 20 "$TMP/cargo.log" >&2
        return 1
    }
    cp "$TMP/cargo/bin/parasight" "$TMP/parasight"
}

if fetch_release || fetch_with_gh; then
    # the checksum file names the archive, so check from inside the folder
    (cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null 2>&1) || fail "the download did not match its checksum; nothing was installed"
    tar -xzf "$TMP/$ASSET" -C "$TMP" parasight || fail "the release archive has no parasight binary in it"
elif ! build_from_source; then
    fail "could not download a release or build from source.
  private repository? sign in with the gh cli (gh auth login) and run this again.
  no release yet? install rust (https://rustup.rs) so it can build from source."
fi

[ -x "$TMP/parasight" ] || chmod +x "$TMP/parasight"
"$TMP/parasight" --help >/dev/null 2>&1 || fail "the binary does not run on this Mac; nothing was installed"

mkdir -p "$DIR"
install -m 755 "$TMP/parasight" "$DIR/parasight"
say "installed parasight to $DIR/parasight"

case ":$PATH:" in
    *":$DIR:"*) say "run it with: parasight" ;;
    *)
        say "$DIR is not on your PATH. add this to your shell profile, then open a new terminal:"
        say "  export PATH=\"$DIR:\$PATH\""
        ;;
esac
