#!/usr/bin/env bash
# Downloads the release archive $TAG for this runner into $DEST and checks
# its SHA-256. Linux (musl, x86_64/aarch64) and macOS runners.
set -euo pipefail

case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) echo "::error::no Mockingbird release binary for $(uname -m)"; exit 1 ;;
esac
case "$(uname -s)" in
  Linux) target="$arch-unknown-linux-musl" ;;
  Darwin) target="$arch-apple-darwin" ;;
  *) echo "::error::no Mockingbird release binary for $(uname -s); use the Docker image or a source build (binary input)"; exit 1 ;;
esac

name="mockingbird-${TAG#v}-$target"
base="https://github.com/RJ-s-Org/Mockingbird/releases/download/$TAG"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL --retry 3 -o "$tmp/$name.tar.gz" "$base/$name.tar.gz"
curl -fsSL --retry 3 -o "$tmp/sum" "$base/$name.tar.gz.sha256"
want=$(cut -d ' ' -f 1 < "$tmp/sum")
if command -v sha256sum > /dev/null; then
  got=$(sha256sum "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
else
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | cut -d ' ' -f 1)
fi
if [ "$want" != "$got" ]; then
  echo "::error::checksum mismatch for $name.tar.gz"
  exit 1
fi
tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
mkdir -p "$DEST"
install -m 755 "$tmp/$name/mockingbird" "$DEST/mockingbird"
