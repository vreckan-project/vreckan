#!/usr/bin/env bash
# trigger-gitea.sh — dispatch a Gitea Actions workflow via the REST API.
#
# This is the "kick off the build on Gitea from GitHub" leg (the vLLM-style
# pattern). A lightweight GitHub workflow calls this; Gitea then runs the
# heavy build on its own runner, keeping GitHub Actions costs near zero.
#
# Endpoint (Gitea):
#   POST /repos/{owner}/{repo}/actions/workflows/{workflow_id}/dispatches
#   body: { "ref": "refs/heads/<branch>", "inputs": { ... } }
#
# Required env (set as GitHub secrets):
#   GITEA_URL      — e.g. https://repo.vreckanproject.com
#   GITEA_REPO     — e.g. vreckan/vreckan (the mirror repo, used as fallback)
#   GITEA_TOKEN    — a Gitea PAT with write access to the repo
#
# Optional env:
#   GITEA_WORKFLOW_REPO — repo where the workflow lives (default: GITEA_REPO).
#     The workflow repo and the mirror repo are different: workflows live in
#     vreckan/vreckan-workflow, code lives in vreckan/vreckan.
#   GITEA_WORKFLOW — workflow filename to dispatch (default: build-test.yml)
#   GITEA_REF      — ref to build (default: refs/heads/<current branch>)
#   GITEA_INPUTS   — JSON object of workflow inputs, e.g. '{"branch":"main","sha":"abc123"}'
set -euo pipefail

: "${GITEA_URL:?GITEA_URL is required}"
: "${GITEA_REPO:?GITEA_REPO is required}"
: "${GITEA_TOKEN:?GITEA_TOKEN is required}"

WORKFLOW="${GITEA_WORKFLOW:-build-test.yml}"
# The workflow lives in a different repo than the mirror.
WORKFLOW_REPO="${GITEA_WORKFLOW_REPO:-${GITEA_REPO}}"
BRANCH="${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD)}"
REF="${GITEA_REF:-refs/heads/${BRANCH}}"
INPUTS="${GITEA_INPUTS:-{}}"

# Build the JSON body. GITEA_INPUTS is a JSON object; merge it with the ref.
BODY="$(printf '{"ref":"%s","inputs":%s}' "$REF" "$INPUTS")"

URL="${GITEA_URL}/api/v1/repos/${WORKFLOW_REPO}/actions/workflows/${WORKFLOW}/dispatches"

echo "Dispatching Gitea workflow '$WORKFLOW' in ${WORKFLOW_REPO} on $REF"
HTTP_CODE="$(curl -sS -o /tmp/gitea-dispatch.out -w '%{http_code}' \
  -X POST \
  -H "Authorization: token ${GITEA_TOKEN}" \
  -H "Content-Type: application/json" \
  -d "$BODY" \
  "$URL")"

echo "Gitea responded: HTTP $HTTP_CODE"
cat /tmp/gitea-dispatch.out || true
echo

# 204 = accepted (no body), 200 = accepted with run details. Anything else is
# a failure.
case "$HTTP_CODE" in
  200|204) exit 0 ;;
  *) echo "ERROR: Gitea dispatch failed (HTTP $HTTP_CODE)" >&2; exit 1 ;;
esac
