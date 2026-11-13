#!/bin/sh
# Installs parasight into ~/.parasight/bin and puts it on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/wabarurse/parasight/main/install.sh | sh
#
# Run it again to update. Everything parasight keeps is under ~/.parasight, so
# uninstalling is `rm -rf ~/.parasight` plus the PATH line this adds to your shell profile.
#
#   PARASIGHT_HOME      install somewhere else (default: ~/.parasight)
#   PARASIGHT_BASE_URL  where to fetch the release from (for testing)
#   PARASIGHT_NO_PATH   set to skip editing your shell profile
set -eu

REPO="wabarurse/parasight"
HOME_DIR="${PARASIGHT_HOME:-$HOME/.parasight}"
BIN="$HOME_DIR/bin"
BASE="${PARASIGHT_BASE_URL:-https://github.com/$REPO/releases/latest/download}"

fail() {
    printf 'parasight: %s\n' "$*" >&2
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

# Get the binary: the release over plain https, then through the gh cli (which can read a
# private repository), then a build from source with cargo.
download() {
    curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET" 2>/dev/null &&
        curl -fsSL "$BASE/$ASSET.sha256" -o "$TMP/$ASSET.sha256" 2>/dev/null
}
download_with_gh() {
    [ -z "${PARASIGHT_BASE_URL:-}" ] && command -v gh >/dev/null 2>&1 || return 1
    gh release download --repo "$REPO" --pattern "$ASSET" --pattern "$ASSET.sha256" --dir "$TMP" --clobber >/dev/null 2>&1
}
build() {
    [ -z "${PARASIGHT_BASE_URL:-}" ] && command -v cargo >/dev/null 2>&1 || return 1
    printf 'no release for %s yet, building from source…\n' "$TARGET"
    # the git cli fetches, so a private repository uses the credentials git already has
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --quiet --git "https://github.com/$REPO" --locked --root "$TMP/cargo" 2>"$TMP/build.log" ||
        { tail -n 20 "$TMP/build.log" >&2; return 1; }
    mv "$TMP/cargo/bin/parasight" "$TMP/parasight"
}

if download || download_with_gh; then
    (cd "$TMP" && shasum -a 256 -c "$ASSET.sha256" >/dev/null 2>&1) || fail "download did not match its checksum; nothing installed"
    tar -xzf "$TMP/$ASSET" -C "$TMP" parasight || fail "release archive has no parasight in it"
elif ! build; then
    fail "could not download or build parasight.
  private repository? sign in with \`gh auth login\` and run this again.
  no release yet? install rust from https://rustup.rs so it can build from source."
fi
"$TMP/parasight" --version >/dev/null 2>&1 || fail "the binary does not run on this Mac; nothing installed"

mkdir -p "$BIN"
install -m 755 "$TMP/parasight" "$BIN/parasight"

# an install from before ~/.parasight existed would shadow this one on the PATH
OLD="$HOME/.local/bin/parasight"
[ -x "$OLD" ] && "$OLD" --version 2>/dev/null | grep -q '^parasight ' && rm -f "$OLD"

VERSION="$("$BIN/parasight" --version)"

# Put the bin folder on the PATH, once, in the profile of the shell you use. The default
# location is written as $HOME/.parasight/bin so the line reads well in your profile.
case "$BIN" in
    "$HOME"/*) SHOWN="~${BIN#"$HOME"}"; PROFILE_BIN="\$HOME${BIN#"$HOME"}" ;;
    *) SHOWN="$BIN"; PROFILE_BIN="$BIN" ;;
esac
on_path() { case ":$PATH:" in *":$BIN:"*) return 0 ;; *) return 1 ;; esac; }
profile=""
added=""
if ! on_path && [ -z "${PARASIGHT_NO_PATH:-}" ]; then
    case "$(basename "${SHELL:-zsh}")" in
        zsh) profile="${ZDOTDIR:-$HOME}/.zshrc"; line="export PATH=\"$PROFILE_BIN:\$PATH\"" ;;
        bash) profile="$HOME/.bash_profile"; line="export PATH=\"$PROFILE_BIN:\$PATH\"" ;;
        fish) profile="$HOME/.config/fish/config.fish"; line="fish_add_path $PROFILE_BIN" ;;
    esac
    if [ -n "$profile" ] && ! grep -qsF "$PROFILE_BIN" "$profile" && ! grep -qsF "$BIN" "$profile"; then
        mkdir -p "$(dirname "$profile")"
        printf '\n# parasight\n%s\n' "$line" >>"$profile"
        added=1
    fi
fi

printf 'installed %s to %s\n' "$VERSION" "$SHOWN/parasight"
if on_path; then
    printf 'run: parasight\n'
elif [ -n "$added" ]; then
    printf 'added %s to your PATH in %s\n' "$SHOWN" "~${profile#"$HOME"}"
    printf 'open a new terminal, then run: parasight\n'
elif [ -n "$profile" ]; then
    printf 'in a new terminal, run: parasight\n'
else
    printf 'add %s to your PATH, then run: parasight\n' "$SHOWN"
fi
