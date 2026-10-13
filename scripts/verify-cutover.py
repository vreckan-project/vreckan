#!/usr/bin/env python3
"""Verify a Vreckan SQLite→Postgres cutover.

Compares the per-table row counts of the original SQLite database against
the Postgres database written by ``migrate-sqlite-to-postgres.py``, and
(optionally) checks the running server's ``/api/health`` endpoint.

Run it in two places (see ``docs/cutover.md``):

  1. Right after the migration (step 6) — confirms every table's contents
     moved, before the new service starts:

         python3 scripts/verify-cutover.py

  2. After the Rust service is up on Postgres (step 8) — adds a live
     health check:

         python3 scripts/verify-cutover.py --health-url https://<your-server>/api/health

A table absent from the SQLite source is expected to be empty in Postgres
(the migration creates missing tables empty) and reads as ``OK``. A table
absent from Postgres fails the check — the migration creates all 13, so
absence usually means the DSN points at the wrong database.

The SQLite side uses the stdlib; the Postgres side needs ``psycopg2``
(already required for a real migration run). Self-signed TLS is accepted
on the health check (cutover deployments commonly still run the default
self-signed certificate).

Usage
-----
    # defaults: --sqlite from VRECKAN_SQLITE_PATH, --postgres from
    # VRECKAN_POSTGRES_DSN or VRECKAN_DATABASE_URL
    python3 scripts/verify-cutover.py

    # explicit
    python3 scripts/verify-cutover.py \\
        --sqlite /var/lib/vreckan/vreckan-data/vreckan.db \\
        --postgres 'postgres://vreckan:vreckan@localhost:5432/vreckan' \\
        --health-url https://localhost/api/health

Exit codes: 0 = all tables match (and health OK when checked);
1 = a count mismatch or health failure; 2 = usage error.
"""

from __future__ import annotations

import argparse
import os
import sqlite3
import ssl
import sys
import urllib.error
import urllib.request
from typing import List, Optional

# Imported lazily inside count_postgres_tables(): the count-only paths work
# without it, mirroring the migration script's lazy import.
try:
    import psycopg2  # type: ignore  # noqa: F401
    _PSYCOPG2 = True
except ImportError:
    psycopg2 = None  # type: ignore
    _PSYCOPG2 = False

# Mirrors the table list in `migrate-sqlite-to-postgres.py` (and the
# `TABLE_ORDER` map in `src/db.rs`).
TABLE_ORDER: List[str] = [
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


def count_sqlite_tables(path: str) -> List[Optional[int]]:
    """Per-table row counts in the SQLite database (None if table absent)."""
    conn = sqlite3.connect(path)
    try:
        counts: List[Optional[int]] = []
        for table in TABLE_ORDER:
            cur = conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
                (table,),
            )
            if cur.fetchone() is None:
                counts.append(None)
                continue
            counts.append(conn.execute(f'SELECT COUNT(*) FROM "{table}"').fetchone()[0])
        return counts
    finally:
        conn.close()


def count_postgres_tables(dsn: str) -> List[Optional[int]]:
    """Per-table row counts in the Postgres database (None if table absent)."""
    if not _PSYCOPG2:
        sys.stderr.write(
            "psycopg2 is required to count Postgres tables:  pip install psycopg2-binary\n"
        )
        sys.exit(2)
    pg = psycopg2.connect(dsn)
    try:
        cur = pg.cursor()
        counts: List[Optional[int]] = []
        for table in TABLE_ORDER:
            cur.execute(
                "SELECT to_regclass(%s)", (table,)
            )
            if cur.fetchone()[0] is None:
                counts.append(None)
                continue
            cur.execute(f'SELECT COUNT(*) FROM "{table}"')
            counts.append(cur.fetchone()[0])
        return counts
    finally:
        pg.close()


def check_health(url: str) -> bool:
    """GET the health endpoint; return True on HTTP 200."""
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # self-signed is normal during a cutover
    try:
        with urllib.request.urlopen(url, context=ctx, timeout=10) as resp:
            body = resp.read().decode("utf-8", "replace")
            print(f"Health check  : {url} -> HTTP {resp.status} {body.strip()}")
            return resp.status == 200
    except urllib.error.HTTPError as e:
        print(f"Health check  : {url} -> HTTP {e.code} (server answered, "
              f"see response/logs)", file=sys.stderr)
        return False
    except (urllib.error.URLError, OSError) as e:
        print(f"Health check  : {url} -> unreachable ({e})", file=sys.stderr)
        return False


def run(args: argparse.Namespace) -> int:
    sqlite_path = args.sqlite or os.environ.get("VRECKAN_SQLITE_PATH")
    if not sqlite_path:
        sys.stderr.write(
            "SQLite path not given. Pass --sqlite or set VRECKAN_SQLITE_PATH.\n"
        )
        return 2
    pg_dsn = (
        args.postgres
        or os.environ.get("VRECKAN_POSTGRES_DSN")
        or os.environ.get("VRECKAN_DATABASE_URL")
    )
    if not pg_dsn:
        sys.stderr.write(
            "Postgres DSN not given. Pass --postgres, or set VRECKAN_POSTGRES_DSN "
            "or VRECKAN_DATABASE_URL (e.g. postgres://vreckan:vreckan@localhost:5432/vreckan).\n"
        )
        return 2
    if not os.path.exists(sqlite_path):
        sys.stderr.write(f"SQLite database not found: {sqlite_path}\n")
        return 2

    print(f"Source SQLite : {sqlite_path}")
    shown = pg_dsn.replace("://", "://***:***@", 1) if "://" in pg_dsn else pg_dsn
    print(f"Target Postgres: {shown}")

    sqlite_counts = count_sqlite_tables(sqlite_path)
    postgres_counts = count_postgres_tables(pg_dsn)

    print()
    print(f"  {'table':28s} {'sqlite':>8s} {'postgres':>10s}  result")
    ok = True
    total_sqlite = 0
    total_postgres = 0
    for table, s, p in zip(TABLE_ORDER, sqlite_counts, postgres_counts):
        s_disp = "absent" if s is None else str(s)
        p_disp = "absent" if p is None else str(p)
        if s is not None:
            total_sqlite += s
        if p is not None:
            total_postgres += p
        # A table absent from SQLite is fine — the migration creates it
        # empty. A table absent from Postgres is not: the migration creates
        # all 13, so absence means the DSN points at the wrong database.
        s_eff = 0 if s is None else s
        p_eff = 0 if p is None else p
        if p is None or s_eff != p_eff:
            status = "MISMATCH"
            ok = False
        else:
            status = "OK"
        print(f"  {table:28s} {s_disp:>8s} {p_disp:>10s}  {status}")
    print(f"  {'total':28s} {total_sqlite:>8d} {total_postgres:>10d}")

    if not ok:
        print("\nVerification FAILED: at least one table's row count differs.")
        return 1
    print("\nAll table row counts match.")

    if args.health_url:
        if not check_health(args.health_url):
            print("\nVerification FAILED: health check did not return HTTP 200.")
            return 1
    return 0


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Verify a Vreckan SQLite→Postgres cutover (row counts + health).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage", 1)[1] if "Usage" in __doc__ else None,
    )
    ap.add_argument("--sqlite", help="Path to the original SQLite database (or set VRECKAN_SQLITE_PATH).")
    ap.add_argument(
        "--postgres",
        help="Postgres DSN (e.g. postgres://user:pass@host:5432/db) (or set VRECKAN_POSTGRES_DSN / VRECKAN_DATABASE_URL).",
    )
    ap.add_argument(
        "--health-url",
        help="Server health endpoint to check after cutover (e.g. https://localhost/api/health). Skipped when omitted.",
    )
    args = ap.parse_args()
    sys.exit(run(args))


if __name__ == "__main__":
    main()
