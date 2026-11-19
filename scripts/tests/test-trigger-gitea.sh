#!/usr/bin/env bash
# Test suite for scripts/trigger-gitea.sh (ANT-1241).
#
# Stands up the stub Gitea actions API (stub-gitea-server.py, one instance
# per case) and runs the dispatch script for real against it. Cases:
#   t200-success       200 + run details, run completes success   -> exit 0
#   t200-no-id         200 but no workflow_run_id in the body     -> exit 1
#   t500               dispatch rejected (HTTP 500)               -> exit 1
#   t204-appears       204 (no details); run shows up in the      -> exit 0
#                      list within the confirm window
#   t204-never         204 (no details); no run ever appears      -> exit 1
#                      within GITEA_DISPATCH_CONFIRM_TIMEOUT
#   t-stall-idle       run created but stuck in 'waiting', no job -> exit 1 early
#                      on a runner, runner pool idle (lost-      (GITEA_START_TIMEOUT,
#                      transition signature, #40279)              not RUN_TIMEOUT)
#   t-stall-busy       same stall, but another run is in_progress -> exit 1 at
#                      (runner-capacity wait)                     GITEA_RUN_TIMEOUT,
#                                                                not early
#   t-stall-assigned   same stall, a job of our run is on a       -> exit 1 at
#                      runner (transition in flight)              GITEA_RUN_TIMEOUT
#   t-failure          run goes in_progress -> completed/failure  -> exit 1
#   t-wait0            GITEA_WAIT=none, run stalls                 -> exit 0
#                      (existence verified, fire and forget)
#   t-started          GITEA_WAIT=started; the run starts but     -> exit 0 the
#                      never completes — the script must not       moment the run
#                      hold the wait for the run's full lifetime   leaves
#                                                                  waiting/queued
#   t-event-derive     GITEA_WAIT unset (the workflow's case);    -> exit 0,
#                      the runner's GITHUB_EVENT_NAME=             and the mode
#                      pull_request selects started                 must be the
#                                                                  derived one
#   t-auth-basic       GITEA_ADMIN_USER switches auth to HTTP      -> exit 0
#                      Basic (stub verifies the Authorization      (Basic header
#                      header is exactly Basic admin:stub-pass)    on dispatch)
#   t-auth-wrong       token scheme, wrong token (stub expects a  -> exit 1
#                      different one) — dispatch 401, loud error   (401 + hint)
set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TRIGGER="$REPO_ROOT/scripts/trigger-gitea.sh"
STUB="$SCRIPT_DIR/stub-gitea-server.py"
PASS=0
FAIL=0

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

# run_case <name> <expected_rc> <output-must-contain> <env pairs...> -- <script env pairs...>
run_case() {
  local name="$1" expected_rc="$2" expect_text="$3"
  shift 3
  local -a stub_env=() script_env=()
  local saw_ddash=0
  for a in "$@"; do
    if [ "$a" = "--" ]; then saw_ddash=1; continue; fi
    if [ "$saw_ddash" = 0 ]; then stub_env+=("$a"); else script_env+=("$a"); fi
  done

  local port out rc started
  port="$(free_port)"
  # env(1) parses NAME=VALUE args; a bare assignment list would not, because
  # bash only treats LITERAL words (not array expansions) as assignments.
  env STUB_PORT="$port" "${stub_env[@]}" python3 "$STUB" &
  local srv=$!
  sleep 0.5
  started="$(date +%s)"

  out="$(cd "$REPO_ROOT" \
    && env GITEA_URL="http://127.0.0.1:$port" GITEA_REPO=vreckan/vreckan GITEA_TOKEN=stub-token \
      GITEA_WORKFLOW_REPO=vreckan/vreckan-workflow GITEA_WORKFLOW=build-test-rust.yml GITEA_REF=refs/heads/main \
      "${script_env[@]}" bash "$TRIGGER" 2>&1)"
  rc=$?

  kill "$srv" 2>/dev/null
  wait "$srv" 2>/dev/null
  local took=$(( $(date +%s) - started ))

  local ok=1
  [ "$rc" -eq "$expected_rc" ] || { ok=0; echo "    rc=$rc expected=$expected_rc"; }
  if [ -n "$expect_text" ]; then
    printf '%s' "$out" | grep -qF "$expect_text" || { ok=0; echo "    output missing: $expect_text"; }
  fi
  if [ "$ok" = 1 ]; then
    PASS=$((PASS + 1))
    echo "PASS $name (${took}s, rc=$rc)"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL $name"
    [ "$rc" -ne "$expected_rc" ] && echo "    rc=$rc expected=$expected_rc"
    [ -n "$expect_text" ] && ! printf '%s' "$out" | grep -qF "$expect_text" && echo "    output missing: $expect_text"
    echo "    --- script output ---"
    printf '%s\n' "$out" | sed 's/^/    /'
  fi
}

# -- t200-success: 200 with run details, run completes successfully.
run_case t200-success 0 "completed" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=success STUB_JOBS=unassigned \
  -- GITEA_WAIT=completion GITEA_RUN_TIMEOUT=30 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t200-no-id: 200 but the body carries no workflow_run_id.
run_case t200-no-id 1 "no usable workflow_run_id" \
  STUB_DISPATCH=no-details-200 STUB_LIST=appears STUB_RUN=success STUB_JOBS=unassigned \
  -- GITEA_WAIT=completion GITEA_POLL_INTERVAL=1

# -- t500: the dispatch itself is rejected.
run_case t500 1 "dispatch failed (HTTP 500)" \
  STUB_DISPATCH=fail-500 \
  -- GITEA_WAIT=completion GITEA_POLL_INTERVAL=1

# -- t204-appears: build without return_run_details support; the run shows up
# in the run list after the 204.
run_case t204-appears 0 "dispatch confirmed" \
  STUB_DISPATCH=ok-204 STUB_LIST=appears STUB_RUN=success STUB_JOBS=unassigned \
  -- GITEA_DISPATCH_CONFIRM_TIMEOUT=15 GITEA_WAIT=completion GITEA_RUN_TIMEOUT=30 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t204-never: accepted (204) but no run ever appears -> the silent no-op
# this issue exists to catch.
run_case t204-never 1 "silently no-opped" \
  STUB_DISPATCH=ok-204 STUB_LIST=never \
  -- GITEA_DISPATCH_CONFIRM_TIMEOUT=3 GITEA_WAIT=none GITEA_POLL_INTERVAL=1

# -- t-stall-idle: run created, stuck in waiting, no job on a runner, pool
# idle -> early failure with the lost-transition diagnosis.
run_case t-stall-idle 1 "lost Actions transition" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=stall STUB_JOBS=unassigned \
  -- GITEA_WAIT=completion GITEA_RUN_TIMEOUT=60 GITEA_START_TIMEOUT=3 GITEA_POLL_INTERVAL=1

# -- t-stall-busy: same stall, but the pool is busy -> legitimate capacity
# wait; must outlive START_TIMEOUT and fail at RUN_TIMEOUT instead.
run_case t-stall-busy 1 "never started" \
  STUB_DISPATCH=ok-200 STUB_LIST=busy-pool STUB_RUN=stall STUB_JOBS=unassigned \
  -- GITEA_WAIT=completion GITEA_RUN_TIMEOUT=6 GITEA_START_TIMEOUT=3 GITEA_POLL_INTERVAL=1

# -- t-stall-assigned: same stall, but a job of our run already has a runner
# (transition in flight) -> keep waiting until RUN_TIMEOUT.
run_case t-stall-assigned 1 "never started" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=stall STUB_JOBS=assigned \
  -- GITEA_WAIT=completion GITEA_RUN_TIMEOUT=6 GITEA_START_TIMEOUT=3 GITEA_POLL_INTERVAL=1

# -- t-failure: run starts then completes with a failure conclusion.
run_case t-failure 1 "conclusion 'failure'" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=failure STUB_JOBS=unassigned \
  -- GITEA_WAIT=completion GITEA_RUN_TIMEOUT=30 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t-wait0: fire-and-forget (existence still verified).
run_case t-wait0 0 "not waiting for the run to finish" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=stall STUB_JOBS=unassigned \
  -- GITEA_WAIT=none GITEA_POLL_INTERVAL=1

# -- t-started: the PR-leg mode. The run leaves waiting/queued but never
# completes (a real build would outlive this case's RUN_TIMEOUT by far):
# the script must exit 0 the moment the run starts, not hold the caller to
# GITEA_RUN_TIMEOUT.
run_case t-started 0 "GITEA_WAIT=started" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=started STUB_JOBS=unassigned \
  -- GITEA_WAIT=started GITEA_RUN_TIMEOUT=15 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t-event-derive: the workflow leaves GITEA_WAIT empty; the mode must
# come from the runner's GITHUB_EVENT_NAME (PR leg -> started). Same stub
# shape as t-started (run starts, never completes): a wrong derived mode
# (completion) would time out instead of exiting 0.
run_case t-event-derive 0 "derived 'started' from GITHUB_EVENT_NAME=pull_request" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=started STUB_JOBS=unassigned \
  -- GITHUB_EVENT_NAME=pull_request GITEA_RUN_TIMEOUT=15 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t-auth-basic: GITEA_ADMIN_USER switches auth to HTTP Basic; the stub
# verifies the dispatch Authorization header is exactly Basic admin:stub-token.
run_case t-auth-basic 0 "completed" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=success STUB_JOBS=unassigned \
  STUB_AUTH_USER=admin STUB_AUTH_PASS=stub-token \
  -- GITEA_ADMIN_USER=admin GITEA_WAIT=completion GITEA_RUN_TIMEOUT=30 GITEA_START_TIMEOUT=10 GITEA_POLL_INTERVAL=1

# -- t-auth-wrong: token scheme with the wrong token (stub expects a
# different one) -> 401 on the dispatch; must fail loudly with the auth hint.
run_case t-auth-wrong 1 "rejected the dispatch credentials" \
  STUB_DISPATCH=ok-200 STUB_LIST=appears STUB_RUN=success STUB_JOBS=unassigned \
  STUB_AUTH_TOKEN=expected-token \
  -- GITEA_WAIT=completion GITEA_POLL_INTERVAL=1

echo
echo "results: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
