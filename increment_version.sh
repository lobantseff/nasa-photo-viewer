#!/usr/bin/env bash
#
# Create the next release tag.
#
# The tag is the only source of the version the application reports, so this
# refuses to run on a dirty tree: a tag placed on uncommitted work would name a
# state that exists nowhere else.
#
# Usage:
#   ./increment_version.sh [major|minor|patch] [--push] [--dry-run]
#
# Defaults to `patch`. With `--push` the new tag is pushed to `origin`.

set -euo pipefail

part="patch"
push=false
dry_run=false

usage() {
    cat <<'USAGE'
Create the next release tag.

Usage:
  ./increment_version.sh [major|minor|patch] [--push] [--dry-run]

  major | minor | patch   Which component to increment (default: patch).
  --push                  Push the new tag to 'origin'.
  --dry-run               Report the next version without creating a tag.

The tag is the only source of the version the application reports, so this
refuses to run on a dirty tree: a tag placed on uncommitted work would name a
state that exists nowhere else.
USAGE
}

for arg in "$@"; do
    case "$arg" in
        major|minor|patch) part="$arg" ;;
        --push)            push=true ;;
        --dry-run)         dry_run=true ;;
        -h|--help)         usage; exit 0 ;;
        *)
            echo "error: unknown argument '$arg'" >&2
            echo >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    echo "error: not inside a git repository" >&2
    exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree has uncommitted changes; commit or stash first" >&2
    exit 1
fi

# `git describe` finds the latest tag reachable from HEAD, which is the one
# being incremented; a tag on some unmerged branch is deliberately ignored.
current="$(git describe --tags --abbrev=0 2>/dev/null || echo "v0.0.0")"

if ! [[ "$current" =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    echo "error: latest tag '$current' is not of the form vMAJOR.MINOR.PATCH" >&2
    exit 1
fi

major="${BASH_REMATCH[1]}"
minor="${BASH_REMATCH[2]}"
patch="${BASH_REMATCH[3]}"

case "$part" in
    major) major=$((major + 1)); minor=0; patch=0 ;;
    minor) minor=$((minor + 1)); patch=0 ;;
    patch) patch=$((patch + 1)) ;;
esac

next="v${major}.${minor}.${patch}"

if git rev-parse -q --verify "refs/tags/$next" >/dev/null; then
    echo "error: tag $next already exists" >&2
    exit 1
fi

echo "$current -> $next ($part)"

if [ "$dry_run" = true ]; then
    echo "dry run: no tag created"
    exit 0
fi

git tag -a "$next" -m "Release $next"
echo "created tag $next at $(git rev-parse --short HEAD)"

if [ "$push" = true ]; then
    if ! git remote get-url origin >/dev/null 2>&1; then
        echo "error: no 'origin' remote to push to; tag was created locally" >&2
        exit 1
    fi
    git push origin "$next"
    echo "pushed $next to origin"
else
    echo "not pushed; run: git push origin $next"
fi
