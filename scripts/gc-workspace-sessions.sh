#!/usr/bin/env bash
# gc-workspace-sessions.sh — standing GC for Paperclip agent workspaces (ANT-716 / ANT-717).
#
# Reclaims, daily and idempotently:
#   1. Stale per-session clones:
#        <workspace>/.paperclip-runtime/detached-repositories/<uuid>/
#      older than --ttl-days (mtime) that are not the workspace of an active run.
#   2. Orphaned worktrees: worktrees whose `.git` pointer is broken (its base
#      repo is gone) or whose branch is merged into main / gone from the
#      remotes — under agent workspaces and under the scratch root.
#
# Safety model (hard constraints from ANT-716):
#   - The current run's own workspace (--cwd) is never reclaimed.
#   - Active runs/checkouts are detected via the Paperclip API (issue run ids,
#     execution-workspace cwds). If the API is unreachable, candidates are
#     KEPT ("when in doubt, keep") and the manifest says so.
#   - Anything with uncommitted work is salvaged first: `git diff` + untracked
#     files go to a patch under --salvage-dir; the patch path is listed in the
#     manifest.
#
# Output: a markdown manifest under --manifest-dir (durable) plus a short
# machine-readable summary on stdout. Exit 0 on a completed sweep (even a
# no-op), 1 on fatal errors.

set -u -o pipefail

# ---------------------------------------------------------------- defaults --
WORKSPACE_ROOT="/paperclip/workspaces"   # agent workspaces: <root>/<agent>/<ws>
SCRATCH_ROOT="/paperclip/tmp"            # scratch root scanned for orphans
TTL_DAYS=7                               # session-clone age threshold
GRACE_HOURS=24                           # min age before reclaiming merged/gone worktrees
SALVAGE_DIR="$SCRATCH_ROOT/ant-716-salvage"
MANIFEST_DIR="$SCRATCH_ROOT/ant-716-manifest"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
MANIFEST_NAME="gc-ant716-${STAMP}"
API_URL="${PAPERCLIP_API_URL:-}"
API_KEY="${PAPERCLIP_API_KEY:-}"
COMPANY_ID="${PAPERCLIP_COMPANY_ID:-}"
RUN_ID="${PAPERCLIP_RUN_ID:-}"
CURRENT_CWD="$(pwd -P)"
DRY_RUN=0
SCAN_MAXDEPTH=4                          # how deep to scan for .git pointer files

usage() {
  cat <<'EOF'
Usage: gc-workspace-sessions.sh [options]

Options:
  --workspace-root DIR  agent-workspace root to sweep (default /paperclip/workspaces)
  --scratch-root DIR    scratch root scanned for orphaned worktrees (default /paperclip/tmp)
  --ttl-days N          session-clone age threshold in days (default 7)
  --salvage-dir DIR     where salvage patches are written (default <scratch>/ant-716-salvage)
  --manifest-dir DIR    where the manifest is written (default <scratch>/ant-716-manifest)
  --api-url URL         Paperclip API URL (default $PAPERCLIP_API_URL)
  --api-key KEY         Paperclip API key (default $PAPERCLIP_API_KEY)
  --company-id ID       company id (default $PAPERCLIP_COMPANY_ID)
  --run-id ID           current run id (default $PAPERCLIP_RUN_ID)
  --cwd PATH            current run's workspace cwd (default $PWD)
  --dry-run             report what would be reclaimed without deleting
  -h, --help            this help
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --workspace-root) WORKSPACE_ROOT="$2"; shift 2 ;;
    --scratch-root)   SCRATCH_ROOT="$2"; shift 2 ;;
    --ttl-days)       TTL_DAYS="$2"; shift 2 ;;
    --salvage-dir)    SALVAGE_DIR="$2"; shift 2 ;;
    --manifest-dir)   MANIFEST_DIR="$2"; shift 2 ;;
    --api-url)        API_URL="$2"; shift 2 ;;
    --api-key)        API_KEY="$2"; shift 2 ;;
    --company-id)     COMPANY_ID="$2"; shift 2 ;;
    --run-id)         RUN_ID="$2"; shift 2 ;;
    --cwd)            CURRENT_CWD="$(realpath -m "$2")"; shift 2 ;;
    --dry-run)        DRY_RUN=1; shift ;;
    -h|--help)        usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage; exit 1 ;;
  esac
done

# ------------------------------------------------------------------ state --
NOW="$(date +%s)"
SCRATCH_ROOT="$(realpath -m "$SCRATCH_ROOT")"
SALVAGE_DIR="$(realpath -m "$SALVAGE_DIR")"
MANIFEST_DIR="$(realpath -m "$MANIFEST_DIR")"
WORKSPACE_ROOT="$(realpath -m "$WORKSPACE_ROOT")"
CURRENT_CWD="$(realpath -m "$CURRENT_CWD")"
MANIFEST="$MANIFEST_DIR/${MANIFEST_NAME}.md"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

RECLAIMED_ROWS=""      # markdown table rows: reclaimed items
KEPT_ROWS=""           # markdown table rows: kept items
BYTES_FREED=0
SALVAGE_FILE="$TMP/salvage-list"   # newline list of salvage artifact paths
touch "$SALVAGE_FILE"
API_OK=0
API_NOTE="not attempted"
RECLAIM_COUNT=0
KEEP_COUNT=0
ERROR_COUNT=0

ACTIVE_IDS="$TMP/active-ids"        # run ids / workspace ids of active issues
ACTIVE_CWDS="$TMP/active-cwds"      # realpath'd cwds of active execution workspaces
touch "$ACTIVE_IDS" "$ACTIVE_CWDS" "$TMP/judged-rows"

REGEN_EXCLUDES="target node_modules dist build .next .nuxt .venv venv __pycache__ .pytest_cache .mypy_cache .ruff_cache .cache coverage vendor out .turbo"

log() { echo "[gc-ant716] $*" >&2; }

add_reclaimed() { # path size age reason salvage
  local path="$1" size="$2" age="$3" reason="$4" salvage="${5:-}"
  grep -qxF "${path}|reclaimed" "$TMP/judged-rows" 2>/dev/null && return 0
  echo "${path}|reclaimed" >> "$TMP/judged-rows"
  RECLAIMED_ROWS="${RECLAIMED_ROWS}| \`${path}\` | ${size} | ${age} | ${reason} | ${salvage} |\n"
  RECLAIM_COUNT=$((RECLAIM_COUNT + 1))
}

add_kept() { # path size age reason
  local path="$1" size="$2" age="$3" reason="$4"
  grep -qxF "${path}|kept" "$TMP/judged-rows" 2>/dev/null && return 0
  echo "${path}|kept" >> "$TMP/judged-rows"
  KEPT_ROWS="${KEPT_ROWS}| \`${path}\` | ${size} | ${age} | ${reason} |\n"
  KEEP_COUNT=$((KEEP_COUNT + 1))
}

add_salvage() { # path
  echo "$1" >> "$SALVAGE_FILE"
}

# ------------------------------------------- active-signal collection (API) --
collect_api_signals() {
  if [[ -z "$API_URL" || -z "$API_KEY" || -z "$COMPANY_ID" ]]; then
    API_NOTE="skipped (env vars not set)"
    return 0
  fi
  local base="${API_URL%/}"; base="${base%/api}"
  local ok=0
  if curl -sS --max-time 60 -H "Authorization: Bearer $API_KEY" \
      "$base/api/companies/$COMPANY_ID/issues?status=in_progress,in_review&limit=500" \
      -o "$TMP/issues.json" 2>"$TMP/issues.err"; then
    if jq -e 'type == "array"' "$TMP/issues.json" >/dev/null 2>&1; then
      jq -r '.[] | [ (.checkoutRunId // ""), (.executionRunId // ""), (.executionWorkspaceId // ""), ((.activeRun // {}).id // "") ] | .[] | select(. != "")' \
        "$TMP/issues.json" >> "$ACTIVE_IDS" 2>/dev/null || true
      ok=1
    else
      API_NOTE="issues endpoint returned non-array response"
    fi
  else
    API_NOTE="issues fetch failed: $(head -c 120 "$TMP/issues.err" 2>/dev/null)"
  fi
  if curl -sS --max-time 120 -H "Authorization: Bearer $API_KEY" \
      "$base/api/companies/$COMPANY_ID/execution-workspaces?limit=2000" \
      -o "$TMP/ws.json" 2>"$TMP/ws.err"; then
    if jq -e 'type == "array"' "$TMP/ws.json" >/dev/null 2>&1; then
      jq -r '.[] | select((.status // "") == "active") | (.cwd // "")' "$TMP/ws.json" 2>/dev/null \
        | while IFS= read -r c; do
            [[ -n "$c" ]] && realpath -m "$c"
          done >> "$ACTIVE_CWDS"
      ok=1
    else
      API_NOTE="${API_NOTE}; workspaces endpoint returned non-array response"
    fi
  else
    API_NOTE="${API_NOTE}; workspaces fetch failed: $(head -c 120 "$TMP/ws.err" 2>/dev/null)"
  fi
  if [[ -n "$RUN_ID" ]]; then
    echo "$RUN_ID" >> "$ACTIVE_IDS"
  fi
  echo "$CURRENT_CWD" >> "$ACTIVE_CWDS"
  sort -u "$ACTIVE_IDS" -o "$ACTIVE_IDS"
  sort -u "$ACTIVE_CWDS" -o "$ACTIVE_CWDS"
  if [[ "$ok" -eq 1 ]]; then
    API_OK=1
    API_NOTE="ok ($(wc -l < "$ACTIVE_IDS") active ids, $(wc -l < "$ACTIVE_CWDS") active cwds)"
  fi
  return 0
}

# Is $1 (a realpath'd path) the current run's workspace or tied to an active
# run/workspace id? Returns 0 if active. $2 = "session" or "worktree":
# a session is the current run's workspace only when it IS the cwd (a run is
# provisioned with the session dir as its cwd); a worktree is protected when
# the cwd is inside it (the run works inside the worktree).
is_active_path() {
  local p="$1" kind="${2:-worktree}" name c
  name="$(basename "$p")"
  if [[ "$kind" == "session" ]]; then
    [[ "$p" == "$CURRENT_CWD" ]] && return 0
  else
    if [[ "$p" == "$CURRENT_CWD" || "$CURRENT_CWD" == "$p"/* || "$p" == "$CURRENT_CWD"/* ]]; then
      return 0
    fi
  fi
  if grep -qxF "$name" "$ACTIVE_IDS"; then
    return 0
  fi
  if grep -qxF "$p" "$ACTIVE_CWDS"; then
    return 0
  fi
  while IFS= read -r c; do
    if [[ "$c" == "$p"/* ]]; then
      return 0
    fi
  done < "$ACTIVE_CWDS"
  return 1
}

# Salvage uncommitted work in git dir $1 (label $2). Records artifacts via
# add_salvage. Returns 0 if anything was salvaged or the tree was clean.
salvage_git_dir() {
  local d="$1" label="$2"
  local tag patch untr st
  tag="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '_')"
  patch="$SALVAGE_DIR/${STAMP}-${tag}.patch"
  if [[ ! -e "$d/.git" ]]; then
    return 1
  fi
  if ! git -C "$d" rev-parse --git-dir >/dev/null 2>&1; then
    return 1
  fi
  st="$(git -C "$d" status --porcelain 2>/dev/null || true)"
  if [[ -z "$st" ]]; then
    return 1
  fi
  mkdir -p "$SALVAGE_DIR"
  (
    cd "$d" || exit 1
    git add -A --intent-to-add >/dev/null 2>&1
    git diff --binary HEAD > "$patch"
  ) || true
  untr="$(git -C "$d" ls-files --others --exclude-standard 2>/dev/null || true)"
  if [[ -n "$untr" ]]; then
    printf '%s\n' "$untr" > "$patch.untracked.txt"
  fi
  if [[ -s "$patch" ]]; then
    add_salvage "$patch"
    return 0
  fi
  if [[ -s "$patch.untracked.txt" ]]; then
    add_salvage "$patch.untracked.txt"
    return 0
  fi
  rm -f "$patch" "$patch.untracked.txt"
  return 1
}

# Tar-based insurance for dirs with broken/missing git metadata. Excludes
# regenerable build output. Returns 0 if an artifact was produced.
salvage_tar_dir() {
  local d="$1" label="$2"
  local tag tar size excludes entry
  tag="$(printf '%s' "$label" | tr -c 'A-Za-z0-9._-' '_')"
  tar="$SALVAGE_DIR/${STAMP}-${tag}.tar.gz"
  size="$(du -sb "$d" 2>/dev/null | awk '{print $1}')"
  excludes=""
  for entry in $REGEN_EXCLUDES; do
    excludes="$excludes --exclude=$entry"
  done
  if [[ -n "${size:-}" && "$size" -gt 2000000000 ]]; then
    # Too big even after exclusions are applied lazily: keep a file inventory
    # instead of a full tarball.
    mkdir -p "$SALVAGE_DIR"
    find "$d" -mindepth 1 \
      \( -name target -o -name node_modules -o -name dist -o -name build -o -name .next -o -name .nuxt -o -name .venv -o -name venv -o -name __pycache__ -o -name .cache -o -name vendor -o -name .turbo \) -prune -o \
      -printf '%s %p\n' 2>/dev/null | sort -rn | head -5000 > "$SALVAGE_DIR/${STAMP}-${tag}.inventory.txt"
    add_salvage "$SALVAGE_DIR/${STAMP}-${tag}.inventory.txt"
    return 0
  fi
  mkdir -p "$SALVAGE_DIR"
  if tar czf "$tar" $excludes -C "$(dirname "$d")" "$(basename "$d")" 2>"$TMP/tar.err"; then
    if [[ -s "$tar" ]]; then
      add_salvage "$tar"
      return 0
    fi
    rm -f "$tar"
  else
    log "tar salvage failed for $d: $(head -c 200 "$TMP/tar.err" 2>/dev/null)"
  fi
  return 1
}

# Salvage for any dir: git patch when metadata is intact, tar insurance when
# it is not. Returns 0 if artifacts were produced.
salvage_dir() {
  salvage_git_dir "$1" "$2" && return 0
  salvage_tar_dir "$1" "$2"
}

# Salvage $1 (label $2) and set NEW_SALVAGES to the newly created artifact
# paths (space-separated; empty when nothing was salvaged). A tree with
# intact git metadata and an empty `git status` has nothing to lose (ignored
# files are regenerable), so it is skipped outright.
run_salvage() {
  local n_before n_after st
  if [[ -e "$1/.git" ]] && git -C "$1" rev-parse --git-dir >/dev/null 2>&1; then
    st="$(git -C "$1" status --porcelain 2>/dev/null || true)"
    if [[ -z "$st" ]]; then
      NEW_SALVAGES=""
      return 0
    fi
  fi
  n_before="$(wc -l < "$SALVAGE_FILE")"
  if ! salvage_dir "$1" "$2"; then
    NEW_SALVAGES=""
    return 0
  fi
  n_after="$(wc -l < "$SALVAGE_FILE")"
  if (( n_after > n_before )); then
    NEW_SALVAGES="$(tail -n +$((n_before + 1)) "$SALVAGE_FILE" | tr '\n' ' ')"
  else
    NEW_SALVAGES=""
  fi
  return 0
}

# ----------------------------------------------- session-clone sweep (1/2) --
consider_session() {
  local agent="$1" ws="$2" s="$3"
  local s_real mt age size reason salvages
  s_real="$(realpath -m "$s")"
  mt="$(stat -c %Y "$s" 2>/dev/null)" || {
    KEPT_ROWS="${KEPT_ROWS}| \`${s}\` | ? | ? | stat failed; kept |\n"
    KEEP_COUNT=$((KEEP_COUNT + 1))
    return 0
  }
  age=$(( (NOW - mt) / 86400 ))
  size="$(du -sb "$s" 2>/dev/null | awk '{print $1}')"
  size="${size:-?}"

  if (( age < TTL_DAYS )); then
    add_kept "$s" "$size" "${age}d" "young (${age}d < ${TTL_DAYS}d TTL)"
    return 0
  fi
  if is_active_path "$s_real" "session"; then
    add_kept "$s" "$size" "${age}d" "active run/workspace (current cwd or API id/cwd match)"
    return 0
  fi
  if (( API_OK == 0 )); then
    add_kept "$s" "$size" "${age}d" "stale but API unreachable; kept when in doubt"
    return 0
  fi

  # --- reclaim: salvage first, then delete
  run_salvage "$s" "session-${agent}-$(basename "$s")"
  local salvages="$NEW_SALVAGES"
  if (( DRY_RUN == 0 )); then
    if rm -rf "$s" 2>/dev/null; then
      add_reclaimed "$s" "$size" "${age}d" "stale session clone (> ${TTL_DAYS}d, not an active run)" "$salvages"
      BYTES_FREED=$((BYTES_FREED + ${size:-0}))
      log "reclaimed session ${s} (${size} bytes, age ${age}d)"
    else
      add_kept "$s" "$size" "${age}d" "rm failed; kept"
    fi
  else
    add_reclaimed "$s" "$size" "${age}d" "DRY-RUN: stale session clone (> ${TTL_DAYS}d, not an active run)" "$salvages"
  fi
  return 0
}

sweep_sessions() {
  local agent_dir agent ws det s
  for agent_dir in "$WORKSPACE_ROOT"/*/; do
    [[ -d "$agent_dir" ]] || continue
    agent="$(basename "$agent_dir")"
    for ws in "$agent_dir"*/; do
      [[ -d "$ws" ]] || continue
      ws="${ws%/}"
      det="$ws/.paperclip-runtime/detached-repositories"
      [[ -d "$det" ]] || continue
      for s in "$det"/*/; do
        [[ -d "$s" ]] || continue
        consider_session "$agent" "$ws" "${s%/}"
      done
    done
  done
}

# --------------------------------------------- orphaned-worktree sweep (2/2) --
# Classify one worktree dir whose .git pointer file is at $1 (gitdir target
# resolved to $2). Decides reclaim/keep for broken-pointer and merged/gone
# branches.
consider_worktree() {
  local wt="$1" gitdir="$2" scope="$3"
  local wt_real base branch mainref remotes age size reason salvages
  wt_real="$(realpath -m "$wt")"
  mt="$(stat -c %Y "$wt" 2>/dev/null)" || return 0
  age=$(( (NOW - mt) / 86400 ))
  size="$(du -sb "$wt" 2>/dev/null | awk '{print $1}')"
  size="${size:-?}"

  if is_active_path "$wt_real"; then
    add_kept "$wt" "$size" "${age}d" "active run/workspace (current cwd or API id/cwd match)"
    return 0
  fi

  # (A) broken pointer: base repo is gone
  if [[ -z "$gitdir" || ! -e "$gitdir" ]]; then
    if (( API_OK == 0 )); then
      add_kept "$wt" "$size" "${age}d" "broken .git pointer but API unreachable; kept when in doubt"
      return 0
    fi
    run_salvage "$wt" "worktree-${scope}-$(basename "$wt")"
    local salvages="$NEW_SALVAGES"
    if (( DRY_RUN == 0 )); then
      if rm -rf "$wt" 2>/dev/null; then
        add_reclaimed "$wt" "$size" "${age}d" "orphaned worktree (broken .git pointer)" "$salvages"
        BYTES_FREED=$((BYTES_FREED + ${size:-0}))
        log "reclaimed orphan worktree ${wt} (${size} bytes)"
      else
        add_kept "$wt" "$size" "${age}d" "rm failed; kept"
      fi
    else
      add_reclaimed "$wt" "$size" "${age}d" "DRY-RUN: orphaned worktree (broken .git pointer)" "$salvages"
    fi
    return 0
  fi

  # (B) pointer resolves: judge the branch against the base repo
  base="$(cd "$gitdir" 2>/dev/null && git rev-parse --git-common-dir 2>/dev/null)" || base=""
  if [[ -z "$base" ]]; then
    add_kept "$wt" "$size" "${age}d" "worktree pointer resolves but base unreadable; kept"
    return 0
  fi
  base="$(realpath -m "$base")"
  branch="$(git -C "$wt" symbolic-ref --short -q HEAD 2>/dev/null || true)"

  if [[ -z "$branch" ]]; then
    add_kept "$wt" "$size" "${age}d" "detached HEAD worktree (no branch rule); kept"
    return 0
  fi
  if ! git -C "$base" rev-parse -q --verify "refs/heads/$branch" >/dev/null 2>&1; then
    add_kept "$wt" "$size" "${age}d" "local branch ${branch} missing from base; kept"
    return 0
  fi

  # Which main? prefer local main, then origin/main, then the remote's HEAD.
  mainref=""
  if git -C "$base" rev-parse -q --verify refs/heads/main >/dev/null 2>&1; then
    mainref="refs/heads/main"
  elif git -C "$base" rev-parse -q --verify refs/remotes/origin/main >/dev/null 2>&1; then
    mainref="refs/remotes/origin/main"
  else
    local originhead
    originhead="$(git -C "$base" symbolic-ref --short -q refs/remotes/origin/HEAD 2>/dev/null || true)"
    if [[ -n "$originhead" ]] && git -C "$base" rev-parse -q --verify "$originhead" >/dev/null 2>&1; then
      mainref="$originhead"
    fi
  fi

  merged=0; gone=0
  if [[ -n "$mainref" ]] && git -C "$base" merge-base --is-ancestor "$branch" "$mainref" >/dev/null 2>&1; then
    merged=1
  fi
  # The "gone" confirmation costs network round-trips, so only run it when
  # the branch was not already caught as merged.
  if (( merged == 0 )); then
  # "gone from remotes" only when the base actually has remote-tracking refs
  # (i.e. has been fetched at least once); otherwise a never-fetched base
  # would make every pushed branch look "gone".
  if [[ -n "$(git -C "$base" for-each-ref --format='%(refname)' 'refs/remotes/*' 2>/dev/null | head -1)" ]]; then
    if [[ -z "$(git -C "$base" for-each-ref --format='%(refname:short)' --count=1 "refs/remotes/*/${branch}" 2>/dev/null)" ]]; then
      # Local absence is ambiguous (never pushed vs. deleted after push), so
      # confirm against the remotes before treating the branch as gone. Any
      # remote that still has it means "not gone"; a network failure means
      # "in doubt" -> keep.
      local r gone_confirmed=1
      for r in $(git -C "$base" remote 2>/dev/null); do
        if git -C "$base" ls-remote --exit-code "$r" "refs/heads/${branch}" >/dev/null 2>&1; then
          gone_confirmed=0
          break
        fi
        if ! git -C "$base" ls-remote "$r" >/dev/null 2>&1; then
          gone_confirmed=1   # unreachable: stay in doubt
          break
        fi
      done
      (( gone_confirmed )) && gone=1
    fi
  fi
  fi

  if (( merged == 0 && gone == 0 )); then
    add_kept "$wt" "$size" "${age}d" "branch ${branch} not merged/gone"
    return 0
  fi
  if (( age * 24 < GRACE_HOURS )); then
    add_kept "$wt" "$size" "${age}d" "merged/gone branch ${branch} but young (${age}d < ${GRACE_HOURS}h grace)"
    return 0
  fi
  if (( API_OK == 0 )); then
    add_kept "$wt" "$size" "${age}d" "merged/gone branch but API unreachable; kept when in doubt"
    return 0
  fi

  run_salvage "$wt" "worktree-${scope}-$(basename "$wt")"
  local salvages="$NEW_SALVAGES"
  local why
  if (( merged == 1 && gone == 1 )); then why="merged into main and gone from remotes";
  elif (( merged == 1 )); then why="merged into main";
  else why="gone from remotes"; fi

  if (( DRY_RUN == 0 )); then
    if ! git -C "$base" worktree remove --force "$wt" >/dev/null 2>&1; then
      git -C "$base" worktree prune >/dev/null 2>&1 || true
    fi
    if rm -rf "$wt" 2>/dev/null; then
      add_reclaimed "$wt" "$size" "${age}d" "worktree branch ${branch} ${why}" "$salvages"
      BYTES_FREED=$((BYTES_FREED + ${size:-0}))
      log "reclaimed worktree ${wt} (${size} bytes, ${why})"
    else
      add_kept "$wt" "$size" "${age}d" "rm failed; kept"
    fi
  else
    add_reclaimed "$wt" "$size" "${age}d" "DRY-RUN: worktree branch ${branch} ${why}" "$salvages"
  fi
  return 0
}

# Scan a root for .git pointer files (worktrees). $1 = root, $2 = scope label.
scan_worktree_pointers() {
  local root="$1" scope="$2" gf gitdir
  while IFS= read -r -d '' gf; do
    [[ -f "$gf" ]] || continue
    local wt_dir; wt_dir="$(dirname "$gf")"
    if grep -qxF "$wt_dir" "$TMP/judged" 2>/dev/null; then
      continue
    fi
    gitdir=""
    if head -c 8 "$gf" 2>/dev/null | grep -q '^gitdir:'; then
      gitdir="$(sed -n '1s/^gitdir:[[:space:]]*//p' "$gf" 2>/dev/null)"
      if [[ -n "$gitdir" && "$gitdir" != /* ]]; then
        gitdir="$(dirname "$gf")/$gitdir"
      fi
      gitdir="$(realpath -m "$gitdir" 2>/dev/null || echo "$gitdir")"
    elif [[ -s "$gf" ]]; then
      # A .git file that is not a worktree pointer: not ours to judge.
      add_kept "$wt_dir" "?" "?" ".git file without a gitdir pointer; kept"
      continue
    fi
    echo "$wt_dir" >> "$TMP/judged"
    consider_worktree "$wt_dir" "$gitdir" "$scope"
  done < <(find "$root" -mindepth 2 -maxdepth "$SCAN_MAXDEPTH" \
      \( -type d \( -name .git -o -name target -o -name node_modules -o -name dist \
        -o -name build -o -name .next -o -name .nuxt -o -name .venv -o -name venv \
        -o -name __pycache__ -o -name .cache -o -name vendor -o -name .turbo \) \) -prune -o \
      \( -type f -name .git -print0 \) 2>/dev/null)
}

# Judge one `worktree list --porcelain` entry. $1=path $2=gitdir $3=base
# $4=scope.
judge_porcelain_entry() {
  local wt="$1" gitdir="$2" base="$3" scope="$4"
  [[ -n "$wt" && "$wt" != "$base" ]] || return 0
  if grep -qxF "$wt" "$TMP/judged" 2>/dev/null; then
    return 0
  fi
  # Some git builds omit the `gitdir:` key from `worktree list --porcelain`;
  # fall back to the worktree's own .git pointer file.
  if [[ -z "$gitdir" && -f "$wt/.git" ]]; then
    gitdir="$(sed -n '1s/^gitdir:[[:space:]]*//p' "$wt/.git" 2>/dev/null)"
    [[ -n "$gitdir" && "$gitdir" != /* ]] && gitdir="$wt/$gitdir"
  fi
  if [[ -z "$gitdir" ]]; then
    if [[ -d "$wt/.git" ]]; then
      add_kept "$wt" "?" "?" "primary checkout of a shared repo; kept"
    else
      add_kept "$wt" "?" "?" "registered worktree without resolvable gitdir; kept"
    fi
    return 0
  fi
  if [[ -d "$wt" ]]; then
    echo "$wt" >> "$TMP/judged"
    consider_worktree "$wt" "$(realpath -m "$gitdir")" "$scope"
  else
    echo "$wt" >> "$TMP/judged"
    add_kept "$wt" "?" "?" "registered worktree, directory already gone (metadata pruned)"
  fi
  return 0
}

# Registered worktrees of a base repo. Parses `git worktree list --porcelain`
# so we get the real gitdir for each worktree and prune vanished metadata.
# $1 = base repo, $2 = scope label.
sweep_base_worktrees() {
  local base="$1" scope="$2" wt gitdir line
  local porcelain; porcelain="$(git -C "$base" worktree list --porcelain 2>/dev/null || true)"
  wt=""; gitdir=""
  while IFS= read -r line; do
    case "$line" in
      worktree\ *)    wt="${line#worktree }" ;;
      "gitdir: "*)    gitdir="${line#gitdir: }" ;;
      "")             # end of block
        judge_porcelain_entry "$wt" "$gitdir" "$base" "$scope"
        wt=""; gitdir=""
        ;;
      *) : ;;
    esac
  done <<< "$porcelain"
  judge_porcelain_entry "$wt" "$gitdir" "$base" "$scope"
  if printf '%s\n' "$porcelain" | grep -q 'prunable'; then
    git -C "$base" worktree prune >/dev/null 2>&1 || true
    log "pruned stale worktree metadata in ${base}"
  fi
  return 0
}

sweep_worktrees() {
  local agent_dir agent ws d gf
  touch "$TMP/judged"
  for agent_dir in "$WORKSPACE_ROOT"/*/; do
    [[ -d "$agent_dir" ]] || continue
    agent="$(basename "$agent_dir")"
    for ws in "$agent_dir"*/; do
      [[ -d "$ws" ]] || continue
      ws="${ws%/}"
      # A workspace root that is itself a worktree (`.git` pointer file).
      if [[ -f "$ws/.git" ]]; then
        if ! grep -qxF "$ws" "$TMP/judged"; then
          echo "$ws" >> "$TMP/judged"
          local gd; gd="$(sed -n '1s/^gitdir:[[:space:]]*//p' "$ws/.git" 2>/dev/null)"
          [[ -n "$gd" && "$gd" != /* ]] && gd="$ws/$gd"
          consider_worktree "$ws" "$(realpath -m "$gd" 2>/dev/null || echo "$gd")" "ws:${agent}:$(basename "$ws")"
        fi
      fi
      # A workspace root that is itself a base repo: judge its registered
      # worktrees and prune vanished worktree metadata.
      [[ -d "$ws/.git" ]] && sweep_base_worktrees "$ws" "ws:${agent}:$(basename "$ws")"
      for d in "$ws"/*/; do
        [[ -d "$d" ]] || continue
        d="${d%/}"
        # Only true base repos (`.git` directory) get the registered-worktree
        # pass; worktrees (`.git` file) are covered by the pointer scans.
        [[ -d "$d/.git" ]] && sweep_base_worktrees "$d" "ws:${agent}:$(basename "$ws")"
      done
      scan_worktree_pointers "$ws" "ws:${agent}:$(basename "$ws")"
    done
  done
  scan_worktree_pointers "$SCRATCH_ROOT" "scratch"
}

# ---------------------------------------------------------------- manifest --
write_manifest() {
  mkdir -p "$MANIFEST_DIR"
  {
    echo "# GC manifest: stale per-session clones + orphaned worktrees (ANT-716)"
    echo
    if (( DRY_RUN == 1 )); then
      echo "- Run: ${STAMP} (UTC) — DRY RUN (nothing deleted)"
    else
      echo "- Run: ${STAMP} (UTC)"
    fi
    echo "- Trigger: routine/manual, run id ${RUN_ID:-unknown}"
    echo "- Agent workspaces root: ${WORKSPACE_ROOT}"
    echo "- Scratch root: ${SCRATCH_ROOT}"
    echo "- TTL: ${TTL_DAYS} days; worktree grace: ${GRACE_HOURS}h"
    echo "- API signals: ${API_NOTE}"
    echo
    echo "## Reclaimed (${RECLAIM_COUNT})"
    echo
    if [[ -n "$RECLAIMED_ROWS" ]]; then
      echo "| Path | Size (bytes) | Age | Reason | Salvage |"
      echo "|---|---|---|---|---|"
      printf '%b' "$RECLAIMED_ROWS"
    else
      echo "Nothing reclaimed — tree was clean (no-op run)."
    fi
    echo
    echo "## Kept (${KEEP_COUNT})"
    echo
    if [[ -n "$KEPT_ROWS" ]]; then
      echo "| Path | Size (bytes) | Age | Reason |"
      echo "|---|---|---|---|"
      printf '%b' "$KEPT_ROWS"
    else
      echo "Nothing kept."
    fi
    echo
    echo "## Salvage artifacts"
    echo
    if grep -q . "$SALVAGE_FILE"; then
      grep . "$SALVAGE_FILE" | sed 's/^/- /'
    else
      echo "None (no uncommitted work found)."
    fi
    echo
    echo "## Totals"
    echo
    echo "- Bytes freed: ${BYTES_FREED}"
    echo "- Items reclaimed: ${RECLAIM_COUNT}; items kept: ${KEEP_COUNT}"
  } > "$MANIFEST"
}

# -------------------------------------------------------------------- main --
collect_api_signals
sweep_sessions
sweep_worktrees
write_manifest

{
  echo "manifest: $MANIFEST"
  echo "api: $API_NOTE"
  echo "reclaimed: $RECLAIM_COUNT"
  echo "kept: $KEEP_COUNT"
  echo "bytes_freed: $BYTES_FREED"
  if grep -q . "$SALVAGE_FILE"; then
    echo "salvage:"
    grep . "$SALVAGE_FILE"
  fi
} | tee "$TMP/summary.txt"

exit 0
