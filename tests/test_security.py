"""Security: argon2 passwords, upload-id validation, path traversal, share passwords."""
import base64
import hashlib
import uuid
from pathlib import Path

import httpx2
import pytest

import server.api as api_module
from server import user_manager
from server.settings import settings


def test_argon2_password_roundtrip(client):
    assert user_manager.verify_password("admin", "admin1234")
    assert not user_manager.verify_password("admin", "wrong-password")
    assert not user_manager.verify_password("ghost", "admin1234")
    # The stored hash is an argon2 hash.
    assert user_manager.USER_DATA["admin"]["password_hash"].startswith("$argon2")


def test_short_password_rejected(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/set_password", params={"target": "admin"}, json={"password": "short"}
    )
    assert resp.status_code == 422  # pydantic min_length=8


def test_upload_id_validation(secure_client):
    status, data = secure_client.call(
        "POST", "/api/upload/initiate", {"filename": "a.txt", "total_size": 5}
    )
    assert status == 200
    good_id = data["upload_id"]
    # A valid id accepts chunks.
    status, _ = secure_client.call(
        "POST",
        "/api/upload/chunk",
        {
            "upload_id": good_id,
            "chunk_index": 0,
            "chunk_data_b64": base64.b64encode(b"hello").decode(),
        },
    )
    assert status == 200
    # Traversal / unknown ids are rejected (the id must name an existing dir).
    for bad in ("../etc", "a/b", "abc"):
        status, _ = secure_client.call(
            "POST",
            "/api/upload/chunk",
            {"upload_id": bad, "chunk_index": 0, "chunk_data_b64": "aGk="},
        )
        assert status == 404, bad


def test_path_traversal_rejected(secure_client):
    user_manager.create_home_dir("admin", "default")
    home = Path(settings.storage_path) / "admin" / "default"
    (home / "Desktop" / "files").mkdir(parents=True, exist_ok=True)
    (home / "Desktop" / "files" / "note.txt").write_text("hi")
    # A valid listing works.
    status, data = secure_client.call("GET", "/api/files/list/default?path=Desktop/files")
    assert status == 200
    assert [i["name"] for i in data["items"]] == ["note.txt"]
    # Traversal in the sub-path is rejected.
    status, _ = secure_client.call("GET", "/api/files/list/default?path=../../etc")
    assert status == 403
    # Traversal in the home-dir name is rejected.
    status, _ = secure_client.call("GET", "/api/files/list/..%2Fetc")
    assert status in (400, 404)


def test_share_password_flow(secure_client):
    # Public sharing is off by default: enable it for the admin.
    user_manager.get_user("admin")["settings"]["public_sharing"] = True
    user_manager.create_home_dir("admin", "default")
    home = Path(settings.storage_path) / "admin" / "default"
    (home / "Desktop" / "files").mkdir(parents=True, exist_ok=True)
    (home / "Desktop" / "files" / "secret.txt").write_text("top secret")

    status, info = secure_client.call(
        "POST",
        "/api/files/share",
        {"home_dir": "default", "path": "Desktop/files/secret.txt", "password": "hunter2"},
    )
    assert status == 200, info
    share_id = info["share_id"]
    assert info["has_password"] is True
    # The stored hash is a plain sha256 hex digest.
    assert api_module.PUBLIC_SHARES_METADATA[share_id].password_hash == hashlib.sha256(
        b"hunter2"
    ).hexdigest()

    # Wrong password -> 401.
    assert secure_client.http.post(f"/public/{share_id}", data={"password": "wrong"}).status_code == 401
    # Correct password -> one-time download token.
    resp = secure_client.http.post(f"/public/{share_id}", data={"password": "hunter2"})
    assert resp.status_code == 303
    location = resp.headers["location"]
    assert location.startswith("/public/download/")
    download = secure_client.http.get(location)
    assert download.status_code == 200
    assert download.content == b"top secret"
    # The token is single-use.
    assert secure_client.http.get(location).status_code == 403


def test_security_headers_present(client):
    """Every response carries the security headers (DAST: ZAP Medium/Low)."""
    resp = client.get("/")
    assert resp.status_code == 200
    h = resp.headers
    # CSP: strict by default — the main app has no inline <script>/<style>
    # blocks and its inline style="" attributes are now CSS classes, so
    # 'unsafe-inline' is not needed (Phase 2; clears ZAP Medium findings).
    csp = h["content-security-policy"]
    assert "default-src 'self'" in csp
    assert "'unsafe-inline'" not in csp
    # The collaboration room spawns Web Workers / AudioWorklets from blob:
    # URLs, so worker-src (and script-src, which governs worklet modules)
    # must allow blob:.
    assert "worker-src 'self' blob:" in csp
    assert "script-src 'self' blob:" in csp
    assert "frame-ancestors 'self'" in csp  # main app iframes the session page
    assert "img-src 'self' data: blob:" in csp  # icon previews + file downloads
    # Store app icons are hosted on GitHub (the default store catalog).
    assert "https://raw.githubusercontent.com" in csp
    # HSTS / nosniff / clickjacking.
    assert h["strict-transport-security"].startswith("max-age=63072000")
    assert h["x-content-type-options"] == "nosniff"
    # SAMEORIGIN (not DENY): the session page is iframed by the main app.
    assert h["x-frame-options"] == "SAMEORIGIN"
    # The CO* + Permissions-Policy headers ZAP flagged as Low.
    assert h["permissions-policy"]
    assert h["cross-origin-embedder-policy"] == "require-corp"
    assert h["cross-origin-opener-policy"] == "same-origin"
    assert h["cross-origin-resource-policy"] == "same-origin"


def test_password_page_strict_csp(secure_client):
    """The public password page uses a strict, nonce-based CSP (Phase 2).

    It owns its inline <style>/<script>, so it can drop 'unsafe-inline'
    from script-src (the nonce authorizes them) — clearing ZAP's
    "CSP: script-src unsafe-inline" Medium finding.
    """
    user_manager.get_user("admin")["settings"]["public_sharing"] = True
    user_manager.create_home_dir("admin", "default")
    home = Path(settings.storage_path) / "admin" / "default"
    (home / "Desktop" / "files").mkdir(parents=True, exist_ok=True)
    (home / "Desktop" / "files" / "secret.txt").write_text("top secret")

    status, info = secure_client.call(
        "POST",
        "/api/files/share",
        {"home_dir": "default", "path": "Desktop/files/secret.txt", "password": "pw"},
    )
    assert status == 200, info
    share_id = info["share_id"]

    resp = secure_client.http.get(f"/public/{share_id}")
    assert resp.status_code == 200
    csp = resp.headers["content-security-policy"]
    # Strict: script-src has a nonce and NO 'unsafe-inline'.
    assert "'unsafe-inline'" not in csp.split("style-src")[0]
    assert "script-src 'self' 'nonce-" in csp
    # The nonce in the header matches the one injected into the page.
    nonce = csp.split("'nonce-")[1].split("'")[0]
    assert f'nonce="{nonce}"' in resp.text
    # The page's inline <style> and <script> carry the nonce.
    assert "<style nonce=" in resp.text
    assert "<script nonce=" in resp.text


def test_session_proxy_gets_loose_csp(client, monkeypatch):
    """The reverse-proxied session page keeps 'unsafe-inline' (Phase 2).

    App containers (noVNC etc.) are third-party HTML we don't control, so
    the proxy overrides the strict default CSP with _SESSION_CSP.
    """
    sid = str(uuid.uuid4())
    api_module.SESSIONS_DB[sid] = {
        "instance_id": "ctr-1",
        "ip": "127.0.0.1",
        "port": 8080,
        "access_token": "tok-123",
        "username": "admin",
    }

    class _FakeUpstreamResponse:
        status_code = 200
        headers = {"Content-Type": "text/html"}

        async def aiter_raw(self):
            yield b"<html>upstream</html>"

        async def aclose(self):
            pass

    class _FakeAsyncClient:
        def __init__(self, *a, **k):
            pass

        def build_request(self, *a, **k):
            return object()

        async def send(self, *a, **k):
            return _FakeUpstreamResponse()

        async def aclose(self):
            pass

    monkeypatch.setattr(api_module.httpx2, "AsyncClient", _FakeAsyncClient)

    # First hit with the access token: 303 redirect + session cookie.
    resp = client.get(f"/api/apps/session/{sid}/?access_token=tok-123")
    assert resp.status_code == 303
    # The cookie is set Secure, but TestClient uses http://, so it isn't
    # auto-sent. Seed it manually to exercise the proxied (cookie) path.
    client.cookies.set(
        f"{settings.session_cookie_name}_{sid}", "tok-123", path="/"
    )
    # Second hit with the cookie: reverse-proxied, and must carry the
    # loose session CSP (with 'unsafe-inline'), not the strict default.
    resp = client.get(f"/api/apps/session/{sid}/")
    assert resp.status_code == 200
    csp = resp.headers["content-security-policy"]
    assert csp == api_module._SESSION_CSP
    assert "'unsafe-inline'" in csp
