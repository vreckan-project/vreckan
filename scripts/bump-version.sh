#!/usr/bin/env bash
# bump-version.sh — bump the VERSION file (single source of truth) and print
# the new version to stdout.
#
# Usage:
#   ./scripts/bump-version.sh major   # 0.3.2 -> 1.0.0
#   ./scripts/bump-version.sh minor   # 0.3.2 -> 0.4.0
#   ./scripts/bump-version.sh patch   # 0.3.2 -> 0.3.3
#
# The script reads the current version from ./VERSION, applies the bump,
# writes it back, and echoes the new version (so a workflow can capture it
# with `NEW_VERSION=$(./scripts/bump-version.sh minor)`).
#
# This is the single place version math lives, so the GitHub and Gitea
# workflows can both call it and stay in sync.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

KIND="${1:-patch}"
VERSION_FILE="VERSION"

if [ ! -f "$VERSION_FILE" ]; then
  echo "ERROR: $VERSION_FILE not found" >&2
  exit 1
fi

CURRENT="$(tr -d '[:space:]' < "$VERSION_FILE")"

# Split MAJOR.MINOR.PATCH. Tolerate a missing patch (e.g. "1.2" -> 1.2.0).
MAJOR="${CURRENT%%.*}"
REST="${CURRENT#*.}"
MINOR="${REST%%.*}"
PATCH="${REST#*.}"
# If there was no patch component, PATCH == MINOR; reset it to 0.
if [ "$PATCH" = "$MINOR" ]; then
  PATCH=0
fi

case "$KIND" in
  major)
    MAJOR=$((MAJOR + 1)); MINOR=0; PATCH=0 ;;
  minor)
    MINOR=$((MINOR + 1)); PATCH=0 ;;
  patch)
    PATCH=$((PATCH + 1)) ;;
  *)
    echo "ERROR: unknown bump kind '$KIND' (use major|minor|patch)" >&2
    exit 1 ;;
esac

NEW_VERSION="${MAJOR}.${MINOR}.${PATCH}"
printf '%s\n' "$NEW_VERSION" > "$VERSION_FILE"
echo "$NEW_VERSION"
