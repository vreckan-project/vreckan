"""Configuration database: init/migrate, dump/restore, persistence round-trips."""
import asyncio
import json
import time

import pytest

import server.api as api_module
from server import db, user_manager
from conftest import DB_PATH, _wipe_db, db_conn


def test_init_and_migrate_are_idempotent(db_path):
    asyncio.run(db.init_db())
    asyncio.run(db.init_db())
    asyncio.run(db.migrate_db())
    asyncio.run(db.migrate_db())
    conn = db_conn(db_path)
    tables = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    conn.close()
    for table in db.TABLE_ORDER:
        assert table in tables


def test_dump_restore_roundtrip(db_path, tmp_path):
    async def seed():
        await user_manager._generate_default_admin()
        await user_manager.create_user("alice", {"public_sharing": True})
        await user_manager.set_password("alice", "alicepass1")
        await user_manager.write_group_file("ops", {"gpu": False})

    asyncio.run(seed())
    dump_path = str(tmp_path / "backup.json")
    asyncio.run(db.db_dump(dump_path))
    # Wipe everything, then restore from the dump.
    _wipe_db()
    asyncio.run(db.db_restore(dump_path))
    conn = db_conn(db_path)
    users = {r[0]: r[1] for r in conn.execute("SELECT username, is_admin FROM users")}
    alice_hash = conn.execute(
        "SELECT password_hash FROM users WHERE username = 'alice'"
    ).fetchone()[0]
    ops_settings = conn.execute("SELECT settings FROM groups WHERE name = 'ops'").fetchone()[0]
    conn.close()
    assert set(users) == {"admin", "alice"}
    assert users["admin"] == 1 and users["alice"] == 0
    assert alice_hash and alice_hash.startswith("$argon2")
    assert json.loads(ops_settings) == {"gpu": False}


def test_sessions_roundtrip(db_path):
    async def scenario():
        api_module.SESSIONS_DB["s-1"] = {"instance_id": "ctr-1", "username": "admin"}
        await api_module.save_sessions_to_disk()
        api_module.SESSIONS_DB.clear()
        await api_module.load_sessions_from_disk()

    asyncio.run(scenario())
    assert api_module.SESSIONS_DB == {"s-1": {"instance_id": "ctr-1", "username": "admin"}}


def test_auth_tokens_prune_expired(db_path):
    async def scenario():
        api_module.AUTH_TOKENS["fresh"] = {"username": "admin", "expires_at": time.time() + 3600}
        api_module.AUTH_TOKENS["stale"] = {"username": "admin", "expires_at": time.time() - 10}
        await api_module.save_auth_tokens_to_disk()
        await api_module.load_auth_tokens_from_disk()

    asyncio.run(scenario())
    assert list(api_module.AUTH_TOKENS) == ["fresh"]


def test_concurrent_saves_do_not_lose_data(db_path):
    async def scenario():
        async def writer(i):
            api_module.SESSIONS_DB[f"s-{i}"] = {"i": i}
            await api_module.save_sessions_to_disk()

        await asyncio.gather(*(writer(i) for i in range(10)))
        api_module.SESSIONS_DB.clear()
        await api_module.load_sessions_from_disk()

    asyncio.run(scenario())
    assert set(api_module.SESSIONS_DB) == {f"s-{i}" for i in range(10)}
