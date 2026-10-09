"""Tests for the OIDC / SSO authorization-code flow and session revocation.

A small in-process mock identity provider (``MockIdp``) stands in for a real
OIDC provider (such as authentik). It serves:

* ``GET /.well-known/openid-configuration`` — the discovery document,
* ``GET /jwks`` — a JWKS exposing the RSA public key (``n``/``e``, base64url),
* ``POST /token`` — the authorization-code token endpoint (form-encoded),

and signs ``id_token``s with a real RSA key (RS256 + ``kid``) so the server's
full JWKS signature-verification path is exercised end to end. The mock binds
to a unique localhost port per test, so the discovery cache never collides
across tests.

The ``oidc`` fixture points the app's OIDC settings at the mock and clears the
discovery cache; the shared ``client`` / ``secure_client`` fixtures then drive
the real endpoints. The mock runs in this (test) process, so the app's
``httpx2`` calls reach it over loopback with no external network.
"""
from __future__ import annotations

import base64
import json
import secrets
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

import pytest
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa
import jwt

import server.api as api_module
from server import user_manager
from server.settings import settings

CLIENT_ID = "vreckan-test-client"
CLIENT_SECRET = "test-client-secret"
COOKIE_NAME = "vreckan_auth"


def _b64u(value: int) -> str:
    """Base64url-encode an integer without padding — the JWK ``n``/``e`` form."""
    return (
        base64.urlsafe_b64encode(value.to_bytes((value.bit_length() + 7) // 8, "big"))
        .rstrip(b"=")
        .decode()
    )


class MockIdp:
    """A minimal OIDC authorization-code identity provider for tests."""

    def __init__(self) -> None:
        # The key whose public half is published in the JWKS.
        self.jwk_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        self.kid = "test-key-1"
        self.client_id = CLIENT_ID
        self.client_secret = CLIENT_SECRET
        # The key actually used to sign id_tokens. Defaults to the JWKS key;
        # the bad-signature test swaps this for a different key so the server's
        # JWKS verification rejects the token.
        self.sign_pem = self.jwk_key.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )
        self._serve_jwks_uri = True
        self._codes: dict[str, dict] = {}
        self._lock = threading.Lock()
        self.issuer: str = ""
        self.port: int = 0
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    # -- lifecycle -----------------------------------------------------------
    def start(self) -> None:
        idp = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):  # silence per-request logging
                pass

            def _send(self, status: int, obj: dict) -> None:
                body = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                path = urlparse(self.path).path
                if path == "/.well-known/openid-configuration":
                    base = idp.issuer
                    doc = {
                        "issuer": base,
                        "authorization_endpoint": f"{base}/authorize",
                        "token_endpoint": f"{base}/token",
                    }
                    if idp._serve_jwks_uri:
                        doc["jwks_uri"] = f"{base}/jwks"
                    self._send(200, doc)
                elif path == "/jwks":
                    pub = idp.jwk_key.public_key().public_numbers()
                    self._send(
                        200,
                        {
                            "keys": [
                                {
                                    "kty": "RSA",
                                    "kid": idp.kid,
                                    "use": "sig",
                                    "alg": "RS256",
                                    "n": _b64u(pub.n),
                                    "e": _b64u(pub.e),
                                }
                            ]
                        },
                    )
                else:
                    self._send(404, {"error": "not_found"})

            def do_POST(self):
                path = urlparse(self.path).path
                if path != "/token":
                    self._send(404, {"error": "not_found"})
                    return
                length = int(self.headers.get("Content-Length") or 0)
                form = parse_qs(self.rfile.read(length).decode())
                grant = (form.get("grant_type") or [""])[0]
                code = (form.get("code") or [""])[0]
                with idp._lock:
                    claims = idp._codes.pop(code, None)
                if grant != "authorization_code" or claims is None:
                    self._send(400, {"error": "invalid_grant"})
                    return
                now = int(time.time())
                full = dict(claims)
                full.update(
                    {"iss": idp.issuer, "aud": idp.client_id, "iat": now, "exp": now + 3600}
                )
                id_token = jwt.encode(full, idp.sign_pem, algorithm="RS256", headers={"kid": idp.kid})
                self._send(
                    200,
                    {
                        "access_token": "opaque-access-token",
                        "id_token": id_token,
                        "token_type": "Bearer",
                        "expires_in": 3600,
                    },
                )

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self.issuer = f"http://127.0.0.1:{self.port}"
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        if self._server:
            self._server.shutdown()
            # ``server_close()`` (not ``close()``) is the teardown hook in this
            # Python build — ``TCPServer.close`` was removed upstream.
            self._server.server_close()
            self._server = None
        if self._thread:
            self._thread.join(timeout=5)
            self._thread = None

    # -- test helpers --------------------------------------------------------
    def issue_code(self, claims: dict) -> str:
        """Record a claims set behind a fresh authorization code."""
        code = secrets.token_urlsafe(16)
        with self._lock:
            self._codes[code] = claims
        return code

    def use_bad_signature(self) -> None:
        """Make the token endpoint sign with a key that does NOT match the JWKS."""
        rogue = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        self.sign_pem = rogue.private_bytes(
            serialization.Encoding.PEM,
            serialization.PrivateFormat.PKCS8,
            serialization.NoEncryption(),
        )

    def disable_jwks(self) -> None:
        """Omit ``jwks_uri`` from the discovery doc (exercises the claims-only
        verification fallback in the server)."""
        self._serve_jwks_uri = False


@pytest.fixture
def oidc(monkeypatch):
    """Point the app's OIDC settings at a live mock IdP for this test."""
    idp = MockIdp()
    idp.start()
    monkeypatch.setattr(settings, "oidc_enabled", True)
    monkeypatch.setattr(settings, "oidc_issuer", idp.issuer)
    monkeypatch.setattr(settings, "oidc_client_id", idp.client_id)
    monkeypatch.setattr(settings, "oidc_client_secret", idp.client_secret)
    monkeypatch.setattr(settings, "oidc_scopes", "openid email profile")
    monkeypatch.setattr(settings, "oidc_allow_signup", True)
    monkeypatch.setattr(settings, "oidc_state_ttl_seconds", 600)
    monkeypatch.setattr(settings, "oidc_admin_groups", "")
    monkeypatch.setattr(settings, "oidc_user_groups", "")
    monkeypatch.setattr(settings, "oidc_redirect_uri", "")
    # The autouse ``isolate`` fixture does not clear the discovery cache; give
    # each test a fresh one so the mock's discovery is fetched, not reused.
    monkeypatch.setattr(api_module, "OIDC_DISCOVERY_CACHE", {})
    yield idp
    idp.stop()


def _state_from_authorize(client) -> str:
    """Start the flow and return the generated ``state`` from the redirect."""
    auth = client.get("/api/auth/oidc/authorize")
    assert auth.status_code == 302, auth.text
    return parse_qs(urlparse(auth.headers["location"]).query)["state"][0]


# ---------------------------------------------------------------------------
# /oidc/status
# ---------------------------------------------------------------------------
def test_oidc_status_disabled_by_default(client):
    resp = client.get("/api/auth/oidc/status")
    assert resp.status_code == 200
    body = resp.json()
    assert body["enabled"] is False
    assert body["configured"] is False


def test_oidc_status_enabled(oidc, client):
    resp = client.get("/api/auth/oidc/status")
    assert resp.status_code == 200
    body = resp.json()
    assert body["enabled"] is True
    assert body["configured"] is True


# ---------------------------------------------------------------------------
# /oidc/authorize
# ---------------------------------------------------------------------------
def test_authorize_400_when_disabled(client):
    resp = client.get("/api/auth/oidc/authorize")
    assert resp.status_code == 400
    assert "not enabled" in resp.json()["detail"]


def test_authorize_redirects_to_provider(oidc, client):
    resp = client.get("/api/auth/oidc/authorize")
    assert resp.status_code == 302
    parsed = urlparse(resp.headers["location"])
    # Points at the mock provider's authorization endpoint.
    assert parsed.hostname == "127.0.0.1"
    assert parsed.port == oidc.port
    assert parsed.path == "/authorize"
    params = parse_qs(parsed.query)
    assert params["response_type"] == ["code"]
    assert params["client_id"] == [oidc.client_id]
    # redirect_uri is derived from the test client's public base URL.
    assert params["redirect_uri"] == ["http://testserver/api/auth/oidc/callback"]
    assert params["scope"] == ["openid email profile"]
    # A one-time state was generated and stored server-side.
    assert len(params["state"]) == 1
    state = params["state"][0]
    assert state in api_module.OIDC_STATES
    assert api_module.OIDC_STATES[state]["expires_at"] > time.time()


# ---------------------------------------------------------------------------
# /oidc/callback
# ---------------------------------------------------------------------------
def test_callback_provisions_user(oidc, client):
    state = _state_from_authorize(client)
    code = oidc.issue_code(
        {
            "sub": "auth0|abc123",
            "preferred_username": "sstest",
            "email": "sstest@example.com",
            "name": "SS Test",
            "groups": ["vdi-users"],
        }
    )
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 302, resp.text
    assert resp.headers["location"] == "/"
    # The web token cookie was set and is now in the client's jar.
    assert client.cookies.get(COOKIE_NAME) is not None
    # The identity was provisioned and flagged as SSO (not admin).
    user = user_manager.get_user("sstest")
    assert user is not None
    assert user["is_sso"] is True
    assert user["is_admin"] is False
    # The issued cookie authenticates against /me.
    me = client.get("/api/auth/me")
    assert me.status_code == 200
    assert me.json()["username"] == "sstest"
    assert me.json()["is_sso"] is True


def test_callback_invalid_state(oidc, client):
    code = oidc.issue_code({"preferred_username": "sstest", "sub": "x"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state=not-a-real-state")
    assert resp.status_code == 400
    assert "CSRF" in resp.json()["detail"]


def test_callback_expired_state(oidc, client):
    state = _state_from_authorize(client)
    # Force the stored state to be past its expiry.
    api_module.OIDC_STATES[state]["expires_at"] = time.time() - 10
    code = oidc.issue_code({"preferred_username": "sstest", "sub": "x"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 400
    assert "CSRF" in resp.json()["detail"]


def test_callback_unknown_user_signup_disabled(oidc, client, monkeypatch):
    monkeypatch.setattr(settings, "oidc_allow_signup", False)
    state = _state_from_authorize(client)
    code = oidc.issue_code({"preferred_username": "brand-new-user", "sub": "xyz"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 403
    assert "auto-signup is disabled" in resp.json()["detail"]
    # No account was created.
    assert user_manager.get_user("brand-new-user") is None


def test_callback_admin_group_promotion(oidc, client, monkeypatch):
    # The admin-groups list is sourced from the environment
    # (VRECKAN_OIDC_ADMIN_GROUPS), so point the setting at the test's group.
    monkeypatch.setattr(settings, "oidc_admin_groups", "vdi-admins")
    state = _state_from_authorize(client)
    code = oidc.issue_code(
        {"preferred_username": "ssadmin-test", "sub": "admin-sub", "groups": ["vdi-admins"]}
    )
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 302, resp.text
    user = user_manager.get_user("ssadmin-test")
    assert user is not None
    assert user["is_admin"] is True
    assert user["is_sso"] is True
    # The full SSO group list is now persisted on the account.
    assert user["sso_groups"] == ["vdi-admins"]


def test_callback_user_group_assignment(oidc, client, monkeypatch):
    # A non-admin SSO user is assigned to their first group that is in the
    # configured user-groups list (VRECKAN_OIDC_USER_GROUPS).
    monkeypatch.setattr(settings, "oidc_user_groups", "vdi-users")
    state = _state_from_authorize(client)
    code = oidc.issue_code(
        {
            "preferred_username": "sstest",
            "sub": "user-sub",
            "groups": ["other-group", "vdi-users"],
        }
    )
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 302, resp.text
    user = user_manager.get_user("sstest")
    assert user is not None
    assert user["is_admin"] is False
    assert user["is_sso"] is True
    # The configured user group (vdi-users) is preferred over the other group.
    assert user["settings"]["group"] == "vdi-users"
    # The full SSO group list is persisted on the account.
    assert user["sso_groups"] == ["other-group", "vdi-users"]


def test_callback_inactive_user_rejected(oidc, client):
    # Seed an already-provisioned user directly in the in-memory store with a
    # deactivated account. The callback resolves users from these in-memory
    # structures (no DB round-trip), so no event loop is needed here.
    user_manager.USER_DATA["deactivated"] = {
        "username": "deactivated",
        "is_admin": False,
        "is_sso": False,
        "settings": dict(user_manager.DEFAULT_USER_SETTINGS, active=False),
        "password_hash": None,
    }
    state = _state_from_authorize(client)
    code = oidc.issue_code({"preferred_username": "deactivated", "sub": "d1"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 403
    assert "inactive" in resp.json()["detail"]


def test_callback_bad_signature_rejected(oidc, client):
    state = _state_from_authorize(client)
    # The provider signs the id_token with a key that does not match its JWKS.
    oidc.use_bad_signature()
    code = oidc.issue_code({"preferred_username": "sstest", "sub": "x"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 502
    assert "verification failed" in resp.json()["detail"]


def test_callback_no_jwks_fails_closed(oidc, client):
    # Default: a signature is required, so an unreachable JWKS fails the
    # login (502) rather than accepting an unsigned token.
    oidc.disable_jwks()
    state = _state_from_authorize(client)
    code = oidc.issue_code({"preferred_username": "sstest", "sub": "x"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 502, resp.text
    assert "JWKS" in resp.json()["detail"]


def test_callback_no_jwks_claims_only_when_disabled(oidc, client, monkeypatch):
    # With oidc_require_signature off, the legacy claims-only fallback is
    # used when the JWKS is unavailable.
    monkeypatch.setattr(settings, "oidc_require_signature", False)
    oidc.disable_jwks()
    state = _state_from_authorize(client)
    code = oidc.issue_code({"preferred_username": "sstest", "sub": "x"})
    resp = client.get(f"/api/auth/oidc/callback?code={code}&state={state}")
    assert resp.status_code == 302, resp.text
    assert user_manager.get_user("sstest") is not None


# ---------------------------------------------------------------------------
# Session revocation (self-service + admin force-logout)
# ---------------------------------------------------------------------------
def test_revoke_own_sessions(secure_client):
    # The bootstrap admin is logged in (cookie set) and E2EE-ready.
    assert secure_client.http.cookies.get(COOKIE_NAME) is not None
    resp = secure_client.http.post("/api/auth/revoke-sessions")
    assert resp.status_code == 200
    body = resp.json()
    assert body["username"] == "admin"
    assert body["revoked"] >= 1
    # The cookie is cleared, so the session is gone.
    assert secure_client.http.cookies.get(COOKIE_NAME) is None
    # Subsequent authenticated calls now fail.
    assert secure_client.http.get("/api/auth/me").status_code == 401


def test_revoke_own_sessions_rejects_other_user(secure_client):
    resp = secure_client.http.post(
        "/api/auth/revoke-sessions", json={"username": "someone-else"}
    )
    assert resp.status_code == 403
    assert "only invalidate your own" in resp.json()["detail"]
    # The admin's own session is untouched.
    assert secure_client.http.get("/api/auth/me").status_code == 200


def test_admin_force_revoke(secure_client):
    # Give a second user an active web token (as if they had signed in) and a
    # matching account, then have the admin force-log them out.
    victim_token = "victim-token-abc123"
    api_module.AUTH_TOKENS[victim_token] = {
        "username": "sstest",
        "expires_at": time.time() + 3600,
    }
    user_manager.USER_DATA["sstest"] = {
        "username": "sstest",
        "is_admin": False,
        "is_sso": True,
        "settings": dict(user_manager.DEFAULT_USER_SETTINGS),
        "password_hash": None,
    }
    status, body = secure_client.call(
        "POST", "/api/admin/oidc/revoke", body={"username": "sstest"}
    )
    assert status == 200, body
    assert body["username"] == "sstest"
    assert body["revoked"] == 1
    assert victim_token not in api_module.AUTH_TOKENS


def test_admin_force_revoke_unknown_user(secure_client):
    status, body = secure_client.call(
        "POST", "/api/admin/oidc/revoke", body={"username": "ghost-user"}
    )
    assert status == 404
    assert "not found" in (body or {}).get("detail", "")
