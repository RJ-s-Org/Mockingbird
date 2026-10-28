#!/usr/bin/env bash
# Builds the documentation site and checks its links, offline.
#
# Usage: scripts/docs-check.sh
#
# Needs mdbook and lychee on PATH (the Docs workflow pins both). Fails when a
# page in docs/SUMMARY.md is missing, when a relative link or #anchor in the
# site or in the top-level Markdown files does not resolve. External URLs are
# not fetched; `lychee --no-progress README.md docs` checks them by hand.
set -euo pipefail
cd "$(dirname "$0")/.."

mdbook build
lychee --offline --include-fragments --no-progress \
    --exclude-path target/book/404.html \
    'target/book/**/*.html' README.md CONTRIBUTING.md CHANGELOG.md SECURITY.md docs/launch
