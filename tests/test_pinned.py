"""Pinned behaviours (/api/pinned): per-user saved launch-option
presets. Rows live in the pinned_behaviors table (no in-memory cache),
so "other user" rows can be seeded with a direct sqlite insert.
"""
import json
import sqlite3

import pytest

import server.api as api_module

PINNED_BODY = {"name": "Work Firefox", "application_id": "app-1"}


def seed_pinned_row(db_path, username, pin_id, created_at, **data_overrides):
    """Insert a pinned_behaviors row directly (e.g. for another user)."""
    data = {"name": "Ghost Pin", "application_id": "app-1"}
    data.update(data_overrides)
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "INSERT INTO pinned_behaviors (id, username, created_at, data) "
            "VALUES (?, ?, ?, ?)",
            (pin_id, username, created_at, json.dumps(data)),
        )
        conn.commit()
    finally:
        conn.close()


def test_pinned_requires_auth(client):
    assert client.get("/api/pinned").status_code == 401


def test_pinned_empty(secure_client):
    status, data = secure_client.call("GET", "/api/pinned")
    assert status == 200
    assert data == []


def test_create_pinned(secure_client):
    status, data = secure_client.call("POST", "/api/pinned", body=PINNED_BODY)
    assert status == 201
    assert data["name"] == "Work Firefox"
    assert data["application_id"] == "app-1"
    assert data["id"]
    # Model defaults are applied.
    assert data["wayland_mode"] is True
    assert data["trigger_type"] == "manual"
    assert data["is_default"] is False
    status, listing = secure_client.call("GET", "/api/pinned")
    assert status == 200
    assert [p["id"] for p in listing] == [data["id"]]


def test_create_pinned_malformed(secure_client):
    status, data = secure_client.call("POST", "/api/pinned", body={"name": ""})
    assert status == 422
    assert "Invalid request body" in data["detail"]


def test_create_pinned_default_clears_previous(secure_client):
    secure_client.call(
        "POST", "/api/pinned", body={**PINNED_BODY, "name": "A", "is_default": True},
    )
    secure_client.call(
        "POST", "/api/pinned", body={**PINNED_BODY, "name": "B", "is_default": True},
    )
    status, listing = secure_client.call("GET", "/api/pinned")
    assert status == 200
    by_name = {p["name"]: p for p in listing}
    assert by_name["A"]["is_default"] is False
    assert by_name["B"]["is_default"] is True


def test_set_default_unknown(secure_client):
    status, data = secure_client.call("POST", "/api/pinned/nope/set-default")
    assert status == 404
    assert data["detail"] == "Pinned behaviour not found."


def test_set_default(secure_client):
    a = secure_client.call(
        "POST", "/api/pinned", body={**PINNED_BODY, "name": "A", "is_default": True},
    )[1]
    b = secure_client.call(
        "POST", "/api/pinned", body={**PINNED_BODY, "name": "B"},
    )[1]
    status, data = secure_client.call("POST", f"/api/pinned/{b['id']}/set-default")
    assert status == 200
    assert data == {"ok": True}
    status, listing = secure_client.call("GET", "/api/pinned")
    by_id = {p["id"]: p for p in listing}
    assert by_id[a["id"]]["is_default"] is False
    assert by_id[b["id"]]["is_default"] is True


def test_delete_pinned_unknown(secure_client):
    status, data = secure_client.call("DELETE", "/api/pinned/nope")
    assert status == 404
    assert data["detail"] == "Pinned behaviour not found."


def test_delete_pinned_other_user(secure_client, db_path):
    seed_pinned_row(db_path, "ghost", "pin-ghost", 1234.0)
    status, data = secure_client.call("DELETE", "/api/pinned/pin-ghost")
    assert status == 404
    assert data["detail"] == "Pinned behaviour not found."
    # The other user's row is untouched.
    conn = sqlite3.connect(db_path)
    try:
        count = conn.execute(
            "SELECT COUNT(*) FROM pinned_behaviors WHERE id = 'pin-ghost'"
        ).fetchone()[0]
    finally:
        conn.close()
    assert count == 1


def test_delete_pinned(secure_client):
    created = secure_client.call("POST", "/api/pinned", body=PINNED_BODY)[1]
    status, _ = secure_client.call("DELETE", f"/api/pinned/{created['id']}")
    assert status == 204
    status, listing = secure_client.call("GET", "/api/pinned")
    assert listing == []


def test_pinned_list_newest_first(secure_client, db_path):
    seed_pinned_row(db_path, "admin", "pin-old", 1000.0, name="Old")
    seed_pinned_row(db_path, "admin", "pin-new", 2000.0, name="New")
    status, listing = secure_client.call("GET", "/api/pinned")
    assert status == 200
    assert [p["id"] for p in listing] == ["pin-new", "pin-old"]
    assert listing[0]["created_at"] == 2000.0
