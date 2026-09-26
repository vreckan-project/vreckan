"""Shared fixtures for the Vreckan test suite.

The server reads every ``VRECKAN_*`` environment variable at import time
and builds its database engine at import time, so the environment must be
pointed at a throwaway directory tree *before* any ``app.*`` module is
imported. Per-test isolation then works like this:

* every filesystem setting is re-pointed (via monkeypatch) at the test's
  own ``tmp_path`` sandbox,
* the shared SQLite database is wiped,
* every in-memory global is cleared,
* the module-level ``asyncio.Lock`` objects are replaced with fresh ones
  (locks bind to the event loop that first uses them, and each test runs
  on its own loop),
* the startup steps that would touch the network or the Docker daemon are
  monkeypatched to no-ops, so the real ASGI lifespan can run hermetically.

The ``client`` fixture boots the real lifespan once per test; the
``secure_client`` fixture additionally logs in as the bootstrap admin and
performs the E2EE handshake, mirroring the browser client's crypto.
"""
from __future__ import annotations

import asyncio
import base64
import json
import os
import re
import sqlite3
import sys
import tempfile
from pathlib import Path
from sqlalchemy import inspect, text

import pytest
import yaml
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from cryptography.hazmat.primitives.ciphers.aead import AESGCM

# tests/ now lives at the repo root, so the import root is the repo root
# itself (the app package is ./server).
SERVER_DIR = os.path.dirname(os.path.abspath(__file__))
if SERVER_DIR not in sys.path:
    sys.path.insert(0, SERVER_DIR)

# --- Session-scoped sandbox, configured BEFORE any app import ---------------
TEST_ROOT = Path(tempfile.mkdtemp(prefix="vreckan-tests-"))
DB_PATH = TEST_ROOT / "test.db"

# A dead local port: any store URL pointed here fails instantly with a
# connection refusal instead of hanging on a real network fetch.
DEAD_URL = "http://127.0.0.1:9/"


def _generate_server_key() -> Path:
    key_dir = TEST_ROOT / "ssl"
    key_dir.mkdir(parents=True, exist_ok=True)
    key_path = key_dir / "server_key.pem"
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    key_path.write_bytes(
        key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
    )
    return key_path


_KEY_PATH = _generate_server_key()

# The database backend is selected by VRECKAN_DATABASE_URL. By default the
# suite runs against a throwaway SQLite file (the development backend); set
# the variable to a postgresql+asyncpg:// URL to run the identical suite
# against PostgreSQL. The URL must be fixed before the app is imported
# because the engine is created at import time.
if not os.environ.get("VRECKAN_DATABASE_URL"):
    os.environ["VRECKAN_DATABASE_URL"] = f"sqlite+aiosqlite:///{DB_PATH}"

os.environ.update(
    {
        "VRECKAN_SERVER_PRIVATE_KEY_PATH": str(_KEY_PATH),
        "VRECKAN_APP_RESOURCE_PATH": DEAD_URL + "apps.yml",
        "VRECKAN_AUTO_UPDATE_APPS": "false",
        "VRECKAN_OIDC_ENABLED": "false",
        # Pin the SSO group lists to empty so tests are deterministic no matter
        # what the host/container environment has configured (the app reads
        # these from the environment at import time).
        "VRECKAN_OIDC_ADMIN_GROUPS": "",
        "VRECKAN_OIDC_USER_GROUPS": "",
        "VRECKAN_LOG_LEVEL": "WARNING",
    }
)

import server.api as api_module  # noqa: E402  (env must be set first)
from server import db, user_manager, volume_mount_manager  # noqa: E402
from server.models import AppStore  # noqa: E402
from server.settings import settings  # noqa: E402

# Path settings re-pointed per test (see ``isolate``).
PATH_SETTINGS = [
    "storage_path",
    "upload_dir",
    "app_icons_path",
    "home_templates_path",
    "public_storage_path",
    "autostart_cache_path",
    "app_store_cache_path",
]

# asyncio locks live in the api module and bind to the first loop that
# uses them; each test runs on a fresh loop, so each test gets fresh locks.
LOCK_NAMES = ["SESSIONS_LOCK", "METADATA_LOCK", "AUTH_TOKENS_LOCK", "OIDC_STATES_LOCK"]


def _wipe_db() -> None:
    """Delete every row from every table (synchronous; runs between tests).

    Drives the app's own engine so it works identically on SQLite and
    PostgreSQL. Tables that do not exist yet (before the first lifespan has
    run ``init_db``) are skipped, mirroring the previous sqlite_master guard.
    """
    async def _wipe():
        async with db.engine.begin() as conn:
            def _wipe_sync(sync_conn):
                inspector = inspect(sync_conn)
                for table in db.TABLE_ORDER:
                    if inspector.has_table(table):
                        sync_conn.execute(text(f"DELETE FROM {table}"))
            await conn.run_sync(_wipe_sync)

    asyncio.run(_wipe())


# --- Direct-DB access shim (sqlite3-compatible) ----------------------------
# Several tests assert on the database by opening it directly (the way a DBA
# would) rather than through the API. That used to mean ``sqlite3.connect``;
# the shim below gives the same tiny API surface on whichever backend the
# suite is pointed at, so those tests run unchanged on SQLite and PostgreSQL.

def _convert_params(sql, params):
    """Translate sqlite3 ``?`` placeholders to SQLAlchemy named parameters."""
    if not params:
        return sql, {}
    names = []

    def _repl(_match):
        name = f"p{len(names)}"
        names.append(name)
        return f":{name}"

    return re.sub(r"\?", _repl, sql), {n: v for n, v in zip(names, params)}


def _normalize_row(row):
    """Re-serialize JSONB values (dict/list) back to JSON text.

    On PostgreSQL a JSONB column is returned as an already-parsed Python
    object; on SQLite the same column is stored as TEXT and comes back as a
    JSON string. Re-serializing keeps ``json.loads(...)`` in the tests
    behaving identically on both backends.
    """
    return tuple(json.dumps(v) if isinstance(v, (dict, list)) else v for v in row)


class _PgCursor:
    """A minimal stand-in for ``sqlite3.Cursor`` over already-fetched rows."""

    def __init__(self, rows):
        self._rows = list(rows)

    def fetchone(self):
        if not self._rows:
            return None
        return self._rows.pop(0)

    def fetchall(self):
        rows, self._rows = self._rows, []
        return rows

    def __iter__(self):
        return iter(self._rows)


class DbConn:
    """A ``sqlite3.Connection``-compatible handle for the active backend.

    Implements only the surface the tests use: ``execute()``, ``commit()``,
    ``close()``, ``upsert()``, and the cursor's ``fetchone()``/``fetchall()``/
    iteration. On PostgreSQL each ``execute()`` drives the app's async engine
    in its own transaction (the engine uses a NullPool, so a fresh connection
    per call is cheap and safe across the per-test event loops).
    """

    def __init__(self, db_path=None):
        self._is_pg = db.engine.dialect.name == "postgresql"
        self._sqlite = None
        if not self._is_pg:
            self._sqlite = sqlite3.connect(db_path)

    def execute(self, sql, params=None):
        if self._is_pg:
            return self._pg_execute(sql, params)
        return self._sqlite.execute(sql, params or ())

    def commit(self):
        if self._is_pg:
            return  # each execute() already committed in its own transaction
        self._sqlite.commit()

    def close(self):
        if self._is_pg:
            return
        self._sqlite.close()

    def upsert(self, table, values, conflict_cols):
        """Insert-or-update a row (SQLite ``INSERT OR REPLACE``)."""
        cols = list(values)
        col_list = ", ".join(cols)
        if self._is_pg:
            placeholders = ", ".join(f":{c}" for c in cols)
            non_conflict = [c for c in cols if c not in conflict_cols]
            sql = f"INSERT INTO {table} ({col_list}) VALUES ({placeholders})"
            if non_conflict:
                updates = ", ".join(f"{c} = EXCLUDED.{c}" for c in non_conflict)
                sql += f" ON CONFLICT ({', '.join(conflict_cols)}) DO UPDATE SET {updates}"
            else:
                sql += f" ON CONFLICT ({', '.join(conflict_cols)}) DO NOTHING"
            self._pg_execute(sql, values)
        else:
            placeholders = ", ".join("?" for _ in cols)
            self._sqlite.execute(
                f"INSERT OR REPLACE INTO {table} ({col_list}) VALUES ({placeholders})",
                tuple(values.values()),
            )

    def _pg_execute(self, sql, params=None):
        if isinstance(params, dict):
            # Named placeholders (e.g. the upsert helper) pass a mapping
            # straight through; only positional ``?`` params need translating.
            bind = params
        else:
            sql, bind = _convert_params(sql, params)
        if "sqlite_master" in sql:
            sql = (
                "SELECT table_name FROM information_schema.tables "
                "WHERE table_schema = 'public'"
            )

        async def _run():
            async with db.engine.begin() as conn:
                result = await conn.execute(text(sql), bind)
                # DML (INSERT/UPDATE/DELETE) results carry no rows; asyncpg
                # closes them eagerly, so only fetch when rows are expected.
                if result.returns_rows:
                    return [tuple(row) for row in result.fetchall()]
                return []

        rows = asyncio.run(_run())
        return _PgCursor([_normalize_row(r) for r in rows])


def db_conn(db_path=None):
    """Open a ``DbConn`` for the active backend (see :class:`DbConn`)."""
    return DbConn(db_path)


def _clear_inmemory() -> None:
    api_module.SESSIONS_DB.clear()
    api_module.CRYPTO_SESSIONS.clear()
    api_module.INSTALLED_APPS.clear()
    api_module.APP_STORES.clear()
    api_module.APP_TEMPLATES.clear()
    api_module.AVAILABLE_GPUS.clear()
    api_module.GLOBAL_DEFAULT_GPU = None
    api_module.PUBLIC_SHARES_METADATA.clear()
    api_module.IMAGE_METADATA.clear()
    api_module.DELETION_TASKS.clear()
    api_module.PULL_STATUS.clear()
    api_module.PULL_PROGRESS.clear()
    api_module.SYSTEM_STATS_CACHE["data"] = None
    api_module.SYSTEM_STATS_CACHE["timestamp"] = 0
    api_module.PATH_PREFIX_MAP.clear()
    api_module.DOWNLOAD_TOKENS.clear()
    api_module.AUTH_TOKENS.clear()
    api_module.OIDC_STATES.clear()
    api_module.TEMPLATE_SCHEMA["settings"].clear()
    user_manager.USER_DATA.clear()
    user_manager.GROUP_DATA.clear()
    user_manager.ROLE_DATA.clear()
    volume_mount_manager.VOLUME_MOUNTS.clear()
    import server.collaboration as collaboration

    collaboration.ROOM_CONNECTIONS.clear()
    collaboration.TOKEN_ENDPOINT_CACHE.clear()


@pytest.fixture(autouse=True)
def isolate(tmp_path, monkeypatch):
    """Per-test sandbox: fresh paths, wiped DB, cleared globals, fresh locks."""
    layout = {
        "storage_path": tmp_path / "storage",
        "upload_dir": tmp_path / "storage" / "uploads",
        "app_icons_path": tmp_path / "storage" / "icons",
        "home_templates_path": tmp_path / "storage" / "home_templates",
        "public_storage_path": tmp_path / "storage" / "public",
        "autostart_cache_path": tmp_path / "config" / "autostart_cache",
        "app_store_cache_path": tmp_path / "config" / "app_stores_cache",
    }
    for name, path in layout.items():
        monkeypatch.setattr(settings, name, str(path))
    monkeypatch.setattr(settings, "app_resource_path", DEAD_URL + "apps.yml")
    monkeypatch.setattr(settings, "auto_update_apps", False)

    async def _noop(*args, **kwargs):
        return None

    # Startup steps that would reach the network or the Docker daemon.
    monkeypatch.setattr(api_module, "_update_all_app_store_caches", _noop)
    monkeypatch.setattr(api_module, "_update_all_autostart_caches", _noop)
    monkeypatch.setattr(api_module, "_get_and_cache_image_metadata", _noop)
    monkeypatch.setattr(api_module, "_do_pull_and_cache_image", _noop)

    # Fresh locks: asyncio primitives bind to the first loop that uses them.
    for lock_name in LOCK_NAMES:
        monkeypatch.setattr(api_module, lock_name, asyncio.Lock())

    _wipe_db()
    _clear_inmemory()
    yield
    _wipe_db()
    _clear_inmemory()


@pytest.fixture
def db_path():
    """The shared SQLite file the app's engine points at."""
    return str(DB_PATH)


@pytest.fixture
def client(isolate):
    """A live TestClient: the real lifespan runs against the wiped state.

    Redirects are NOT followed (starlette's default is to follow them), so
    tests can assert on intermediate 303 responses such as the one-time
    public-download token redirect.

    After the lifespan runs, the VRECKAN_OIDC_* rows it seeded into
    ``app_settings`` (via ``sync_app_settings_from_env``) are cleared. This
    lets tests monkeypatch ``settings.oidc_*`` freely: ``get_setting`` reads
    the DB first, so a seeded row would otherwise shadow the monkeypatched
    value.
    """
    from fastapi.testclient import TestClient

    with TestClient(api_module.api_app, follow_redirects=False) as http:
        conn = db_conn(str(DB_PATH))
        conn.execute("DELETE FROM app_settings WHERE key LIKE 'VRECKAN_OIDC_%'")
        conn.commit()
        conn.close()
        yield http


class SecureClient:
    """A logged-in, E2EE-capable test client (mirrors the browser client).

    Authentication is the HttpOnly cookie set by ``/api/auth/login`` — the
    TestClient sends it automatically. Every request body and JSON response
    is wrapped in AES-256-GCM under the key established by the handshake.
    """

    def __init__(self, http):
        self.http = http
        self.session_id = None
        self.aes_key = None

    def login(self, username: str, password: str):
        return self.http.post(
            "/api/auth/login", json={"username": username, "password": password}
        )

    def handshake(self):
        pub_pem = self.http.get("/api/handshake/public_key").json()["server_public_key"]
        self.server_pub = serialization.load_pem_public_key(pub_pem.encode())
        init = self.http.post("/api/handshake/initiate").json()
        # Prove the server holds the private key matching the public one.
        self.server_pub.verify(
            base64.b64decode(init["signature"]),
            base64.b64decode(init["nonce"]),
            padding.PSS(mgf=padding.MGF1(hashes.SHA256()), salt_length=32),
            hashes.SHA256(),
        )
        self.aes_key = os.urandom(32)
        wrapped = self.server_pub.encrypt(
            self.aes_key,
            padding.OAEP(
                mgf=padding.MGF1(hashes.SHA256()), algorithm=hashes.SHA256(), label=None
            ),
        )
        resp = self.http.post(
            "/api/handshake/exchange",
            json={"encrypted_session_key": base64.b64encode(wrapped).decode()},
        )
        assert resp.status_code == 200, resp.text
        self.session_id = resp.json()["session_id"]

    def call(self, method, url, body=None, extra_headers=None):
        """Perform an encrypted API call; returns (status_code, decrypted_json)."""
        headers = {}
        if self.session_id:
            headers["X-Session-ID"] = self.session_id
        if extra_headers:
            headers.update(extra_headers)
        content = None
        if body is not None:
            iv = os.urandom(12)
            ct = AESGCM(self.aes_key).encrypt(iv, json.dumps(body).encode(), None)
            content = json.dumps(
                {
                    "iv": base64.b64encode(iv).decode(),
                    "ciphertext": base64.b64encode(ct).decode(),
                }
            )
            headers["Content-Type"] = "application/json"
        resp = self.http.request(method, url, headers=headers, content=content)
        if resp.status_code == 204 or not resp.content:
            return resp.status_code, None
        payload = resp.json()
        if isinstance(payload, dict) and "ciphertext" in payload:
            plain = AESGCM(self.aes_key).decrypt(
                base64.b64decode(payload["iv"]), base64.b64decode(payload["ciphertext"]), None
            )
            return resp.status_code, json.loads(plain)
        return resp.status_code, payload


@pytest.fixture
def secure_client(client):
    """A SecureClient already logged in as the bootstrap admin, E2EE ready."""
    sc = SecureClient(client)
    resp = sc.login("admin", "admin1234")
    assert resp.status_code == 200, resp.text
    sc.handshake()
    return sc


STORE_APP = {
    "id": "firefox",
    "name": "Firefox",
    "logo": "https://example.invalid/firefox.png",
    "url": "https://example.invalid/firefox",
    "provider": "docker",
    "provider_config": {
        "image": "lscr.io/linuxserver/firefox:latest",
        "port": 3000,
        "nvidia_support": True,
        "dri3_support": True,
        "type": "browser",
        "url_support": True,
        "open_support": False,
        "extensions": [["html", "htm"], "pdf"],
        "autostart": False,
    },
}


@pytest.fixture
def store_with_firefox(isolate):
    """Register a store whose cached catalog contains the Firefox entry."""
    cache_dir = Path(settings.app_store_cache_path)
    cache_dir.mkdir(parents=True, exist_ok=True)
    (cache_dir / "Test Store.yml").write_text(
        yaml.safe_dump({"apps": [STORE_APP]}), encoding="utf-8"
    )
    # Persist the store so a lifespan (re)load keeps it. Ensure the table
    # exists in case this fixture runs before the TestClient lifespan.
    conn = db_conn(str(DB_PATH))
    conn.execute(
        "CREATE TABLE IF NOT EXISTS app_stores (name VARCHAR(255) PRIMARY KEY, url TEXT)"
    )
    conn.upsert("app_stores", {"name": "Test Store", "url": DEAD_URL + "apps.yml"}, ["name"])
    conn.commit()
    conn.close()
    api_module.APP_STORES.append(AppStore(name="Test Store", url=DEAD_URL + "apps.yml"))
    return STORE_APP


# --- Dedicated PostgreSQL backend (for tests/test_postgres.py) --------------
# The Postgres-specific tests need a live PostgreSQL server. Resolution order:
#   1. VRECKAN_TEST_POSTGRES_URL — use an externally provided URL as-is.
#   2. VRECKAN_DATABASE_URL — if the whole suite is already running against
#      PostgreSQL, reuse that server (no extra container).
#   3. Otherwise spin up a throwaway postgres:16 container on the same Docker
#      network as the test process and tear it down at session end.
def _wait_for_postgres(url: str, timeout: int = 60) -> None:
    """Block until PostgreSQL accepts connections at ``url``."""
    import asyncio
    import time

    import asyncpg

    async def _probe():
        dsn = url.replace("postgresql+asyncpg://", "postgresql://")
        conn = await asyncpg.connect(dsn)
        await conn.close()

    deadline = time.time() + timeout
    last_err = None
    while time.time() < deadline:
        try:
            asyncio.run(_probe())
            return
        except Exception as err:  # noqa: BLE001 - any connection error means "not yet"
            last_err = err
            time.sleep(1)
    raise RuntimeError(f"PostgreSQL did not become ready at {url}: {last_err}")


@pytest.fixture(scope="session")
def postgres_url():
    """A ``postgresql+asyncpg://`` URL for the dedicated Postgres tests."""
    url = os.environ.get("VRECKAN_TEST_POSTGRES_URL")
    if url:
        yield url
        return
    if os.environ.get("VRECKAN_DATABASE_URL", "").startswith("postgresql"):
        yield os.environ["VRECKAN_DATABASE_URL"]
        return

    import time

    import docker

    client = docker.from_env()
    network_name = os.environ.get("VRECKAN_TEST_POSTGRES_NETWORK", "vreckan_default")
    try:
        client.networks.get(network_name)
    except Exception:
        client.networks.create(network_name)

    name = f"vreckan-test-pg-{os.getpid()}"
    container = client.containers.run(
        "postgres:16",
        detach=True,
        name=name,
        environment={
            "POSTGRES_USER": "vreckan",
            "POSTGRES_PASSWORD": "vreckan",
            "POSTGRES_DB": "vreckan",
        },
    )
    try:
        client.networks.get(network_name).connect(container)
        ip = None
        for _ in range(60):
            container.reload()
            nets = container.attrs["NetworkSettings"]["Networks"]
            if network_name in nets and nets[network_name].get("IPAddress"):
                ip = nets[network_name]["IPAddress"]
                break
            time.sleep(1)
        assert ip, f"postgres container did not join network {network_name}"
        url = f"postgresql+asyncpg://vreckan:vreckan@{ip}:5432/vreckan"
        _wait_for_postgres(url)
        yield url
    finally:
        try:
            container.stop(timeout=5)
            container.remove(force=True)
        except Exception:
            pass
