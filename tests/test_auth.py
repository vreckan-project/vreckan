"""Web-login authentication: opaque tokens, cookies, Bearer, lifecycle."""
import asyncio

from server import user_manager

ADMIN = ("admin", "admin1234")


def test_login_success(client):
    resp = client.post("/api/auth/login", json={"username": ADMIN[0], "password": ADMIN[1]})
    assert resp.status_code == 200
    body = resp.json()
    assert body["username"] == "admin"
    assert body["is_admin"] is True
    assert body["active"] is True
    assert body["has_password"] is True
    # The credential is issued both as an HttpOnly cookie and a header.
    set_cookie = resp.headers.get("set-cookie", "")
    assert "httponly" in set_cookie.lower()
    assert resp.headers.get("x-auth-token")


def test_login_wrong_password(client):
    resp = client.post("/api/auth/login", json={"username": "admin", "password": "nope"})
    assert resp.status_code == 401


def test_login_unknown_user(client):
    resp = client.post("/api/auth/login", json={"username": "ghost", "password": "whatever"})
    assert resp.status_code == 401


def test_me_with_cookie(client):
    assert client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"}).status_code == 200
    resp = client.get("/api/auth/me")
    assert resp.status_code == 200
    assert resp.json()["username"] == "admin"


def test_me_with_bearer_token(client):
    login = client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    token = login.headers["x-auth-token"]
    client.cookies.clear()  # drop the cookie: authenticate via the header only
    resp = client.get("/api/auth/me", headers={"Authorization": f"Bearer {token}"})
    assert resp.status_code == 200
    assert resp.json()["username"] == "admin"


def test_me_without_credentials(client):
    assert client.get("/api/auth/me").status_code == 401


def test_me_with_garbage_bearer(client):
    resp = client.get("/api/auth/me", headers={"Authorization": "Bearer not-a-issued-token"})
    assert resp.status_code == 401


def test_logout_revokes_the_token(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    assert client.post("/api/auth/logout").status_code == 200
    assert client.get("/api/auth/me").status_code == 401


def test_change_password(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/change_password",
        json={"old_password": "admin1234", "new_password": "freshpass1"},
    )
    assert resp.status_code == 200
    assert user_manager.verify_password("admin", "freshpass1")
    assert not user_manager.verify_password("admin", "admin1234")


def test_change_password_wrong_old_password(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/change_password",
        json={"old_password": "wrong", "new_password": "freshpass1"},
    )
    assert resp.status_code == 401
    assert resp.json()["detail"] == "Current password is incorrect."


def test_change_password_short_new_password(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/change_password",
        json={"old_password": "admin1234", "new_password": "short"},
    )
    assert resp.status_code == 422


def test_change_password_keeps_session_valid(client):
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/change_password",
        json={"old_password": "admin1234", "new_password": "freshpass1"},
    )
    assert resp.status_code == 200
    # The opaque token is not password-bound: the live session survives.
    assert client.get("/api/auth/me").status_code == 200


def test_set_password_requires_admin(client):
    # No credentials at all: the auth guard rejects before the admin check.
    resp = client.post("/api/auth/set_password", json={"password": "somepass123"})
    assert resp.status_code == 401


def test_set_password_for_other_user(client):
    asyncio.run(user_manager.create_user("bob"))
    client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    resp = client.post(
        "/api/auth/set_password", params={"target": "bob"}, json={"password": "bobpass123"}
    )
    assert resp.status_code == 200
    assert user_manager.verify_password("bob", "bobpass123")
    # bob can now log in with the assigned password.
    assert client.post("/api/auth/login", json={"username": "bob", "password": "bobpass123"}).status_code == 200


def test_inactive_user_cannot_login(client):
    user_manager.USER_DATA["admin"]["settings"]["active"] = False
    resp = client.post("/api/auth/login", json={"username": "admin", "password": "admin1234"})
    assert resp.status_code == 403


def test_admin_only_endpoint_rejects_regular_user(secure_client):
    # /api/admin/* (unlike /api/admin/status) sits behind verify_admin.
    asyncio.run(user_manager.create_user("bob"))
    asyncio.run(user_manager.set_password("bob", "bobpass123"))
    # Switch the cookie to the regular user's token.
    resp = secure_client.http.post(
        "/api/auth/login", json={"username": "bob", "password": "bobpass123"}
    )
    assert resp.status_code == 200
    status, _ = secure_client.call("POST", "/api/admin/data", {})
    assert status == 403
