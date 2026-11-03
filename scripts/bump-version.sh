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
# the root Cargo.toml [workspace.package] version and the matching
# Cargo.lock entries for every workspace crate in the same run, so the
# files never drift (the 0.8.5 cut found Cargo at 0.8.0 while VERSION was
# 0.8.4 — ANT-450). Before the Candidate C split (ANT-770) the repo was one
# crate and only the root lock entry needed re-pointing; after the split
# the other five members' lock entries drifted on every bump until
# something else forced a lockfile rewrite (ANT-805). Commit the file
# changes the script makes together with the CHANGELOG entry. All
# diagnostics go to stderr; stdout carries only the new version.
#
# This is the single place version math lives, so the GitHub and Gitea
# workflows can both call it and stay in sync.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

KIND="${1:-patch}"
VERSION_FILE="VERSION"
CARGO_TOML="Cargo.toml"
CARGO_LOCK="Cargo.lock"

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

# Workspace member package names (ANT-805). All workspace crates inherit
# `workspace.package.version` from the root Cargo.toml, but Cargo.lock
# carries one version entry per crate — so the lock re-pointing must cover
# every member, not just the root. The members list is parsed from the
# [workspace] table (single- or multi-line), and each member's [package]
# name is read from its own manifest: the first column-0 `name = "..."`
# line of a manifest is the [package] name (the package table precedes any
# other table; in the root manifest that line is the root [package] name,
# which comes after the [workspace] table).
resolve_pkg_name() { # $1 = workspace member directory
  local toml="$1/Cargo.toml"
  if [ ! -f "$toml" ]; then
    echo "ERROR: $toml not found (workspace member '$1')" >&2
    return 1
  fi
  local n
  n="$(sed -n '/^name = "/{ s/^name = "\(.*\)"$/\1/; p; q; }' "$toml")"
  if [ -z "$n" ]; then
    echo "ERROR: no [package] name found in $toml" >&2
    return 1
  fi
  printf '%s\n' "$n"
}

MEMBERS="$(awk '
  /^[[:space:]]*members[[:space:]]*=/ { in_arr = 1 }
  in_arr {
    while (match($0, /"[^"]*"/)) {
      print substr($0, RSTART + 1, RLENGTH - 2)
      $0 = substr($0, RSTART + RLENGTH)
    }
    if (index($0, "]") > 0) exit
  }
' "$CARGO_TOML")"
if [ -z "$MEMBERS" ]; then
  echo "ERROR: no [workspace] members list found in $CARGO_TOML" >&2
  exit 1
fi

PKG_NAMES=()
while IFS= read -r MEMBER; do
  [ -n "$MEMBER" ] || continue
  N="$(resolve_pkg_name "$MEMBER")" || exit 1
  if [[ " ${PKG_NAMES[*]:-} " != *" $N "* ]]; then
    PKG_NAMES+=("$N")
  fi
done <<< "$MEMBERS"

# Keep the Rust crate versions pinned to VERSION. In the root Cargo.toml
# the [workspace.package] version is the only line that starts at column 0
# with `version = "..."` (member manifests and dependencies are written
# `version = { workspace = ... }`), so a column-0 anchor re-points exactly
# that line. In Cargo.lock, every workspace member's package block version
# line is re-pointed; each is a path crate with no checksum, so nothing
# else in those blocks changes. A missing Cargo.lock (e.g. a pre-first-
# build tree) is skipped: the next `cargo` run regenerates it from
# Cargo.toml with the right version.
if [ ! -f "$CARGO_TOML" ]; then
  echo "ERROR: $CARGO_TOML not found (expected next to $VERSION_FILE)" >&2
  exit 1
fi
sed -i "s/^version = \"[^\"]*\"/version = \"${NEW_VERSION}\"/" "$CARGO_TOML"
if [ -f "$CARGO_LOCK" ]; then
  for N in "${PKG_NAMES[@]}"; do
    sed -i "/^name = \"${N}\"$/,/^$/{ s/^version = \"[^\"]*\"/version = \"${NEW_VERSION}\"/; }" "$CARGO_LOCK"
  done
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
  for N in "${PKG_NAMES[@]}"; do
    check "$CARGO_LOCK ($N package)" "$(awk -v n="\"${N}\"" '$0 == "name = " n { f = 1; next } f && /^version = / { sub(/^version = "/, ""); sub(/".*$/, ""); print; exit }' "$CARGO_LOCK")"
  done
fi

echo "$NEW_VERSION"
