#!/usr/bin/env bash
# Bump the package version in Cargo.toml, refresh Cargo.lock, commit, and tag.
# Usage: scripts/bump-version.sh [patch|minor|major|X.Y.Z]  (default: patch)
#
# Does not push. Review with `git show`, then:
#   git push && git push origin "$(git describe --tags --abbrev=0)"
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -n "$(git status --porcelain)" ]]; then
    echo "error: working tree is not clean" >&2
    exit 1
fi

bump="${1:-patch}"
current=$(awk -F'"' '/^\[package\]/{p=1} p && /^version/{print $2; exit}' Cargo.toml)
IFS=. read -r major minor patch <<<"$current"

case "$bump" in
patch) patch=$((patch + 1)) ;;
minor) minor=$((minor + 1)); patch=0 ;;
major) major=$((major + 1)); minor=0; patch=0 ;;
[0-9]*.[0-9]*.[0-9]*) IFS=. read -r major minor patch <<<"$bump" ;;
*)
    echo "usage: $0 [patch|minor|major|X.Y.Z]" >&2
    exit 1
    ;;
esac

new="$major.$minor.$patch"
echo "Bumping $current -> $new"

sed -i.bak "0,/^version = \"$current\"/s//version = \"$new\"/" Cargo.toml
rm -f Cargo.toml.bak

# Refresh Cargo.lock's recorded version for this package (deps untouched).
cargo check -q

git add Cargo.toml Cargo.lock
git commit -m "chore: release v$new"
git tag "v$new"

echo "Tagged v$new."
echo "Push with: git push && git push origin v$new"
