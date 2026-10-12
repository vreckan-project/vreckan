#!/usr/bin/env bash
# mirror-to-gitea.sh — push the current GitHub checkout to the Gitea mirror.
#
# This is the "GitHub is source of truth, Gitea is a mirror" leg. A GitHub
# workflow (or a cron) runs this to keep the Gitea repo in sync, so the
# Gitea Actions runner can build from it.
#
# Required env (set as GitHub secrets):
#   GITEA_URL      — e.g. https://repo.vreckanproject.com
#   GITEA_REPO     — e.g. vreckan/vreckan
#   GITEA_TOKEN    — a Gitea PAT with write access to the repo
#
# Optional env:
#   GITEA_TARGET_BRANCH — branch name to push on Gitea (defaults to the
#     current GitHub branch). Used for PRs: push to `pr/<number>` instead
#     of the feature branch name.
#
# The Gitea remote is added as `gitea` and the branch is force-pushed so the
# mirror always matches GitHub exactly.
set -euo pipefail

: "${GITEA_URL:?GITEA_URL is required}"
: "${GITEA_REPO:?GITEA_REPO is required}"
: "${GITEA_TOKEN:?GITEA_TOKEN is required}"

# Use GITEA_TARGET_BRANCH if set (e.g. "pr/42"), otherwise the current branch.
BRANCH="${GITEA_TARGET_BRANCH:-${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD)}}"

# Build the plain remote URL and an auth URL with the token embedded.
# Strip the scheme to get the host, then rebuild with credentials so the
# substitution is robust (a pattern-replace on the full URL is fragile).
HOST="${GITEA_URL#https://}"
REMOTE="https://${HOST}/$(echo "$GITEA_REPO" | sed 's#^/###').git"
AUTH_REMOTE="https://x-access-token:${GITEA_TOKEN}@${HOST}/$(echo "$GITEA_REPO" | sed 's#^/###').git"

git remote remove gitea 2>/dev/null || true
git remote add gitea "$AUTH_REMOTE"

echo "Mirroring -> $GITEA_URL/$GITEA_REPO (branch: $BRANCH)"
git push --force gitea "HEAD:refs/heads/$BRANCH"

# Strip the token from the remote so it isn't left in the repo config.
git remote set-url gitea "$REMOTE"
