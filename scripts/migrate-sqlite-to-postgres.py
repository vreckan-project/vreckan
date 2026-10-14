#!/usr/bin/env python3
"""Migrate an existing Vreckan SQLite configuration database to Postgres.

The Vreckan configuration database (users, groups, roles, volume mounts,
app stores, installed apps, app templates, public shares, sessions, auth
tokens, pinned behaviors, app settings) is stored as a single database whose
backend is chosen at runtime. Up through the v0.5.0 Python release the default
backend was SQLite; from the v0.6.0 Rust switchover the deployment default is
Postgres (see docker-compose.yml). This script moves an existing SQLite
database onto a Postgres database so a running deployment can be cut over
without re-seeding users and settings.

How it works
------------
The schema is identical across backends. This script:

  1. Creates the Postgres schema (``CREATE TABLE IF NOT EXISTS``) with the
     same tables/columns the server expects (JSON columns as ``jsonb``,
     booleans native, ``type``/``default`` quoted as reserved words).
  2. Reads every row of every table from the SQLite database.
  3. Replaces the contents of each Postgres table with the migrated rows
     (``DELETE`` + ``INSERT`` inside a single transaction), converting JSON
     columns from SQLite TEXT to Postgres ``jsonb``.

It is safe to re-run: each pass replaces the target tables' contents, so the
migration is idempotent. The server does not need to be running during the
migration; stop it first for a clean cut-over.

Usage
-----
    # defaults: sqlite at $VRECKAN_SQLITE_PATH, postgres from $VRECKAN_DATABASE_URL
    python3 scripts/migrate-sqlite-to-postgres.py

    # explicit
    python3 scripts/migrate-sqlite-to-postgres.py \\
        --sqlite /var/lib/vreckan/vreckan-data/vreckan.db \\
        --postgres 'postgres://vreckan:vreckan@postgres:5432/vreckan'

    # preview the schema + row counts without touching Postgres
    python3 scripts/migrate-sqlite-to-postgres.py --dry-run

Dependencies: Python 3.8+ (stdlib ``sqlite3``) and ``psycopg2``
(``pip install psycopg2-binary``).

Environment variable sources (in order of precedence): CLI flag, then the
named environment variable.
"""

from __future__ import annotations

import argparse
import os
import re
import sqlite3
import sys
from typing import Any, Dict, List, Optional, Tuple

# Imported lazily inside run(): a ``--dry-run`` only needs the stdlib, so a
# machine without psycopg2 can still preview the DDL and row counts.
try:
    import psycopg2  # type: ignore  # noqa: F401
    from psycopg2 import sql  # type: ignore  # noqa: F401
    from psycopg2.extras import Json  # type: ignore  # noqa: F401
    _PSYCOPG2 = True
except ImportError:
    psycopg2 = None  # type: ignore
    sql = None  # type: ignore
    Json = None  # type: ignore
    _PSYCOPG2 = False

# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------
# (table, [(column, kind), ...]) where kind is one of:
#   "text"    -> text
#   "bool"    -> boolean
#   "int"     -> integer
#   "float"   -> double precision
#   "json"    -> jsonb (stored as TEXT in SQLite)
#
# The column set and order mirror `src/entities.rs` (SeaORM entities) and the
# `known_columns`/`json_columns` maps in `src/db.rs`. JSON columns always hold
# an object or array in this schema, so the TEXT -> jsonb conversion below is
# unambiguous.

COLUMNS: Dict[str, List[Tuple[str, str]]] = {
    "users": [
        ("username", "text"),
        ("is_admin", "bool"),
        ("is_sso", "bool"),
        ("settings", "json"),
        ("password_hash", "text"),
        ("must_change_password", "bool"),
        ("sso_groups", "json"),
        ("roles", "json"),
        ("permissions", "json"),
        ("groups", "json"),
    ],
    "groups": [
        ("name", "text"),
        ("settings", "json"),
        ("roles", "json"),
        ("permissions", "json"),
    ],
    "roles": [
        ("name", "text"),
        ("description", "text"),
        ("permissions", "json"),
        ("is_builtin", "bool"),
    ],
    "volume_mounts": [
        ("name", "text"),
        ("host_path", "text"),
        ("container_path", "text"),
        ("read_only", "bool"),
        ("scope", "text"),
        ("target", "text"),
    ],
    "app_stores": [
        ("name", "text"),
        ("url", "text"),
    ],
    "installed_apps": [
        ("id", "text"),
        ("data", "json"),
    ],
    "sessions": [
        ("session_id", "text"),
        ("data", "json"),
    ],
    "auth_tokens": [
        ("token", "text"),
        ("username", "text"),
        ("expires_at", "float"),
    ],
    "public_shares": [
        ("share_id", "text"),
        ("data", "json"),
    ],
    "app_templates": [
        ("name", "text"),
        ("settings", "json"),
    ],
    "template_schema_settings": [
        ("name", "text"),
        ("sort_order", "int"),
        ("label", "text"),
        ("description", "text"),
        ("category", "text"),
        ("type", "text"),
        ("default", "text"),
        ("current", "text"),
        ("docker", "bool"),
        ("options", "json"),
    ],
    "pinned_behaviors": [
        ("id", "text"),
        ("username", "text"),
        ("created_at", "float"),
        ("data", "json"),
    ],
    "app_settings": [
        ("key", "text"),
        ("value", "text"),
    ],
}

# Tables are created/migrated in a stable order (matches db.rs TABLE_ORDER).
TABLE_ORDER: List[str] = list(COLUMNS.keys())

KIND_TO_PG = {
    "text": "text",
    "bool": "boolean",
    "int": "integer",
    "float": "double precision",
    "json": "jsonb",
}

PRIMARY_KEYS = {
    "users": "username",
    "groups": "name",
    "roles": "name",
    "volume_mounts": "name",
    "app_stores": "name",
    "installed_apps": "id",
    "sessions": "session_id",
    "auth_tokens": "token",
    "public_shares": "share_id",
    "app_templates": "name",
    "template_schema_settings": "name",
    "pinned_behaviors": "id",
    "app_settings": "key",
}


def q(ident: str) -> str:
    """Quote an identifier (handles reserved words like ``type``/``default``)."""
    return '"' + ident.replace('"', '""') + '"'


# ``DEFAULT`` literals for non-nullable columns that carry an entity default
# (mirrors the ``#[sea_orm(default_value = ...)]`` attributes in
# ``src/entities.rs``). A source row omits a column when the source database
# predates it (e.g. a pre-v0.6.1 SQLite database has no
# ``users.must_change_password``); the INSERT then leaves the column out and
# the target column must fall back to this default instead of tripping the
# NOT NULL constraint.
COLUMN_DEFAULTS: Dict[str, Dict[str, str]] = {
    "users": {
        "is_admin": "FALSE",
        "is_sso": "FALSE",
        "must_change_password": "FALSE",
    },
    "groups": {
        "settings": "'{}'::jsonb",
    },
    "roles": {
        "description": "''",
        "permissions": "'[]'::jsonb",
        "is_builtin": "FALSE",
    },
    "volume_mounts": {
        "host_path": "''",
        "container_path": "''",
        "read_only": "FALSE",
        "scope": "'none'",
        "target": "''",
    },
    "app_templates": {
        "settings": "'{}'::jsonb",
    },
    "template_schema_settings": {
        "sort_order": "0",
        "label": "''",
        "description": "''",
        "category": "'general'",
        "type": "'text'",
        "default": "''",
        "docker": "FALSE",
        "options": "'[]'::jsonb",
    },
    "pinned_behaviors": {
        "created_at": "0.0",
        "data": "'{}'::jsonb",
    },
    "app_settings": {
        "value": "''",
    },
}


def build_ddl(table: str) -> str:
    cols = COLUMNS[table]
    pk = PRIMARY_KEYS[table]
    defaults = COLUMN_DEFAULTS.get(table, {})
    defs = []
    for col, kind in cols:
        d = f"{q(col)} {KIND_TO_PG[kind]}"
        if col == pk:
            d += " PRIMARY KEY"
        elif not _nullable(table, col, kind):
            d += " NOT NULL"
            if col in defaults:
                d += f" DEFAULT {defaults[col]}"
        defs.append(d)
    return f"CREATE TABLE IF NOT EXISTS {q(table)} ({', '.join(defs)})"


def _nullable(table: str, col: str, kind: str) -> bool:
    """A column is nullable if it is a JSON column or an explicitly-Optional
    scalar in the entity (mirrors `src/entities.rs`). Every other scalar has a
    default in the entity and is NOT NULL."""
    if kind == "json":
        return True
    return (table, col) in {
        ("users", "password_hash"),
        ("template_schema_settings", "current"),
    }


# ---------------------------------------------------------------------------
# Conversion helpers
# ---------------------------------------------------------------------------
_DSN_USERINFO_RE = re.compile(r"//[^@/]+@")


def mask_dsn(dsn: str) -> str:
    """Mask the ``user:pass`` userinfo in a DSN's netloc for logging.

    Replaces only the userinfo component (``//user:pass@``) so the host,
    port, and database name stay readable: ``postgres://user:pass@h:5432/db``
    becomes ``postgres://***:***@h:5432/db``.
    """
    return _DSN_USERINFO_RE.sub("//***:***@", dsn, count=1) if "@" in dsn else dsn


def convert_cell(kind: str, value: Any) -> Any:
    """Map a SQLite cell onto the Postgres parameter type for a column kind."""
    if value is None:
        return None
    if kind == "json":
        # SQLite stores JSON as TEXT. Pass it back as a jsonb value; an empty
        # string becomes NULL (an empty string is not valid JSON).
        if isinstance(value, (bytes, bytearray)):
            value = value.decode("utf-8", "replace")
        if not isinstance(value, str):
            value = str(value)
        if value.strip() == "":
            return None
        return Json(value)
    if kind == "bool":
        return bool(value)
    if kind == "int":
        return int(value)
    if kind == "float":
        return float(value)
    # text
    if isinstance(value, (bytes, bytearray)):
        value = value.decode("utf-8", "replace")
    return value


# ---------------------------------------------------------------------------
# Core
# ---------------------------------------------------------------------------
def read_sqlite_table(conn: sqlite3.Connection, table: str) -> Tuple[Optional[List[str]], List[Dict[str, Any]]]:
    """Return (column_names_or_None, rows) for a table, or (None, []) if absent."""
    cur = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
    )
    if cur.fetchone() is None:
        return None, []
    cur = conn.execute(f"SELECT * FROM {q(table)}")
    names = [d[0] for d in cur.description]
    rows = [dict(zip(names, row)) for row in cur.fetchall()]
    return names, rows


def sqlite_table_exists(conn: sqlite3.Connection, table: str) -> bool:
    cur = conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
    )
    return cur.fetchone() is not None


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
    if not pg_dsn and not args.dry_run:
        sys.stderr.write(
            "Postgres DSN not given. Pass --postgres, or set VRECKAN_POSTGRES_DSN "
            "or VRECKAN_DATABASE_URL (e.g. postgres://vreckan:vreckan@postgres:5432/vreckan).\n"
        )
        return 2
    if not args.dry_run and not _PSYCOPG2:
        sys.stderr.write(
            "psycopg2 is required for the migration (not for --dry-run):  pip install psycopg2-binary\n"
        )
        return 2
    if not os.path.exists(sqlite_path):
        sys.stderr.write(f"SQLite database not found: {sqlite_path}\n")
        return 2

    print(f"Source SQLite : {sqlite_path}")
    if pg_dsn:
        print(f"Target Postgres: {mask_dsn(pg_dsn)}")

    sq = sqlite3.connect(sqlite_path)
    sq.row_factory = None

    total_read = 0
    summary: List[Tuple[str, int]] = []
    missing: List[str] = []

    # Pre-read all tables from SQLite.
    read: Dict[str, Tuple[Optional[List[str]], List[Dict[str, Any]]]] = {}
    for table in TABLE_ORDER:
        names, rows = read_sqlite_table(sq, table)
        read[table] = (names, rows)
        if names is None:
            missing.append(table)
        total_read += len(rows)
        summary.append((table, len(rows)))
    sq.close()

    if missing:
        print(f"Note: tables not present in SQLite (will be created empty): {', '.join(missing)}")
    print(f"Total rows in SQLite: {total_read}")
    for table, n in summary:
        print(f"  {table:28s} {n}")

    if args.dry_run:
        print("\n--dry-run: printing Postgres DDL and stopping (target not touched).")
        for table in TABLE_ORDER:
            print(build_ddl(table) + ";")
        return 0

    try:
        pg = psycopg2.connect(pg_dsn)
    except Exception as exc:  # noqa: BLE001 - scrub the DSN, then re-raise
        # psycopg2's error message (and therefore the traceback) can contain
        # the full DSN including the password; re-raise with the masked form.
        raise type(exc)(str(exc).replace(pg_dsn, mask_dsn(pg_dsn))) from exc
    pg.autocommit = False
    try:
        cur = pg.cursor()
        # 1. Create the schema (idempotent).
        for table in TABLE_ORDER:
            cur.execute(build_ddl(table))
        # 2. Replace contents of each table (single transaction).
        for table in TABLE_ORDER:
            names, rows = read[table]
            schema_cols = [c for c, _ in COLUMNS[table]]
            cur.execute(sql.SQL("DELETE FROM {}").format(sql.Identifier(table)))
            if not rows:
                continue
            # Only insert columns present in BOTH the schema and the source rows.
            insert_cols = [c for c in schema_cols if names and c in names]
            if not insert_cols:
                continue
            col_ident = sql.SQL(", ").join(sql.Identifier(c) for c in insert_cols)
            placeholders = sql.SQL(", ").join(
                sql.Placeholder() for _ in insert_cols
            )
            insert_sql = sql.SQL("INSERT INTO {} ({}) VALUES ({})").format(
                sql.Identifier(table), col_ident, placeholders
            )
            for row in rows:
                params = [
                    convert_cell(kind, row.get(col))
                    for col, kind in COLUMNS[table]
                    if col in insert_cols
                ]
                cur.execute(insert_sql, params)
        pg.commit()
        print("\nMigration complete.")
        for table in TABLE_ORDER:
            print(f"  {table:28s} {len(read[table][1])} rows")
    except Exception:
        pg.rollback()
        raise
    finally:
        pg.close()

    return 0


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Migrate a Vreckan SQLite configuration database to Postgres.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage", 1)[1] if "Usage" in __doc__ else None,
    )
    ap.add_argument("--sqlite", help="Path to the existing SQLite database file (or set VRECKAN_SQLITE_PATH).")
    ap.add_argument(
        "--postgres",
        help="Postgres DSN (e.g. postgres://user:pass@host:5432/db) (or set VRECKAN_POSTGRES_DSN / VRECKAN_DATABASE_URL).",
    )
    ap.add_argument("--dry-run", action="store_true", help="Print the Postgres DDL and row counts without touching the target.")
    args = ap.parse_args()
    sys.exit(run(args))


if __name__ == "__main__":
    main()
