#!/usr/bin/env bash
# Test suite for scripts/gc-workspace-sessions.sh (ANT-965).
#
# Builds a local fixture — a bare "origin" plus one agent workspace holding a
# base repo and one worktree per scenario — serves a stub Paperclip API
# (issues + execution-workspaces both return []), runs the GC script for real
# against the fixture, and asserts the reclaim/keep verdicts from the
# manifest.
#
# Scenarios (worktrees are aged 2 days unless noted, past the 24 h grace):
#   stale     : branch deleted from origin, local tracking ref NOT pruned
#               -> RECLAIMED, "gone from remotes" (the ANT-965 fix: the stale
#               tracking ref must not short-circuit the ls-remote confirm)
#   unpushed  : branch never pushed, no tracking ref
#               -> RECLAIMED, "gone from remotes" (ANT-962/964 regression)
#   merged    : branch merged into main
#               -> RECLAIMED, "merged into main" (unchanged behavior)
#   live      : branch still present on origin
#               -> KEPT, "not merged/gone" (no false reclamation)
#   noremotes : tracking ref present, all remotes removed from config
#               -> KEPT (in doubt -> keep)
#   unreach   : tracking ref present, origin URL bogus (unreachable)
#               -> KEPT (in doubt -> keep; a remote that cannot be
#               confirmed against must not count as "confirmed absent")
#   doubt     : branch never pushed (no tracking ref), origin URL bogus
#               -> KEPT (in doubt -> keep; the pre-fix code reclaimed
#               here, since an unreachable remote left gone_confirmed=1)
#   young     : gone from origin but worktree mtime is fresh
#               -> KEPT, "young" (24 h grace)
#   active    : gone from origin but the worktree is the GC run's cwd
#               -> KEPT, "active" (active-run protection)

set -u -o pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$TESTS_DIR/.." && pwd)"
GC="$REPO_DIR/gc-workspace-sessions.sh"

if [[ ! -f "$GC" ]]; then
  echo "FAIL: $GC not found" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "FAIL: python3 is required for the stub API server" >&2
  exit 1
fi

PASS=0
FAIL=0
pass() { echo "ok   - $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL - $1" >&2; FAIL=$((FAIL + 1)); }

FIXTURE="${GC_TEST_FIXTURE:-$(mktemp -d "${TMPDIR:-/tmp}/gc-workspace-sessions-test.XXXXXX")}"
cleanup() {
  # shellcheck disable=SC2317  # body runs from the EXIT trap
  [[ -n "${FAKE_PID:-}" ]] && kill "$FAKE_PID" 2>/dev/null
  # shellcheck disable=SC2317
  [[ "${GC_TEST_KEEP_FIXTURE:-0}" == 1 ]] || rm -rf "$FIXTURE"
}
trap cleanup EXIT

# ------------------------------------------------------------------- fixture --
WSROOT="$FIXTURE/workspaces/bobara"
ORIGINS="$FIXTURE/origins"
SCRATCH="$FIXTURE/scratch"
MANIFESTS="$FIXTURE/manifests"
PORT_FILE="$FIXTURE/api-port"
FAKE_API="$FIXTURE/fake-api.py"
FAKE_PID=""

mkdir -p "$ORIGINS" "$SCRATCH" "$MANIFESTS" "$WSROOT"

cat > "$FAKE_API" <<'PY'
import http.server
import sys

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"[]"
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

port_file = sys.argv[1]
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
PY

# Each scenario gets its own base repo + bare origin so remote-config
# mutations (set-url / remote remove) cannot leak between scenarios.
# mk_base <name> creates $WSROOT/<name> (base; remote "origin" ->
# $ORIGINS/<name>; main pushed) and prints the base path.
mk_base() {
  local name="$1" base="$WSROOT/$1" origin="$ORIGINS/$1"
  git init -q --bare "$origin"
  git init -q -b main "$base"
  git -C "$base" config user.name gc-test
  git -C "$base" config user.email gc-test@vreckan.local
  git -C "$base" remote add origin "$origin"
  echo "$name" > "$base/base.txt"
  git -C "$base" add -A
  git -C "$base" commit -qm seed
  git -C "$base" push -q origin main >/dev/null
  echo "$base"
}

# add_wt <base> <wt-name> <branch> [start-point] — prints the worktree path
add_wt() {
  local base="$1" name="$2" branch="$3" start="${4:-main}"
  git -C "$base" worktree add -q -b "$branch" "$base/$name" "$start" >/dev/null
  echo "$base/$name"
}

commit_in() { # $1 = dir, $2 = file
  echo "$1 content" > "$1/$2"
  git -C "$1" add -A
  git -C "$1" commit -qm "$2"
}

push_b() { git -C "$1" push -q origin "$2" >/dev/null; }
# Delete the branch ON the bare origin without touching the base's tracking
# ref: `git push --delete` from the base would prune it, which is NOT the
# scenario under test (remote-side deletion, e.g. a squash-merge PR, with
# no `git fetch --prune` since).
del_b() {
  local origin
  origin="$(git -C "$1" config --get remote.origin.url)"
  git --git-dir="$origin" update-ref -d "refs/heads/$2"
}

# Age dirs 2 days back (past the 24 h grace). Must run after all in-tree git
# work: any file creation would bump the dir mtime again.
age_out() {
  local d
  for d in "$@"; do touch -d "2 days ago" "$d"; done
}

# --- scenario: stale (deleted on origin, tracking ref left behind) ---
S="$(mk_base stale)"; WT_STALE="$(add_wt "$S" stale stale-branch)"
commit_in "$WT_STALE" stale.txt
push_b "$S" stale-branch
del_b "$S" stale-branch
# NOTE: no `git fetch --prune` — refs/remotes/origin/stale-branch stays.
# Uncommitted work: exercises salvage-before-delete on a reclaimed worktree
# (must stay in front of the age_out batch, which backdates the dir mtime).
echo dirty > "$WT_STALE/dirty.txt"

# --- scenario: unpushed (never pushed, no tracking ref at all) ---
S="$(mk_base unpushed)"; WT_UNPUSHED="$(add_wt "$S" unpushed unpushed-branch)"
commit_in "$WT_UNPUSHED" unpushed.txt

# --- scenario: merged (fast-forwarded into main) ---
S="$(mk_base merged)"; WT_MERGED="$(add_wt "$S" merged merged-branch)"
commit_in "$WT_MERGED" merged.txt
push_b "$S" merged-branch
git -C "$S" merge -q --ff-only merged-branch
push_b "$S" main

# --- scenario: live (still present on origin) ---
S="$(mk_base live)"; WT_LIVE="$(add_wt "$S" live live-branch)"
commit_in "$WT_LIVE" live.txt
push_b "$S" live-branch

# --- scenario: noremotes (tracking ref present, remotes removed) ---
S="$(mk_base noremotes)"; WT_NOREMOTES="$(add_wt "$S" noremotes noremotes-branch)"
commit_in "$WT_NOREMOTES" noremotes.txt
push_b "$S" noremotes-branch
git -C "$S" remote remove origin

# --- scenario: unreach (tracking ref present, origin URL bogus) ---
S="$(mk_base unreach)"; WT_UNREACH="$(add_wt "$S" unreach unreach-branch)"
commit_in "$WT_UNREACH" unreach.txt
push_b "$S" unreach-branch
git -C "$S" remote set-url origin "$ORIGINS/no-such-remote"

# --- scenario: doubt (never pushed, no tracking ref, origin unreachable) ---
# The in-doubt case where the pre-fix code reclaimed: with no tracking ref
# the confirmation loop ran, found the only remote unreachable, and set
# gone_confirmed=1 anyway -> reclaim. Now unreachable means "in doubt" ->
# keep (consistent with the script's "when in doubt, keep" model).
S="$(mk_base doubt)"; WT_DOUBT="$(add_wt "$S" doubt doubt-branch)"
commit_in "$WT_DOUBT" doubt.txt
git -C "$S" remote set-url origin "$ORIGINS/no-such-remote-2"

# --- scenario: young (gone from origin, but worktree mtime is fresh) ---
S="$(mk_base young)"; WT_YOUNG="$(add_wt "$S" young young-branch)"
commit_in "$WT_YOUNG" young.txt
push_b "$S" young-branch
del_b "$S" young-branch

# --- scenario: active (gone from origin, but is the GC run's cwd) ---
S="$(mk_base active)"; WT_ACTIVE="$(add_wt "$S" active active-branch)"
commit_in "$WT_ACTIVE" active.txt
push_b "$S" active-branch
del_b "$S" active-branch

age_out "$WT_STALE" "$WT_UNPUSHED" "$WT_MERGED" "$WT_LIVE" \
  "$WT_NOREMOTES" "$WT_UNREACH" "$WT_DOUBT" "$WT_ACTIVE"
# $WT_YOUNG intentionally keeps its fresh mtime.

# --- stub API server ---
python3 "$FAKE_API" "$PORT_FILE" &
FAKE_PID=$!
for _ in $(seq 1 50); do
  [[ -s "$PORT_FILE" ]] && break
  sleep 0.1
done
if [[ ! -s "$PORT_FILE" ]]; then
  echo "FAIL: stub API server did not start" >&2
  exit 1
fi
API_PORT="$(cat "$PORT_FILE")"

# ------------------------------------------------------------------- run GC --
SUMMARY="$(
  PAPERCLIP_API_URL="http://127.0.0.1:${API_PORT}/api" \
  PAPERCLIP_API_KEY=test \
  PAPERCLIP_COMPANY_ID=test \
  PAPERCLIP_RUN_ID=gc-test-run \
    bash "$GC" \
    --workspace-root "$FIXTURE/workspaces" \
    --scratch-root "$SCRATCH" \
    --salvage-dir "$SCRATCH/salvage" \
    --manifest-dir "$MANIFESTS" \
    --cwd "$WT_ACTIVE" \
    2>"$FIXTURE/gc-stderr"
)" || {
  echo "FAIL: gc script exited non-zero:" >&2
  cat "$FIXTURE/gc-stderr" >&2
  exit 1
}
MANIFEST="$(grep '^manifest: ' <<<"$SUMMARY" | sed 's/^manifest: //')"
if [[ ! -f "$MANIFEST" ]]; then
  echo "FAIL: manifest not found at $MANIFEST" >&2
  echo "$SUMMARY" >&2
  exit 1
fi

reclaimed_rows() { awk '/^## Reclaimed/,/^## Kept/' "$MANIFEST"; }
kept_rows() { awk '/^## Kept/,/^## Salvage/' "$MANIFEST"; }

# expect_reclaimed <name> <worktree-dir> <reason-substring>. The manifest
# greps on the unique worktree basename (robust to symlinked TMPDIR
# prefixes); the on-disk check uses the full dir.
expect_reclaimed() {
  local name="$1" dir="$2" why="$3" row
  row="$(reclaimed_rows | grep -F "/${name}\`" || true)"
  if [[ -n "$row" && "$row" == *"$why"* && ! -d "$dir" ]]; then
    pass "${name}: reclaimed (${why}), directory removed"
  else
    [[ -n "$row" ]] || row="<absent from Reclaimed section>"
    fail "${name}: expected RECLAIMED (${why}); manifest row: ${row}"
  fi
}

expect_kept() {
  local name="$1" dir="$2" why="$3" row
  row="$(kept_rows | grep -F "/${name}\`" || true)"
  if [[ -n "$row" && "$row" == *"$why"* && -d "$dir" ]]; then
    pass "${name}: kept (${why}), directory intact"
  else
    [[ -n "$row" ]] || row="<absent from Kept section>"
    fail "${name}: expected KEPT (${why}); manifest row: ${row}"
  fi
}

# ------------------------------------------------------------------ asserts --
expect_reclaimed stale     "$WT_STALE"     "gone from remotes"
expect_reclaimed unpushed  "$WT_UNPUSHED"  "gone from remotes"
expect_reclaimed merged    "$WT_MERGED"    "merged into main"
expect_kept      live      "$WT_LIVE"      "not merged/gone"
expect_kept      unreach   "$WT_UNREACH"   "not merged/gone"
expect_kept      doubt     "$WT_DOUBT"     "not merged/gone"
expect_kept      noremotes "$WT_NOREMOTES" "not merged/gone"
expect_kept      young     "$WT_YOUNG"     "young"
expect_kept      active    "$WT_ACTIVE"    "active"

# Salvage-before-delete: the reclaimed stale worktree had uncommitted work;
# the manifest's Salvage section must list the patch and it must exist.
SALVAGE_LISTED="$(awk '/^## Salvage artifacts/,/^## Totals/' "$MANIFEST" | grep -c '^- /' || true)"
if [[ "$SALVAGE_LISTED" -ge 1 ]] && ls "$SCRATCH/salvage"/*.patch >/dev/null 2>&1; then
  pass "salvage-before-delete: patch listed in manifest and written to $SCRATCH/salvage"
else
  fail "salvage-before-delete: expected a salvage patch for the reclaimed stale worktree (listed=${SALVAGE_LISTED})"
fi

# The stub API must have been reachable (otherwise every merged/gone
# candidate is kept "when in doubt" and the reclaimed asserts above would
# already have failed — make it explicit for the manifest narrative).
if grep -q 'API signals: ok' "$MANIFEST"; then
  pass "API signals: ok (stub served both endpoints)"
else
  fail "API signals not ok: $(grep 'API signals' "$MANIFEST" || echo 'missing')"
fi

echo
echo "test-gc-workspace-sessions: ${PASS} passed, ${FAIL} failed"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
