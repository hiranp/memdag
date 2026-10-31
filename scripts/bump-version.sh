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

# First `version = ` line only (the [package] one). perl, not `sed 0,/re/`, which is GNU-only.
perl -0pi -e "s/^version = \"\Q$current\E\"/version = \"$new\"/m" Cargo.toml

# The release notes are CHANGELOG.md's section for this version (release.yml fails without
# one), so turn the [Unreleased] section into it and start a new, empty [Unreleased].
if grep -q '^## \[Unreleased\]' CHANGELOG.md; then
    today=$(date -u +%Y-%m-%d)
    perl -0pi -e "s/^## \[Unreleased\]\n/## [Unreleased]\n\n## [$new] - $today\n/m" CHANGELOG.md
elif ! grep -q "^## \[$new\]" CHANGELOG.md; then
    echo "error: CHANGELOG.md has neither an [Unreleased] nor a [$new] section" >&2
    git checkout -- Cargo.toml
    exit 1
fi

# Refresh Cargo.lock's recorded version for this package (deps untouched).
cargo check -q

git add Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "chore: release v$new"
git tag "v$new"

echo "Tagged v$new."
echo "Push with: git push && git push origin v$new"
