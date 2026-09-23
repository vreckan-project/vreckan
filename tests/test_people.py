"""Tests for the people (users/admins), groups, and home-directory endpoints.

All of these live on the encrypted admin router, so the tests drive them
through the E2EE-capable ``secure_client``. The autouse ``isolate`` fixture
wipes the database and in-memory state per test, so every test starts from
the bootstrap ``admin`` account alone.

Notable status-code semantics covered here:

* ``POST /people`` maps ``ValueError`` to 409 — so an *invalid* username is
  a 409 (the username pattern is enforced in the manager, not the model),
  while a *missing* username is a 422 (Pydantic validation).
* ``DELETE /people/admin`` is a 403 (the root account is protected); every
  other unknown-account error is a 404.
* Home-directory endpoints are 403 when the account's effective
  ``persistent_storage`` setting is off.
"""
from __future__ import annotations

from pathlib import Path

import pytest

from server import user_manager
from server.settings import settings


def _roster(secure_client) -> dict:
    """Fetch the management roster (``POST /api/admin/data``)."""
    status, body = secure_client.call("POST", "/api/admin/data")
    assert status == 200, body
    return body


# ---------------------------------------------------------------------------
# Creating people
# ---------------------------------------------------------------------------
def test_create_user(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201, body
    user = body["user"]
    assert user["username"] == "alice"
    assert user["is_admin"] is False
    assert user["is_sso"] is False
    assert user["has_password"] is False
    # New accounts are always created active, with complete default settings.
    assert user["settings"]["active"] is True
    assert user["settings"]["persistent_storage"] is True
    # The account is really in the in-memory roster.
    assert "alice" in user_manager.USER_DATA


def test_create_admin(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bob", "is_admin": True}
    )
    assert status == 201, body
    assert body["user"]["is_admin"] is True
    assert "bob" in user_manager.USER_DATA


def test_create_user_duplicate(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 409
    assert body["detail"] == "User 'alice' already exists."


def test_create_admin_collides_with_user(secure_client):
    # The roster is unified: an admin cannot reuse an existing user's name.
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice", "is_admin": True}
    )
    assert status == 409
    assert body["detail"] == "User or admin 'alice' already exists."


def test_create_person_invalid_username(secure_client):
    # The username pattern is enforced in the manager (ValueError -> 409),
    # not in the request model — so a malformed name is a 409, not a 422.
    status, body = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bad name!"}
    )
    assert status == 409
    assert body["detail"] == (
        "Invalid username. Use only letters, numbers, underscore, or hyphen."
    )


def test_create_person_missing_username(secure_client):
    # A missing username *is* a model-level validation error -> 422.
    status, body = secure_client.call("POST", "/api/admin/people", body={})
    assert status == 422
    assert "Invalid request body" in body["detail"]


# ---------------------------------------------------------------------------
# Updating people
# ---------------------------------------------------------------------------
def test_update_user_settings(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    # Settings are replaced wholesale; missing keys fall back to defaults.
    status, body = secure_client.call(
        "PUT", "/api/admin/people/alice", body={"settings": {"active": False}}
    )
    assert status == 200, body
    assert body["username"] == "alice"
    assert body["settings"]["active"] is False
    assert body["settings"]["gpu"] is True  # untouched key keeps its default
    # The roster reflects the change.
    roster = _roster(secure_client)
    alice = next(u for u in roster["users"] if u["username"] == "alice")
    assert alice["settings"]["active"] is False


def test_update_user_unknown(secure_client):
    status, body = secure_client.call(
        "PUT", "/api/admin/people/ghost", body={"settings": {}}
    )
    assert status == 404
    assert body["detail"] == "User or admin 'ghost' not found."


def test_update_user_missing_settings(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call("PUT", "/api/admin/people/alice", body={})
    assert status == 422
    assert "Invalid request body" in body["detail"]


# ---------------------------------------------------------------------------
# Deleting people
# ---------------------------------------------------------------------------
def test_delete_user(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/people/alice")
    assert status == 204
    assert "alice" not in user_manager.USER_DATA
    roster = _roster(secure_client)
    assert all(u["username"] != "alice" for u in roster["users"] + roster["admins"])


def test_delete_root_admin_forbidden(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/people/admin")
    assert status == 403
    assert body["detail"] == "The root 'admin' account cannot be deleted."
    # ...and it is still there.
    assert "admin" in user_manager.USER_DATA


def test_delete_unknown_user(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/people/ghost")
    assert status == 404
    assert body["detail"] == "User or admin 'ghost' not found."


def test_delete_other_admin(secure_client):
    # Only the root 'admin' account is protected; other admins are deletable.
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bob", "is_admin": True}
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/people/bob")
    assert status == 204
    assert "bob" not in user_manager.USER_DATA


# ---------------------------------------------------------------------------
# Changing admin status (promote / demote)
# ---------------------------------------------------------------------------
def test_promote_user_to_admin(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people/alice/admin-status", body={"is_admin": True}
    )
    assert status == 200, body
    assert body["is_admin"] is True
    # The account moved from the users list to the admins list.
    roster = _roster(secure_client)
    assert all(u["username"] != "alice" for u in roster["users"])
    assert any(u["username"] == "alice" for u in roster["admins"])


def test_demote_admin_to_user(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bob", "is_admin": True}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people/bob/admin-status", body={"is_admin": False}
    )
    assert status == 200, body
    assert body["is_admin"] is False
    roster = _roster(secure_client)
    assert all(u["username"] != "bob" for u in roster["admins"])
    assert any(u["username"] == "bob" for u in roster["users"])


def test_set_admin_status_idempotent(secure_client):
    # Promoting an account that is already an admin is a no-op (200, unchanged).
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bob", "is_admin": True}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people/bob/admin-status", body={"is_admin": True}
    )
    assert status == 200
    assert body["is_admin"] is True


def test_demote_root_admin_forbidden(secure_client):
    # The bootstrap 'admin' account cannot be demoted (would lock everyone out).
    status, body = secure_client.call(
        "POST", "/api/admin/people/admin/admin-status", body={"is_admin": False}
    )
    assert status == 403
    assert "cannot be demoted" in body["detail"]
    # ...and it is still an admin.
    assert user_manager.USER_DATA["admin"]["is_admin"] is True


def test_set_admin_status_unknown(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/people/ghost/admin-status", body={"is_admin": True}
    )
    assert status == 404
    assert body["detail"] == "User 'ghost' not found."


def test_set_admin_status_missing_body(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people/alice/admin-status", body={}
    )
    assert status == 422


# ---------------------------------------------------------------------------
# Home directories
# ---------------------------------------------------------------------------
def test_list_homedirs_empty(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call("GET", "/api/admin/people/alice/homedirs")
    assert status == 200, body
    assert body == {"home_dirs": []}


def test_list_homedirs_unknown_user(secure_client):
    status, body = secure_client.call("GET", "/api/admin/people/ghost/homedirs")
    assert status == 404
    assert body["detail"] == "User or admin 'ghost' not found."


def test_homedirs_disabled_when_persistent_storage_off(secure_client):
    status, _ = secure_client.call(
        "POST",
        "/api/admin/people",
        body={"username": "carol", "settings": {"persistent_storage": False}},
    )
    assert status == 201
    # Capabilities are permission-driven: carol's built-in 'user' role
    # grants user.storage, so strip her roles to drop the capability.
    user_manager.USER_DATA["carol"]["roles"] = []
    status, body = secure_client.call("GET", "/api/admin/people/carol/homedirs")
    assert status == 403
    assert body["detail"] == "Persistent storage is disabled for this account."


def test_create_home_dir(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people/alice/homedirs", body={"home_name": "docs"}
    )
    assert status == 201, body
    assert body == {"status": "success"}
    # The directory skeleton really exists under the user's storage dir.
    home = Path(settings.storage_path) / "alice" / "docs"
    assert (home / "Desktop" / "files").is_dir()
    # ...and the listing reports it.
    status, listed = secure_client.call("GET", "/api/admin/people/alice/homedirs")
    assert status == 200
    assert listed == {"home_dirs": ["docs"]}


def test_create_home_dir_duplicate(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, _ = secure_client.call(
        "POST", "/api/admin/people/alice/homedirs", body={"home_name": "docs"}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/people/alice/homedirs", body={"home_name": "docs"}
    )
    assert status == 409
    assert body["detail"] == "Home directory 'docs' already exists for user 'alice'."


def test_create_home_dir_invalid_name(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    # The home-name pattern lives in the model -> 422 (unlike usernames).
    status, body = secure_client.call(
        "POST", "/api/admin/people/alice/homedirs", body={"home_name": "bad name!"}
    )
    assert status == 422
    assert "Invalid request body" in body["detail"]


def test_create_home_dir_unknown_user(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/people/ghost/homedirs", body={"home_name": "docs"}
    )
    assert status == 404
    assert body["detail"] == "User or admin 'ghost' not found."


def test_delete_home_dir(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, _ = secure_client.call(
        "POST", "/api/admin/people/alice/homedirs", body={"home_name": "docs"}
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/people/alice/homedirs/docs")
    assert status == 204
    # The directory is really gone from disk.
    assert not (Path(settings.storage_path) / "alice" / "docs").exists()
    status, listed = secure_client.call("GET", "/api/admin/people/alice/homedirs")
    assert listed == {"home_dirs": []}


def test_delete_home_dir_missing(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, body = secure_client.call(
        "DELETE", "/api/admin/people/alice/homedirs/docs"
    )
    assert status == 404
    assert body["detail"] == "Home directory 'docs' not found for user 'alice'."


# ---------------------------------------------------------------------------
# Groups
# ---------------------------------------------------------------------------
def test_create_group(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201, body
    assert body["name"] == "devs"
    assert "devs" in user_manager.GROUP_DATA


def test_create_group_duplicate(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201
    status, body = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 409
    assert body["detail"] == "Group 'devs' already exists."


def test_create_group_invalid_name(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "bad name!", "settings": {}}
    )
    assert status == 422
    assert "Invalid request body" in body["detail"]


def test_update_group(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201
    status, body = secure_client.call(
        "PUT", "/api/admin/groups/devs", body={"settings": {"gpu": False}}
    )
    assert status == 200, body
    assert body["name"] == "devs"
    assert body["settings"]["gpu"] is False


def test_update_group_unknown(secure_client):
    status, body = secure_client.call(
        "PUT", "/api/admin/groups/ghost", body={"settings": {}}
    )
    assert status == 404
    assert body["detail"] == "Group 'ghost' not found."


def test_delete_group(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201
    status, _ = secure_client.call("DELETE", "/api/admin/groups/devs")
    assert status == 204
    assert "devs" not in user_manager.GROUP_DATA


def test_delete_group_unknown(secure_client):
    status, body = secure_client.call("DELETE", "/api/admin/groups/ghost")
    assert status == 404
    assert body["detail"] == "Group 'ghost' not found."


# ---------------------------------------------------------------------------
# Management roster
# ---------------------------------------------------------------------------
def test_roster_reflects_people_and_groups(secure_client):
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "alice"}
    )
    assert status == 201
    status, _ = secure_client.call(
        "POST", "/api/admin/people", body={"username": "bob", "is_admin": True}
    )
    assert status == 201
    status, _ = secure_client.call(
        "POST", "/api/admin/groups", body={"name": "devs", "settings": {}}
    )
    assert status == 201

    roster = _roster(secure_client)
    admin_names = [u["username"] for u in roster["admins"]]
    user_names = [u["username"] for u in roster["users"]]
    assert "admin" in admin_names
    assert "bob" in admin_names
    assert user_names == ["alice"]
    assert [g["name"] for g in roster["groups"]] == ["devs"]
    assert roster["volume_mounts"] == []

    # The bootstrap admin has a local web password; API-created accounts do
    # not (their password_hash is NULL).
    admin_entry = next(u for u in roster["admins"] if u["username"] == "admin")
    assert admin_entry["has_password"] is True
    alice = next(u for u in roster["users"] if u["username"] == "alice")
    assert alice["has_password"] is False
    # Roster settings are always complete (defaults fill any missing keys).
    assert set(alice["settings"].keys()) == set(
        user_manager.DEFAULT_USER_SETTINGS.keys()
    )
