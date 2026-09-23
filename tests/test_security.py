"""Security: argon2 passwords, upload-id validation, path traversal, share passwords."""
import base64
import hashlib
from pathlib import Path

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
