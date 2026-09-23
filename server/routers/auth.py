"""Auth endpoints: password login/logout, self-service session revocation,
and the OIDC/SSO authorization-code flow.

Extracted from ``app.api`` during the router split. These live OUTSIDE the
encrypted router on purpose: the E2EE handshake proves server identity, not
user identity, so it cannot gate the login flow that establishes the
credential in the first place. The token issued here is the credential
accepted by ``verify_token``.

Shared module state and helpers are accessed through the ``api`` module
(``api.issue_auth_token``, ``api.OIDC_STATES``, ...) rather than imported by
name, so test fixtures that mutate ``app.api`` in place keep working
unchanged. Only the auth dependencies (``api.verify_token`` /
``api.verify_admin``) are resolved at import time (inside ``Depends(...)``);
everything else is looked up on ``api`` at call time. The OIDC helpers
(``_oidc_*``) and ``_me_response`` are auth-local and stay in this module.
"""

import json
import secrets
import time
from typing import Dict, Optional

import httpx2
from fastapi import APIRouter, Depends, HTTPException, Query, Request, Response
from fastapi.responses import RedirectResponse
from jose import JWTError, jwt
from pydantic import ValidationError

from server import api
from server.models import (
    ChangePasswordRequest,
    LoginRequest,
    MeResponse,
    RevokeOidcSessionResponse,
    RevokeOwnSessionsRequest,
    SetPasswordRequest,
    UserSettings,
)
from server.settings import settings
from server import user_manager


auth_router = APIRouter(prefix="/api/auth", tags=["auth"])


def _me_response(user: dict) -> MeResponse:
    """Build a MeResponse from a user dict (with effective_settings applied)."""
    username = user["username"]
    effective = user.get("effective_settings") or user_manager.get_effective_settings(
        username
    )
    # No admin bypass in the reported `active` flag: it reflects the
    # effective setting so the UI (and any client) sees the true state,
    # including a disabled admin.
    active = effective.get("active", True)
    try:
        has_pw = user_manager.has_password(username)
    except Exception:
        has_pw = False
    settings_obj = None
    if effective is not None:
        try:
            settings_obj = UserSettings(**effective)
        except Exception:
            settings_obj = None
    return MeResponse(
        username=username,
        is_admin=bool(user.get("is_admin", False)),
        active=active,
        has_password=has_pw,
        is_sso=bool(user.get("is_sso", False)),
        settings=settings_obj,
        permissions=user.get("permissions") or user_manager.get_effective_permissions(username),
        roles=user.get("roles") or [],
        groups=user.get("groups") or [],
    )


@auth_router.post("/login", response_model=MeResponse)
async def auth_login(req: LoginRequest, request: Request):
    """Authenticate with a username/password and issue a server token.

    Sets an HttpOnly cookie (and returns the token in the body) so that both
    browser cookie flows and explicit Bearer clients work.
    """
    # argon2 availability check — degrade gracefully if not installed.
    if not user_manager._HAS_ARGON2:
        raise HTTPException(
            status_code=503,
            detail="Password authentication is not available on this server.",
        )

    if not user_manager.verify_password(req.username, req.password):
        # Do not leak whether the account exists.
        raise HTTPException(status_code=401, detail="Invalid username or password.")

    user = user_manager.get_user(req.username)
    if user is None:
        raise HTTPException(status_code=401, detail="Invalid username or password.")

    effective = user_manager.get_effective_settings(req.username)
    # No admin bypass: a disabled admin cannot log in, same as any other
    # inactive account.
    is_active = effective.get("active", True)
    if not is_active:
        raise HTTPException(status_code=403, detail="This account is not active.")

    token = await api.issue_auth_token(req.username)
    await api.save_auth_tokens_to_disk()

    resp = Response(
        content=json.dumps(_me_response(user).model_dump()),
        media_type="application/json",
    )
    resp.set_cookie(**api.auth_cookie_kwargs(token, request))
    resp.headers["X-Auth-Token"] = token
    return resp


@auth_router.post("/logout")
async def auth_logout(request: Request):
    """Revoke the caller's server token (cookie or Bearer) and clear the cookie."""
    token = request.cookies.get(settings.auth_cookie_name)
    if not token:
        auth_header = request.headers.get("Authorization", "")
        if auth_header.startswith("Bearer "):
            token = auth_header[7:].strip()

    if token:
        await api.revoke_auth_token(token)
        await api.save_auth_tokens_to_disk()

    resp = Response(
        content=json.dumps({"message": "Logged out"}), media_type="application/json"
    )
    # Clear the cookie (max_age=0, empty value).
    resp.set_cookie(**api.auth_cookie_kwargs(None, request))
    return resp


@auth_router.post("/revoke-sessions", response_model=RevokeOidcSessionResponse)
async def revoke_own_sessions(
    request: Request,
    user: dict = Depends(api.verify_token),
):
    """Revoke ALL of the CALLER's own web sessions (self-service).

    Unlike the admin force-invalidate endpoint (``/api/admin/oidc/revoke``,
    which may target *any* user), this endpoint always operates on the
    authenticated caller: the target is derived from the caller's own
    credential, never from untrusted input. If a different username is
    supplied in the body the request is rejected (403) — a user can only
    ever invalidate their own sessions.

    Revoking your own sessions logs *you* out: every active web token
    belonging to you is revoked and your ``vreckan_auth`` cookie is
    cleared. You can simply sign in again.
    """
    caller = user["username"]

    # Optional body. If a username is supplied it must match the caller.
    body = await request.body()
    if body:
        try:
            req = RevokeOwnSessionsRequest(**json.loads(body))
        except (json.JSONDecodeError, ValidationError) as e:
            raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
        if req.username and req.username != caller:
            api.logger.warning(
                f"User '{caller}' attempted to revoke sessions for "
                f"'{req.username}' — rejected (self-revoke only)."
            )
            raise HTTPException(
                status_code=403,
                detail="You can only invalidate your own sessions.",
            )

    revoked = await api.revoke_all_tokens_for_user(caller)
    await api.save_auth_tokens_to_disk()
    api.logger.info(f"Self-revoke: user '{caller}' revoked {revoked} of their own web token(s).")

    resp = Response(
        content=json.dumps(
            RevokeOidcSessionResponse(username=caller, revoked=revoked).model_dump()
        ),
        media_type="application/json",
    )
    # Clear the caller's cookie so this browser is logged out immediately.
    resp.set_cookie(**api.auth_cookie_kwargs(None, request))
    return resp


# ---------------------------------------------------------------------------
# OIDC / SSO (authorization-code flow, no client-side secrets)
#
# These endpoints are on the *plaintext* auth_router (like /login and /logout)
# because they are reached by the browser in clear text (no E2EE session), and
# they never expose a client secret. They set the SAME `vreckan_auth` web
# token cookie that password login sets, so the rest of the app is unaffected.
# ---------------------------------------------------------------------------


async def _oidc_config() -> Dict:
    """The effective OIDC/SSO settings, read DB-first (a value saved on the
    admin SSO page overrides the environment, which overrides the default).
    Returns a dict with the raw values; callers derive what they need."""
    return {
        "enabled": await settings.get_setting("oidc_enabled"),
        "issuer": await settings.get_setting("oidc_issuer"),
        "client_id": await settings.get_setting("oidc_client_id"),
        "client_secret": await settings.get_setting("oidc_client_secret"),
        "redirect_uri": await settings.get_setting("oidc_redirect_uri"),
        "scopes": await settings.get_setting("oidc_scopes"),
        "allow_signup": await settings.get_setting("oidc_allow_signup"),
        "state_ttl": await settings.get_setting("oidc_state_ttl_seconds"),
        "admin_groups": await settings.get_setting("oidc_admin_groups"),
        "user_groups": await settings.get_setting("oidc_user_groups"),
    }


async def _oidc_effective_issuer() -> str:
    """Return the configured OIDC issuer base URL, trimmed and without a
    trailing slash, or an empty string when not configured."""
    issuer = (await settings.get_setting("oidc_issuer") or "").strip()
    return issuer.rstrip("/")


def _oidc_base_url(request: Request) -> str:
    """Derive the public base URL for this request (scheme + host), honoring
    reverse-proxy headers (X-Forwarded-Proto / Host) when present."""
    scheme = request.headers.get("x-forwarded-proto") or request.url.scheme
    host = request.headers.get("host") or request.url.netloc
    return f"{scheme}://{host}"


async def _oidc_redirect_uri(request: Request) -> str:
    """The redirect_uri registered with the provider. Prefer the explicit
    setting; otherwise derive it from the request's public base URL."""
    configured = (await settings.get_setting("oidc_redirect_uri") or "").strip()
    if configured:
        return configured
    return _oidc_base_url(request) + "/api/auth/oidc/callback"


async def _oidc_discovery(issuer: str) -> Dict:
    """Fetch (and cache) the OIDC discovery document for the given issuer.
    Raises HTTPException(400/502) when discovery is unavailable."""
    if issuer in api.OIDC_DISCOVERY_CACHE:
        return api.OIDC_DISCOVERY_CACHE[issuer]
    url = f"{issuer}/.well-known/openid-configuration"
    try:
        async with httpx2.AsyncClient(timeout=10.0) as client:
            r = await client.get(url)
    except httpx2.HTTPError as e:
        api.logger.warning("OIDC discovery fetch failed for %s: %s", url, e)
        raise HTTPException(status_code=502, detail="Unable to reach the OIDC provider (discovery).")
    if r.status_code != 200:
        api.logger.warning("OIDC discovery returned %s for %s", r.status_code, url)
        raise HTTPException(status_code=502, detail="OIDC provider discovery failed.")
    try:
        doc = r.json()
    except ValueError:
        raise HTTPException(status_code=502, detail="OIDC provider discovery returned invalid JSON.")
    for field in ("issuer", "authorization_endpoint", "token_endpoint"):
        if field not in doc:
            raise HTTPException(status_code=502, detail=f"OIDC discovery document missing '{field}'.")
    api.OIDC_DISCOVERY_CACHE[issuer] = doc
    return doc


def _rsa_jwk_to_pem(jwk: Dict) -> Optional[str]:
    """Convert an RSA JWK (base64url-encoded ``n``/``e``) to a PEM-encoded
    RSA public key, without relying on ``jose.jwk`` (whose ``as_pem()`` is
    version-fragile). Returns None on any failure."""
    try:
        import base64

        def _b64u_int(value: str) -> int:
            pad = "=" * (-len(value) % 4)
            return int.from_bytes(base64.urlsafe_b64decode(value + pad), "big")

        from cryptography.hazmat.primitives.asymmetric import rsa
        from cryptography.hazmat.primitives import serialization

        n = _b64u_int(jwk["n"])
        e = _b64u_int(jwk["e"])
        pub = rsa.RSAPublicNumbers(e, n).public_key()
        return pub.public_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PublicFormat.SubjectPublicKeyInfo,
        ).decode("utf-8")
    except Exception as e:  # noqa: BLE001
        api.logger.warning("OIDC JWKS: failed to build RSA public key: %s", e)
        return None


async def _oidc_verify_id_token(id_token: str, issuer: str, client_id: str) -> Dict:
    """Verify the id_token signature (via the provider's JWKS) and standard
    claims (iss/aud/exp), then return its claims. Raises HTTPException(502)
    on any verification failure."""
    try:
        header = jwt.get_unverified_header(id_token)
    except JWTError as e:
        raise HTTPException(status_code=502, detail="Invalid id_token header.")

    key_pem = None
    # Discovery is normally already cached from the authorize step; if not,
    # fetch it now so we can resolve the provider's JWKS.
    jwks_uri = None
    discovery = api.OIDC_DISCOVERY_CACHE.get(await _oidc_effective_issuer())
    if not discovery:
        try:
            discovery = await _oidc_discovery(await _oidc_effective_issuer())
        except HTTPException:
            discovery = None
    if discovery and discovery.get("jwks_uri"):
        jwks_uri = discovery["jwks_uri"]
    if jwks_uri:
        try:
            import jose
            async with httpx2.AsyncClient(timeout=10.0) as client:
                jwks_resp = await client.get(jwks_uri)
            if jwks_resp.status_code == 200:
                jwks = jwks_resp.json()
                kid = header.get("kid")
                jwk_dict = None
                for k in jwks.get("keys", []):
                    if kid is None or k.get("kid") == kid:
                        jwk_dict = k
                        break
                if jwk_dict:
                    if jwk_dict.get("kty") == "RSA" and jwk_dict.get("n") and jwk_dict.get("e"):
                        key_pem = _rsa_jwk_to_pem(jwk_dict)
                    else:
                        try:
                            key_obj = jose.jwk.construct(jwk_dict)
                            key_pem = key_obj.as_pem().decode("utf-8")
                        except Exception:  # noqa: BLE001
                            key_pem = None
        except (httpx2.HTTPError, JWTError, Exception) as e:  # noqa: BLE001
            api.logger.warning("OIDC JWKS fetch/parse failed: %s", e)

    alg = header.get("alg", "RS256")
    if key_pem is not None:
        try:
            claims = jwt.decode(
                id_token,
                key_pem,
                algorithms=[alg],
                audience=client_id,
                options={"verify_iss": False},
            )
        except JWTError as e:
            raise HTTPException(status_code=502, detail=f"id_token verification failed: {e}")
        # Tolerate a trailing slash: some providers (e.g. authentik) sign `iss`
        # with a trailing "/" while we store the issuer without one.
        if str(claims.get("iss", "")).rstrip("/") != issuer.rstrip("/"):
            raise HTTPException(status_code=502, detail="id_token issuer mismatch.")
        return claims
    # Fallback (no JWKS available): verify claims only and warn loudly.
    api.logger.warning("OIDC id_token: no JWKS available; verifying claims only.")
    try:
        claims = jwt.decode(
            id_token,
            None,
            algorithms=[alg],
            audience=client_id,
            options={"verify_signature": False, "verify_iss": False},
        )
    except JWTError as e:
        raise HTTPException(status_code=502, detail=f"id_token verification failed: {e}")
    if str(claims.get("iss", "")).rstrip("/") != issuer.rstrip("/"):
        raise HTTPException(status_code=502, detail="id_token issuer mismatch.")
    return claims


def _oidc_username_from_claims(claims: Dict) -> Optional[str]:
    """Derive a sanitized Vreckan username from OIDC claims, preferring
    preferred_username, then the email local-part, then `sub`."""
    import re as _re
    candidates = []
    if claims.get("preferred_username"):
        candidates.append(claims["preferred_username"])
    email = claims.get("email")
    if email and "@" in email:
        candidates.append(email.split("@", 1)[0])
    if claims.get("sub"):
        candidates.append(claims["sub"])
    for cand in candidates:
        cleaned = _re.sub(r"[^a-zA-Z0-9_-]", "_", str(cand)).strip("_")
        if cleaned:
            return cleaned
    return None


async def _oidc_admin_groups_set() -> set:
    """Return the configured OIDC admin group names as a set.

    Read from the VRECKAN_OIDC_ADMIN_GROUPS environment variable (exposed as
    ``settings.oidc_admin_groups``). These are the provider (IdP) group names
    whose members are auto-promoted to Vreckan admin on SSO login.
    """
    cfg = await _oidc_config()
    return {t for t in str(cfg["admin_groups"]).replace(",", " ").split() if t}


async def _oidc_user_groups_set() -> set:
    """Return the configured OIDC user group names as a set.

    Read from the VRECKAN_OIDC_USER_GROUPS environment variable (exposed as
    ``settings.oidc_user_groups``). These are the provider (IdP) group names
    that map to regular (non-admin) Vreckan groups; when an SSO user is not in
    an admin group, their first group in this list is used as their Vreckan
    group.
    """
    cfg = await _oidc_config()
    return {t for t in str(cfg["user_groups"]).replace(",", " ").split() if t}


async def _oidc_apply_groups(username: str, claims: Dict):
    """Map provider (e.g. authentik) groups onto the Vreckan account.

    Under the RBAC model this works by *group membership*:

      * The full list of provider groups is recorded on the account (so the
        admin UI can show which SSO groups the user is in).
      * The user is added to a Vreckan group for *every* provider group they
        are in (auto-creating the group definition if it doesn't exist yet).
      * Only the groups named in the configured admin/user lists get roles
        auto-assigned on login: the built-in ``user`` role for all of them, and
        additionally the ``admin`` role for any group in the admin-groups list.
        Because ``is_admin`` is derived from a user's effective permissions
        (and the ``admin`` role grants the ``admin`` super-permission), a member
        of an admin-list group becomes an admin automatically — no
        hand-promotion needed on first login.
      * Any *other* (newly observed) SSO group is created empty — no roles, no
        permissions — and is left for an admin to grant access to explicitly.
        Gating the re-stamp on the configured lists is what keeps an admin's
        manual edits to a group's roles/permissions from being clobbered on
        every login.
      * One of the user's groups is also set as their *primary* group
        (``settings.group``) for quota/settings overrides, preferring a group
        in the configured user-groups list.

    No-op if the id_token carries no recognizable `groups` claim."""
    groups = claims.get("groups")
    if isinstance(groups, str):
        groups = [g for g in groups.split(",") if g]
    if not groups:
        return
    user_groups = [str(g) for g in groups if str(g).strip()]
    # Persist the full SSO group list on the account (idempotent).
    await user_manager.set_sso_groups(username, user_groups)
    admin_groups = await _oidc_admin_groups_set()
    user_groups_set = await _oidc_user_groups_set()
    user = user_manager.get_user(username)
    if user is None:
        return
    # Join the user to a Vreckan group for every provider group they're in.
    # Only the groups named in the configured admin/user lists get roles
    # auto-assigned on login (the user role for all of them, plus the admin
    # role for admin-list groups). Any other — newly observed — SSO group is
    # created empty (no roles, no permissions) and left for an admin to grant
    # access to explicitly. Gating the re-stamp on the configured lists is what
    # keeps an admin's manual edits to a group's roles/permissions from being
    # clobbered on every login.
    joined = list(user.get("groups") or [])
    for g in user_groups:
        await user_manager.ensure_group(g)
        if g in admin_groups or g in user_groups_set:
            group = user_manager.GROUP_DATA.get(g)
            if group is not None:
                roles = list(group.get("roles") or [])
                if "user" not in roles:
                    roles.append("user")
                if g in admin_groups and "admin" not in roles:
                    roles.append("admin")
                group["roles"] = roles
                await user_manager._db_save_group(g, group.get("settings") or {}, roles=roles)
        if g not in joined:
            joined.append(g)
    user["groups"] = joined
    await user_manager._db_save_user_row(username, groups=joined)
    # Pick a primary group (for quota/settings overrides): prefer a group in
    # the configured user-groups list, else the first non-admin group, else
    # the first group.
    primary = next((g for g in user_groups if g in user_groups_set), None)
    if primary is None:
        primary = next((g for g in user_groups if g not in admin_groups), user_groups[0])
    settings_dict = dict(user.get("settings") or user_manager.DEFAULT_USER_SETTINGS.copy())
    settings_dict["group"] = primary
    user["settings"] = settings_dict
    await user_manager._db_save_user_row(username, settings=settings_dict)
    # Re-derive is_admin from the (now group-driven) effective permissions.
    await user_manager.recompute_is_admin(username)


@auth_router.get("/oidc/status")
async def oidc_status(request: Request):
    """Report whether OIDC/SSO is enabled and fully configured. Unauthenticated
    (the browser needs this on the login screen before any session exists)."""
    cfg = await _oidc_config()
    issuer = (cfg["issuer"] or "").strip().rstrip("/")
    enabled = bool(cfg["enabled"] and issuer and cfg["client_id"])
    return {
        "enabled": enabled,
        "configured": enabled,
    }


@auth_router.get("/oidc/authorize")
async def oidc_authorize(request: Request):
    """Begin the OIDC authorization-code flow. Generates a one-time `state`,
    stores it, and 302-redirects the browser to the provider."""
    cfg = await _oidc_config()
    issuer = (cfg["issuer"] or "").strip().rstrip("/")
    if not (cfg["enabled"] and issuer and cfg["client_id"]):
        raise HTTPException(status_code=400, detail="OIDC/SSO is not enabled on this server.")

    state = secrets.token_urlsafe(32)
    now = time.time()
    ttl = int(cfg["state_ttl"] or 600)
    async with api.OIDC_STATES_LOCK:
        # Prune expired states while we hold the lock.
        for k in [k for k, v in api.OIDC_STATES.items() if v.get("expires_at", 0) < now]:
            api.OIDC_STATES.pop(k, None)
        api.OIDC_STATES[state] = {"expires_at": now + ttl}

    discovery = await _oidc_discovery(issuer)
    auth_url = httpx2.URL(discovery["authorization_endpoint"])
    # NOTE: no `prompt=login` here. With an authorization-code flow, sending
    # prompt=login makes the IdP (e.g. authentik) force a *second* full
    # re-authentication after the first one (its `next` param still carries
    # prompt=login), which (a) is bad UX and (b) pushes the round-trip past the
    # state TTL so the callback fails the CSRF check. A plain authorize lets an
    # already-authenticated IdP session proceed straight through.
    params = {
        "response_type": "code",
        "client_id": cfg["client_id"],
        "redirect_uri": await _oidc_redirect_uri(request),
        "scope": cfg["scopes"] or "openid email profile",
        "state": state,
    }

    final = auth_url.copy_merge_params(params)
    return RedirectResponse(str(final), status_code=302)


@auth_router.get("/oidc/callback")
async def oidc_callback(request: Request, code: str = Query(...), state: str = Query(...)):
    """OIDC redirect target. Validates `state`, exchanges the code for tokens,
    verifies the id_token, provisions/logs in the user, sets the web token
    cookie, and redirects to the app root."""
    # 1) Validate and consume the one-time state.
    now = time.time()
    async with api.OIDC_STATES_LOCK:
        st = api.OIDC_STATES.pop(state, None)
    if not st or st.get("expires_at", 0) < now:
        raise HTTPException(status_code=400, detail="Invalid or expired OIDC state (CSRF check failed).")

    cfg = await _oidc_config()
    issuer = (cfg["issuer"] or "").strip().rstrip("/")
    redirect_uri = await _oidc_redirect_uri(request)

    # 2) Exchange the authorization code for tokens.

    discovery = await _oidc_discovery(issuer)
    token_endpoint = discovery["token_endpoint"]
    data = {
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": cfg["client_id"],
    }
    if cfg["client_secret"]:
        data["client_secret"] = cfg["client_secret"]
    try:
        async with httpx2.AsyncClient(timeout=15.0) as client:
            tr = await client.post(token_endpoint, data=data)
    except httpx2.HTTPError as e:
        api.logger.warning("OIDC token exchange failed: %s", e)
        raise HTTPException(status_code=502, detail="OIDC token exchange failed.")
    if tr.status_code != 200:
        api.logger.warning("OIDC token endpoint returned %s: %s", tr.status_code, tr.text[:200])
        raise HTTPException(status_code=502, detail="OIDC token exchange failed.")
    try:
        tokens = tr.json()
    except ValueError:
        raise HTTPException(status_code=502, detail="OIDC token endpoint returned invalid JSON.")

    id_token = tokens.get("id_token")
    if not id_token:
        raise HTTPException(status_code=502, detail="OIDC provider did not return an id_token.")

    # 3) Verify the id_token (signature + claims).
    claims = await _oidc_verify_id_token(id_token, issuer, cfg["client_id"])

    # 4) Derive a username; if the user does not exist, auto-provision.
    username = _oidc_username_from_claims(claims)
    if not username:
        raise HTTPException(status_code=502, detail="Could not derive a username from the OIDC identity.")

    user = user_manager.get_user(username)
    if not user:
        if not cfg["allow_signup"]:
            raise HTTPException(status_code=403, detail="Unknown user and OIDC auto-signup is disabled.")
        try:
            user = await user_manager.create_user(username, {})
        except ValueError as e:
            raise HTTPException(status_code=400, detail=f"Could not provision user: {e}")
    if not user:
        raise HTTPException(status_code=500, detail="Failed to resolve or create the OIDC user.")

    # 4a) This user authenticated through the identity provider, so flag the
    #     account as SSO-provisioned. The UI uses this to hide the local
    #     change-password control (their password is managed by the IdP).
    #     Idempotent: a no-op if the account is already flagged.
    try:
        await user_manager.mark_user_sso(username)
    except Exception as e:  # noqa: BLE001
        api.logger.warning("Could not flag '%s' as SSO: %s", username, e)

    # 4b) Map provider groups onto the account: promote to admin if the user is
    #     in an `oidc_admin_groups` group, otherwise associate their primary
    #     group. These mutate the on-disk user and reload the in-memory store,
    #     so re-fetch to pick up the (possibly new) is_admin state.
    try:
        await _oidc_apply_groups(username, claims)
    except Exception as e:  # noqa: BLE001
        api.logger.warning("OIDC group mapping failed for '%s': %s", username, e)
    user = user_manager.get_user(username) or user

    # No admin bypass, and no reliance on the (dead) user-dict `active` key:
    # resolve the effective setting so a disabled admin is rejected at the SSO
    # callback exactly like any other inactive account.
    if not user_manager.get_effective_settings(username).get("active", False):
        raise HTTPException(status_code=403, detail="User account is inactive.")

    # 5) Issue the web token cookie (same mechanism as password login).
    token = await api.issue_auth_token(username)
    await api.save_auth_tokens_to_disk()

    # 6) Redirect to the app root.
    target = "/"
    resp = RedirectResponse(target, status_code=302)
    resp.set_cookie(**api.auth_cookie_kwargs(token, request))
    return resp


@auth_router.get("/me", response_model=MeResponse)
async def auth_me(user: dict = Depends(api.verify_token)):
    """Return the caller's account metadata."""
    return _me_response(user)


@auth_router.post("/change_password")
async def auth_change_password(
    req: ChangePasswordRequest, user: dict = Depends(api.verify_token)
):
    """Let the caller change their own password (verifies the old one first)."""
    username = user["username"]
    if not user_manager._HAS_ARGON2:
        raise HTTPException(
            status_code=503,
            detail="Password authentication is not available on this server.",
        )
    if not user_manager.verify_password(username, req.old_password):
        raise HTTPException(status_code=401, detail="Current password is incorrect.")
    try:
        await user_manager.set_password(username, req.new_password)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return {"message": "Password updated."}


@auth_router.post("/set_password")
async def auth_set_password(
    req: SetPasswordRequest,
    target: Optional[str] = Query(None, description="Target username; defaults to caller."),
    admin: dict = Depends(api.verify_admin),
):
    """Admin: set or reset the password for a user (or oneself)."""
    username = target or admin["username"]
    if not user_manager._HAS_ARGON2:
        raise HTTPException(
            status_code=503,
            detail="Password authentication is not available on this server.",
        )
    if user_manager.get_user(username) is None:
        raise HTTPException(status_code=404, detail="User not found.")
    try:
        await user_manager.set_password(username, req.password)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return {"message": f"Password set for '{username}'."}


