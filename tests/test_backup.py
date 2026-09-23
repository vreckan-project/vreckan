"""Tests for the admin backup / restore endpoints.

A backup is a ``tar.gz`` of the persistent data root — including a logical
JSON dump of the configuration database — written into the backups dir.
Restore either re-extracts a stored archive or reassembles one uploaded in
base64 chunks, then restores the database and reloads the durable in-memory
state (users, groups, app configs, templates, public shares).

Every endpoint lives on the encrypted admin router, so the tests drive them
through the E2EE-capable ``secure_client``.

The autouse ``isolate`` fixture re-points the storage/cache path settings at
the per-test sandbox but not ``data_root`` / ``backups_path`` (those default
to the live ``/data`` tree). The ``backup_sandbox`` fixture below re-points
those two as well, so no test can touch the real data directory.
"""
from __future__ import annotations

import base64
import io
import os
import re
import tarfile
import time

import pytest

from server import backup_manager, user_manager
from server.settings import settings

BACKUP_NAME_RE = re.compile(r"^backup_\d{8}_\d{6}(_ud)?(_\d+)?\.tar\.gz$")


@pytest.fixture
def backup_sandbox(tmp_path, monkeypatch):
    """Point the data root and backups dir at the per-test sandbox.

    Listed before ``client`` / ``secure_client`` in test signatures so the
    sandbox is in place before the app boots (the backup endpoints read both
    settings at request time).
    """
    data_root = tmp_path / "data"
    data_root.mkdir(parents=True, exist_ok=True)
    monkeypatch.setattr(settings, "data_root", str(data_root))
    monkeypatch.setattr(settings, "backups_path", str(data_root / "backups"))
    return data_root


def _backup_path(name: str) -> str:
    return os.path.join(settings.backups_path, name)


def _split(data: bytes, n: int) -> list:
    """Split ``data`` into at most ``n`` roughly-equal chunks."""
    size = (len(data) + n - 1) // n
    return [data[i : i + size] for i in range(0, len(data), size)]


def _malicious_archive() -> bytes:
    """A valid gzip tarball whose single member points outside the data root."""
    buf = io.BytesIO()
    payload = b"pwned"
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        info = tarfile.TarInfo("../evil.txt")
        info.size = len(payload)
        info.mtime = 0
        tar.addfile(info, io.BytesIO(payload))
    return buf.getvalue()


# ---------------------------------------------------------------------------
# Listing / creation
# ---------------------------------------------------------------------------
def test_list_backups_empty(backup_sandbox, secure_client):
    status, body = secure_client.call("GET", "/api/admin/backup")
    assert status == 200
    assert body == []


def test_create_and_list_backup(backup_sandbox, secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/backup/create", body={"include_user_data": False}
    )
    assert status == 201, body
    assert BACKUP_NAME_RE.match(body["name"])
    assert body["size"] > 0
    assert body["include_user_data"] is False
    # The archive really exists on disk in the sandboxed backups dir ...
    assert os.path.isfile(_backup_path(body["name"]))
    # ... and it is a valid gzip tarball carrying the database dump.
    with tarfile.open(_backup_path(body["name"]), "r:gz") as tar:
        assert "db_dump.json" in tar.getnames()
    # The listing reports it.
    status, listed = secure_client.call("GET", "/api/admin/backup")
    assert status == 200
    assert [b["name"] for b in listed] == [body["name"]]


def test_create_backup_with_user_data(backup_sandbox, secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/backup/create", body={"include_user_data": True}
    )
    assert status == 201, body
    assert "_ud" in body["name"]
    assert body["include_user_data"] is True


def test_create_backup_same_second_collision(backup_sandbox, secure_client):
    # Two creations in the same second must not clobber each other: the
    # second gets a numeric suffix.
    _, first = secure_client.call("POST", "/api/admin/backup/create", body={})
    _, second = secure_client.call("POST", "/api/admin/backup/create", body={})
    assert first["name"] != second["name"]
    status, listed = secure_client.call("GET", "/api/admin/backup")
    assert status == 200
    assert {b["name"] for b in listed} == {first["name"], second["name"]}


# ---------------------------------------------------------------------------
# Download (chunked)
# ---------------------------------------------------------------------------
def test_download_backup_reassembles_file(backup_sandbox, secure_client):
    _, created = secure_client.call("POST", "/api/admin/backup/create", body={})
    name = created["name"]
    with open(_backup_path(name), "rb") as f:
        on_disk = f.read()
    # The test archive is far smaller than one 2 MiB chunk.
    status, chunk0 = secure_client.call(
        "GET", f"/api/admin/backup/{name}/download?chunk_index=0"
    )
    assert status == 200
    assert chunk0["filename"] == name
    assert chunk0["is_last_chunk"] is True
    assert base64.b64decode(chunk0["chunk_data_b64"]) == on_disk
    # Asking for the chunk past the end yields an empty final chunk.
    status, chunk1 = secure_client.call(
        "GET", f"/api/admin/backup/{name}/download?chunk_index=1"
    )
    assert status == 200
    assert chunk1["is_last_chunk"] is True
    assert base64.b64decode(chunk1["chunk_data_b64"]) == b""


def test_download_backup_invalid_name(backup_sandbox, secure_client):
    status, body = secure_client.call("GET", "/api/admin/backup/bogus.tar.gz/download")
    assert status == 404
    assert "Invalid backup name" in body["detail"]


def test_download_backup_unknown_name(backup_sandbox, secure_client):
    # Valid name shape, but no such archive exists.
    status, body = secure_client.call(
        "GET", "/api/admin/backup/backup_20200101_000000.tar.gz/download"
    )
    assert status == 404
    assert "not found" in body["detail"]


def test_resolve_backup_rejects_traversal(backup_sandbox):
    # Unit-level: the name guard must reject anything that could escape the
    # backups dir, independent of HTTP routing / URL normalisation.
    with pytest.raises(backup_manager.BackupError):
        backup_manager._resolve_backup("../vreckan.db")
    with pytest.raises(backup_manager.BackupError):
        backup_manager._resolve_backup("/etc/passwd")
    # Valid shape but missing on disk.
    with pytest.raises(backup_manager.BackupError):
        backup_manager._resolve_backup("backup_20200101_000000.tar.gz")


# ---------------------------------------------------------------------------
# Deletion
# ---------------------------------------------------------------------------
def test_delete_backup(backup_sandbox, secure_client):
    _, created = secure_client.call("POST", "/api/admin/backup/create", body={})
    name = created["name"]
    status, _ = secure_client.call("DELETE", f"/api/admin/backup/{name}")
    assert status == 204
    assert not os.path.exists(_backup_path(name))
    status, listed = secure_client.call("GET", "/api/admin/backup")
    assert status == 200
    assert listed == []
    # Deleting again is a 404.
    status, body = secure_client.call("DELETE", f"/api/admin/backup/{name}")
    assert status == 404


# ---------------------------------------------------------------------------
# Restore (stored + chunked upload)
# ---------------------------------------------------------------------------
def test_restore_from_stored_rolls_back_changes(backup_sandbox, secure_client):
    # Snapshot the current state (bootstrap admin only).
    _, created = secure_client.call("POST", "/api/admin/backup/create", body={})
    name = created["name"]
    # Drift: provision a user after the snapshot was taken.
    status, _ = secure_client.call("POST", "/api/admin/people", body={"username": "temp-user"})
    assert status == 201
    assert user_manager.get_user("temp-user") is not None
    # Restore the snapshot: the database is rolled back and the in-memory
    # user cache is reloaded from it.
    status, body = secure_client.call(
        "POST", "/api/admin/backup/restore", body={"name": name}
    )
    assert status == 200, body
    assert body == {"source": name, "restored": True}
    assert user_manager.get_user("temp-user") is None
    # The pre-snapshot admin is still present.
    assert user_manager.get_user("admin") is not None


def test_restore_missing_name(backup_sandbox, secure_client):
    status, body = secure_client.call("POST", "/api/admin/backup/restore", body={})
    assert status == 422
    assert "name" in body["detail"]


def test_restore_unknown_backup(backup_sandbox, secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/backup/restore", body={"name": "backup_20200101_000000.tar.gz"}
    )
    assert status == 400
    assert "not found" in body["detail"]


def test_restore_uploaded_archive_chunked(backup_sandbox, secure_client):
    # Snapshot, then drift.
    _, created = secure_client.call("POST", "/api/admin/backup/create", body={})
    name = created["name"]
    with open(_backup_path(name), "rb") as f:
        archive = f.read()
    status, _ = secure_client.call("POST", "/api/admin/people", body={"username": "temp-user"})
    assert status == 201
    # Upload the same archive back through the chunked upload flow.
    status, initiated = secure_client.call("POST", "/api/admin/backup/restore/initiate")
    assert status == 200
    upload_id = initiated["upload_id"]
    chunks = _split(archive, 2)
    assert len(chunks) == 2
    for i, chunk in enumerate(chunks):
        status, body = secure_client.call(
            "POST",
            "/api/admin/backup/restore/chunk",
            body={
                "upload_id": upload_id,
                "chunk_index": i,
                "chunk_data_b64": base64.b64encode(chunk).decode(),
            },
        )
        assert status == 200, body
        assert body["chunk_index"] == i
    status, body = secure_client.call(
        "POST",
        "/api/admin/backup/restore/finalize",
        body={"upload_id": upload_id, "total_chunks": 2},
    )
    assert status == 200, body
    assert body == {"source": "upload", "restored": True}
    # The restore rolled the drift back again.
    assert user_manager.get_user("temp-user") is None
    # The temporary upload dir is cleaned up.
    assert not os.path.isdir(os.path.join(settings.backups_path, f"restore_{upload_id}"))


def test_restore_chunk_unknown_session(backup_sandbox, secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/backup/restore/chunk",
        body={
            "upload_id": "no-such-session",
            "chunk_index": 0,
            "chunk_data_b64": base64.b64encode(b"x").decode(),
        },
    )
    assert status == 404
    assert "not found" in body["detail"]


def test_restore_finalize_missing_chunk(backup_sandbox, secure_client):
    status, initiated = secure_client.call("POST", "/api/admin/backup/restore/initiate")
    assert status == 200
    upload_id = initiated["upload_id"]
    # Upload only the first of the two promised chunks.
    status, _ = secure_client.call(
        "POST",
        "/api/admin/backup/restore/chunk",
        body={
            "upload_id": upload_id,
            "chunk_index": 0,
            "chunk_data_b64": base64.b64encode(b"partial").decode(),
        },
    )
    assert status == 200
    status, body = secure_client.call(
        "POST",
        "/api/admin/backup/restore/finalize",
        body={"upload_id": upload_id, "total_chunks": 2},
    )
    assert status == 400
    assert "Missing chunk 1" in body["detail"]
    # The half-finished upload dir is cleaned up even on failure.
    assert not os.path.isdir(os.path.join(settings.backups_path, f"restore_{upload_id}"))


def test_restore_rejects_path_traversal_archive(backup_sandbox, secure_client):
    status, initiated = secure_client.call("POST", "/api/admin/backup/restore/initiate")
    assert status == 200
    upload_id = initiated["upload_id"]
    status, _ = secure_client.call(
        "POST",
        "/api/admin/backup/restore/chunk",
        body={
            "upload_id": upload_id,
            "chunk_index": 0,
            "chunk_data_b64": base64.b64encode(_malicious_archive()).decode(),
        },
    )
    assert status == 200
    status, body = secure_client.call(
        "POST",
        "/api/admin/backup/restore/finalize",
        body={"upload_id": upload_id, "total_chunks": 1},
    )
    assert status == 400
    assert "traversal" in body["detail"]
    # Nothing was written outside the sandboxed data root.
    assert not (backup_sandbox / "evil.txt").exists()


# ---------------------------------------------------------------------------
# Retention
# ---------------------------------------------------------------------------
def test_backup_retention_prunes_old_backups(backup_sandbox, secure_client, monkeypatch):
    monkeypatch.setattr(settings, "backup_retention", 1)
    _, first = secure_client.call("POST", "/api/admin/backup/create", body={})
    time.sleep(1.1)  # cross a second boundary so names and mtimes differ
    _, second = secure_client.call("POST", "/api/admin/backup/create", body={})
    time.sleep(1.1)
    _, third = secure_client.call("POST", "/api/admin/backup/create", body={})
    assert first["name"] < second["name"] < third["name"]
    status, listed = secure_client.call("GET", "/api/admin/backup")
    assert status == 200
    # Retention keeps only the single newest archive.
    assert [b["name"] for b in listed] == [third["name"]]
    assert os.path.exists(_backup_path(third["name"]))
    assert not os.path.exists(_backup_path(first["name"]))
