"""Tests for the admin volume-mount endpoints.

A volume mount binds a directory that exists on the Docker host into the app
containers spawned for a single user (``scope=user``) or for every member of
a group (``scope=group``); ``scope=none`` stores the mount but never binds it.

All endpoints live on the encrypted admin router, so the tests drive them
through the E2EE-capable ``secure_client``. One test additionally verifies
that writes reach the configuration database by reading it from an
independent (synchronous) connection — the same access pattern the ``isolate``
fixture uses to wipe it.
"""
from __future__ import annotations

import pytest

from server import volume_mount_manager
from conftest import db_conn

MOUNT_BODY = {
    "name": "media",
    "host_path": "/mnt/media",
    "container_path": "/media",
}


# ---------------------------------------------------------------------------
# Listing / creation
# ---------------------------------------------------------------------------
def test_list_mounts_empty(secure_client):
    status, body = secure_client.call("GET", "/api/admin/volume_mounts")
    assert status == 200, body
    assert body == []


def test_create_mount_scope_none(secure_client):
    status, body = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201, body
    # Missing optional fields fall back to their defaults.
    assert body == {
        "name": "media",
        "host_path": "/mnt/media",
        "container_path": "/media",
        "read_only": False,
        "scope": "none",
        "target": "",
    }
    assert "media" in volume_mount_manager.VOLUME_MOUNTS


def test_create_mount_duplicate(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={"name": "media", "host_path": "/mnt/other", "container_path": "/other"},
    )
    assert status == 409
    assert body["detail"] == "Volume mount 'media' already exists."


def test_create_mount_invalid_name(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={"name": "bad name!", "host_path": "/mnt/x", "container_path": "/x"},
    )
    assert status == 422
    assert "Invalid request body" in body["detail"]


def test_create_mount_invalid_scope(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={**MOUNT_BODY, "scope": "everyone"},
    )
    assert status == 422
    assert body["detail"] == "scope must be 'none', 'user', or 'group'."


def test_create_mount_user_scope_unknown_user(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={**MOUNT_BODY, "scope": "user", "target": "ghost"},
    )
    assert status == 404
    assert body["detail"] == "User 'ghost' not found."


def test_create_mount_group_scope_unknown_group(secure_client):
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={**MOUNT_BODY, "scope": "group", "target": "ghost"},
    )
    assert status == 404
    assert body["detail"] == "Group 'ghost' not found."


def test_create_mount_user_scope(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={**MOUNT_BODY, "scope": "user", "target": "alice"},
    )
    assert status == 201, body
    assert body["scope"] == "user"
    assert body["target"] == "alice"
    # The mount applies to alice ...
    assert volume_mount_manager.get_mounts_for_user("alice", "none") == [body]
    # ... but not to anyone else.
    assert volume_mount_manager.get_mounts_for_user("bob", "none") == []


def test_create_mount_group_scope(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST",
        "/api/admin/volume_mounts",
        body={
            "name": "depot",
            "host_path": "/mnt/depot",
            "container_path": "depot",
            "scope": "group",
            "target": "devs",
        },
    )
    assert status == 201, body
    assert body["scope"] == "group"
    assert body["target"] == "devs"
    # A member of 'devs' sees the mount; a member of another group does not.
    assert volume_mount_manager.get_mounts_for_user("alice", "devs") == [body]
    assert volume_mount_manager.get_mounts_for_user("alice", "other") == []


# ---------------------------------------------------------------------------
# Updating
# ---------------------------------------------------------------------------
def test_update_mount(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    status, body = secure_client.call(
        "PUT",
        "/api/admin/volume_mounts/media",
        body={"host_path": "/mnt/new", "container_path": "/media2", "read_only": True},
    )
    assert status == 200, body
    assert body["host_path"] == "/mnt/new"
    assert body["container_path"] == "/media2"
    assert body["read_only"] is True
    # Fields omitted from the update keep their previous values.
    assert body["scope"] == "none"
    assert body["target"] == ""


def test_update_mount_unknown(secure_client):
    status, body = secure_client.call(
        "PUT",
        "/api/admin/volume_mounts/nope",
        body={"host_path": "/mnt/x", "container_path": "/x"},
    )
    assert status == 404
    assert body["detail"] == "Volume mount 'nope' not found."


def test_update_mount_invalid_scope(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    status, body = secure_client.call(
        "PUT",
        "/api/admin/volume_mounts/media",
        body={**MOUNT_BODY, "scope": "everyone"},
    )
    assert status == 422
    assert body["detail"] == "scope must be 'none', 'user', or 'group'."


def test_update_mount_unknown_target(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    status, body = secure_client.call(
        "PUT",
        "/api/admin/volume_mounts/media",
        body={**MOUNT_BODY, "scope": "user", "target": "ghost"},
    )
    assert status == 404
    assert body["detail"] == "User 'ghost' not found."


# ---------------------------------------------------------------------------
# Deletion / persistence
# ---------------------------------------------------------------------------
def test_delete_mount(secure_client):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/volume_mounts/media")
    assert status == 204
    assert "media" not in volume_mount_manager.VOLUME_MOUNTS
    status, listed = secure_client.call("GET", "/api/admin/volume_mounts")
    assert status == 200
    assert listed == []


def test_delete_mount_unknown(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/volume_mounts/nope")
    assert status == 404
    assert body["detail"] == "Volume mount 'nope' not found."


def test_mount_persists_to_database(secure_client, db_path):
    status, _ = secure_client.call("POST", "/api/admin/volume_mounts", body=MOUNT_BODY)
    assert status == 201
    # The row is committed and visible from an independent connection.
    conn = db_conn(db_path)
    try:
        rows = conn.execute(
            "SELECT name, host_path, container_path, scope, target FROM volume_mounts"
        ).fetchall()
    finally:
        conn.close()
    assert rows == [("media", "/mnt/media", "/media", "none", "")]

    # Deleting removes the row as well.
    status, _ = secure_client.call("DELETE", "/api/admin/volume_mounts/media")
    assert status == 204
    conn = db_conn(db_path)
    try:
        rows = conn.execute("SELECT name FROM volume_mounts").fetchall()
    finally:
        conn.close()
    assert rows == []
