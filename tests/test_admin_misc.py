"""Admin endpoints not covered by the other suites: global settings
(GPU default), session management, and the installed-apps
list / install / pull-latest flows.

Session records are seeded directly into SESSIONS_DB with the same
shape _launch_common stores. provider_app_id points at an app that is
NOT installed, so the stop-session cleanup path has no containers to
talk to (no Docker daemon needed).
"""
import copy
import sqlite3
import time
import uuid

import pytest

import server.api as api_module
from server import user_manager
from server.models import InstalledApp
from server.settings import settings
from conftest import STORE_APP

GPU = {"device": "/dev/dri/renderD128", "driver": "amdgpu", "type": "dri3"}

# A valid install body (InstalledApp) for the firefox store app.
INSTALL_BODY = {
    "id": "app-1",
    "name": "Firefox",
    "logo": "x",
    "url": "https://example.invalid/firefox",
    "source": "Test Store",
    "source_app_id": "firefox",
    "provider": "docker",
    "home_directories": True,
    "users": ["all"],
    "groups": [],
    "app_template": "Default",
    # InstalledAppProviderConfig.extensions is List[str] (flat), unlike the
    # store-entry shape in conftest.STORE_APP.
    "provider_config": {
        **STORE_APP["provider_config"],
        "extensions": ["html", "htm", "pdf"],
    },
}


def gpu_device():
    """A device path the endpoint will accept.

    The test container may expose a real DRI device (detected at startup);
    prefer that, and only seed a fake GPU when the host has none.
    """
    if api_module.AVAILABLE_GPUS:
        return api_module.AVAILABLE_GPUS[0]["device"]
    api_module.AVAILABLE_GPUS.append(GPU)
    return GPU["device"]


def seed_session(username="admin", **overrides):
    """Insert a session record shaped like the one _launch_common stores."""
    record = {
        "instance_id": "ctr-1",
        "ip": None,
        "port": None,
        "created_at": time.time(),
        "access_token": f"tok-{uuid.uuid4()}",
        "controller_token": f"ctrl-{uuid.uuid4()}",
        "provider_app_id": "ghost-app",
        "username": username,
        "app_name": "Test App",
        "app_logo": "x",
        "name": None,
        "host_mount_path": None,
        "shared_files_path": None,
        "launch_context": None,
        "is_collaboration": False,
        "container_registry": {},
    }
    record.update(overrides)
    sid = str(uuid.uuid4())
    api_module.SESSIONS_DB[sid] = record
    return sid


# ---------------------------------------------------------------------------
# Global settings (GPU default)
# ---------------------------------------------------------------------------

def test_global_settings_fresh(secure_client):
    status, data = secure_client.call("GET", "/api/admin/global-settings")
    assert status == 200
    assert data["global_default_gpu"] is None
    # The GPU list mirrors whatever the test host exposes at startup.
    assert data["gpus"] == api_module.AVAILABLE_GPUS


def test_global_settings_unknown_gpu_rejected(secure_client):
    status, data = secure_client.call(
        "POST", "/api/admin/global-settings",
        body={"global_default_gpu": "/dev/dri/renderD999"},
    )
    assert status == 400
    assert data["detail"] == "Unknown GPU device: '/dev/dri/renderD999'."


def test_global_settings_set_and_persist(secure_client, db_path):
    device = gpu_device()
    status, data = secure_client.call(
        "POST", "/api/admin/global-settings",
        body={"global_default_gpu": device},
    )
    assert status == 200
    assert data == {"ok": True}
    status, data = secure_client.call("GET", "/api/admin/global-settings")
    assert status == 200
    assert data["global_default_gpu"] == device
    # The choice is persisted to the app_settings shadow table.
    conn = sqlite3.connect(db_path)
    try:
        row = conn.execute(
            "SELECT value FROM app_settings WHERE key = 'global_default_gpu'"
        ).fetchone()
    finally:
        conn.close()
    assert row == (device,)


def test_global_settings_clear(secure_client, db_path):
    device = gpu_device()
    secure_client.call(
        "POST", "/api/admin/global-settings",
        body={"global_default_gpu": device},
    )
    status, data = secure_client.call(
        "POST", "/api/admin/global-settings", body={"global_default_gpu": ""},
    )
    assert status == 200
    assert data == {"ok": True}
    status, data = secure_client.call("GET", "/api/admin/global-settings")
    assert data["global_default_gpu"] is None
    conn = sqlite3.connect(db_path)
    try:
        count = conn.execute(
            "SELECT COUNT(*) FROM app_settings WHERE key = 'global_default_gpu'"
        ).fetchone()[0]
    finally:
        conn.close()
    assert count == 0


def test_global_settings_missing_key_is_noop(secure_client):
    device = gpu_device()
    secure_client.call(
        "POST", "/api/admin/global-settings",
        body={"global_default_gpu": device},
    )
    status, data = secure_client.call("POST", "/api/admin/global-settings", body={})
    assert status == 200
    assert data == {"ok": True}
    status, data = secure_client.call("GET", "/api/admin/global-settings")
    assert data["global_default_gpu"] == device


def test_sso_groups_fresh(secure_client):
    # No SSO groups are configured in the test environment, and no SSO users
    # exist yet, so all three lists are empty.
    status, data = secure_client.call("GET", "/api/admin/sso-groups")
    assert status == 200
    assert data["admin_groups"] == []
    assert data["user_groups"] == []
    assert data["observed"] == []


def test_sso_groups_reflects_env(secure_client, monkeypatch):
    # The admin/user group lists are read from the environment (exposed as
    # settings attributes), so the endpoint reflects the configured values.
    monkeypatch.setattr(settings, "oidc_admin_groups", "vdi-admins, ops")
    monkeypatch.setattr(settings, "oidc_user_groups", "vdi-users")
    status, data = secure_client.call("GET", "/api/admin/sso-groups")
    assert status == 200
    assert data["admin_groups"] == ["vdi-admins", "ops"]
    assert data["user_groups"] == ["vdi-users"]


def test_sso_groups_observed(secure_client):
    # The observed list is derived from the sso_groups recorded on SSO
    # accounts. Seed two SSO users directly in the in-memory store.
    user_manager.USER_DATA["obs-a"] = {
        "username": "obs-a",
        "is_admin": False,
        "is_sso": True,
        "has_password": False,
        "sso_groups": ["vdi-users", "vdi-admins"],
        "settings": {},
    }
    user_manager.USER_DATA["obs-b"] = {
        "username": "obs-b",
        "is_admin": False,
        "is_sso": True,
        "has_password": False,
        "sso_groups": ["vdi-users"],
        "settings": {},
    }
    # A non-SSO user's groups are not counted as observed SSO groups.
    user_manager.USER_DATA["local"] = {
        "username": "local",
        "is_admin": False,
        "is_sso": False,
        "has_password": True,
        "sso_groups": ["should-not-appear"],
        "settings": {},
    }
    status, data = secure_client.call("GET", "/api/admin/sso-groups")
    assert status == 200
    assert data["observed"] == ["vdi-admins", "vdi-users"]


# ---------------------------------------------------------------------------
# Admin session management
# ---------------------------------------------------------------------------

def test_admin_sessions_empty(secure_client):
    status, data = secure_client.call("GET", "/api/admin/sessions")
    assert status == 200
    assert data == []


def test_admin_sessions_grouped_and_sorted(secure_client):
    sid_old = seed_session(username="bob", created_at=1000.0, name="Old")
    sid_new = seed_session(username="bob", created_at=2000.0, name="New")
    seed_session(username="admin", created_at=1500.0)
    status, data = secure_client.call("GET", "/api/admin/sessions")
    assert status == 200
    # Users sorted by name; sessions within a user newest-first.
    assert [u["username"] for u in data] == ["admin", "bob"]
    bob = data[1]
    assert [s["session_id"] for s in bob["sessions"]] == [sid_new, sid_old]
    assert [s["name"] for s in bob["sessions"]] == ["New", "Old"]
    # session_url carries the per-session access token.
    tok = api_module.SESSIONS_DB[sid_new]["access_token"]
    assert bob["sessions"][0]["session_url"] == (
        f"/api/apps/session/{sid_new}/?access_token={tok}"
    )


def test_admin_delete_session_unknown(secure_client):
    status, data = secure_client.call("DELETE", "/api/admin/sessions/nope")
    assert status == 404
    assert data["detail"] == "Session not found."


def test_admin_delete_session(secure_client):
    sid = seed_session()
    status, _ = secure_client.call("DELETE", f"/api/admin/sessions/{sid}")
    assert status == 204
    assert sid not in api_module.SESSIONS_DB


# ---------------------------------------------------------------------------
# Installed apps: list / install / pull-latest
# ---------------------------------------------------------------------------

def test_installed_apps_empty(secure_client):
    status, data = secure_client.call("GET", "/api/admin/apps/installed")
    assert status == 200
    assert data == []


def test_install_app_success(secure_client):
    status, data = secure_client.call(
        "POST", "/api/admin/apps/installed", body=INSTALL_BODY,
    )
    assert status == 201
    assert data["id"] == "app-1"
    assert "app-1" in api_module.INSTALLED_APPS
    status, listing = secure_client.call("GET", "/api/admin/apps/installed")
    assert status == 200
    assert len(listing) == 1
    assert listing[0]["name"] == "Firefox"
    # The image pull is queued (the pull task itself is a no-op in tests).
    assert listing[0]["pull_status"] == "queued"
    assert listing[0]["image_sha"] is None


def test_install_app_duplicate(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/apps/installed", body=INSTALL_BODY,
    )
    assert status == 201
    status, data = secure_client.call(
        "POST", "/api/admin/apps/installed", body=INSTALL_BODY,
    )
    assert status == 409
    assert data["detail"] == "App with this ID already exists."


def test_pull_latest_unknown(secure_client):
    status, data = secure_client.call(
        "POST", "/api/admin/apps/installed/ghost/pull_latest",
    )
    assert status == 404
    assert data["detail"] == "Installed app not found."


def test_pull_latest_already_queued(secure_client):
    secure_client.call("POST", "/api/admin/apps/installed", body=INSTALL_BODY)
    status, data = secure_client.call(
        "POST", "/api/admin/apps/installed/app-1/pull_latest",
    )
    assert status == 200
    assert data == {"status": "pulling", "new_sha": None}


def test_pull_latest_idle_app(secure_client):
    image = "registry.example.invalid/other:latest"
    body = copy.deepcopy(INSTALL_BODY)
    body["id"] = "app-2"
    body["provider_config"]["image"] = image
    app = InstalledApp(**body)
    api_module.INSTALLED_APPS[app.id] = app
    assert image not in api_module.PULL_STATUS
    status, data = secure_client.call(
        "POST", f"/api/admin/apps/installed/{app.id}/pull_latest",
    )
    assert status == 200
    assert data == {"status": "pulling", "new_sha": None}
    assert api_module.PULL_STATUS[image] == "queued"
