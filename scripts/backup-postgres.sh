#!/usr/bin/env bash
# backup-postgres.sh — dated `pg_dump` backups and restores for the Vreckan
# Postgres database (see docs/ops.md §4.2).
#
# Complements the admin-UI backup archives (tar.gz of the data tree + a
# logical JSON dump of the database): this script produces a native
# PostgreSQL custom-format dump (`pg_dump -Fc`) that can be restored with
# `pg_restore` without the server or the data tree, on any Postgres 11+
# server — including a different one (disaster recovery, migration).
#
# Configuration — the same env vars the server reads (docs/ops.md §4). When
# running from a cron job or on the host of a compose deployment, source the
# same `.env` the server uses first (see the cron example in §4.2).
#   VRECKAN_DATABASE_URL     postgres://user:pass@host:port/db. Wins over
#                            VRECKAN_DB_* (optional). `postgresql://` and the
#                            legacy `postgresql+asyncpg://` forms are accepted.
#   VRECKAN_DB_BACKEND       postgres|postgresql (default: postgres)
#   VRECKAN_DB_HOST          default: localhost
#   VRECKAN_DB_PORT          default: 5432
#   VRECKAN_DB_USER          default: postgres
#   VRECKAN_DB_PASS          default: (empty)
#   VRECKAN_DB_NAME          default: vreckan
#   VRECKAN_BACKUP_DIR       where dated dumps are written (default:
#                            ${VRECKAN_DATA_ROOT:-.}/backups — for a compose
#                            deployment use the host path of the vreckan-data
#                            mount, e.g. $PWD/vreckan-data/backups)
#   VRECKAN_BACKUP_RETENTION how many dated dumps to keep; 0 keeps all
#                            (default: 14)
#   VRECKAN_POSTGRES_CONTAINER  name of the compose Postgres container
#                            (default: vreckan-postgres). When that container
#                            exists and the target host is local,
#                            pg_dump/pg_restore run inside it via
#                            `docker exec` — no Postgres client needed on the
#                            host. When no such container is present the host
#                            `pg_dump`/`pg_restore` are used instead.
#
# Usage:
#   scripts/backup-postgres.sh backup          write a dated dump
#   scripts/backup-postgres.sh restore FILE    restore a dump (a path, or a
#                                               name inside VRECKAN_BACKUP_DIR);
#                                               stop the vreckan server first;
#                                               --no-clean restores over
#                                               existing objects (default:
#                                               --clean --if-exists)
#   scripts/backup-postgres.sh list            list stored dumps, newest first
#   scripts/backup-postgres.sh help            this text
set -euo pipefail

log() { printf '%s\n' "$*" >&2; }
die() { printf 'backup-postgres: %s\n' "$*" >&2; exit 1; }

BACKUP_DIR="${VRECKAN_BACKUP_DIR:-${VRECKAN_DATA_ROOT:-.}/backups}"
RETENTION="${VRECKAN_BACKUP_RETENTION:-14}"
PG_CONTAINER="${VRECKAN_POSTGRES_CONTAINER:-vreckan-postgres}"
CONTAINER_EXPLICIT=0
[[ -n "${VRECKAN_POSTGRES_CONTAINER+x}" ]] && CONTAINER_EXPLICIT=1
DUMP_PREFIX="vreckan-postgres"

[[ "$RETENTION" =~ ^[0-9]+$ ]] || die "VRECKAN_BACKUP_RETENTION must be a non-negative integer (got '$RETENTION')"

usage() { sed -n '2,/^set /p' "$0" | sed 's/^# \{0,1\}//' | sed '$d'; }

# Percent-decode one %XX pair at a time (URL userinfo/paths may be encoded).
pct_decode() {
  local s="$1" out="" i c
  for ((i = 0; i < ${#s}; i++)); do
    c="${s:i:1}"
    if [[ "$c" == "%" ]] && (( ${#s} >= i + 3 )); then
      # shellcheck disable=SC2059
      out+=$(printf "\\x${s:i+1:2}")
      i=$((i + 2))
    else
      out+="$c"
    fi
  done
  printf '%s' "$out"
}

# ---------------------------------------------------------------------------
# Resolve the target database the same way the server does
# (src/settings.rs: build_database_url)
# ---------------------------------------------------------------------------
URL="${VRECKAN_DATABASE_URL:-}"
URL_EXPLICIT=0
[[ -n "$URL" ]] && URL_EXPLICIT=1

# Normalise legacy schemes the server also accepts.
case "$URL" in
  postgresql+asyncpg://*) URL="postgres://${URL#postgresql+asyncpg://}";;
  postgresql://*)         URL="postgres://${URL#postgresql://}";;
esac

if (( URL_EXPLICIT )); then
  [[ "$URL" == postgres://* ]] || die "VRECKAN_DATABASE_URL must be a postgres:// URL (got: ${URL%%:*})"
  rest="${URL#postgres://}"
  if [[ "$rest" == */* ]]; then
    dbname="${rest##*/}"
    hostport="${rest%%/*}"
  else
    hostport="$rest"
    dbname=""
  fi
  dbname="${dbname%%\?*}"; dbname="${dbname%%#*}"
  if [[ "$hostport" == *@* ]]; then
    userpass="${hostport%%@*}"
    hostpart="${hostport#*@}"
  else
    userpass=""
    hostpart="$hostport"
  fi
  if [[ "$userpass" == *:* ]]; then
    user="${userpass%%:*}"
    pass="${userpass#*:}"
  else
    user="$userpass"
    pass=""
  fi
  host="${hostpart%%:*}"
  if [[ "$hostpart" == *:* ]]; then
    port="${hostpart#*:}"
  else
    port="5432"
  fi
  user="$(pct_decode "$user")"
  pass="$(pct_decode "$pass")"
  [[ -n "$user" ]] || die "VRECKAN_DATABASE_URL has no username"
  [[ -n "$host" ]] || die "VRECKAN_DATABASE_URL has no host"
  [[ -n "$dbname" ]] || dbname="vreckan"
else
  backend="${VRECKAN_DB_BACKEND:-postgres}"
  case "$backend" in
    postgres|postgresql) ;;
    *) die "VRECKAN_DB_BACKEND='$backend' — this script targets Postgres (set VRECKAN_DATABASE_URL for the server's other backends)" ;;
  esac
  host="${VRECKAN_DB_HOST:-localhost}"
  port="${VRECKAN_DB_PORT:-5432}"
  [[ -n "$port" ]] || port="5432"
  user="${VRECKAN_DB_USER:-postgres}"
  pass="${VRECKAN_DB_PASS:-}"
  dbname="${VRECKAN_DB_NAME:-vreckan}"
fi

# ---------------------------------------------------------------------------
# Pick the execution path: bundled compose container via `docker exec` when
# available (no host Postgres client needed), host client otherwise.
# ---------------------------------------------------------------------------
USE_CONTAINER=0
if command -v docker >/dev/null 2>&1; then
  if docker inspect "$PG_CONTAINER" >/dev/null 2>&1; then
    localish=0
    case "$host" in
      postgres|localhost|127.0.0.1) localish=1;;
    esac
    (( localish || CONTAINER_EXPLICIT )) && USE_CONTAINER=1
  fi
fi

# The bundled container is initialised with POSTGRES_USER/POSTGRES_PASSWORD/
# POSTGRES_DB (docker-compose.yml). When the operator did not override the
# URL, those are the credentials the server actually uses — prefer them over
# the generic defaults.
if (( USE_CONTAINER && ! URL_EXPLICIT )); then
  cont_env="$(docker inspect -f '{{range .Config.Env}}{{.}}{{"\n"}}{{end}}' "$PG_CONTAINER")"
  cont_val() { local line; while IFS= read -r line; do [[ "$line" == "$1="* ]] && { printf '%s' "${line#*=}"; return 0; }; done <<< "$cont_env"; return 1; }
  if v="$(cont_val POSTGRES_USER)" && [[ -n "$v" ]]; then user="$v"; fi
  if v="$(cont_val POSTGRES_PASSWORD)" && [[ -n "$v" ]]; then pass="$v"; fi
  if v="$(cont_val POSTGRES_DB)" && [[ -n "$v" ]]; then dbname="$v"; fi
fi

# pg_dump/pg_restore inside the container (credentials from the environment).
pg_exec() { docker exec -e PGPASSWORD="$pass" "$PG_CONTAINER" "$@"; }

require_running() {
  if (( USE_CONTAINER )); then
    local state
    state="$(docker inspect -f '{{.State.Status}}' "$PG_CONTAINER")"
    [[ "$state" == "running" ]] || die "container '$PG_CONTAINER' is '$state' — start it, or point VRECKAN_DATABASE_URL at an external Postgres and unset VRECKAN_POSTGRES_CONTAINER"
  else
    command -v pg_dump >/dev/null 2>&1 || die "no host pg_dump found and no usable '$PG_CONTAINER' container — install postgresql-client (or set VRECKAN_POSTGRES_CONTAINER)"
  fi
}

# ---------------------------------------------------------------------------
# backup
# ---------------------------------------------------------------------------
do_backup() {
  require_running
  local ts out
  ts="$(date +%Y%m%d-%H%M%S)"
  out="$BACKUP_DIR/$DUMP_PREFIX-$ts.dump"
  mkdir -p "$BACKUP_DIR"
  [[ -e "$out" ]] && out="$BACKUP_DIR/$DUMP_PREFIX-$ts-$$.$RANDOM.dump"

  if (( USE_CONTAINER )); then
    local stage="vreckan-backup-$ts-$$.$RANDOM.dump"
    log "backup: pg_dump -Fc via container '$PG_CONTAINER' (db '$dbname' as '$user')"
    pg_exec pg_dump -U "$user" -d "$dbname" -Fc -f "/tmp/$stage"
    pg_exec pg_restore --list "/tmp/$stage" >/dev/null   # verify before keeping
    docker cp "$PG_CONTAINER:/tmp/$stage" "$out"
    docker exec "$PG_CONTAINER" rm -f "/tmp/$stage" || true
  else
    log "backup: pg_dump -Fc via host client (postgres://$user@${host}:${port}/${dbname})"
    local tmp
    tmp="$(mktemp "$BACKUP_DIR/.$DUMP_PREFIX-$ts.XXXXXX")"
    PGPASSWORD="$pass" pg_dump -h "$host" -p "$port" -U "$user" -d "$dbname" -Fc -f "$tmp"
    pg_restore --list "$tmp" >/dev/null   # verify before keeping
    mv "$tmp" "$out"
  fi

  rotate_dumps
  log "backup: wrote $out ($(du -h "$out" | cut -f1)); keeps the newest $RETENTION dump(s) (0 = all)"
}

# Drop dated dumps beyond the retention count (newest first; names sort
# chronologically). Only this script's own files are touched, so admin-UI
# archives in the same directory are left alone.
rotate_dumps() {
  [[ "$RETENTION" == "0" ]] && return 0
  local kept=0 f
  while IFS= read -r f; do
    if (( kept >= RETENTION )); then
      rm -f "$f"
      log "rotation: removed $f"
    fi
    kept=$((kept + 1))
  done < <(find "$BACKUP_DIR" -maxdepth 1 -type f -name "$DUMP_PREFIX-*.dump" 2>/dev/null | sort -r)
}

# ---------------------------------------------------------------------------
# restore FILE [--no-clean]
# ---------------------------------------------------------------------------
do_restore() {
  local file="${1:-}" noclean="${2:-0}" clean="--clean --if-exists"
  [[ -n "$file" ]] || die "usage: backup-postgres.sh restore FILE [--no-clean]"
  [[ -f "$file" ]] || file="$BACKUP_DIR/$file"
  [[ -f "$file" ]] || die "no such dump: $file (see 'backup-postgres.sh list')"
  [[ "$noclean" == "1" ]] && clean=""

  require_running
  log "restore: $file -> postgres://$user@${host}:${port}/${dbname} ($([[ -n "$clean" ]] && echo 'clean' || echo 'no-clean'))"
  log "restore: tip — stop the vreckan server first; it re-syncs the schema on startup"

  if (( USE_CONTAINER )); then
    local stage="vreckan-restore-$$.dump"
    docker cp "$file" "$PG_CONTAINER:/tmp/$stage"
    pg_exec pg_restore --list "/tmp/$stage" >/dev/null
    # shellcheck disable=SC2086
    pg_exec pg_restore $clean --no-owner --no-privileges -U "$user" -d "$dbname" "/tmp/$stage"
    docker exec "$PG_CONTAINER" rm -f "/tmp/$stage" || true
  else
    command -v pg_restore >/dev/null 2>&1 || die "no host pg_restore found — install postgresql-client"
    pg_restore --list "$file" >/dev/null
    # shellcheck disable=SC2086
    PGPASSWORD="$pass" pg_restore $clean --no-owner --no-privileges -h "$host" -p "$port" -U "$user" -d "$dbname" "$file"
  fi
  log "restore: done"
}

# ---------------------------------------------------------------------------
# list
# ---------------------------------------------------------------------------
do_list() {
  mkdir -p "$BACKUP_DIR"
  local found=0 f
  while IFS= read -r f; do
    found=1
    printf '%10s  %s\n' "$(du -h "$f" | cut -f1)" "$(basename "$f")"
  done < <(find "$BACKUP_DIR" -maxdepth 1 -type f -name "$DUMP_PREFIX-*.dump" 2>/dev/null | sort -r)
  (( found )) || log "no dated dumps in $BACKUP_DIR yet (run 'backup')"
}

# ---------------------------------------------------------------------------
# dispatch
# ---------------------------------------------------------------------------
cmd="${1:-backup}"
case "$cmd" in
  backup)
    shift
    [[ $# -eq 0 ]] || die "backup takes no arguments (got: $*)"
    do_backup
    ;;
  restore)
    shift
    file=""
    noclean=0
    for a in "$@"; do
      case "$a" in
        --no-clean) noclean=1;;
        *) file="$a";;
      esac
    done
    do_restore "$file" "$noclean"
    ;;
  list)
    do_list
    ;;
  -h|--help|help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
