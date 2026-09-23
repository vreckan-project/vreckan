"""User-facing session endpoints (/api/sessions): list my sessions,
poll readiness, stop, and send files — the self-service counterpart of
the admin session views.

Session records are seeded directly into SESSIONS_DB with the same
shape _launch_common stores. provider_app_id points at an app that is
NOT installed, so the stop-session cleanup path has no containers to
talk to (no Docker daemon needed). "Other user" cases use the
non-existent username "ghost" — no second login required.
"""
import time
import uuid

import pytest

import server.api as api_module


def seed_session(username="admin", **overrides):
    """Insert a session record shaped like the one _launch_common stores."""
    record = {
        "instance_id": "ctr-1",
        "ip": None,
        "port": None,
        "created_at": time.time(),
        "access_token": f"tok-{uuid.uuid4()}",
        "controller_token": f"ctrl-{uuid.uuid4()}",
        "provider_app_id": "ghost-app",
        "username": username,
        "app_name": "Test App",
        "app_logo": "x",
        "name": None,
        "host_mount_path": None,
        "shared_files_path": None,
        "launch_context": None,
        "is_collaboration": False,
        "container_registry": {},
    }
    record.update(overrides)
    sid = str(uuid.uuid4())
    api_module.SESSIONS_DB[sid] = record
    return sid


# ---------------------------------------------------------------------------
# List my sessions
# ---------------------------------------------------------------------------

def test_my_sessions_requires_auth(client):
    assert client.get("/api/sessions").status_code == 401


def test_my_sessions_empty(secure_client):
    status, data = secure_client.call("GET", "/api/sessions")
    assert status == 200
    assert data == []


def test_my_sessions_only_own(secure_client):
    mine = seed_session(username="admin")
    seed_session(username="ghost")  # someone else's session
    status, data = secure_client.call("GET", "/api/sessions")
    assert status == 200
    assert [s["session_id"] for s in data] == [mine]
    assert data[0]["app_name"] == "Test App"
    # Non-collaboration sessions expose the access-token URL.
    tok = api_module.SESSIONS_DB[mine]["access_token"]
    assert data[0]["session_url"] == f"/api/apps/session/{mine}/?access_token={tok}"


# ---------------------------------------------------------------------------
# Session status
# ---------------------------------------------------------------------------

def test_session_status_unknown(secure_client):
    status, data = secure_client.call("GET", "/api/sessions/nope/status")
    assert status == 404
    assert data["detail"] == "Session not found or permission denied."


def test_session_status_other_user_denied(secure_client):
    other = seed_session(username="ghost")
    status, data = secure_client.call("GET", f"/api/sessions/{other}/status")
    assert status == 404
    assert data["detail"] == "Session not found or permission denied."


def test_session_status_no_upstream(secure_client):
    sid = seed_session()  # ip=None, port=None
    status, data = secure_client.call("GET", f"/api/sessions/{sid}/status")
    assert status == 200
    assert data == {"ready": False, "detail": "No upstream address recorded."}


def test_session_status_unreachable(secure_client):
    # Port 9 (discard) is not listening inside the container.
    sid = seed_session(ip="127.0.0.1", port=9)
    status, data = secure_client.call("GET", f"/api/sessions/{sid}/status")
    assert status == 200
    assert data["ready"] is False
    assert data["detail"].startswith("unreachable (")


# ---------------------------------------------------------------------------
# Stop a session
# ---------------------------------------------------------------------------

def test_stop_session_unknown(secure_client):
    status, data = secure_client.call("DELETE", "/api/sessions/nope")
    assert status == 404
    assert data["detail"] == "Session not found or permission denied."


def test_stop_session_other_user_denied(secure_client):
    other = seed_session(username="ghost")
    status, data = secure_client.call("DELETE", f"/api/sessions/{other}")
    assert status == 404
    assert data["detail"] == "Session not found or permission denied."
    assert other in api_module.SESSIONS_DB  # untouched


def test_stop_my_session(secure_client):
    sid = seed_session()
    status, _ = secure_client.call("DELETE", f"/api/sessions/{sid}")
    assert status == 204
    assert sid not in api_module.SESSIONS_DB


# ---------------------------------------------------------------------------
# Send a file to a session
# ---------------------------------------------------------------------------

def test_send_file_malformed_body(secure_client):
    sid = seed_session()
    status, data = secure_client.call(
        "POST", f"/api/sessions/{sid}/send_file", body={"filename": "a.txt"},
    )
    assert status == 422
    assert "Invalid request body" in data["detail"]


def test_send_file_unknown_session(secure_client):
    status, data = secure_client.call(
        "POST", "/api/sessions/nope/send_file",
        body={"filename": "a.txt", "upload_id": "u-1", "total_chunks": 1},
    )
    assert status == 404
    assert data["detail"] == "Session not found or permission denied."


def test_send_file_no_mount(secure_client):
    sid = seed_session()  # host_mount_path=None
    status, data = secure_client.call(
        "POST", f"/api/sessions/{sid}/send_file",
        body={"filename": "a.txt", "upload_id": "u-1", "total_chunks": 1},
    )
    assert status == 400
    assert data["detail"] == (
        "Cannot send files to this session as it has no mounted storage."
    )
