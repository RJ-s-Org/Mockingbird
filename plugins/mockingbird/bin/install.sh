#!/bin/sh
# Installs the mockingbird binary (macOS, Linux).
#
#   curl -fsSL https://raw.githubusercontent.com/rjs-git/mockingbird/main/install.sh | sh
#
# Environment:
#   MOCKINGBIRD_VERSION      release tag to install (default: the latest release)
#   MOCKINGBIRD_INSTALL_DIR  where the binary goes (default: ~/.mockingbird/bin)
#
# It downloads the release archive for this platform, checks its SHA-256 and
# installs the binary. When no release binary fits (no release yet, an
# unsupported platform, no network to GitHub releases), it builds from source
# with `cargo install` if cargo is available. Progress goes to stderr; the
# last line on stdout is the installed binary's path.
set -eu

REPO="rjs-git/mockingbird"
INSTALL_DIR="${MOCKINGBIRD_INSTALL_DIR:-$HOME/.mockingbird/bin}"

say() { printf 'mockingbird-install: %s\n' "$*" >&2; }
fail() { say "error: $*"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # url dest
    if have curl; then
        curl -fsSL --retry 3 -o "$2" "$1"
    elif have wget; then
        wget -q -O "$2" "$1"
    else
        fail "need curl or wget"
    fi
}

fetch_stdout() { # url
    if have curl; then
        curl -fsSL --retry 3 "$1"
    elif have wget; then
        wget -q -O - "$1"
    else
        fail "need curl or wget"
    fi
}

sha256() {
    if have sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif have shasum; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        echo ""
    fi
}

target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        arm64 | aarch64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    case "$os" in
        Linux) echo "$arch-unknown-linux-musl" ;;
        Darwin) echo "$arch-apple-darwin" ;;
        *) return 1 ;;
    esac
}

latest_tag() {
    fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1
}

install_release() {
    tgt=$(target) || { say "no release binary for $(uname -s) $(uname -m)"; return 1; }
    tag="${MOCKINGBIRD_VERSION:-}"
    [ -n "$tag" ] || tag=$(latest_tag) || true
    [ -n "$tag" ] || { say "no published release found"; return 1; }
    version=${tag#v}
    name="mockingbird-$version-$tgt"
    base="https://github.com/$REPO/releases/download/$tag"
    tmp=$(mktemp -d)
    say "downloading $name ($tag)"
    fetch "$base/$name.tar.gz" "$tmp/$name.tar.gz" || { rm -rf "$tmp"; say "download failed"; return 1; }
    if fetch "$base/$name.tar.gz.sha256" "$tmp/sum" 2>/dev/null; then
        want=$(cut -d ' ' -f 1 < "$tmp/sum")
        got=$(sha256 "$tmp/$name.tar.gz")
        if [ -n "$got" ] && [ "$want" != "$got" ]; then
            rm -rf "$tmp"
            fail "checksum mismatch for $name.tar.gz"
        fi
        [ -n "$got" ] && say "checksum ok"
    fi
    tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
    mkdir -p "$INSTALL_DIR"
    cp "$tmp/$name/mockingbird" "$INSTALL_DIR/mockingbird"
    chmod 755 "$INSTALL_DIR/mockingbird"
    rm -rf "$tmp"
}

install_source() {
    have cargo || return 1
    say "building from source with cargo (a few minutes)"
    tmp=$(mktemp -d)
    if [ -n "${MOCKINGBIRD_VERSION:-}" ]; then
        set -- --tag "$MOCKINGBIRD_VERSION"
    else
        set --
    fi
    cargo install --locked --git "https://github.com/$REPO" "$@" --root "$tmp" mockingbird >&2 ||
        { rm -rf "$tmp"; return 1; }
    mkdir -p "$INSTALL_DIR"
    cp "$tmp/bin/mockingbird" "$INSTALL_DIR/mockingbird"
    rm -rf "$tmp"
}

if ! install_release; then
    install_source || fail "could not install: no release binary for this platform and no cargo.
  Install Rust (https://rustup.rs) and rerun, or use Docker: docker run ghcr.io/$REPO --help"
fi

"$INSTALL_DIR/mockingbird" --version >&2 || fail "the installed binary does not run"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) say "add it to your PATH: export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
say "next: mockingbird agent install <claude-code|codex|cursor|gemini|kiro|vscode|windsurf|claude-desktop>"
echo "$INSTALL_DIR/mockingbird"
