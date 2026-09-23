#!/usr/bin/env bash
# dev-container.sh — create / stop / remove the Vreckan dev container.
#
# Linux host: `docker compose` works fine here. (On the old WSL2 host the app
# terminated cleanly right after startup on any compose-created user-defined
# network — Docker's embedded DNS 127.0.0.11 was the trigger — so the dev
# container had to be created with `docker run` on the default bridge via
# dev-container.ps1. That workaround does NOT apply on a normal Linux host, so
# this script simply wraps `docker compose` against docker-compose.yml.)
#
# Usage:
#   ./dev-container.sh              # recreate + start the dev container
#   ./dev-container.sh stop         # stop (data persists)
#   ./dev-container.sh remove       # stop + remove (data persists in ./vreckan-data)
#
# Data persists in ./vreckan-data (bind-mounted to /data), so recreating the
# container does NOT lose the database, storage, keys, or caches.
#
# NOTE: your user must be in the `docker` group. If you were just added to it,
# open a NEW terminal first (group membership is read at login time).

set -euo pipefail

SERVER_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SERVER_DIR"

ACTION="${1:-up}"

case "$ACTION" in
  stop)
    docker compose stop
    echo "Stopped the dev container (vreckan-http)."
    ;;
  remove)
    docker compose down
    echo "Removed the dev container (data persists in ./vreckan-data)."
    ;;
  up)
    # --force-recreate matches the old ps1 behaviour (rm -f + run): always
    # rebuild the container from the current compose config.
    docker compose up -d --force-recreate
    echo "Started the dev container (vreckan-http)."
    echo "  logs : docker compose logs -f"
    echo "  url  : https://127.0.0.1:8443/"
    ;;
  *)
    echo "Unknown action: $ACTION  (use: up | stop | remove)" >&2
    exit 1
    ;;
esac
