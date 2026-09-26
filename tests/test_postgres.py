"""PostgreSQL-specific configuration-database tests.

These exercise the code paths that branch on the database dialect — the
``migrate_db()`` ALTER statements (``JSONB`` / ``BOOLEAN DEFAULT FALSE`` on
Postgres vs ``TEXT`` / ``DEFAULT 0`` on SQLite), the JSON→JSONB column
mapping, and the backend-agnostic ``db_dump``/``db_restore`` round-trip.

Each test gets a dedicated engine pointed at the session-scoped
``postgres_url`` fixture (see ``conftest.py``), which either reuses an
externally-provided Postgres server or spins up a throwaway ``postgres:16``
container. The app's ``db.engine`` / ``db.async_session_factory`` are
monkeypatched so every ``server.*`` module under test talks to that engine.

Run the *entire* suite against PostgreSQL with::

    VRECKAN_DATABASE_URL=postgresql+asyncpg://user:pass@host:5432/db \
        python -m pytest tests/

These focused tests run on either backend (they bring their own engine).
"""
import asyncio
import json

import pytest
from sqlalchemy import inspect, text

import server.api as api_module
from server import db, user_manager
from conftest import _wipe_db


@pytest.fixture
def pg_engine(isolate, postgres_url, monkeypatch):
    """A dedicated engine + session factory for the Postgres tests.

    Patches ``db.engine`` and ``db.async_session_factory`` so the app's
    persistence layer (and every manager that uses it) targets the test
    database. Depends on ``isolate`` so it is set up after (and torn down
    before) the per-test sandbox. The schema is dropped and recreated for
    every test, so each starts from a clean database.
    """
    from sqlalchemy.ext.asyncio import create_async_engine, async_sessionmaker
    from sqlalchemy.pool import NullPool

    engine = create_async_engine(postgres_url, echo=False, poolclass=NullPool)

    async def _init():
        async with engine.begin() as conn:
            def _sync(sc):
                db.Base.metadata.drop_all(sc)
                db.Base.metadata.create_all(sc)
            await conn.run_sync(_sync)

    asyncio.run(_init())

    monkeypatch.setattr(db, "engine", engine)
    monkeypatch.setattr(
        db, "async_session_factory", async_sessionmaker(engine, expire_on_commit=False)
    )
    yield engine
    asyncio.run(engine.dispose())


def _cols(engine, table):
    """Return {column_name: type_class_name} for ``table`` via the engine.

    Uses the type's class name (e.g. ``"JSONB"``) rather than ``str(type)``
    because ``str(JSONB(...))`` returns ``"JSON"`` (the parent class).
    """
    async def _inspect():
        async with engine.connect() as conn:
            def _sync(sc):
                return {c["name"]: type(c["type"]).__name__ for c in inspect(sc).get_columns(table)}
            return await conn.run_sync(_sync)
    return asyncio.run(_inspect())


def test_pg_schema_and_jsonb_types(pg_engine):
    """init_db creates every table; JSON columns are real JSONB on Postgres."""
    async def _tables():
        async with pg_engine.connect() as conn:
            def _sync(sc):
                return set(inspect(sc).get_table_names())
            return await conn.run_sync(_sync)
    tables = asyncio.run(_tables())
    for table in db.TABLE_ORDER:
        assert table in tables, f"missing table {table}"

    # The models use the generic ``sqlalchemy.JSON`` type, which compiles to
    # Postgres's ``json`` type (reported as "JSON") — a real JSON type, not
    # the "TEXT" fallback SQLite uses. (The ``migrate_db`` path separately
    # adds explicit ``JSONB`` columns; that is covered by the legacy-schema
    # test below.)
    user_cols = _cols(pg_engine, "users")
    for col in ("settings", "sso_groups", "roles", "permissions", "groups"):
        assert user_cols[col] in ("JSON", "JSONB"), (
            f"users.{col} is {user_cols[col]}, expected a JSON type"
        )
    group_cols = _cols(pg_engine, "groups")
    for col in ("settings", "roles", "permissions"):
        assert group_cols[col] in ("JSON", "JSONB"), (
            f"groups.{col} is {group_cols[col]}, expected a JSON type"
        )


def test_pg_migrate_idempotent_on_fresh_schema(pg_engine):
    """migrate_db is a no-op when the schema is already current."""
    asyncio.run(db.migrate_db())
    asyncio.run(db.migrate_db())  # second run must not raise
    user_cols = _cols(pg_engine, "users")
    assert "is_sso" in user_cols
    assert "sso_groups" in user_cols


def test_pg_migrate_adds_columns_to_legacy_schema(pg_engine):
    """The dialect-specific ALTER path: a pre-migration schema gains the new
    columns as JSONB / BOOLEAN NOT NULL DEFAULT FALSE."""
    async def _make_legacy():
        async with pg_engine.begin() as conn:
            await conn.execute(text("DROP TABLE IF EXISTS groups"))
            await conn.execute(text("DROP TABLE IF EXISTS users"))
            # A users table as it shipped before the is_sso/sso_groups/RBAC
            # columns existed.
            await conn.execute(text(
                "CREATE TABLE users ("
                " username VARCHAR(255) PRIMARY KEY,"
                " is_admin BOOLEAN,"
                " settings JSONB,"
                " password_hash TEXT"
                ")"
            ))
            await conn.execute(text(
                "CREATE TABLE groups ("
                " name VARCHAR(255) PRIMARY KEY,"
                " settings JSONB"
                ")"
            ))
    asyncio.run(_make_legacy())

    asyncio.run(db.migrate_db())
    asyncio.run(db.migrate_db())  # idempotent

    user_cols = _cols(pg_engine, "users")
    for col in ("sso_groups", "roles", "permissions", "groups"):
        assert col in user_cols, f"users.{col} not added by migration"
        assert user_cols[col] == "JSONB", (
            f"users.{col} is {user_cols[col]}, expected JSONB"
        )
    assert "is_sso" in user_cols
    assert user_cols["is_sso"] == "BOOLEAN"

    group_cols = _cols(pg_engine, "groups")
    for col in ("roles", "permissions"):
        assert col in group_cols, f"groups.{col} not added by migration"
        assert group_cols[col] == "JSONB"


def test_pg_jsonb_roundtrip(pg_engine):
    """dict/list values survive a write + read through JSONB columns."""
    async def _scenario():
        await user_manager.create_user(
            "bob",
            {"public_sharing": True, "storage_limit": 512},
        )
        await user_manager._db_save_user_row(
            "bob",
            is_admin=False,
            settings={"public_sharing": True, "storage_limit": 512},
            roles=["user", "operator"],
            permissions=["admin.apps"],
            groups=["devs", "ops"],
            sso_groups=["vdi-users"],
        )
        await user_manager.write_group_file(
            "devs", {"gpu": True}, roles=["user"], permissions=["admin.templates"]
        )

    asyncio.run(_scenario())

    async def _read():
        async with db.async_session_factory() as session:
            from sqlalchemy import select
            from server.db import User, Group
            u = (await session.execute(select(User).where(User.username == "bob"))).scalar_one()
            g = (await session.execute(select(Group).where(Group.name == "devs"))).scalar_one()
            return u, g

    u, g = asyncio.run(_read())
    assert u.settings == {"public_sharing": True, "storage_limit": 512}
    assert u.roles == ["user", "operator"]
    assert u.permissions == ["admin.apps"]
    assert u.groups == ["devs", "ops"]
    assert u.sso_groups == ["vdi-users"]
    assert g.settings == {"gpu": True}
    assert g.roles == ["user"]
    assert g.permissions == ["admin.templates"]


def test_pg_dump_restore_roundtrip(pg_engine, tmp_path):
    """db_dump → wipe → db_restore preserves rows across the JSONB backend."""
    async def _seed():
        await user_manager._generate_default_admin()
        await user_manager.create_user("carol", {"public_sharing": True})
        await user_manager.set_password("carol", "carolpass1")
        await user_manager.write_group_file("ops", {"gpu": False})

    asyncio.run(_seed())
    dump_path = str(tmp_path / "backup.json")
    asyncio.run(db.db_dump(dump_path))

    _wipe_db()
    asyncio.run(db.db_restore(dump_path))

    async def _read():
        async with db.async_session_factory() as session:
            from sqlalchemy import select
            from server.db import User, Group
            users = {
                r.username: r.is_admin
                for r in (await session.execute(select(User))).scalars().all()
            }
            carol_hash = (
                await session.execute(select(User.password_hash).where(User.username == "carol"))
            ).scalar_one()
            ops_settings = (
                await session.execute(select(Group.settings).where(Group.name == "ops"))
            ).scalar_one()
            return users, carol_hash, ops_settings

    users, carol_hash, ops_settings = asyncio.run(_read())
    assert set(users) == {"admin", "carol"}
    assert users["admin"] is True and users["carol"] is False
    assert carol_hash and carol_hash.startswith("$argon2")
    assert json.loads(ops_settings) if isinstance(ops_settings, str) else ops_settings == {"gpu": False}


def test_pg_sessions_roundtrip(pg_engine):
    """Session persistence (JSONB ``data`` column) round-trips."""
    async def _scenario():
        api_module.SESSIONS_DB["s-pg"] = {"instance_id": "ctr-pg", "username": "admin"}
        await api_module.save_sessions_to_disk()
        api_module.SESSIONS_DB.clear()
        await api_module.load_sessions_from_disk()

    asyncio.run(_scenario())
    assert api_module.SESSIONS_DB == {"s-pg": {"instance_id": "ctr-pg", "username": "admin"}}
