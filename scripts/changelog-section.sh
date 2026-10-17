#!/usr/bin/env bash
# Prints the body of one version's section of CHANGELOG.md (without its
# `## [<version>]` heading), for release notes.
#
# Usage: scripts/changelog-section.sh <version>
# Example: scripts/changelog-section.sh 0.1.0
#
# Exits 1 when CHANGELOG.md has no `## [<version>]` section, 2 on bad usage.
set -euo pipefail

if [[ $# -ne 1 || -z "$1" ]]; then
    echo "usage: $0 <version>" >&2
    exit 2
fi
VERSION=${1#v}
CHANGELOG=${CHANGELOG:-$(dirname "$0")/../CHANGELOG.md}

awk -v version="$VERSION" '
    BEGIN { heading = "## [" version "]" }
    index($0, "## [") == 1 {
        if (found) { exit }
        if (substr($0, 1, length(heading)) == heading) { found = 1; next }
    }
    # Link reference definitions at the end of the file are not part of a section.
    found && /^\[[^]]+\]: / { exit }
    found { lines[++n] = $0 }
    END {
        if (!found) { exit 1 }
        start = 1
        while (start <= n && lines[start] == "") start++
        end = n
        while (end >= start && lines[end] == "") end--
        for (i = start; i <= end; i++) print lines[i]
    }
' "$CHANGELOG" || { echo "changelog-section: no section for $VERSION in $CHANGELOG" >&2; exit 1; }
