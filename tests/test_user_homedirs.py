"""User-facing home-directory endpoints (/api/homedirs): the
self-service counterpart of the admin people/homedirs API.

The admin account has no home directories by default (none are
created at startup), so a fresh list is empty.
"""
import os

import pytest

import server.api as api_module
from server import user_manager


def test_my_homedirs_requires_auth(client):
    assert client.get("/api/homedirs").status_code == 401


def test_my_homedirs_empty(secure_client):
    status, data = secure_client.call("GET", "/api/homedirs")
    assert status == 200
    assert data == {"home_dirs": []}


def test_create_my_home_dir(secure_client):
    status, data = secure_client.call(
        "POST", "/api/homedirs", body={"home_name": "work"},
    )
    assert status == 201
    assert data == {"status": "success", "home_name": "work"}
    status, data = secure_client.call("GET", "/api/homedirs")
    assert data == {"home_dirs": ["work"]}
    # The skeleton directory exists on disk under the user's storage.
    assert os.path.isdir(
        os.path.join(api_module.settings.storage_path, "admin", "work")
    )


def test_create_my_home_dir_duplicate(secure_client):
    secure_client.call("POST", "/api/homedirs", body={"home_name": "work"})
    status, data = secure_client.call(
        "POST", "/api/homedirs", body={"home_name": "work"},
    )
    assert status == 409
    assert data["detail"] == "Home directory 'work' already exists for user 'admin'."


def test_create_my_home_dir_invalid_name(secure_client):
    status, data = secure_client.call(
        "POST", "/api/homedirs", body={"home_name": "bad name!"},
    )
    assert status == 422
    assert "Invalid request body" in data["detail"]


def test_homedirs_disabled_when_persistent_storage_off(secure_client):
    # Capabilities are permission-driven: strip the admin's roles and
    # direct permissions so user.storage (persistent storage) is lost.
    # Effective permissions re-resolve on every request, so the in-memory
    # change takes effect immediately.
    user_manager.USER_DATA["admin"]["roles"] = []
    user_manager.USER_DATA["admin"]["permissions"] = []
    status, data = secure_client.call("GET", "/api/homedirs")
    assert status == 403
    assert data["detail"] == "Persistent storage is disabled for this account."


def test_delete_my_home_dir(secure_client):
    secure_client.call("POST", "/api/homedirs", body={"home_name": "work"})
    path = os.path.join(api_module.settings.storage_path, "admin", "work")
    assert os.path.isdir(path)
    status, _ = secure_client.call("DELETE", "/api/homedirs/work")
    assert status == 204
    assert not os.path.exists(path)
    status, data = secure_client.call("GET", "/api/homedirs")
    assert data == {"home_dirs": []}


def test_delete_my_home_dir_missing(secure_client):
    status, data = secure_client.call("DELETE", "/api/homedirs/nope")
    assert status == 404
    assert data["detail"] == "Home directory 'nope' not found for user 'admin'."
