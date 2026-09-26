"""End-to-end smoke test: login, E2EE handshake, encrypted admin flows.

Exercises the real ASGI app (lifespan included) over the TestClient: the
handshake proves the server's RSA keypair, then every admin call travels
through the AES-GCM encrypted router exactly as the browser client does.
"""
import json

from conftest import db_conn


def test_template_schema_served_to_admin(secure_client):
    status, data = secure_client.call("GET", "/api/admin/apps/templates/schema")
    assert status == 200
    names = [s["name"] for s in data["settings"]]
    assert "SELKIES_ENCODER" in names
    assert "DOCKER_PRIVILEGED" in names


def test_status_install_patch_flow(secure_client, store_with_firefox, db_path):
    status, data = secure_client.call("POST", "/api/admin/status", {})
    assert status == 200
    assert data["is_admin"] is True
    assert data["username"] == "admin"

    status, available = secure_client.call(
        "GET", "/api/admin/apps/available?url=http://127.0.0.1:9/apps.yml&store_name=Test%20Store"
    )
    assert status == 200
    assert available[0]["id"] == "firefox"
    # Nested extension lists from the store YAML are flattened.
    assert available[0]["provider_config"]["extensions"] == ["html", "htm", "pdf"]

    install_body = {
        **available[0],
        "id": "inst-1",
        "source": "Test Store",
        "source_app_id": "firefox",
        "home_directories": True,
        "users": ["all"],
        "groups": [],
        "app_template": "Default",
    }
    status, installed = secure_client.call("POST", "/api/admin/apps/installed", install_body)
    assert status == 201, installed
    assert installed["name"] == "Firefox"

    # Installing the same ID again is a conflict.
    status, _ = secure_client.call("POST", "/api/admin/apps/installed", install_body)
    assert status == 409

    # PUT replaces the whole record (logo emptied to skip icon handling).
    full = dict(installed)
    full["name"] = "Work Firefox"
    full["users"] = ["admin"]
    full["logo"] = ""
    status, patched = secure_client.call("PUT", "/api/admin/apps/installed/inst-1", full)
    assert status == 200
    assert patched["name"] == "Work Firefox"
    assert patched["users"] == ["admin"]

    # The database row reflects the update.
    conn = db_conn(db_path)
    row = conn.execute("SELECT id, data FROM installed_apps WHERE id = 'inst-1'").fetchone()
    conn.close()
    assert row is not None
    assert json.loads(row[1])["name"] == "Work Firefox"

    status, apps = secure_client.call("POST", "/api/applications", {})
    assert status == 200
    assert [a["name"] for a in apps] == ["Work Firefox"]

    status, listing = secure_client.call("GET", "/api/admin/apps/installed")
    assert status == 200
    assert listing[0]["provider_config"]["image"].endswith("firefox:latest")

    status, _ = secure_client.call("DELETE", "/api/admin/apps/installed/inst-1")
    assert status == 204
    status, listing = secure_client.call("GET", "/api/admin/apps/installed")
    assert status == 200 and listing == []


def test_unauthenticated_requests_are_rejected(client):
    # No credentials at all: the auth dependency rejects the request.
    assert client.post("/api/admin/status", json={}).status_code == 401
    # ...same with a bogus session header.
    assert (
        client.post("/api/admin/status", json={}, headers={"X-Session-ID": "nope"}).status_code
        == 401
    )
    # Unknown paths still 404 through the static catch-all.
    assert client.get("/internal/resolve_session/abc").status_code == 404


def test_valid_session_but_no_auth_is_rejected(secure_client, client):
    # A valid crypto session without a login cookie still cannot call
    # authenticated endpoints.
    client.cookies.clear()
    resp = client.post(
        "/api/admin/status", json={}, headers={"X-Session-ID": secure_client.session_id}
    )
    assert resp.status_code == 401


def test_authenticated_without_valid_session_is_blocked(secure_client, client):
    # Authenticated (cookie) but the crypto session is invalid: the
    # encrypted router must not leak the plaintext response body.
    resp = client.post("/api/admin/status", json={}, headers={"X-Session-ID": "nope"})
    assert resp.status_code == 400
    assert "secure session" in resp.json()["detail"].lower()
