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
# VERSION stays the single source of version math; the script also re-points
# the root Cargo.toml [package] version and the matching Cargo.lock entry in
# the same run, so the three files never drift (the 0.8.5 cut found Cargo at
# 0.8.0 while VERSION was 0.8.4 — ANT-450). Commit the file changes the
# script makes together with the CHANGELOG entry. All diagnostics go to
# stderr; stdout carries only the new version.
#
# This is the single place version math lives, so the GitHub and Gitea
# workflows can both call it and stay in sync.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

KIND="${1:-patch}"
VERSION_FILE="VERSION"
CARGO_TOML="Cargo.toml"
CARGO_LOCK="Cargo.lock"
CRATE_NAME="vreckan"

if [ ! -f "$VERSION_FILE" ]; then
  echo "ERROR: $VERSION_FILE not found" >&2
  exit 1
fi

CURRENT="$(tr -d '[:space:]' < "$VERSION_FILE")"

# Split MAJOR.MINOR.PATCH. Tolerate a missing patch (e.g. "1.2" -> 1.2.0);
# reject anything that is not plain MAJOR.MINOR[.PATCH] digits.
IFS='.' read -r MAJOR MINOR PATCH <<< "$CURRENT"
MAJOR="${MAJOR:-0}"
MINOR="${MINOR:-0}"
PATCH="${PATCH:-0}"
for part in "$MAJOR" "$MINOR" "$PATCH"; do
  if ! [[ "$part" =~ ^[0-9]+$ ]]; then
    echo "ERROR: '$CURRENT' in $VERSION_FILE is not a MAJOR.MINOR[.PATCH] version" >&2
    exit 1
  fi
done

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

# Keep the Rust crate version pinned to VERSION. In Cargo.toml the [package]
# version is the only line that starts at column 0 with `version = "..."`
# (dependencies are written `name = { version = ... }`), so a column-0
# anchor re-points exactly that line. In Cargo.lock only the vreckan package
# block's version line is re-pointed; vreckan is a path crate with no
# checksum, so nothing else in its block changes. A missing Cargo.lock (e.g.
# a pre-first-build tree) is skipped: the next `cargo` run regenerates it
# from Cargo.toml with the right version.
if [ ! -f "$CARGO_TOML" ]; then
  echo "ERROR: $CARGO_TOML not found (expected next to $VERSION_FILE)" >&2
  exit 1
fi
sed -i "s/^version = \"[^\"]*\"/version = \"${NEW_VERSION}\"/" "$CARGO_TOML"
if [ -f "$CARGO_LOCK" ]; then
  sed -i "/^name = \"${CRATE_NAME}\"$/,/^$/{ s/^version = \"[^\"]*\"/version = \"${NEW_VERSION}\"/; }" "$CARGO_LOCK"
fi

# Self-verify: every file written above must read back the new version, so a
# missing or moved version line fails the bump instead of recreating the
# drift.
check() { # $1 = label, $2 = value read back
  if [ "$2" != "$NEW_VERSION" ]; then
    echo "ERROR: $1 does not read back $NEW_VERSION (got '${2:-<none>}')" >&2
    exit 1
  fi
}
check "$VERSION_FILE" "$(tr -d '[:space:]' < "$VERSION_FILE")"
check "$CARGO_TOML" "$(sed -n '/^version = "/{ s/^version = "\(.*\)"$/\1/; p; q; }' "$CARGO_TOML")"
if [ -f "$CARGO_LOCK" ]; then
  check "$CARGO_LOCK ($CRATE_NAME package)" "$(awk -v n="\"${CRATE_NAME}\"" '$0 == "name = " n { f = 1; next } f && /^version = / { sub(/^version = "/, ""); sub(/".*$/, ""); print; exit }' "$CARGO_LOCK")"
fi

echo "$NEW_VERSION"
