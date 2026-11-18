#!/usr/bin/env bash
# trigger-gitea.sh — dispatch a Gitea Actions workflow via the REST API,
# verify that a run was actually created, and (by default) wait for that
# run to reach a terminal state — so the GitHub mirror leg fails loudly
# instead of no-opping silently (ANT-1241, 2026-11-18).
#
# This is the "kick off the build on Gitea from GitHub" leg (the vLLM-style
# pattern). A lightweight GitHub workflow calls this; Gitea then runs the
# heavy build on its own runner, keeping GitHub Actions costs near zero.
#
# Endpoint (Gitea):
#   POST /repos/{owner}/{repo}/actions/workflows/{workflow_id}/dispatches?return_run_details=true
#   body: { "ref": "refs/heads/<branch>", "inputs": { ... } }
#
# Hardening (ANT-1241):
#   * `return_run_details=true` makes the dispatch response itself carry the
#     created run id (HTTP 200 + JSON). In Gitea 28.1.0 the run row and its
#     jobs are inserted in a single transaction (services/actions/run.go
#     InsertRun, db.WithTx) that commits BEFORE the response is returned, so
#     a 200 with no usable run id already means the run was lost. We still
#     verify the id against the REST run endpoint (one GET) so a response/
#     DB divergence cannot slip through.
#   * Builds that answer 204 without run details (older Gitea versions
#     without return_run_details support) fall back to polling the REST run
#     list for a new run of this workflow (up to
#     GITEA_DISPATCH_CONFIRM_TIMEOUT, default 600s). No new run by the
#     deadline => non-zero exit.
#   * GITEA_WAIT selects how long the script keeps the caller (a GitHub
#     Actions runner) busy after the run exists:
#       completion (default) — poll until the run reaches a terminal state
#         within GITEA_RUN_TIMEOUT (default 5400s — the slowest observed
#         build is ~60 min), so the mirror run goes red when the Gitea run
#         itself stalls or fails. Use for the push-to-main leg, where the
#         GitHub run IS the release-critical signal.
#       started — poll until the run leaves waiting/queued (a job is on a
#         runner), then exit 0 and release the runner. Use for the PR leg:
#         it still fails loudly (a 204 with no run; the lost-transition
#         diagnosis below) but does not hold a GitHub runner for the full
#         30-60 min build, keeping the vLLM-style split cheap.
#       none — fire and forget after the existence check (the pre-ANT-1241
#         shape; kept for the release leg, where the GitHub Release must be
#         created immediately after the tag and the freshness run's
#         tag-without-image check is the failure backstop).
#     The realistic silent mode is the known lost-Actions-transition
#     (go-gitea/gitea#40279; the 2026-11-07 stall; tracked by ANT-1033):
#     the run row exists but no job is ever dispatched, so the run sits in
#     waiting/queued forever and no commit status is ever posted. A run that
#     has not left waiting/queued within GITEA_START_TIMEOUT (default 600s)
#     fails early with that diagnosis instead of burning the full timeout —
#     in both the completion and the started mode.
#   * The REST actions API is the GitHub-style route
#     /api/v1/repos/{o}/{r}/actions/runs[/{id}] — it works on the 28.1.0
#     deployment; the older /api/v1/actions/repos/… prefix 404s there.
#
# Required env (set as GitHub secrets):
#   GITEA_URL      — e.g. https://repo.vreckanproject.com
#   GITEA_REPO     — e.g. vreckan/vreckan (the mirror repo, used as fallback)
#   GITEA_TOKEN    — a Gitea PAT with write access to the repo
#
# Optional env:
#   GITEA_ADMIN_USER — set (e.g. "admin") to authenticate with HTTP Basic
#     (user:GITEA_TOKEN) instead of the PAT `token` scheme. Needed on
#     SSO-only Gitea instances where the `token` scheme 401s even with
#     valid credentials (documented degraded form on this deployment);
#     Basic auth works there. Unset = `Authorization: token $GITEA_TOKEN`
#     (the production default; unchanged behavior).
#   GITEA_WORKFLOW_REPO — repo where the workflow lives (default: GITEA_REPO).
#     The workflow repo and the mirror repo are different: workflows live in
#     vreckan/vreckan-workflow, code lives in vreckan/vreckan.
#   GITEA_WORKFLOW — workflow filename to dispatch (default: build-test.yml)
#   GITEA_REF      — ref to build (default: refs/heads/<current branch>)
#   GITEA_INPUTS   — JSON object of workflow inputs, e.g. '{"branch":"main","sha":"abc123"}'
#   GITEA_DISPATCH_CONFIRM_TIMEOUT — seconds to poll for run existence on the
#     204 fallback path (default 600)
#   GITEA_WAIT — what to wait for after the run exists (default completion).
#     completion = poll to a terminal state, so the caller mirrors the Gitea
#     run's outcome (use for the push legs, where the GitHub run is the
#     release-critical signal); started = exit 0 as soon as the run leaves
#     waiting/queued (use for the PR leg — the vLLM-style split exists to
#     keep GitHub runner time near zero, and the run's own commit statuses
#     are the PR's signal); none = fire and forget after the existence
#     check (release leg — the GitHub Release must be created immediately;
#     the freshness run's tag-without-image check is the failure backstop).
#   GITEA_RUN_TIMEOUT — seconds to wait for the run to reach a terminal
#     state (default 5400 = 90 min, well past the slowest observed build of
#     ~36-60 min)
#   GITEA_START_TIMEOUT — seconds a run may sit in waiting/queued with no
#     job on a runner and an idle pool before failing with the
#     lost-transition diagnosis (default 600)
#   GITEA_POLL_INTERVAL — seconds between status polls (default 30)
set -euo pipefail

: "${GITEA_URL:?GITEA_URL is required}"
: "${GITEA_REPO:?GITEA_REPO is required}"
: "${GITEA_TOKEN:?GITEA_TOKEN is required}"

# Auth form: default is the PAT scheme. GITEA_ADMIN_USER switches to HTTP
# Basic (user:GITEA_TOKEN) for SSO-only Gitea instances where the `token`
# scheme 401s with otherwise-valid credentials.
if [ -n "${GITEA_ADMIN_USER:-}" ]; then
  AUTH_HEADER="Basic $(printf '%s:%s' "$GITEA_ADMIN_USER" "$GITEA_TOKEN" | base64 | tr -d '\n')"
else
  AUTH_HEADER="token ${GITEA_TOKEN}"
fi

WORKFLOW="${GITEA_WORKFLOW:-build-test.yml}"
# The workflow lives in a different repo than the mirror.
WORKFLOW_REPO="${GITEA_WORKFLOW_REPO:-${GITEA_REPO}}"
BRANCH="${GITHUB_REF_NAME:-$(git rev-parse --abbrev-ref HEAD)}"
REF="${GITEA_REF:-refs/heads/${BRANCH}}"
# Prefer an explicit GITEA_INPUTS JSON object; otherwise build it here from the
# scalar TARGET_BRANCH / TARGET_SHA env vars. Building the JSON in one place,
# from plain scalars, avoids a stray '}' that leaked in when the JSON was
# assembled in the workflow YAML and passed across the process boundary.
if [ -n "${GITEA_INPUTS:-}" ]; then
  INPUTS="$GITEA_INPUTS"
else
  INPUTS="$(printf '{"branch":"%s","sha":"%s"}' "${TARGET_BRANCH:-main}" "${TARGET_SHA:-}")"
fi

DISPATCH_CONFIRM_TIMEOUT="${GITEA_DISPATCH_CONFIRM_TIMEOUT:-600}"
# completion | started | none (see the GITEA_WAIT docs above)
WAIT="${GITEA_WAIT:-completion}"
case "$WAIT" in
  completion|started|none) ;;
  *)
    echo "ERROR: GITEA_WAIT must be 'completion', 'started', or 'none' (got '$WAIT')" >&2
    exit 2
    ;;
esac
RUN_TIMEOUT="${GITEA_RUN_TIMEOUT:-5400}"
START_TIMEOUT="${GITEA_START_TIMEOUT:-600}"
POLL_INTERVAL="${GITEA_POLL_INTERVAL:-30}"

# Build the JSON body. GITEA_INPUTS is a JSON object; merge it with the ref.
BODY="$(printf '{"ref":"%s","inputs":%s}' "$REF" "$INPUTS")"

DISPATCH_URL="${GITEA_URL}/api/v1/repos/${WORKFLOW_REPO}/actions/workflows/${WORKFLOW}/dispatches?return_run_details=true"
RUN_URL="${GITEA_URL}/api/v1/repos/${WORKFLOW_REPO}/actions/runs"

gitea_get() {
  # GET one path, retrying transient connection-level failures within the
  # current poll tick so a single blip never fails the wait.
  local path="$1" out rc code attempt
  for attempt in 1 2 3; do
    if out="$(curl -sS --max-time 30 -o /tmp/gitea-get.out -w '%{http_code}' \
      -H "Authorization: ${AUTH_HEADER}" \
      -H "Accept: application/json" \
      "${GITEA_URL}${path}")"; then
      code="$out"
    else
      code="000"
    fi
    case "$code" in
      2*) return 0 ;;
      404) return 4 ;;
      *) rc=1; sleep 5 ;;
    esac
  done
  return "$rc"
}

now() { date +%s; }

elapsed_hms() {
  local s=$(( $(now) - $1 ))
  printf '%dm%02ds' $(( s / 60 )) $(( s % 60 ))
}

echo "Dispatching Gitea workflow '$WORKFLOW' in ${WORKFLOW_REPO} on $REF"
DISPATCH_EPOCH="$(now)"
HTTP_CODE="$(curl -sS -o /tmp/gitea-dispatch.out -w '%{http_code}' \
  -X POST \
  -H "Authorization: ${AUTH_HEADER}" \
  -H "Content-Type: application/json" \
  -d "$BODY" \
  "$DISPATCH_URL")"

echo "Gitea responded: HTTP $HTTP_CODE"
cat /tmp/gitea-dispatch.out || true
echo

RUN_ID=""
case "$HTTP_CODE" in
  200)
    # return_run_details: the body carries the created run id.
    RUN_ID="$(jq -r '.workflow_run_id // empty' /tmp/gitea-dispatch.out 2>/dev/null || true)"
    if [ -z "$RUN_ID" ] || [ "$RUN_ID" = "null" ]; then
      echo "ERROR: Gitea returned 200 (run details) but no usable workflow_run_id — the dispatch was accepted without a created run. Body:" >&2
      cat /tmp/gitea-dispatch.out >&2 || true
      exit 1
    fi
    ;;
  204)
    # Build without return_run_details support: accepted, no details.
    # Fall back to polling the run list for a run of this workflow created
    # at or after our dispatch (created_at is ISO-8601 with the server's
    # offset; GNU date parses both that and the epoch comparison).
    echo "No run details in the response (HTTP 204) — polling the run list for up to ${DISPATCH_CONFIRM_TIMEOUT}s to confirm a run was created"
    deadline=$(( $(now) + DISPATCH_CONFIRM_TIMEOUT ))
    while :; do
      RUN_ID=""
      if gitea_get "/api/v1/repos/${WORKFLOW_REPO}/actions/runs?limit=30"; then
        while IFS=$'\t' read -r id created; do
          [ -n "$id" ] || continue
          created_epoch="$(date -d "$created" +%s 2>/dev/null || echo 0)"
          if [ "$created_epoch" -ge "$DISPATCH_EPOCH" ]; then
            RUN_ID="$id"
            break
          fi
        done < <(jq -r '.workflow_runs[]? | select((.path // "" | startswith("'"$WORKFLOW"'@"))) | [.id, (.created_at // "")] | @tsv' /tmp/gitea-get.out 2>/dev/null)
      else
        echo "WARN: run-list poll failed (transient) — retrying"
      fi
      if [ -n "$RUN_ID" ]; then
        echo "Run $RUN_ID appeared in the run list — dispatch confirmed"
        break
      fi
      if [ "$(now)" -ge "$deadline" ]; then
        echo "ERROR: Gitea accepted the dispatch (HTTP 204) but no run for ${WORKFLOW} appeared within ${DISPATCH_CONFIRM_TIMEOUT}s — the dispatch silently no-opped" >&2
        exit 1
      fi
      sleep "$POLL_INTERVAL"
    done
    ;;
  401|403)
    echo "ERROR: Gitea rejected the dispatch credentials (HTTP $HTTP_CODE)" >&2
    if [ -z "${GITEA_ADMIN_USER:-}" ]; then
      echo "Hint: on SSO-only Gitea instances the 'token' scheme 401s even with valid credentials; set GITEA_ADMIN_USER (e.g. admin) to authenticate with HTTP Basic instead." >&2
    fi
    cat /tmp/gitea-dispatch.out >&2 || true
    exit 1
    ;;
  *)
    echo "ERROR: Gitea dispatch failed (HTTP $HTTP_CODE)" >&2
    cat /tmp/gitea-dispatch.out >&2 || true
    exit 1
    ;;
esac

# Verify the run id actually resolves (guards a response/DB divergence the
# transaction should make impossible, but the check is one cheap GET).
if ! gitea_get "/api/v1/repos/${WORKFLOW_REPO}/actions/runs/${RUN_ID}"; then
  echo "ERROR: dispatch reported run ${RUN_ID} but GET ${RUN_URL}/${RUN_ID} does not resolve it — the run was not created" >&2
  exit 1
fi

RUN_HTML="$(jq -r '.html_url // empty' /tmp/gitea-get.out 2>/dev/null || true)"
if [ -n "$RUN_HTML" ]; then
  echo "Gitea run ${RUN_ID} confirmed: ${RUN_HTML}"
else
  echo "Gitea run ${RUN_ID} confirmed"
fi

if [ "$WAIT" = "none" ]; then
  echo "GITEA_WAIT=none — not waiting for the run to finish (existence was verified above)"
  exit 0
fi

# Poll the run to a terminal state. Terminal in this API: status "completed"
# (conclusion success|failure|skipped|cancelled). Non-terminal: queued /
# waiting / in_progress / the run not (yet) resolvable.
#
# Two ways a non-terminal run can be sick, and how they are told apart:
#   * lost emit (go-gitea/gitea#40279): the run row exists but no job is
#     ever dispatched — the run stays waiting/queued with no job started
#     and no runner assigned. If the runner pool is IDLE at that point,
#     the run cannot be waiting for capacity, so fail early
#     (START_TIMEOUT) with the lost-transition diagnosis.
#   * runner capacity: the pool is busy (other runs in_progress) and the
#     run is queued behind them — that legitimately takes as long as the
#     slowest active run (observed up to ~40 min under a saturated pool),
#     so keep waiting up to RUN_TIMEOUT.
wait_start="$(now)"
deadline=$(( $(now) + RUN_TIMEOUT ))
last_status=""
ever_started=0
while :; do
  status=""
  conclusion=""
  rc=0
  gitea_get "/api/v1/repos/${WORKFLOW_REPO}/actions/runs/${RUN_ID}" || rc=$?
  if [ "$rc" -eq 0 ]; then
    status="$(jq -r '.status // empty' /tmp/gitea-get.out 2>/dev/null || true)"
    conclusion="$(jq -r '.conclusion // empty' /tmp/gitea-get.out 2>/dev/null || true)"
  fi
  case "${status:-}" in
    in_progress) ever_started=1 ;;
  esac

  case "${status:-}" in
    completed)
      echo "Gitea run ${RUN_ID} completed after $(elapsed_hms "$wait_start"): conclusion=${conclusion:-unknown} (${RUN_HTML:-$RUN_URL/$RUN_ID})"
      case "${conclusion:-}" in
        success|""|skipped|neutral) exit 0 ;;
        *)
          echo "ERROR: Gitea run ${RUN_ID} finished with conclusion '${conclusion}' — ${RUN_HTML:-$RUN_URL/$RUN_ID}" >&2
          exit 1
          ;;
      esac
      ;;
    waiting|queued|pending|requested|action_required)
      if [ "$(now)" -ge $(( wait_start + START_TIMEOUT )) ] && [ "$ever_started" = "0" ]; then
        # Distinguish lost emit from a capacity wait: is anything else in
        # this repo actively running (pool busy), or did a job of OUR run
        # get a runner? A busy pool or an assigned job means queued-for
        # capacity -> keep waiting; an idle pool with no started job is
        # the lost-transition signature -> fail now, not in 90 minutes.
        job_started=""
        gitea_get "/api/v1/repos/${WORKFLOW_REPO}/actions/runs/${RUN_ID}/jobs" || true
        job_started="$(jq -r '[.jobs[]? | select(.status == "in_progress" or .status == "completed" or .runner_name != null)] | length' /tmp/gitea-get.out 2>/dev/null || echo 0)"
        pool_busy=0
        gitea_get "/api/v1/repos/${WORKFLOW_REPO}/actions/runs?limit=50" || true
        pool_busy="$(jq -r --argjson self "$RUN_ID" '[.workflow_runs[]? | select(.id != $self and .status == "in_progress")] | length' /tmp/gitea-get.out 2>/dev/null || echo 0)"
        if [ "${job_started:-0}" -gt 0 ] 2>/dev/null; then
          echo "Gitea run ${RUN_ID}: a job is on a runner but the run has not transitioned yet — keeping the wait ($(elapsed_hms "$wait_start") elapsed)"
        elif [ "${pool_busy:-0}" -gt 0 ] 2>/dev/null; then
          echo "Gitea run ${RUN_ID} is still in '${status}' after ${START_TIMEOUT}s, but ${pool_busy} other run(s) are in_progress — queued for runner capacity, keeping the wait"
        else
          echo "ERROR: Gitea run ${RUN_ID} has stayed in '${status}' for ${START_TIMEOUT}s with no job started and the runner pool idle — no job was ever dispatched to a runner. Known failure mode: lost Actions transition (go-gitea/gitea#40279; see ANT-1033). Run: ${RUN_HTML:-$RUN_URL/$RUN_ID}" >&2
          exit 1
        fi
      fi
      ;;
    in_progress)
      if [ "$WAIT" = "started" ]; then
        # started mode: the run has left waiting/queued — the silent modes
        # (no run created; lost transition) are behind us. Release the
        # caller; the run's own commit statuses carry its outcome.
        echo "Gitea run ${RUN_ID} left waiting/queued (in_progress) after $(elapsed_hms "$wait_start") — GITEA_WAIT=started, not waiting for completion (${RUN_HTML:-$RUN_URL/$RUN_ID})"
        exit 0
      fi
      ;;
    *)
      : # unknown-but-live — keep polling
      ;;
  esac

  if [ "$(now)" -ge "$deadline" ]; then
    if [ "$ever_started" = "1" ]; then
      echo "ERROR: Gitea run ${RUN_ID} started but is still '${status:-unknown}' after ${RUN_TIMEOUT}s — the run may be stalled; check ${RUN_HTML:-$RUN_URL/$RUN_ID}" >&2
    else
      echo "ERROR: Gitea run ${RUN_ID} never started (status '${status:-unknown}') within ${RUN_TIMEOUT}s — either a lost Actions transition (go-gitea/gitea#40279) or a runner-capacity backlog; check ${RUN_HTML:-$RUN_URL/$RUN_ID}" >&2
    fi
    exit 1
  fi

  if [ "$status" != "$last_status" ]; then
    echo "Gitea run ${RUN_ID}: status=${status:-unknown} ($(elapsed_hms "$wait_start") elapsed)"
    last_status="$status"
  fi
  sleep "$POLL_INTERVAL"
done
