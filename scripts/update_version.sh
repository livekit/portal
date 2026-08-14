#!/bin/sh
# Set the project version in the workspace Cargo.toml and python/pyproject.toml.
# Usage: scripts/update_version.sh <version>
set -eu

if [ $# -ne 1 ]; then
    echo "usage: $0 <version>" >&2
    exit 1
fi

VERSION=$1
if ! echo "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
    echo "error: '$VERSION' is not a semver (expected X.Y.Z)" >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

for file in "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/python/pyproject.toml"; do
    sed "s/^version = \".*\"/version = \"$VERSION\"/" "$file" > "$file.tmp"
    mv "$file.tmp" "$file"
    echo "updated $file"
done
