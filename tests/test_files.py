"""Tier 4 — files API.

Covers /api/files: chunked download, folder creation, asynchronous
deletion (+ status polling), directory listing, in-home uploads,
public shares (create/list/delete) and launch-from-storage.

All tests are Docker-free: launch-from-storage is exercised only up
to its pre-launch checks (unknown app / access denied), which raise
before any container work.
"""
import base64
import sqlite3
import time
from pathlib import Path

import pytest

import server.api as api_module
from server import user_manager
from server.models import InstalledApp, PublicShareMetadata
from server.settings import settings
from conftest import STORE_APP

CHUNK_SIZE = 2 * 1024 * 1024  # mirrors api_module.CHUNK_SIZE


# --- fixtures / helpers ---------------------------------------------------------

@pytest.fixture()
def home(secure_client):
    """The admin's 'default' home dir with a known layout.

    The skeleton (Desktop/files) comes from create_home_dir; the rest
    is created here so listing tests can assert exact output.
    """
    user_manager.create_home_dir("admin", "default")
    root = Path(settings.storage_path) / "admin" / "default"
    (root / "docs").mkdir()
    (root / "docs" / "inner.txt").write_text("inner")
    (root / "a.txt").write_text("alpha")
    (root / "B.txt").write_text("bravo")
    (root / "c.txt").write_text("charlie")
    (root / "d.txt").write_text("delta")
    return root


def set_admin_settings(secure_client, **overrides):
    status, data = secure_client.call(
        "PUT", "/api/admin/people/admin", {"settings": overrides}
    )
    assert status == 200, data


def initiate_upload(secure_client, filename="report.pdf"):
    status, data = secure_client.call(
        "POST", "/api/upload/initiate", {"filename": filename, "total_size": 100}
    )
    assert status == 200, data
    return data["upload_id"]


def send_chunk(secure_client, upload_id, chunk_index, payload: bytes):
    return secure_client.call(
        "POST",
        "/api/upload/chunk",
        {
            "upload_id": upload_id,
            "chunk_index": chunk_index,
            "chunk_data_b64": base64.b64encode(payload).decode(),
        },
    )


def wait_for_task(secure_client, task_id, timeout=10.0):
    """Poll the deletion task until it reaches a terminal state."""
    deadline = time.time() + timeout
    while True:
        status, data = secure_client.call("GET", f"/api/files/delete_status/{task_id}")
        assert status == 200, data
        if data["status"] in ("completed", "error"):
            return data
        assert time.time() < deadline, f"Deletion task did not finish: {data}"
        time.sleep(0.05)


def share_rows(db_path):
    con = sqlite3.connect(str(db_path))
    try:
        return [r[0] for r in con.execute("SELECT share_id FROM public_shares")]
    finally:
        con.close()


# --- /download/chunk -------------------------------------------------------------

def test_download_chunk_missing_path(secure_client, home):
    status, data = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=nope.txt&chunk_index=0"
    )
    assert status == 404
    assert data["detail"] == "Path not found."


def test_download_chunk_path_is_directory(secure_client, home):
    status, data = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=docs&chunk_index=0"
    )
    assert status == 404
    assert data["detail"] == "File not found or is a directory."


def test_download_chunk_small_file(secure_client, home):
    status, data = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=a.txt&chunk_index=0"
    )
    assert status == 200
    assert base64.b64decode(data["chunk_data_b64"]) == b"alpha"
    assert data["is_last_chunk"] is True


def test_download_chunk_multi_chunk(secure_client, home):
    # Exactly 2 chunks worth of data: the third read is empty and last.
    (home / "big.bin").write_bytes(b"x" * (2 * CHUNK_SIZE))
    status, c0 = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=0"
    )
    assert status == 200
    assert len(base64.b64decode(c0["chunk_data_b64"])) == CHUNK_SIZE
    assert c0["is_last_chunk"] is False
    status, c1 = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=1"
    )
    assert status == 200
    assert len(base64.b64decode(c1["chunk_data_b64"])) == CHUNK_SIZE
    assert c1["is_last_chunk"] is False
    status, c2 = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=2"
    )
    assert status == 200
    assert base64.b64decode(c2["chunk_data_b64"]) == b""
    assert c2["is_last_chunk"] is True


def test_download_chunk_negative_index(secure_client, home):
    status, data = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=a.txt&chunk_index=-1"
    )
    assert status == 422


def test_download_chunk_traversal(secure_client, home):
    status, data = secure_client.call(
        "GET", "/api/files/download/chunk/default?path=../../etc&chunk_index=0"
    )
    assert status == 403
    assert data["detail"] == "Directory traversal attempt detected."


# --- /create_folder ----------------------------------------------------------------

def test_create_folder_malformed_body(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "/"}
    )
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_create_folder_success(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "/", "folder_name": "notes"}
    )
    assert status == 200
    assert data["message"] == "Folder 'notes' created successfully."
    assert (home / "notes").is_dir()


def test_create_folder_duplicate(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "/", "folder_name": "docs"}
    )
    assert status == 409
    assert data["detail"] == "Folder 'docs' already exists."


def test_create_folder_invalid_name(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "/", "folder_name": "a/b"}
    )
    assert status == 422


def test_create_folder_parent_missing(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "nope", "folder_name": "x"}
    )
    assert status == 404
    assert data["detail"] == "Path not found."


def test_create_folder_parent_is_file(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/create_folder/default", {"path": "a.txt", "folder_name": "x"}
    )
    assert status == 400
    assert data["detail"] == "Path is not a valid directory."


# --- /delete + /delete_status ---------------------------------------------------------

def test_delete_malformed_body(secure_client, home):
    status, data = secure_client.call("POST", "/api/files/delete/default", {})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_delete_success(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/delete/default", {"paths": ["a.txt", "docs"]}
    )
    assert status == 200
    assert data["message"] == "Deletion task started."
    task = wait_for_task(secure_client, data["task_id"])
    assert task["status"] == "completed"
    assert task["message"] == "Successfully deleted 2 items."
    assert not (home / "a.txt").exists()
    assert not (home / "docs").exists()


def test_delete_missing_path(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/delete/default", {"paths": ["nope.txt"]}
    )
    assert status == 200
    task = wait_for_task(secure_client, data["task_id"])
    assert task["status"] == "error"
    assert task["message"] == "Path not found."


def test_delete_traversal_path(secure_client, home):
    status, data = secure_client.call(
        "POST", "/api/files/delete/default", {"paths": ["../../etc"]}
    )
    assert status == 200
    task = wait_for_task(secure_client, data["task_id"])
    assert task["status"] == "error"
    assert task["message"] == "Directory traversal attempt detected."


def test_delete_invalid_home(secure_client, home):
    # A home dir name failing the charset check is rejected inside the
    # background task, so the endpoint still reports "task started".
    status, data = secure_client.call(
        "POST", "/api/files/delete/bad%20name", {"paths": ["a.txt"]}
    )
    assert status == 200
    task = wait_for_task(secure_client, data["task_id"])
    assert task["status"] == "error"
    assert task["message"] == "Invalid home directory name."


def test_delete_status_unknown(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/delete_status/nope")
    assert status == 404
    assert data["detail"] == "Task not found."


# --- /list -----------------------------------------------------------------------------

def test_list_requires_auth(client):
    resp = client.get("/api/files/list/default")
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Not authenticated."


def test_list_success(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/list/default")
    assert status == 200
    names = [item["name"] for item in data["items"]]
    # Directories first, then files, case-insensitive.
    assert names == ["Desktop", "docs", "a.txt", "B.txt", "c.txt", "d.txt"]
    paths = [item["path"] for item in data["items"]]
    assert paths == ["/Desktop", "/docs", "/a.txt", "/B.txt", "/c.txt", "/d.txt"]
    assert data["total"] == 6
    assert data["page"] == 1
    assert data["per_page"] == 50
    assert data["path"] == "/"
    by_name = {item["name"]: item for item in data["items"]}
    assert by_name["docs"]["is_dir"] is True
    assert by_name["a.txt"]["is_dir"] is False
    assert by_name["a.txt"]["size"] == 5
    assert isinstance(by_name["a.txt"]["mtime"], float)


def test_list_pagination(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/list/default?per_page=2&page=2")
    assert status == 200
    names = [item["name"] for item in data["items"]]
    assert names == ["a.txt", "B.txt"]
    assert data["total"] == 6
    assert data["page"] == 2
    assert data["per_page"] == 2


def test_list_subdirectory(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/list/default?path=docs")
    assert status == 200
    assert [item["name"] for item in data["items"]] == ["inner.txt"]
    # The response echoes the raw query parameter (item paths are the
    # normalized "/..." form).
    assert data["path"] == "docs"
    assert data["total"] == 1


def test_list_path_not_directory(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/list/default?path=a.txt")
    assert status == 400
    assert data["detail"] == "Path is not a valid directory."


def test_list_missing_path(secure_client, home):
    status, data = secure_client.call("GET", "/api/files/list/default?path=nope")
    assert status == 404
    assert data["detail"] == "Path not found."


# --- /upload_to_dir -----------------------------------------------------------------------

def test_upload_to_dir_malformed_body(secure_client, home):
    status, data = secure_client.call("POST", "/api/files/upload_to_dir/default", {})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_upload_to_dir_dest_missing(secure_client, home):
    upload_id = initiate_upload(secure_client)
    status, data = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "nope", "filename": "x.txt", "upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 404
    assert data["detail"] == "Path not found."


def test_upload_to_dir_dest_is_file(secure_client, home):
    upload_id = initiate_upload(secure_client)
    status, data = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "a.txt", "filename": "x.txt", "upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 400
    assert data["detail"] == "Destination path is not a valid directory."


def test_upload_to_dir_unknown_upload(secure_client, home):
    status, data = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "docs", "filename": "x.txt", "upload_id": "nope", "total_chunks": 1},
    )
    assert status == 404
    assert data["detail"] == "Upload session not found."


def test_upload_to_dir_missing_chunk(secure_client, home):
    upload_id = initiate_upload(secure_client)
    status, data = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "docs", "filename": "x.txt", "upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 400
    assert data["detail"] == "Missing chunk 0 for upload."


def test_upload_to_dir_success(secure_client, home):
    upload_id = initiate_upload(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"part one ")
    send_chunk(secure_client, upload_id, 1, b"part two")
    status, data = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "docs", "filename": "report.pdf", "upload_id": upload_id, "total_chunks": 2},
    )
    assert status == 200
    assert data["message"] == "File uploaded successfully."
    assert (home / "docs" / "report.pdf").read_bytes() == b"part one part two"
    assert not (Path(settings.upload_dir) / upload_id).exists()


def test_upload_to_dir_unique_filename(secure_client, home):
    upload_id = initiate_upload(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"one")
    status, _ = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "docs", "filename": "report.pdf", "upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 200
    upload_id = initiate_upload(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"two")
    status, _ = secure_client.call(
        "POST",
        "/api/files/upload_to_dir/default",
        {"path": "docs", "filename": "report.pdf", "upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 200
    assert (home / "docs" / "report.pdf").read_bytes() == b"one"
    assert (home / "docs" / "report-1.pdf").read_bytes() == b"two"


# --- /share + /shares -----------------------------------------------------------------------

def test_share_disabled_by_default(secure_client, home):
    # Capabilities are permission-driven: drop the admin's roles (which
    # grant user.sharing) but keep user.storage, so the 403 is specifically
    # about public sharing, not about persistent storage.
    user_manager.USER_DATA["admin"]["roles"] = []
    user_manager.USER_DATA["admin"]["permissions"] = ["user.storage"]
    status, data = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "a.txt"}
    )
    assert status == 403
    assert data["detail"] == "Public file sharing is disabled for this account."


def test_share_malformed_body(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, data = secure_client.call("POST", "/api/files/share", {"home_dir": "default"})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_share_path_not_file(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, data = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "docs"}
    )
    assert status == 400
    assert data["detail"] == "Path does not point to a file."


def test_share_path_missing(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, data = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "nope.txt"}
    )
    assert status == 404
    assert data["detail"] == "Path not found."


def test_share_home_denied(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, data = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "other", "path": "a.txt"}
    )
    assert status == 403
    assert data["detail"] == "Access to home directory 'other' denied."


def test_share_success(secure_client, home, db_path):
    set_admin_settings(secure_client, public_sharing=True)
    before = time.time()
    status, info = secure_client.call(
        "POST",
        "/api/files/share",
        {"home_dir": "default", "path": "a.txt", "password": "hunter2", "expiry_hours": 24},
    )
    assert status == 200, info
    share_id = info["share_id"]
    assert info["original_filename"] == "a.txt"
    assert info["size_bytes"] == 5
    assert info["has_password"] is True
    assert info["url"] == f"/public/{share_id}"
    assert info["expiry_timestamp"] is not None
    assert before + 86400 - 5 < info["expiry_timestamp"] < time.time() + 86400 + 5
    # The file is copied into the public storage area...
    public_copy = Path(settings.public_storage_path) / share_id
    assert public_copy.read_bytes() == b"alpha"
    # ...and the metadata is persisted to the database.
    assert share_id in share_rows(db_path)


def test_share_no_password(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, info = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "a.txt"}
    )
    assert status == 200
    assert info["has_password"] is False
    assert info["expiry_timestamp"] is None


def test_shares_list(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, first = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "a.txt"}
    )
    assert status == 200
    # A share owned by someone else must never appear in this user's list...
    api_module.PUBLIC_SHARES_METADATA["ghost-share"] = PublicShareMetadata(
        owner_username="ghost",
        original_filename="g.txt",
        created_at=time.time() + 100,
        size_bytes=1,
    )
    # ...and a newer own share sorts first.
    api_module.PUBLIC_SHARES_METADATA["newer-share"] = PublicShareMetadata(
        owner_username="admin",
        original_filename="n.txt",
        created_at=time.time() + 50,
        size_bytes=1,
    )
    status, data = secure_client.call("GET", "/api/files/shares")
    assert status == 200
    ids = [s["share_id"] for s in data]
    assert ids == ["newer-share", first["share_id"]]
    entry = data[1]
    # No internal fields (password hash, owner) leak into the response.
    assert set(entry.keys()) == {
        "share_id",
        "original_filename",
        "size_bytes",
        "created_at",
        "expiry_timestamp",
        "has_password",
        "url",
    }
    assert entry["url"] == f"/public/{first['share_id']}"
    assert entry["has_password"] is False


def test_shares_requires_auth(client):
    resp = client.get("/api/files/shares")
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Not authenticated."


def test_delete_share_success(secure_client, home, db_path):
    set_admin_settings(secure_client, public_sharing=True)
    status, info = secure_client.call(
        "POST", "/api/files/share", {"home_dir": "default", "path": "a.txt"}
    )
    assert status == 200
    share_id = info["share_id"]
    status, _ = secure_client.call("DELETE", f"/api/files/share/{share_id}")
    assert status == 204
    assert share_id not in api_module.PUBLIC_SHARES_METADATA
    assert not (Path(settings.public_storage_path) / share_id).exists()
    assert share_id not in share_rows(db_path)


def test_delete_share_unknown(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    status, data = secure_client.call("DELETE", "/api/files/share/nope")
    assert status == 404
    assert data["detail"] == "Share not found or permission denied."


def test_delete_share_other_user(secure_client, home):
    set_admin_settings(secure_client, public_sharing=True)
    api_module.PUBLIC_SHARES_METADATA["ghost-share"] = PublicShareMetadata(
        owner_username="ghost",
        original_filename="g.txt",
        created_at=time.time(),
        size_bytes=1,
    )
    status, data = secure_client.call("DELETE", "/api/files/share/ghost-share")
    assert status == 403
    assert data["detail"] == "Share not found or permission denied."
    # The other user's share is untouched.
    assert "ghost-share" in api_module.PUBLIC_SHARES_METADATA


# --- /launch_from_storage --------------------------------------------------------------------

def test_launch_from_storage_requires_auth(client):
    resp = client.post(
        "/api/files/launch_from_storage",
        json={"home_dir": "default", "path": "a.txt", "application_id": "x"},
    )
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Not authenticated."


def test_launch_from_storage_malformed_body(secure_client, home):
    status, data = secure_client.call(
        "POST",
        "/api/files/launch_from_storage",
        {"home_dir": "default", "path": "a.txt"},
    )
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_launch_from_storage_path_not_file(secure_client, home):
    status, data = secure_client.call(
        "POST",
        "/api/files/launch_from_storage",
        {"home_dir": "default", "path": "docs", "application_id": "whatever"},
    )
    assert status == 400
    assert data["detail"] == "Only files can be opened in an application."


def test_launch_from_storage_unknown_app(secure_client, home):
    status, data = secure_client.call(
        "POST",
        "/api/files/launch_from_storage",
        {"home_dir": "default", "path": "a.txt", "application_id": "ghost-app"},
    )
    assert status == 404
    assert data["detail"] == "Application with ID 'ghost-app' not found."


def test_launch_from_storage_app_not_accessible(secure_client, home):
    # An app restricted to another user denies even admins (no bypass).
    provider_config = {**STORE_APP["provider_config"]}
    provider_config["extensions"] = ["html", "htm", "pdf"]  # flatten store groups
    api_module.INSTALLED_APPS["app-restricted"] = InstalledApp(
        id="app-restricted",
        name="Restricted",
        logo="https://example.invalid/restricted.png",
        url="https://example.invalid/restricted",
        source="Test Store",
        source_app_id="restricted",
        provider="docker",
        home_directories=True,
        users=["someone-else"],
        groups=[],
        app_template="Default",
        provider_config=provider_config,
    )
    status, data = secure_client.call(
        "POST",
        "/api/files/launch_from_storage",
        {"home_dir": "default", "path": "a.txt", "application_id": "app-restricted"},
    )
    assert status == 403
    assert data["detail"] == "You do not have access to 'Restricted'."


def test_launch_from_storage_persistent_storage_off(secure_client, home):
    # Capabilities are permission-driven: strip the admin's roles and
    # direct permissions so user.storage (persistent storage) is lost.
    user_manager.USER_DATA["admin"]["roles"] = []
    user_manager.USER_DATA["admin"]["permissions"] = []
    status, data = secure_client.call(
        "POST",
        "/api/files/launch_from_storage",
        {"home_dir": "default", "path": "a.txt", "application_id": "whatever"},
    )
    assert status == 403
    assert data["detail"] == "Persistent storage is disabled for this account."
