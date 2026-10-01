"""Tests for the admin app-store, installed-app, and meta-app endpoints.

App stores are remote YAML catalogs (``{apps: [...]}``) that admins
register, browse, and install from.  Catalogs are cached on disk under
``app_store_cache_path`` and only re-fetched on an explicit refresh, so
most of these tests run fully offline: a dead URL plus a pre-seeded cache
proves the cache is honoured, and a tiny local HTTP server stands in for a
real store when a network round-trip is required.

The happy paths (install, update, delete, schema) are already covered by
the Tier-1 smoke tests; this file focuses on the error paths and the
caching semantics those tests cannot reach.  The launch endpoints
themselves are out of scope — they spawn real containers.
"""
from __future__ import annotations

import base64
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import quote

import pytest
import yaml

import server.api as api_module
import server.routers.admin as admin_router_module
from conftest import DEAD_URL, STORE_APP

# Dead local port: any fetch against this URL fails with an instant
# connection refusal, which keeps the tests hermetic.
STORE_URL = DEAD_URL + "apps.yml"

# A 1x1 transparent PNG, base64 — small enough to inline, valid enough for
# the icon round-trip assertions.
PNG_B64 = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0CgAAAAASUVORK5CYII="
)


# ---------------------------------------------------------------------------
# Local stand-in for a remote app store
# ---------------------------------------------------------------------------
class StoreServer:
    """A throwaway HTTP server serving a fixed app-store YAML payload.

    Mirrors the ``MockIdp`` pattern from test_oidc.py: a ThreadingHTTPServer
    on an ephemeral port in a daemon thread, plus a hit counter so tests can
    prove whether — and how often — the app actually fetched the URL.
    """

    def __init__(self, content: str):
        self.content = content.encode("utf-8")
        self.hits = 0
        self.url = ""
        self._lock = threading.Lock()
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> "StoreServer":
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                with outer._lock:
                    outer.hits += 1
                self.send_response(200)
                self.send_header("Content-Type", "application/x-yaml")
                self.send_header("Content-Length", str(len(outer.content)))
                self.end_headers()
                self.wfile.write(outer.content)

            def log_message(self, format, *args):  # silence request logging
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}/apps.yml"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        return self

    def stop(self):
        if self._server:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
        if self._thread:
            self._thread.join(timeout=5)
            self._thread = None


@pytest.fixture
def store_server():
    """Factory: start a local store server serving the given YAML text."""
    servers: list[StoreServer] = []

    def _start(content: str) -> StoreServer:
        server = StoreServer(content).start()
        servers.append(server)
        return server

    yield _start
    for server in servers:
        server.stop()


# ---------------------------------------------------------------------------
# Store CRUD
# ---------------------------------------------------------------------------
def test_list_stores_seeds_default(secure_client):
    # A fresh install (empty DB) is seeded with the built-in store, whose
    # URL is the configured app resource path.
    status, body = secure_client.call("GET", "/api/admin/apps/stores")
    assert status == 200, body
    assert body == [{"name": "Vreckan Apps", "url": STORE_URL}]


def test_add_store(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/stores", body={"name": "Test Store", "url": STORE_URL}
    )
    assert status == 201, body
    assert body == {"name": "Test Store", "url": STORE_URL}
    status, stores = secure_client.call("GET", "/api/admin/apps/stores")
    assert status == 200
    names = [s["name"] for s in stores]
    assert "Test Store" in names
    assert "Vreckan Apps" in names  # the seeded default store is kept


def test_add_store_duplicate(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/stores", body={"name": "Test Store", "url": STORE_URL}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/stores",
        body={"name": "Test Store", "url": "https://other.invalid/apps.yml"},
    )
    assert status == 409
    assert body["detail"] == "App store with name 'Test Store' already exists."


def test_add_store_malformed_body(secure_client):
    # Missing the required 'url' field: the unguarded-parse bug used to
    # surface as a 500; the fix maps it to a 422.
    status, body = secure_client.call("POST", "/api/admin/apps/stores", body={"name": "X"})
    assert status == 422
    assert body["detail"].startswith("Invalid request body")


def test_delete_store_unknown(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/apps/stores/Ghost")
    assert status == 404
    assert body["detail"] == "App store not found."


def test_delete_store(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/stores", body={"name": "Test Store", "url": STORE_URL}
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/apps/stores/Test%20Store")
    assert status == 204
    status, stores = secure_client.call("GET", "/api/admin/apps/stores")
    assert status == 200
    # The custom store is gone; the seeded default store remains.
    assert [s["name"] for s in stores] == ["Vreckan Apps"]


# ---------------------------------------------------------------------------
# Available apps: caching and fetch semantics
# ---------------------------------------------------------------------------
def test_available_apps_refresh_bypasses_cache(secure_client, store_with_firefox):
    # A cached copy exists, but refresh=true must ignore it and hit the
    # (dead) URL.
    url = (
        "/api/admin/apps/available"
        f"?url={quote(STORE_URL)}&store_name=Test%20Store&refresh=true"
    )
    status, body = secure_client.call("GET", url)
    assert status == 400
    assert body["detail"].startswith("Failed to fetch app store from URL")


def test_available_apps_fetch_failure(secure_client):
    # No cache file for this store: the fetch is attempted and fails.
    url = f"/api/admin/apps/available?url={quote(STORE_URL)}&store_name=No%20Cache"
    status, body = secure_client.call("GET", url)
    assert status == 400
    assert body["detail"].startswith("Failed to fetch app store from URL")
    assert STORE_URL in body["detail"]


def test_available_apps_invalid_format(store_server, secure_client):
    # A well-formed HTTP response whose YAML is not a catalog: the
    # fresh-fetch path lets the parser's 500 propagate.
    server = store_server("just a plain string\n")
    url = f"/api/admin/apps/available?url={quote(server.url)}&store_name=Broken%20Store"
    status, body = secure_client.call("GET", url)
    assert status == 500
    assert body["detail"] == "App store YAML has an invalid format."


def test_available_apps_yaml_parse_error(store_server, secure_client):
    server = store_server("foo: [unclosed\n")
    url = f"/api/admin/apps/available?url={quote(server.url)}&store_name=Broken%20Store"
    status, body = secure_client.call("GET", url)
    assert status == 500
    assert body["detail"].startswith("Failed to parse app store YAML:")


def test_available_apps_cache_prevents_refetch(store_server, secure_client):
    server = store_server(yaml.safe_dump({"apps": [STORE_APP]}))
    url = f"/api/admin/apps/available?url={quote(server.url)}&store_name=Local%20Store"
    status, apps = secure_client.call("GET", url)
    assert status == 200, apps
    assert [a["id"] for a in apps] == ["firefox"]
    # Second request without refresh must be served from the cache ...
    status, _ = secure_client.call("GET", url)
    assert status == 200
    # ... so the server was hit exactly once.
    assert server.hits == 1


def test_available_apps_poisoned_cache_refetches(store_server, secure_client):
    # A failed parse still writes the (bad) cache file. The cache-hit path
    # swallows the resulting 500 and falls through to a re-fetch, so a
    # poisoned cache is not sticky: the next request goes out to the network
    # again and fails the same way.
    server = store_server("just a plain string\n")
    url = f"/api/admin/apps/available?url={quote(server.url)}&store_name=Poisoned%20Store"
    status, body = secure_client.call("GET", url)
    assert status == 500
    status, body = secure_client.call("GET", url)
    assert status == 500
    assert server.hits == 2


# ---------------------------------------------------------------------------
# Installed apps: error paths (happy paths live in test_api_smoke.py)
# ---------------------------------------------------------------------------
def install_body(
    app_id="app-1",
    name="Firefox",
    image="lscr.io/linuxserver/firefox:latest",
    **overrides,
):
    body = {
        "id": app_id,
        "name": name,
        "logo": "https://example.invalid/firefox.png",
        "url": "https://example.invalid/firefox",
        "source": "Test Store",
        "source_app_id": "firefox",
        "provider": "docker",
        "home_directories": True,
        "users": ["admin"],
        "groups": [],
        "app_template": "Default",
        "provider_config": {
            "image": image,
            "port": 3000,
            "nvidia_support": False,
            "dri3_support": False,
            "type": "browser",
            "url_support": True,
            "open_support": False,
            "extensions": ["html", "pdf"],
        },
    }
    body.update(overrides)
    return body


def test_install_app_malformed_body(secure_client):
    body = install_body()
    del body["provider_config"]["image"]
    status, resp = secure_client.call("POST", "/api/admin/apps/installed", body)
    assert status == 422
    assert resp["detail"].startswith("Invalid request body")


def test_update_installed_app_unknown(secure_client):
    status, body = secure_client.call(
        "PUT", "/api/admin/apps/installed/ghost", body=install_body(app_id="ghost")
    )
    assert status == 404
    assert body["detail"] == "Installed app not found."


def test_update_installed_app_id_mismatch(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    assert status == 201
    # The path ID exists, but the body carries a different ID.
    body = install_body(app_id="other")
    status, resp = secure_client.call("PUT", "/api/admin/apps/installed/app-1", body=body)
    assert status == 400
    assert resp["detail"] == "App ID in path does not match body."


def test_update_installed_app_malformed_body(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    assert status == 201
    body = install_body()
    del body["provider_config"]
    status, resp = secure_client.call("PUT", "/api/admin/apps/installed/app-1", body=body)
    assert status == 422
    assert resp["detail"].startswith("Invalid request body")


def test_delete_installed_app_unknown(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/apps/installed/ghost")
    assert status == 404
    assert body["detail"] == "Installed app not found."


def test_check_app_update_unknown(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/apps/installed/ghost/check_update"
    )
    assert status == 404
    assert body["detail"] == "Installed app not found."


def test_check_app_update_no_remote(secure_client):
    # The image's registry cannot be resolved (.invalid is reserved for
    # exactly this), so the daemon reports no digest and the endpoint
    # surfaces a 502 instead of a bogus "up to date" answer.
    image = "registry.invalid/ghost:latest"
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/installed", body=install_body(image=image)
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/apps/installed/app-1/check_update"
    )
    assert status == 502
    assert image in body["detail"]


# ---------------------------------------------------------------------------
# Check-all / update-all (bulk) endpoints
# ---------------------------------------------------------------------------
class FakeProvider:
    """Stands in for DockerProvider so the bulk endpoints never touch the
    Docker daemon. The fake reports a remote digest that matches the local
    image for app-1 (up to date) and a different one for every other image
    (update available)."""

    def __init__(self, app_config):
        self.image = app_config["provider_config"]["image"]

    async def get_local_image_info(self, image_name):
        return {
            "id": "sha256:local",
            "short_id": "local123",
            "digests": ["sha256:local"],
        }

    async def get_remote_image_digest(self, image_name):
        return "sha256:local" if image_name == "lscr.io/linuxserver/firefox:latest" else "sha256:remote"


def test_check_all_updates(secure_client, monkeypatch):
    monkeypatch.setattr(admin_router_module, "DockerProvider", FakeProvider)
    monkeypatch.setattr(
        api_module, "_do_pull_and_cache_image", lambda *a, **k: _async_noop()
    )
    secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    body2 = install_body(app_id="app-2", name="Jellyfin", image="lscr.io/linuxserver/jellyfin:latest")
    secure_client.call("POST", "/api/admin/apps/installed", body=body2)

    status, data = secure_client.call("POST", "/api/admin/apps/installed/check_all_updates")
    assert status == 200, data
    assert data["updates_available"] == 1
    by_id = {r["app_id"]: r for r in data["results"]}
    assert set(by_id) == {"app-1", "app-2"}
    assert by_id["app-1"]["update_available"] is False
    assert by_id["app-2"]["update_available"] is True
    assert by_id["app-1"]["current_sha"] == "local123"


def test_check_all_updates_empty(secure_client, monkeypatch):
    monkeypatch.setattr(admin_router_module, "DockerProvider", FakeProvider)
    status, data = secure_client.call("POST", "/api/admin/apps/installed/check_all_updates")
    assert status == 200
    assert data == {"results": [], "updates_available": 0}


def test_pull_all_latest(secure_client, monkeypatch):
    monkeypatch.setattr(admin_router_module, "DockerProvider", FakeProvider)
    started = []

    async def _fake_pull(image_name, app=None):
        started.append(image_name)

    monkeypatch.setattr(api_module, "_do_pull_and_cache_image", _fake_pull)
    secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    body2 = install_body(app_id="app-2", name="Jellyfin", image="lscr.io/linuxserver/jellyfin:latest")
    secure_client.call("POST", "/api/admin/apps/installed", body=body2)

    status, data = secure_client.call("POST", "/api/admin/apps/installed/pull_all_latest")
    assert status == 200, data
    assert data["started"] == 2
    assert {r["app_id"] for r in data["results"]} == {"app-1", "app-2"}
    assert all(r["status"] == "pulling" for r in data["results"])
    # Every image was claimed for a background pull.
    assert api_module.PULL_STATUS["lscr.io/linuxserver/firefox:latest"] == "queued"
    assert api_module.PULL_STATUS["lscr.io/linuxserver/jellyfin:latest"] == "queued"


def test_pull_all_latest_already_queued(secure_client, monkeypatch):
    monkeypatch.setattr(admin_router_module, "DockerProvider", FakeProvider)
    monkeypatch.setattr(
        api_module, "_do_pull_and_cache_image", lambda *a, **k: _async_noop()
    )
    secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    # A pull for this image is already in flight.
    api_module.PULL_STATUS["lscr.io/linuxserver/firefox:latest"] = "pulling"
    status, data = secure_client.call("POST", "/api/admin/apps/installed/pull_all_latest")
    assert status == 200
    assert data["started"] == 1
    assert data["results"][0]["status"] == "pulling"


async def _async_noop():
    return None


# ---------------------------------------------------------------------------
# Meta apps (per-user variants of an installed app)
# ---------------------------------------------------------------------------
def test_create_meta_app(secure_client):
    status, base = secure_client.call(
        "POST", "/api/admin/apps/installed", body=install_body(app_id="base-app")
    )
    assert status == 201
    status, meta = secure_client.call(
        "POST",
        "/api/admin/apps/meta",
        body={
            "name": "Firefox (admin)",
            "base_app_id": "base-app",
            "logo": PNG_B64,
            "users": ["admin"],
            "groups": [],
        },
    )
    assert status == 201, meta
    assert meta["is_meta_app"] is True
    assert meta["base_app_id"] == "base-app"
    assert meta["name"] == "Firefox (admin)"
    assert meta["logo"] == f"/api/app_icon/{meta['id']}"
    assert meta["home_template_name"].startswith("meta_")
    assert meta["auto_update"] is False
    assert meta["source"] == "Firefox"
    # The meta app is a full copy of the base app's provider config.
    assert meta["provider_config"]["image"] == base["provider_config"]["image"]


def test_create_meta_app_missing_base(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/apps/meta",
        body={
            "name": "Orphan",
            "base_app_id": "ghost",
            "logo": PNG_B64,
            "users": ["admin"],
            "groups": [],
        },
    )
    assert status == 404
    assert body["detail"] == "Base application with ID 'ghost' not found."


def test_create_meta_app_malformed_body(secure_client):
    status, body = secure_client.call("POST", "/api/admin/apps/meta", body={"name": "X"})
    assert status == 422
    assert "base_app_id" in body["detail"]


def test_create_meta_app_invalid_logo(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/installed", body=install_body(app_id="base-app")
    )
    assert status == 201
    status, meta = secure_client.call(
        "POST",
        "/api/admin/apps/meta",
        body={
            "name": "Firefox (admin)",
            "base_app_id": "base-app",
            "logo": "!!!not-base64!!!",
            "users": ["admin"],
            "groups": [],
        },
    )
    assert status == 201, meta
    # Undecodable logos are kept verbatim (treated as a URL).
    assert meta["logo"] == "!!!not-base64!!!"


# ---------------------------------------------------------------------------
# App icons
# ---------------------------------------------------------------------------
def test_app_icon_invalid_id(secure_client):
    status, body = secure_client.call("GET", "/api/app_icon/bad..id")
    assert status == 400
    assert body["detail"] == "Invalid application ID."


def test_app_icon_missing(secure_client):
    status, body = secure_client.call("GET", "/api/app_icon/doesnotexist")
    assert status == 404
    assert body["detail"] == "Icon not found."


def test_app_icon_serves_saved_icon(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/installed", body=install_body(app_id="base-app")
    )
    assert status == 201
    status, meta = secure_client.call(
        "POST",
        "/api/admin/apps/meta",
        body={
            "name": "Firefox (admin)",
            "base_app_id": "base-app",
            "logo": PNG_B64,
            "users": ["admin"],
            "groups": [],
        },
    )
    assert status == 201
    status, body = secure_client.call("GET", f"/api/app_icon/{meta['id']}")
    assert status == 200, body
    assert base64.b64decode(body["icon_data_b64"]) == base64.b64decode(PNG_B64)


# ---------------------------------------------------------------------------
# Meta-app customization launch (pre-checks only — the success path spawns
# a real container and is out of scope for hermetic tests)
# ---------------------------------------------------------------------------
def test_launch_meta_customize_unknown_app(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/launch/meta_customize", body={"application_id": "ghost"}
    )
    assert status == 400
    assert body["detail"] == "This endpoint is only for customizing meta applications."


def test_launch_meta_customize_non_meta_app(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/apps/installed", body=install_body())
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/launch/meta_customize", body={"application_id": "app-1"}
    )
    assert status == 400
    assert body["detail"] == "This endpoint is only for customizing meta applications."


def test_launch_meta_customize_malformed_body(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/launch/meta_customize", body={"wrong_field": 1}
    )
    assert status == 422
    assert body["detail"].startswith("Invalid request body")
