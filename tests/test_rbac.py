"""RBAC: the role/permission/group access-control model.

Covers the permission catalog, role CRUD, effective-permission resolution
(direct + roles + groups, union semantics, admin super-permission), the
derived is_admin flag (including its persistence), capability derivation
from permissions, per-endpoint 403s for users holding only some admin
permissions, SSO group-join + admin promotion, and the bootstrap-admin
guard.
"""
import asyncio
import sqlite3

import pytest

from server import db, user_manager
from server.permissions import (
    ADMIN_PERMISSIONS,
    ALL_PERMISSIONS,
    USER_PERMISSIONS,
    expand_permissions,
    is_valid_permission,
    union_permissions,
)
from server.settings import settings
from conftest import DB_PATH, SecureClient


# --- helpers ----------------------------------------------------------------

def run(coro):
    """Run a coroutine on a fresh event loop (tests are sync)."""
    return asyncio.new_event_loop().run_until_complete(coro)


def make_logged_in_client(client, username, password):
    """A SecureClient logged in as the given (password-authenticated) user."""
    sc = SecureClient(client)
    resp = sc.login(username, password)
    assert resp.status_code == 200, resp.text
    sc.handshake()
    return sc


# --- Permission catalog ------------------------------------------------------

def test_catalog_shape():
    assert "admin" in ADMIN_PERMISSIONS
    assert "admin" in ALL_PERMISSIONS
    assert len(ALL_PERMISSIONS) == len(ADMIN_PERMISSIONS) + len(USER_PERMISSIONS)
    # No duplicates in the catalog.
    assert len(set(ALL_PERMISSIONS)) == len(ALL_PERMISSIONS)
    for p in ALL_PERMISSIONS:
        assert is_valid_permission(p)
    assert not is_valid_permission("admin.nonexistent")


def test_admin_super_permission_expands_to_whole_catalog():
    assert expand_permissions(["admin"]) == set(ALL_PERMISSIONS)


def test_union_permissions_most_permissive_wins():
    # A single "admin" in any source grants the whole catalog.
    assert union_permissions(["user.storage"], ["admin"]) == set(ALL_PERMISSIONS)
    # Plain union otherwise.
    assert union_permissions(["user.gpu"], ["user.storage"]) == {
        "user.gpu",
        "user.storage",
    }
    # Unknown permissions are dropped.
    assert union_permissions(["bogus.perm"]) == set()


# --- Role CRUD ---------------------------------------------------------------

def test_builtin_roles_seeded_on_load(client):
    # The lifespan (client fixture) runs load_roles(), which seeds the
    # built-in roles on first run.
    assert set(user_manager.ROLE_DATA) == {"admin", "operator", "user"}
    assert user_manager.ROLE_DATA["admin"]["permissions"] == ["admin"]
    assert user_manager.ROLE_DATA["admin"]["is_builtin"]
    assert user_manager.ROLE_DATA["user"]["permissions"] == [
        "user.storage",
        "user.gpu",
    ]


def test_create_role(client):
    role = run(user_manager.create_role("editors", "Can edit", ["user.sharing"]))
    assert role["name"] == "editors"
    assert role["permissions"] == ["user.sharing"]
    assert role["is_builtin"] is False
    assert "editors" in user_manager.ROLE_DATA


def test_create_role_duplicate_rejected(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    with pytest.raises(ValueError):
        run(user_manager.create_role("editors", "", []))


def test_create_role_invalid_name_rejected(client):
    with pytest.raises(ValueError):
        run(user_manager.create_role("bad name!", "", []))


def test_create_role_unknown_permission_rejected(client):
    with pytest.raises(ValueError):
        run(user_manager.create_role("editors", "", ["admin.bogus"]))


def test_update_role(client):
    run(user_manager.create_role("editors", "old", ["user.sharing"]))
    role = run(user_manager.update_role("editors", description="new", permissions=["user.gpu"]))
    assert role["description"] == "new"
    assert role["permissions"] == ["user.gpu"]


def test_update_role_unknown_permission_rejected(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    with pytest.raises(ValueError):
        run(user_manager.update_role("editors", permissions=["nope.nope"]))


def test_update_role_missing_rejected(client):
    with pytest.raises(ValueError):
        run(user_manager.update_role("ghost", description="x"))


def test_delete_role(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    run(user_manager.delete_role("editors"))
    assert "editors" not in user_manager.ROLE_DATA


def test_delete_role_missing_rejected(client):
    with pytest.raises(ValueError):
        run(user_manager.delete_role("ghost"))


def test_delete_role_refused_while_assigned(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    run(user_manager.create_user("alice"))
    run(user_manager.set_user_access("alice", roles=["user", "editors"]))
    with pytest.raises(ValueError):
        run(user_manager.delete_role("editors"))
    # After removing the role from the user, deletion succeeds.
    run(user_manager.set_user_access("alice", roles=["user"]))
    run(user_manager.delete_role("editors"))
    assert "editors" not in user_manager.ROLE_DATA


def test_delete_builtin_role_refused(client):
    # Built-in roles are always assigned (admin to the bootstrap admin, user
    # to every user), so they can never be deleted.
    with pytest.raises(ValueError):
        run(user_manager.delete_role("user"))


# --- Effective permission resolution ------------------------------------------

def test_direct_permissions(client):
    run(user_manager.create_user("alice"))
    run(user_manager.set_user_access("alice", permissions=["user.sharing"]))
    perms = user_manager.get_effective_permissions("alice")
    assert "user.sharing" in perms
    # The user role's permissions are still there (union).
    assert "user.storage" in perms
    assert "user.gpu" in perms


def test_role_permissions(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    run(user_manager.create_user("alice"))
    run(user_manager.set_user_access("alice", roles=["user", "editors"]))
    assert "user.sharing" in user_manager.get_effective_permissions("alice")


def test_group_permissions_and_roles(client):
    run(user_manager.create_role("editors", "", ["user.sharing"]))
    run(user_manager.create_user("alice"))
    run(user_manager.ensure_group("writers"))
    # The group grants a permission directly and a role indirectly.
    user_manager.GROUP_DATA["writers"]["permissions"] = ["user.harden_container"]
    user_manager.GROUP_DATA["writers"]["roles"] = ["editors"]
    run(user_manager.set_user_access("alice", groups=["writers"]))
    perms = user_manager.get_effective_permissions("alice")
    assert "user.harden_container" in perms  # from the group directly
    assert "user.sharing" in perms  # via the group's 'editors' role
    assert "user.storage" in perms  # still via the user's own 'user' role


def test_admin_super_permission_in_effective_set(client):
    # The bootstrap admin has the admin role -> the whole catalog.
    assert user_manager.get_effective_permissions("admin") == sorted(ALL_PERMISSIONS)


def test_unknown_roles_and_permissions_ignored(client):
    run(user_manager.create_user("alice"))
    run(user_manager.set_user_access("alice", roles=["user"]))
    user_manager.USER_DATA["alice"]["roles"].append("ghost-role")
    user_manager.USER_DATA["alice"]["permissions"].append("ghost.perm")
    # No crash, and the unknown entries contribute nothing.
    perms = user_manager.get_effective_permissions("alice")
    assert "ghost.perm" not in perms
    assert "user.storage" in perms


# --- is_admin derivation + persistence ----------------------------------------

def test_is_admin_derived_from_permissions(client):
    run(user_manager.create_user("alice"))
    assert user_manager.USER_DATA["alice"]["is_admin"] is False
    # Granting the admin role makes her an admin.
    run(user_manager.set_user_access("alice", roles=["admin"]))
    assert user_manager.USER_DATA["alice"]["is_admin"] is True
    # Removing it demotes her.
    run(user_manager.set_user_access("alice", roles=["user"]))
    assert user_manager.USER_DATA["alice"]["is_admin"] is False


def test_is_admin_persisted_to_db(client):
    run(user_manager.create_user("alice"))
    run(user_manager.set_user_access("alice", roles=["admin"]))
    # Wipe the in-memory state and reload from the DB: the flag must survive.
    user_manager.USER_DATA.clear()
    user_manager.GROUP_DATA.clear()
    user_manager.ROLE_DATA.clear()
    run(user_manager.load_users_and_groups())
    assert user_manager.USER_DATA["alice"]["is_admin"] is True


def test_promote_and_demote(client):
    run(user_manager.create_user("alice"))
    run(user_manager.promote_user_to_admin("alice"))
    assert user_manager.USER_DATA["alice"]["is_admin"] is True
    assert "admin" in user_manager.USER_DATA["alice"]["roles"]
    run(user_manager.demote_admin_to_user("alice"))
    assert user_manager.USER_DATA["alice"]["is_admin"] is False
    # Demotion leaves the account with the built-in user role.
    assert user_manager.USER_DATA["alice"]["roles"] == ["user"]


def test_bootstrap_admin_cannot_be_demoted(client):
    with pytest.raises(ValueError):
        run(user_manager.demote_admin_to_user("admin"))


def test_bootstrap_admin_must_keep_admin_role(client):
    with pytest.raises(ValueError):
        run(user_manager.set_user_access("admin", roles=["user"]))
    # The guard did not mutate the account.
    assert "admin" in user_manager.USER_DATA["admin"]["roles"]


# --- Capability derivation -----------------------------------------------------

def test_capabilities_derived_from_permissions(client):
    run(user_manager.create_user("alice"))
    s = user_manager.get_effective_settings("alice")
    # The 'user' role grants user.storage + user.gpu but not user.sharing.
    assert s["persistent_storage"] is True
    assert s["gpu"] is True
    assert s["public_sharing"] is False
    # Granting user.sharing flips the capability on.
    run(user_manager.set_user_access("alice", permissions=["user.sharing"]))
    assert user_manager.get_effective_settings("alice")["public_sharing"] is True


def test_capabilities_off_without_permissions(client):
    run(user_manager.create_user("alice"))
    # Strip every role/permission: no capabilities at all.
    run(user_manager.set_user_access("alice", roles=[], permissions=[]))
    s = user_manager.get_effective_settings("alice")
    assert s["persistent_storage"] is False
    assert s["public_sharing"] is False
    assert s["gpu"] is False


# --- Per-endpoint 403s ----------------------------------------------------------

def test_partial_admin_permissions_gate_endpoints(client):
    # The built-in 'operator' role grants admin.apps/admin.stores/
    # admin.sessions but NOT admin.accounts.
    run(user_manager.create_user("bob"))
    run(user_manager.set_password("bob", "bobpass123"))
    run(user_manager.set_user_access("bob", roles=["operator"]))
    sc = make_logged_in_client(client, "bob", "bobpass123")
    # Allowed: operator holds admin.apps.
    status, _ = sc.call("GET", "/api/admin/apps/installed")
    assert status == 200
    # Denied: operator does not hold admin.accounts.
    status, data = sc.call("GET", "/api/admin/people/bob/homedirs")
    assert status == 403
    # Denied: operator does not hold admin.roles.
    status, _ = sc.call("GET", "/api/admin/roles")
    assert status == 403
    # Denied: operator does not hold admin.global_settings.
    status, _ = sc.call("GET", "/api/admin/global-settings")
    assert status == 403


def test_plain_user_denied_all_admin_endpoints(client):
    run(user_manager.create_user("carol"))
    run(user_manager.set_password("carol", "carolpass1"))
    sc = make_logged_in_client(client, "carol", "carolpass1")
    # /data is a POST (the aggregate management payload); the rest are GETs.
    status, _ = sc.call("POST", "/api/admin/data", body={})
    assert status == 403
    for path in [
        "/api/admin/apps/installed",
        "/api/admin/roles",
        "/api/admin/global-settings",
        "/api/admin/sessions",
    ]:
        status, _ = sc.call("GET", path)
        assert status == 403, f"{path} should be 403 for a plain user"


def test_full_admin_allowed_everywhere(client):
    sc = make_logged_in_client(client, "admin", "admin1234")
    # /data is a POST (the aggregate management payload); the rest are GETs.
    status, _ = sc.call("POST", "/api/admin/data", body={})
    assert status == 200
    for path in [
        "/api/admin/apps/installed",
        "/api/admin/roles",
        "/api/admin/permissions",
        "/api/admin/global-settings",
        "/api/admin/sso-groups",
        "/api/admin/sessions",
        "/api/admin/volume_mounts",
        "/api/admin/backup",
    ]:
        status, _ = sc.call("GET", path)
        assert status == 200, f"{path} should be 200 for the full admin"


# --- Roles API (through the encrypted client) -----------------------------------

def test_roles_api_crud(secure_client):
    # Create.
    status, role = secure_client.call(
        "POST", "/api/admin/roles",
        body={"name": "editors", "description": "d", "permissions": ["user.sharing"]},
    )
    assert status == 201
    assert role["name"] == "editors"
    # Duplicate -> 409.
    status, data = secure_client.call(
        "POST", "/api/admin/roles", body={"name": "editors", "permissions": []}
    )
    assert status == 409
    # List.
    status, roles = secure_client.call("GET", "/api/admin/roles")
    assert status == 200
    assert {r["name"] for r in roles} == {"admin", "operator", "user", "editors"}
    # Update.
    status, role = secure_client.call(
        "PUT", "/api/admin/roles/editors",
        body={"description": "new", "permissions": ["user.gpu"]},
    )
    assert status == 200
    assert role["permissions"] == ["user.gpu"]
    # Delete (nothing is assigned to it).
    status, _ = secure_client.call("DELETE", "/api/admin/roles/editors")
    assert status == 204
    status, roles = secure_client.call("GET", "/api/admin/roles")
    assert {r["name"] for r in roles} == {"admin", "operator", "user"}


def test_permissions_catalog_endpoint(secure_client):
    status, data = secure_client.call("GET", "/api/admin/permissions")
    assert status == 200
    assert {p["name"] for p in data["admin"]} == set(ADMIN_PERMISSIONS)
    assert {p["name"] for p in data["user"]} == set(USER_PERMISSIONS)


def test_people_access_endpoint(secure_client):
    # Create a user, then grant them a role + permission + group via /access.
    status, _ = secure_client.call("POST", "/api/admin/people", body={"username": "dave"})
    assert status == 201
    status, user = secure_client.call(
        "POST", "/api/admin/people/dave/access",
        body={"roles": ["operator"], "permissions": ["user.sharing"], "groups": ["writers"]},
    )
    assert status == 200
    assert user["roles"] == ["operator"]
    assert user["permissions"] == ["user.sharing"]
    assert user["groups"] == ["writers"]
    # The first group becomes the primary group.
    assert user["settings"]["group"] == "writers"
    # The bootstrap admin cannot be stripped of the admin role.
    status, data = secure_client.call(
        "POST", "/api/admin/people/admin/access", body={"roles": ["user"]}
    )
    assert status == 403


# --- SSO group-join + admin promotion --------------------------------------------

def test_oidc_admin_group_promotes_to_admin(client, monkeypatch):
    # Point the admin-groups list at 'vdi-admins' (read at call time).
    monkeypatch.setattr(settings, "oidc_admin_groups", "vdi-admins")
    monkeypatch.setattr(settings, "oidc_user_groups", "vdi-users")
    run(user_manager.create_user("sstest"))
    from server.routers import auth as auth_router

    run(auth_router._oidc_apply_groups("sstest", {"groups": ["vdi-admins", "vdi-users"]}))
    user = user_manager.USER_DATA["sstest"]
    # Joined both provider groups.
    assert set(user["groups"]) == {"vdi-admins", "vdi-users"}
    # The admin-list group carries the admin role -> the user is an admin.
    assert "admin" in user_manager.GROUP_DATA["vdi-admins"]["roles"]
    assert "user" in user_manager.GROUP_DATA["vdi-users"]["roles"]
    assert user["is_admin"] is True
    # The primary group is the one in the user-groups list.
    assert user["settings"]["group"] == "vdi-users"


def test_oidc_non_admin_group_does_not_promote(client, monkeypatch):
    monkeypatch.setattr(settings, "oidc_admin_groups", "vdi-admins")
    monkeypatch.setattr(settings, "oidc_user_groups", "vdi-users")
    run(user_manager.create_user("plain"))
    from server.routers import auth as auth_router

    run(auth_router._oidc_apply_groups("plain", {"groups": ["vdi-users"]}))
    user = user_manager.USER_DATA["plain"]
    assert "vdi-users" in user["groups"]
    assert user["is_admin"] is False
    # Still a normal user with the user role's capabilities.
    assert "user.storage" in user_manager.get_effective_permissions("plain")


# --- SSO / OIDC configuration (editable from the UI) --------------------------

def test_sso_get_reports_values_and_sources(client, secure_client, monkeypatch):
    # A DB value (as if saved on the SSO page) overrides the environment.
    run(db.set_app_setting("VRECKAN_OIDC_ISSUER", "https://db.example/o/app/"))
    run(db.set_app_setting("VRECKAN_OIDC_ENABLED", "true"))
    monkeypatch.setattr(settings, "oidc_issuer", "https://env.example/o/app/")
    monkeypatch.setattr(settings, "oidc_client_id", "env-client")

    status, data = secure_client.call("GET", "/api/admin/sso")
    assert status == 200
    # DB wins over env for the issuer; env is used for the client id (no DB row).
    assert data["values"]["oidc_issuer"] == "https://db.example/o/app/"
    assert data["values"]["oidc_client_id"] == "env-client"
    assert data["sources"]["oidc_issuer"] == "db"
    assert data["sources"]["oidc_client_id"] == "env"
    # The effective config resolves the same way.
    from server.routers import auth as auth_router

    cfg = run(auth_router._oidc_config())
    assert cfg["issuer"] == "https://db.example/o/app/"
    assert cfg["client_id"] == "env-client"
    assert cfg["enabled"] is True


def test_sso_put_persists_and_overrides_env(client, secure_client, monkeypatch):
    monkeypatch.setattr(settings, "oidc_admin_groups", "env-admin")
    status, data = secure_client.call(
        "PUT",
        "/api/admin/sso",
        body={
            "oidc_enabled": True,
            "oidc_issuer": "https://idp.example/o/app/",
            "oidc_client_id": "cid",
            "oidc_client_secret": "",
            "oidc_redirect_uri": "",
            "oidc_scopes": "openid email profile",
            "oidc_allow_signup": True,
            "oidc_state_ttl_seconds": 600,
            "oidc_admin_groups": ["vdi-admins"],
            "oidc_user_groups": ["vdi-users"],
        },
    )
    assert status == 200
    # The saved values are now the effective ones (DB overrides the env).
    from server.routers import auth as auth_router

    cfg = run(auth_router._oidc_config())
    assert cfg["issuer"] == "https://idp.example/o/app/"
    assert cfg["admin_groups"] == "vdi-admins"
    assert cfg["user_groups"] == "vdi-users"
    # The source badge now reports db for the edited fields.
    status, data = secure_client.call("GET", "/api/admin/sso")
    assert data["sources"]["oidc_admin_groups"] == "db"
    assert data["values"]["oidc_admin_groups"] == ["vdi-admins"]


def test_sso_test_connection(client, secure_client, monkeypatch):
    from server.routers import admin as admin_router

    async def fake_discovery(issuer):
        # Real discovery documents return full endpoint URLs (with slashes).
        return {
            "issuer": issuer,
            "authorization_endpoint": issuer + "/authorize",
            "token_endpoint": issuer + "/token",
            "userinfo_endpoint": issuer + "/userinfo",
            "jwks_uri": issuer + "/jwks",
        }

    # The SSO test endpoint lives in the admin router and imports
    # _oidc_discovery from the auth module, so patch the admin module's
    # reference (not auth's) for the monkeypatch to take effect.
    monkeypatch.setattr(admin_router, "_oidc_discovery", fake_discovery)
    run(db.set_app_setting("VRECKAN_OIDC_ISSUER", "https://idp.example/o/app/"))
    status, data = secure_client.call("POST", "/api/admin/sso/test", body={})
    assert status == 200
    assert data["ok"] is True
    # The endpoint strips the trailing slash before discovery, so the echoed
    # issuer is the normalized (slash-free) form.
    assert data["issuer"] == "https://idp.example/o/app"
    assert data["authorization_endpoint"] == "https://idp.example/o/app/authorize"


def test_sso_endpoint_requires_admin_sso(client, monkeypatch):
    # A user with only admin.groups can see the (groups) sso-groups endpoint
    # but not the SSO config endpoint.
    run(user_manager.create_user("ssoonly"))
    run(user_manager.set_password("ssoonly", "ssoonly1"))
    run(user_manager.set_user_access("ssoonly", roles=[], permissions=["admin.groups"], groups=[]))
    sc = make_logged_in_client(client, "ssoonly", "ssoonly1")
    status, _ = sc.call("GET", "/api/admin/sso")
    assert status == 403
    status, _ = sc.call("GET", "/api/admin/sso-groups")
    assert status == 200
    # A user with admin.sso can reach the SSO config endpoint.
    run(user_manager.create_user("ssoadmin"))
    run(user_manager.set_password("ssoadmin", "ssoadmin1"))
    run(user_manager.set_user_access("ssoadmin", roles=[], permissions=["admin.sso"], groups=[]))
    sc2 = make_logged_in_client(client, "ssoadmin", "ssoadmin1")
    status, _ = sc2.call("GET", "/api/admin/sso")
    assert status == 200


# --- Bootstrap admin (configurable via environment) -------------------------

def test_bootstrap_admin_configurable(client, monkeypatch):
    # Point the bootstrap username/password at custom values (read at seed
    # time). The lifespan already ran with the defaults, so the DB has no
    # admin yet and _generate_default_admin will create the custom one.
    monkeypatch.setattr(settings, "bootstrap_admin_username", "rooty")
    monkeypatch.setattr(settings, "bootstrap_admin_password", "s3cret")
    # The lifespan already seeded the default 'admin'; wipe the users table so
    # _generate_default_admin runs its create path with the custom values.
    conn = sqlite3.connect(str(DB_PATH))
    conn.execute("DELETE FROM users")
    conn.commit()
    conn.close()
    run(user_manager._generate_default_admin())
    run(user_manager.load_users_and_groups())
    # The account is created with the configured username and password.
    assert "rooty" in user_manager.USER_DATA
    assert user_manager.USER_DATA["rooty"]["is_admin"] is True
    assert user_manager.verify_password("rooty", "s3cret")
    assert not user_manager.verify_password("rooty", "admin1234")
    # The protection guards follow the configured name, not the literal
    # 'admin': the bootstrap account can't be deleted or demoted, and a
    # non-bootstrap admin can.
    with pytest.raises(ValueError):
        run(user_manager.delete_person("rooty"))
    with pytest.raises(ValueError):
        run(user_manager.demote_admin_to_user("rooty"))
    run(user_manager.create_user("otheradmin"))
    run(user_manager.promote_user_to_admin("otheradmin"))
    run(user_manager.demote_admin_to_user("otheradmin"))
    run(user_manager.delete_person("otheradmin"))


def test_admin_sso_in_catalog(secure_client):
    status, data = secure_client.call("GET", "/api/admin/permissions")
    assert status == 200
    assert "admin.sso" in {p["name"] for p in data["admin"]}
