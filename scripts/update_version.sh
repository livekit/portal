#!/bin/sh
# Set the project version in the workspace Cargo.toml and python/pyproject.toml,
# then sync Cargo.lock and uv.lock. The lockfiles must be committed with the
# bump: a stale Cargo.lock gets rewritten by `cargo build` during the publish
# workflow, dirtying the tree at the tag, and hatch-vcs then stamps the wheels
# with a local dev version that PyPI rejects.
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

# Sync lockfile entries for workspace members to the new version.
(cd "$REPO_ROOT" && cargo update --workspace --offline)
echo "updated $REPO_ROOT/Cargo.lock"

(cd "$REPO_ROOT/python" && uv lock)
echo "updated $REPO_ROOT/python/uv.lock"
