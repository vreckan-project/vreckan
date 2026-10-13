#!/usr/bin/env python3
"""Take a point-in-time snapshot of the Vreckan SQLite configuration database.

Run this as the first step of the SQLite→Postgres cutover
(``docs/cutover.md``) so the rollback path has a known-good file to restore.
The backup uses SQLite's online backup API, which produces a consistent
copy even while the server is running (the runbook takes it before the
legacy service is stopped, but either order is safe).

Usage
-----
    python3 scripts/snapshot-sqlite.py --src ./vreckan-data/vreckan.db --dest-dir ./vreckan-snapshot

Writes ``<dest-dir>/vreckan-<UTC-timestamp>.db``, runs
``PRAGMA integrity_check`` on the copy, and prints the snapshot path, its
size, and a per-table row-count baseline (the same 13 configuration tables
the migration script uses) — keep that output: it is the reference for the
post-migration row-count verification in ``verify-cutover.py``.

Dependencies: stdlib only (Python 3.7+, for ``sqlite3.Connection.backup``).

Environment variable sources (in order of precedence): CLI flag, then the
named environment variable (``VRECKAN_SQLITE_PATH``).
"""

from __future__ import annotations

import argparse
import datetime
import os
import sqlite3
import sys

# Mirrors the table list in `migrate-sqlite-to-postgres.py` (and the
# `TABLE_ORDER` map in `src/db.rs`).
TABLE_ORDER: list = [
    "users",
    "groups",
    "roles",
    "volume_mounts",
    "app_stores",
    "installed_apps",
    "sessions",
    "auth_tokens",
    "public_shares",
    "app_templates",
    "template_schema_settings",
    "pinned_behaviors",
    "app_settings",
]


def count_table(conn: sqlite3.Connection, table: str):
    """Return the row count of `table`, or None if the table is absent."""
    cur = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
    )
    if cur.fetchone() is None:
        return None
    return conn.execute(f"SELECT COUNT(*) FROM \"{table}\"").fetchone()[0]


def run(args: argparse.Namespace) -> int:
    src = args.src or os.environ.get("VRECKAN_SQLITE_PATH")
    if not src:
        sys.stderr.write(
            "SQLite path not given. Pass --src or set VRECKAN_SQLITE_PATH.\n"
        )
        return 2
    if not os.path.exists(src):
        sys.stderr.write(f"SQLite database not found: {src}\n")
        return 2

    dest_dir = args.dest_dir
    os.makedirs(dest_dir, exist_ok=True)
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    dest = os.path.join(dest_dir, f"vreckan-{stamp}.db")

    src_conn = sqlite3.connect(src)
    dst_conn = sqlite3.connect(dest)
    try:
        src_conn.backup(dst_conn)
        dst_conn.commit()
        result = dst_conn.execute("PRAGMA integrity_check").fetchone()[0]
        if result != "ok":
            print(f"Snapshot written to {dest} but integrity_check = {result!r}",
                  file=sys.stderr)
            return 1

        size = os.path.getsize(dest)
        print(f"Source SQLite : {src}")
        print(f"Snapshot      : {dest} ({size} bytes)")
        print(f"Integrity     : ok")
        print("\nRow-count baseline (reference for post-migration verification):")
        total = 0
        for table in TABLE_ORDER:
            n = count_table(dst_conn, table)
            if n is None:
                print(f"  {table:28s} (absent)")
            else:
                print(f"  {table:28s} {n}")
                total += n
        print(f"  {'total':28s} {total}")
    finally:
        src_conn.close()
        dst_conn.close()
    return 0


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Snapshot a Vreckan SQLite configuration database.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage", 1)[1] if "Usage" in __doc__ else None,
    )
    ap.add_argument("--src", help="Path to the SQLite database (or set VRECKAN_SQLITE_PATH).")
    ap.add_argument(
        "--dest-dir",
        default="vreckan-snapshot",
        help="Directory the snapshot is written to (created if missing; default: vreckan-snapshot/).",
    )
    args = ap.parse_args()
    sys.exit(run(args))


if __name__ == "__main__":
    main()
