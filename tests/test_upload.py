"""Tier 4 — upload flow: /api/upload/{initiate,chunk,to_storage}.

Covers the encrypted upload pipeline: session initiation, chunked
Base64 uploads, and reassembly into the user's persistent storage.
Also pins the upload-id containment rule: ids that resolve outside
``settings.upload_dir`` (e.g. ``..``) must be rejected as unknown
sessions rather than read from / written to elsewhere on disk.
"""
import base64
import json
import os
from pathlib import Path

from server import user_manager
from server.settings import settings


# --- helpers -----------------------------------------------------------------

def initiate(secure_client, filename="report.pdf", total_size=100):
    status, data = secure_client.call(
        "POST",
        "/api/upload/initiate",
        {"filename": filename, "total_size": total_size},
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


def to_storage(secure_client, upload_id, home_name, filename="report.pdf", total_chunks=1):
    return secure_client.call(
        "POST",
        "/api/upload/to_storage",
        {
            "filename": filename,
            "upload_id": upload_id,
            "total_chunks": total_chunks,
            "home_name": home_name,
        },
    )


def set_admin_settings(secure_client, **overrides):
    status, data = secure_client.call(
        "PUT", "/api/admin/people/admin", {"settings": overrides}
    )
    assert status == 200, data


# --- /initiate ----------------------------------------------------------------

def test_initiate_requires_auth(client):
    resp = client.post("/api/upload/initiate", json={"filename": "a.txt", "total_size": 1})
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Not authenticated."


def test_initiate_success(secure_client):
    upload_id = initiate(secure_client, filename="report.pdf", total_size=1234)
    assert len(upload_id) == 36  # uuid4
    session_dir = Path(settings.upload_dir) / upload_id
    assert session_dir.is_dir()
    assert os.access(str(session_dir), os.X_OK)  # created 0o700
    metadata = json.loads((session_dir / "metadata.json").read_text())
    assert metadata["filename"] == "report.pdf"
    assert metadata["size"] == 1234
    assert isinstance(metadata["started"], float)


def test_initiate_malformed_body(secure_client):
    status, data = secure_client.call("POST", "/api/upload/initiate", {"filename": "a.txt"})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


# --- /chunk ---------------------------------------------------------------------

def test_chunk_unknown_upload(secure_client):
    status, data = secure_client.call(
        "POST",
        "/api/upload/chunk",
        {"upload_id": "nope", "chunk_index": 0, "chunk_data_b64": "aGk="},
    )
    assert status == 404
    assert data["detail"] == "Upload session not found."


def test_chunk_success(secure_client):
    upload_id = initiate(secure_client)
    status, data = send_chunk(secure_client, upload_id, 0, b"hello world")
    assert status == 200
    assert data == {"status": "ok", "chunk_index": 0}
    written = (Path(settings.upload_dir) / upload_id / "chunk_0").read_bytes()
    assert written == b"hello world"


def test_chunk_invalid_base64(secure_client):
    upload_id = initiate(secure_client)
    status, data = secure_client.call(
        "POST",
        "/api/upload/chunk",
        {"upload_id": upload_id, "chunk_index": 0, "chunk_data_b64": "not-valid-b64!!!"},
    )
    assert status == 400
    assert data["detail"].startswith("Invalid Base64 chunk data:")


def test_chunk_malformed_body(secure_client):
    status, data = secure_client.call("POST", "/api/upload/chunk", {})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_chunk_traversal_rejected(secure_client):
    """Upload ids must resolve inside settings.upload_dir.

    ``..`` points at the upload dir's parent, which exists — without a
    containment check the chunk would be written there (and
    /to_storage would reassemble it from there).
    """
    parent = Path(settings.upload_dir).parent
    for bad in ("..", ".", ""):
        status, data = secure_client.call(
            "POST",
            "/api/upload/chunk",
            {"upload_id": bad, "chunk_index": 0, "chunk_data_b64": "aGk="},
        )
        assert status == 404, (bad, data)
        assert data["detail"] == "Upload session not found."
    # Nothing may have been written outside the upload dir.
    assert not (parent / "chunk_0").exists()


# --- /to_storage ------------------------------------------------------------------

def test_to_storage_requires_auth(client):
    resp = client.post(
        "/api/upload/to_storage",
        json={"filename": "a.txt", "upload_id": "x", "total_chunks": 1, "home_name": "work"},
    )
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Not authenticated."


def test_to_storage_persistent_storage_disabled(secure_client):
    # Capabilities are permission-driven: strip the admin's roles and
    # direct permissions so user.storage (persistent storage) is lost.
    user_manager.USER_DATA["admin"]["roles"] = []
    user_manager.USER_DATA["admin"]["permissions"] = []
    upload_id = initiate(secure_client)
    status, data = to_storage(secure_client, upload_id, home_name="work")
    assert status == 403
    assert data["detail"] == "Persistent storage is disabled for this account."


def test_to_storage_malformed_body(secure_client):
    status, data = secure_client.call("POST", "/api/upload/to_storage", {})
    assert status == 422
    assert data["detail"].startswith("Invalid request body:")


def test_to_storage_unknown_home(secure_client):
    status, data = to_storage(secure_client, upload_id="whatever", home_name="nope")
    assert status == 404
    assert data["detail"] == "Home directory 'nope' not found for user."


def test_to_storage_unknown_upload(secure_client):
    user_manager.create_home_dir("admin", "work")
    status, data = to_storage(secure_client, upload_id="nope", home_name="work")
    assert status == 404
    assert data["detail"] == "Upload session not found."


def test_to_storage_missing_chunk(secure_client):
    user_manager.create_home_dir("admin", "work")
    upload_id = initiate(secure_client, filename="a.txt")
    send_chunk(secure_client, upload_id, 0, b"data")
    status, data = to_storage(
        secure_client, upload_id, home_name="work", filename="a.txt", total_chunks=2
    )
    assert status == 400
    assert data["detail"] == "Missing chunk 1 for upload."


def test_to_storage_success(secure_client):
    user_manager.create_home_dir("admin", "work")
    upload_id = initiate(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"part one ")
    send_chunk(secure_client, upload_id, 1, b"part two")
    status, data = to_storage(secure_client, upload_id, home_name="work", total_chunks=2)
    assert status == 200
    assert data["status"] == "success"
    assert data["message"] == "File 'report.pdf' uploaded successfully."
    dest = Path(settings.storage_path) / "admin" / "_vreckan_shared_files" / "report.pdf"
    assert dest.read_bytes() == b"part one part two"
    # The upload session is consumed and cleaned up.
    assert not (Path(settings.upload_dir) / upload_id).exists()


def test_to_storage_unique_filename(secure_client):
    user_manager.create_home_dir("admin", "work")
    shared = Path(settings.storage_path) / "admin" / "_vreckan_shared_files"
    upload_id = initiate(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"one")
    status, _ = to_storage(secure_client, upload_id, home_name="work")
    assert status == 200
    upload_id = initiate(secure_client, filename="report.pdf")
    send_chunk(secure_client, upload_id, 0, b"two")
    status, _ = to_storage(secure_client, upload_id, home_name="work")
    assert status == 200
    assert (shared / "report.pdf").read_bytes() == b"one"
    assert (shared / "report-1.pdf").read_bytes() == b"two"
