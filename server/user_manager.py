import os
import logging
import re
import shutil
from typing import Dict, Optional, List
from sqlalchemy import delete, select, update

from .settings import settings
from . import db

# argon2id password hashing. Guarded so a missing dependency degrades to a
# clear runtime error on password features instead of breaking module import.
try:
    from argon2 import PasswordHasher
    from argon2.exceptions import (
        VerifyMismatchError,
        VerificationError,
        InvalidHashError,
    )

    _HAS_ARGON2 = True
except ImportError:  # pragma: no cover - dependency present in normal builds
    PasswordHasher = None  # type: ignore
    VerifyMismatchError = VerificationError = InvalidHashError = Exception  # type: ignore
    _HAS_ARGON2 = False

# Module-level argon2id hasher (parameters are embedded in each stored hash,
# so a single shared instance is safe across restarts and version upgrades).
_password_hasher = PasswordHasher() if _HAS_ARGON2 else None

logger = logging.getLogger(__name__)

USER_DATA: Dict[str, Dict] = {}
GROUP_DATA: Dict[str, Dict] = {}
# Role name -> {name, description, permissions, is_builtin}. Roles are named
# bundles of permissions that can be assigned to users and groups.
ROLE_DATA: Dict[str, Dict] = {}

# The built-in roles seeded on first run. They are fully editable (an admin
# can change their permission sets) but flagged is_builtin so the UI can show
# they ship with the application.
BUILTIN_ROLES = [
    {
        "name": "admin",
        "description": "Full administrator. Grants every admin permission.",
        "permissions": ["admin"],
        "is_builtin": True,
    },
    {
        "name": "operator",
        "description": "Manages apps, stores, and sessions; can use GPU and storage.",
        "permissions": [
            "admin.apps",
            "admin.stores",
            "admin.sessions",
            "user.gpu",
            "user.storage",
        ],
        "is_builtin": True,
    },
    {
        "name": "user",
        "description": "Standard user. Can use persistent storage and the GPU.",
        "permissions": ["user.storage", "user.gpu"],
        "is_builtin": True,
    },
]

DEFAULT_USER_SETTINGS = {
    "active": True,
    "group": "none",
    "persistent_storage": True,
    "public_sharing": False,
    "harden_container": False,
    "harden_openbox": False,
    "gpu": True,
    "storage_limit": -1,
    "session_limit": -1,
    # Personal UI language preference (None = follow the browser). Not an
    # admin-managed capability; set via the sidebar language selector.
    "ui_language": None,
}

# The capability settings that are now driven by permissions. Each maps a
# settings-form field (a boolean capability) to the permission that grants it.
# A user/group "has" the capability iff the corresponding permission is in their
# effective (union) permission set. The settings-form checkboxes edit these
# permissions at the user/group level.
CAPABILITY_PERMISSIONS = {
    "persistent_storage": "user.storage",
    "public_sharing": "user.sharing",
    "harden_container": "user.harden_container",
    "harden_openbox": "user.harden_openbox",
    "gpu": "user.gpu",
}
# The reverse mapping (permission -> settings field) for syncing the form.
_PERMISSION_CAPABILITY = {v: k for k, v in CAPABILITY_PERMISSIONS.items()}

SERVER_PUBLIC_KEY_PEM: Optional[str] = None
EXTERNAL_API_PORT: int = getattr(settings, "api_port", 8000)
EXTERNAL_SESSION_PORT: int = getattr(settings, "session_port", 8443)

def set_server_public_key(key: str):
    """Sets the server's public key for use in log messages."""
    global SERVER_PUBLIC_KEY_PEM
    SERVER_PUBLIC_KEY_PEM = key

def set_external_ports(api_port: int, session_port: int):
    global EXTERNAL_API_PORT, EXTERNAL_SESSION_PORT
    EXTERNAL_API_PORT = api_port
    EXTERNAL_SESSION_PORT = session_port

# ---------------------------------------------------------------------------
# Database persistence helpers
#
# The in-memory dictionaries above (USER_DATA / GROUP_DATA) are the runtime
# source of truth. These helpers keep the configuration database in sync with
# them. All are async because the database layer is async.
# ---------------------------------------------------------------------------

async def _db_save_user_row(username: str, **fields) -> None:
    """Insert or update a user row, setting only the provided (non-None) fields."""
    async with db.async_session_factory() as session:
        user = (
            await session.execute(select(db.User).where(db.User.username == username))
        ).scalar_one_or_none()
        if user is None:
            user = db.User(username=username)
            session.add(user)
        for key, value in fields.items():
            if value is not None:
                setattr(user, key, value)
        await session.commit()


async def _db_delete_user_row(username: str) -> None:
    async with db.async_session_factory() as session:
        await session.execute(delete(db.User).where(db.User.username == username))
        await session.commit()


async def _db_save_group(
    group_name: str,
    group_settings: dict,
    roles: Optional[list] = None,
    permissions: Optional[list] = None,
) -> None:
    """Insert or update a group row (settings, and optionally roles/permissions)."""
    async with db.async_session_factory() as session:
        group = (
            await session.execute(select(db.Group).where(db.Group.name == group_name))
        ).scalar_one_or_none()
        if group is None:
            group = db.Group(name=group_name)
            session.add(group)
        group.settings = group_settings
        if roles is not None:
            group.roles = roles
        if permissions is not None:
            group.permissions = permissions
        await session.commit()


async def _db_delete_group(group_name: str) -> None:
    async with db.async_session_factory() as session:
        await session.execute(delete(db.Group).where(db.Group.name == group_name))
        await session.commit()


async def _db_set_password(username: str, password_hash) -> None:
    """Set (or clear, when ``password_hash`` is None) a user's password hash."""
    async with db.async_session_factory() as session:
        await session.execute(
            update(db.User).where(db.User.username == username).values(password_hash=password_hash)
        )
        await session.commit()


async def _generate_default_admin() -> None:
    """Bootstrap a default admin account in the database on first run.

    The default admin is seeded with a well-known bootstrap password
    (``admin1234``) which is logged once at first run; the admin is expected
    to change it after first login. No key material is generated — web login
    is password-based.
    """
    async with db.async_session_factory() as session:
        existing = await session.execute(
            select(db.User.username).where(db.User.is_admin.is_(True))
        )
        if existing.first() is not None:
            return

    admin_username = str(settings.bootstrap_admin_username or "admin")
    bootstrap_password = str(settings.bootstrap_admin_password or "admin1234")
    password_hash = _password_hasher.hash(bootstrap_password) if _HAS_ARGON2 else None

    async with db.async_session_factory() as session:
        session.add(
            db.User(
                username=admin_username,
                is_admin=True,
                settings=None,
                password_hash=password_hash,
            )
        )
        await session.commit()

    # Record the actual bootstrap username so the protection guards (see
    # ``_bootstrap_username``) can identify it deterministically, regardless of
    # database row ordering or later environment changes.
    await db.set_app_setting("VRECKAN_BOOTSTRAP_ADMIN_USERNAME", admin_username)

    logger.warning(
        "No admin users found. Created default '%s' admin with bootstrap "
        "password '%s' — change it after first login.",
        admin_username,
        bootstrap_password,
    )


async def _bootstrap_username() -> str:
    """Resolve the bootstrap admin's username.

    The bootstrap account is the one created on first run; its name is
    configurable via ``VRECKAN_BOOTSTRAP_ADMIN_USERNAME``. The protection
    guards (delete / demote / role-strip) compare against this value.

    ``_generate_default_admin`` records the actual username it created in the
    ``app_settings`` table, so once the account exists that value wins over
    the environment (which may have changed since first run). ``get_setting``
    resolves the precedence (stored value → environment → default), so this
    is deterministic across database backends — unlike picking "an admin"
    from the users table, whose row order is database-dependent.
    """
    return str(await settings.get_setting("bootstrap_admin_username") or "admin")


async def _migrate_admin_settings() -> None:
    """One-time migration: give admin accounts that still have NULL settings
    an explicit default settings dict (active=True).

    Before the admin/user unification, admins stored ``settings=None`` and
    their settings were ignored. Now admins use the same settings model as
    regular users, and the per-user ``active`` precedence only applies when
    ``active`` is explicitly present. Seeding the defaults gives every admin
    an explicit ``active=True`` so they stay active (per the admin-group
    guarantee) unless individually disabled. Non-admins are left untouched.
    Idempotent: a no-op once no admin has NULL settings.
    """
    changed = 0
    for username, user in USER_DATA.items():
        if user.get("is_admin") and user.get("settings") is None:
            user["settings"] = dict(DEFAULT_USER_SETTINGS)
            await _db_save_user_row(username, settings=user["settings"])
            changed += 1
    if changed:
        logger.info(
            "Migrated %d account(s) from NULL settings to explicit defaults.",
            changed,
        )


async def _migrate_rbac() -> None:
    """One-time migration from the old single-flag/group model to RBAC.

    Runs on the first startup after the roles/permissions columns are added.
    It snapshots each account's *current* capability settings into the new
    permission model so no one loses (or gains) capabilities on upgrade:

      * admins get the ``admin`` role (they keep full access);
      * every other user gets the ``user`` role plus direct permissions for
        any capability their settings grant that the ``user`` role does not
        (e.g. public_sharing, or a disabled gpu);
      * the user's single ``settings.group`` is moved into the new multi-group
        ``groups`` list (and that group's settings become its primary group);
      * each group's capability settings are snapshotted into the group's
        permissions (so group-level capability overrides keep working).

    Idempotent: it only acts while the new columns are still empty (the first
    run). Once an admin has edited roles/permissions in the UI the columns are
    populated and the migration is a no-op.
    """
    # Only migrate accounts that have not been touched by the new model yet.
    if any((u.get("roles") or u.get("permissions") or u.get("groups")) for u in USER_DATA.values()):
        return
    if any((g.get("roles") or g.get("permissions")) for g in GROUP_DATA.values()):
        return

    changed_users = 0
    for username, user in USER_DATA.items():
        settings = user.get("settings") or {}
        if user.get("is_admin"):
            roles = ["admin"]
            permissions = []
        else:
            roles = ["user"]
            # The "user" role grants user.storage + user.gpu. Add direct
            # permissions for any capability the settings grant that the role
            # does not, and (for capabilities the role grants) a direct
            # permission only when the setting is explicitly enabled.
            permissions = []
            for field, perm in CAPABILITY_PERMISSIONS.items():
                value = settings.get(field)
                if value is None:
                    continue  # not explicitly set -> role/default decides
                if value and perm not in _role_permissions("user"):
                    permissions.append(perm)
        # Move the single group into the multi-group list.
        groups = []
        primary = settings.get("group")
        if primary and primary != "none":
            groups = [primary]
        user["roles"] = roles
        user["permissions"] = permissions
        user["groups"] = groups
        await _db_save_user_row(
            username, roles=roles, permissions=permissions, groups=groups
        )
        changed_users += 1

    # Snapshot each group's capability settings into its permissions so
    # group-level capability overrides keep applying to members.
    changed_groups = 0
    for group_name, group in GROUP_DATA.items():
        gsettings = group.get("settings") or {}
        perms = []
        for field, perm in CAPABILITY_PERMISSIONS.items():
            if gsettings.get(field):
                perms.append(perm)
        if perms:
            group["permissions"] = perms
            await _db_save_group(group_name, gsettings, roles=group.get("roles") or [], permissions=perms)
            changed_groups += 1

    if changed_users or changed_groups:
        logger.info(
            "RBAC migration: snapshotted capabilities for %d user(s) and %d group(s).",
            changed_users,
            changed_groups,
        )


async def load_roles() -> None:
    """Load roles from the database, seeding the built-in roles on first run.

    Populates the in-memory ROLE_DATA dictionary (the runtime source of truth
    for role definitions). If the roles table is empty, the built-in roles
    (admin / operator / user) are seeded.
    """
    global ROLE_DATA
    ROLE_DATA.clear()
    async with db.async_session_factory() as session:
        # NOTE: .scalars() returns a lazy ScalarResult that is *always*
        # truthy (even when empty), so we must materialise with .all() before
        # the emptiness check — otherwise the first-run seed branch never runs
        # and the roles table is left empty.
        rows = (await session.execute(select(db.Role))).scalars().all()
        if not rows:
            # First run: seed the built-in roles.
            for r in BUILTIN_ROLES:
                session.add(
                    db.Role(
                        name=r["name"],
                        description=r["description"],
                        permissions=r["permissions"],
                        is_builtin=r["is_builtin"],
                    )
                )
            await session.commit()
            rows = (await session.execute(select(db.Role))).scalars().all()
        for r in rows:
            ROLE_DATA[r.name] = {
                "name": r.name,
                "description": r.description or "",
                "permissions": r.permissions or [],
                "is_builtin": bool(r.is_builtin),
            }
    logger.info(f"Loaded {len(ROLE_DATA)} role(s) from the database.")


async def load_users_and_groups() -> None:
    """Load users, admins, groups, and roles from the configuration database.

    Populates the in-memory USER_DATA / GROUP_DATA / ROLE_DATA dictionaries
    (the runtime source of truth). Bootstraps a default admin on first run,
    loads the roles, then runs the settings + RBAC migrations.
    """
    global USER_DATA, GROUP_DATA
    logger.info("Loading users, admins, groups, and roles from the configuration database...")

    USER_DATA.clear()
    GROUP_DATA.clear()

    await _generate_default_admin()
    await load_roles()

    async with db.async_session_factory() as session:
        for u in (await session.execute(select(db.User))).scalars():
            USER_DATA[u.username] = {
                "username": u.username,
                "is_admin": u.is_admin,
                "is_sso": bool(u.is_sso),
                "settings": u.settings,
                "password_hash": u.password_hash,
                # Normalise NULL (non-SSO accounts) to an empty list so the
                # in-memory dict — and any endpoint that returns it as a User
                # response — always carries a list, not None.
                "sso_groups": u.sso_groups or [],
                "roles": u.roles or [],
                "permissions": u.permissions or [],
                "groups": u.groups or [],
            }
        for g in (await session.execute(select(db.Group))).scalars():
            GROUP_DATA[g.name] = {
                "name": g.name,
                "settings": g.settings,
                "roles": g.roles or [],
                "permissions": g.permissions or [],
            }

    await _migrate_admin_settings()
    await _migrate_rbac()

    logger.info(
        f"Loaded {len(USER_DATA)} user(s), {len(GROUP_DATA)} group(s), "
        f"{len(ROLE_DATA)} role(s) from the database."
    )


def get_user(username: str) -> Optional[Dict]:
    return USER_DATA.get(username)


def get_effective_settings(username: str) -> Dict:
    """Return the final, calculated settings for a user.

    Two kinds of settings are resolved differently:

    * **Capabilities** (persistent_storage, public_sharing, harden_container,
      harden_openbox, gpu) are now driven by the *permission* system: each is
      True iff the corresponding ``user.*`` permission is in the user's
      effective (union) permission set. This is what makes capabilities
      inheritable from roles and groups in both directions.
    * **Quotas and account state** (active, storage_limit, session_limit) are
      still resolved from the settings model: per-user settings are the base
      (defaults fill missing keys) and the user's *primary* group's settings
      override them — EXCEPT ``active``, where an explicit per-user value
      always wins over the group, so an account can be individually disabled
      (or kept active) regardless of its group.
    """
    user = get_user(username)
    if not user:
        return DEFAULT_USER_SETTINGS.copy()

    user_settings = user.get("settings")  # may be None (no explicit settings)
    # Backfill defaults first so consumers always see a complete settings dict,
    # then overlay the user's explicit values.
    merged = DEFAULT_USER_SETTINGS.copy()
    if user_settings:
        merged.update(user_settings)

    # Quota / account-state overrides from the user's primary group.
    group_name = merged.get("group", "none")
    if group_name and group_name != "none" and group_name in GROUP_DATA:
        group_settings = GROUP_DATA[group_name].get("settings", {})
        effective_settings = merged.copy()
        effective_settings.update(group_settings)
        # Explicit per-user `active` wins over the group's value.
        if user_settings and "active" in user_settings:
            effective_settings["active"] = user_settings["active"]
    else:
        effective_settings = merged

    # Capabilities come from the permission system (union of user + roles +
    # groups), not from the settings dict, so they inherit both ways.
    perms = set(get_effective_permissions(username))
    for field, perm in CAPABILITY_PERMISSIONS.items():
        effective_settings[field] = perm in perms

    return effective_settings


def get_all_users() -> List[Dict]:
    return [u for u in USER_DATA.values() if not u["is_admin"]]


def get_all_admins() -> List[Dict]:
    return [u for u in USER_DATA.values() if u["is_admin"]]


def get_all_groups() -> List[Dict]:
    return list(GROUP_DATA.values())


def get_all_roles() -> List[Dict]:
    return list(ROLE_DATA.values())


def get_role(name: str) -> Optional[Dict]:
    return ROLE_DATA.get(name)


# ---------------------------------------------------------------------------
# Permission resolution (RBAC)
#
# A user's effective permissions are the union (most-permissive-wins) of:
#   * the permissions assigned directly to the user,
#   * the permissions of the user's roles,
#   * the permissions (and roles) of every group the user belongs to.
# The ``admin`` super-permission expands to the whole catalog. ``is_admin`` is
# derived from this set (it is True iff ``admin`` is in it).
# ---------------------------------------------------------------------------
def _role_permissions(role_name: str) -> List[str]:
    role = ROLE_DATA.get(role_name)
    if not role:
        return []
    return role.get("permissions") or []


def get_effective_permissions(username: str) -> List[str]:
    """Return the user's effective (expanded) permission set as a sorted list.

    Unions the user's direct permissions, their roles, and the permissions and
    roles of every group they belong to, then expands (so ``admin`` grants the
    whole catalog). Unknown role/permission names are ignored.
    """
    user = USER_DATA.get(username)
    if not user:
        return []
    from .permissions import union_permissions

    sources = [user.get("permissions") or []]
    for role in user.get("roles") or []:
        sources.append(_role_permissions(role))
    for group_name in user.get("groups") or []:
        group = GROUP_DATA.get(group_name)
        if not group:
            continue
        sources.append(group.get("permissions") or [])
        for role in group.get("roles") or []:
            sources.append(_role_permissions(role))
    return sorted(union_permissions(*sources))


def has_permission(username: str, permission: str) -> bool:
    """True if the user's effective permissions include ``permission``."""
    return permission in get_effective_permissions(username)


async def recompute_is_admin(username: str) -> bool:
    """Re-derive and persist the user's ``is_admin`` flag from their
    effective permissions. Returns the new value. The flag is stored (not just
    computed) so it is available without re-resolving and stays consistent in
    the database."""
    user = USER_DATA.get(username)
    if not user:
        return False
    is_admin = "admin" in get_effective_permissions(username)
    if user.get("is_admin") != is_admin:
        user["is_admin"] = is_admin
        await _db_save_user_row(username, is_admin=is_admin)
    return is_admin


# ---------------------------------------------------------------------------
# Observed SSO groups
#
# The distinct provider (IdP) groups that SSO users are currently in, derived
# from the sso_groups recorded on each SSO account. This is the "observed"
# set - it reflects reality and updates as users log in, as opposed to the
# configured admin/user group lists (which come from the environment).
# ---------------------------------------------------------------------------
def observed_sso_groups() -> List[str]:
    """Return the sorted, de-duplicated list of provider groups that SSO users
    are currently in (from the sso_groups recorded on each SSO account)."""
    seen = set()
    for u in USER_DATA.values():
        if u.get("is_sso"):
            for g in (u.get("sso_groups") or []):
                if g:
                    seen.add(g)
    return sorted(seen)


async def create_admin(username: str, settings_dict: Optional[dict] = None) -> Dict:
    if not re.match(r"^[a-zA-Z0-9_-]+$", username):
        raise ValueError(
            "Invalid username. Use only letters, numbers, underscore, or hyphen."
        )
    if username in USER_DATA:
        raise ValueError(f"User or admin '{username}' already exists.")

    # Admins are always created active: an explicit active=True is stored so
    # the per-user `active` precedence keeps them active even if their group
    # says otherwise (they can still be individually disabled later).
    admin_settings = dict(DEFAULT_USER_SETTINGS)
    if settings_dict:
        admin_settings.update(settings_dict)
    admin_settings["active"] = True

    USER_DATA[username] = {
        "username": username,
        "is_admin": True,
        "is_sso": False,
        "settings": admin_settings,
        "password_hash": None,
        "sso_groups": [],
        "roles": ["admin"],
        "permissions": [],
        "groups": [],
    }
    await _db_save_user_row(
        username, is_admin=True, settings=admin_settings, roles=["admin"]
    )
    logger.info(f"Created admin '{username}'.")
    return get_user(username)


async def delete_person(username: str):
    """Delete a user or admin. Only the root 'admin' account is protected.

    Admins and regular users are managed through the same roster, so deletion
    is unified: the only account that cannot be removed is the bootstrap
    'admin' (deleting it would lock everyone out of the admin UI).
    """
    if username == await _bootstrap_username():
        raise ValueError(f"The root '{username}' account cannot be deleted.")

    user = get_user(username)
    if not user:
        raise ValueError(f"User or admin '{username}' not found.")

    user_storage_path = os.path.join(settings.storage_path, username)
    if os.path.isdir(user_storage_path):
        shutil.rmtree(user_storage_path)
        logger.info(f"Deleted storage for '{username}'.")

    USER_DATA.pop(username, None)
    await _db_delete_user_row(username)
    logger.info(f"Deleted user or admin '{username}'.")


async def create_user(username: str, settings_dict: Optional[dict] = None) -> Dict:
    if not re.match(r"^[a-zA-Z0-9_-]+$", username):
        raise ValueError(
            "Invalid username. Use only letters, numbers, underscore, or hyphen."
        )
    if username in USER_DATA:
        raise ValueError(f"User '{username}' already exists.")

    # New users are always created active (whether provisioned via SSO or
    # manually in the admin UI); an explicit active=True is stored so the
    # per-user `active` precedence applies from the first login.
    user_settings = dict(DEFAULT_USER_SETTINGS)
    if settings_dict:
        user_settings.update(settings_dict)
    user_settings["active"] = True

    USER_DATA[username] = {
        "username": username,
        "is_admin": False,
        "is_sso": False,
        "settings": user_settings,
        "password_hash": None,
        "sso_groups": [],
        "roles": ["user"],
        "permissions": [],
        "groups": [],
    }
    await _db_save_user_row(
        username, is_admin=False, settings=user_settings, roles=["user"]
    )
    logger.info(f"Created user '{username}'.")
    return get_user(username)


async def ensure_group(group_name: str) -> bool:
    """Create an empty group definition if it doesn't exist yet.

    Groups live in the configuration database; an empty settings dict is a
    valid (settings-less) group. Returns True if it was created, False if it
    already existed (or the name was empty)."""
    if not group_name:
        return False
    if group_name in GROUP_DATA:
        return False
    GROUP_DATA[group_name] = {
        "name": group_name,
        "settings": {},
        "roles": [],
        "permissions": [],
    }
    await _db_save_group(group_name, {}, roles=[], permissions=[])
    logger.info(f"Created group definition '{group_name}'.")
    return True


async def set_user_group(username: str, group_name: str):
    """Associate a Vreckan user (admin or not) with a group.

    The group is added to the user's multi-group membership list (which drives
    permission inheritance) and also recorded as the user's *primary* group in
    ``settings.group`` (which drives quota/settings overrides). Idempotent.
    """
    user = get_user(username)
    if not user:
        return
    settings_dict = dict(user.get("settings") or DEFAULT_USER_SETTINGS.copy())
    groups = list(user.get("groups") or [])
    if settings_dict.get("group", "none") == group_name and group_name in groups:
        return
    await ensure_group(group_name)
    settings_dict["group"] = group_name
    user["settings"] = settings_dict
    if group_name not in groups:
        groups.append(group_name)
    user["groups"] = groups
    await _db_save_user_row(username, settings=settings_dict, groups=groups)
    logger.info(f"Set group '{group_name}' for user '{username}'.")


async def promote_user_to_admin(username: str):
    """Promote an existing user to admin.

    The user's per-user settings are preserved (admins are managed through the
    same settings model as regular users). ``active`` is forced to True unless
    the account was explicitly set to active=False — we never re-activate a
    manually-disabled account. Idempotent: a no-op if already an admin.
    """
    user = get_user(username)
    if not user:
        raise ValueError(f"User '{username}' not found.")
    if user.get("is_admin"):
        return

    # Promotion is now a permission matter: grant the ``admin`` role (which
    # carries the ``admin`` super-permission) and re-derive is_admin from the
    # resulting effective permissions.
    roles = list(user.get("roles") or [])
    if "admin" not in roles:
        roles.append("admin")
    user["roles"] = roles
    # Ensure the promoted admin has a complete settings dict with an explicit
    # active flag. Preserve any existing per-user settings; only force
    # active=True when it was not explicitly disabled.
    current = user.get("settings") or {}
    new_settings = dict(DEFAULT_USER_SETTINGS)
    new_settings.update(current)
    if new_settings.get("active") is not False:
        new_settings["active"] = True
    user["settings"] = new_settings
    await _db_save_user_row(username, settings=new_settings, roles=roles)
    await recompute_is_admin(username)
    logger.info(f"Promoted user '{username}' to admin.")


async def demote_admin_to_user(username: str):
    """Demote an existing admin to a regular user.

    The admin's per-user settings are preserved (users and admins share the
    same settings model). ``active`` is left untouched — demoting never
    re-activates or deactivates an account. Idempotent: a no-op if the
    account is not an admin. The bootstrap 'admin' account is protected: it
    cannot be demoted, since that would lock everyone out of the admin UI.
    """
    user = get_user(username)
    if not user:
        raise ValueError(f"User '{username}' not found.")
    if not user.get("is_admin"):
        return
    if username == await _bootstrap_username():
        raise ValueError(
            f"Admin '{username}' is the bootstrap account and cannot be demoted."
        )

    # Demotion is a permission matter: remove the ``admin`` role and
    # re-derive is_admin. If the account ends up with no roles at all, give
    # it the built-in ``user`` role so it keeps standard user capabilities.
    roles = [r for r in (user.get("roles") or []) if r != "admin"]
    if not roles:
        roles = ["user"]
    user["roles"] = roles
    # Keep the settings dict complete (fill any missing keys with defaults)
    # so the account still has a well-formed settings object after demotion.
    current = user.get("settings") or {}
    new_settings = dict(DEFAULT_USER_SETTINGS)
    new_settings.update(current)
    user["settings"] = new_settings
    await _db_save_user_row(username, settings=new_settings, roles=roles)
    await recompute_is_admin(username)
    logger.info(f"Demoted admin '{username}' to user.")


async def update_user_settings(username: str, new_settings: dict):
    """Update a user's (or admin's) per-user settings.

    Admins are managed through the same roster as regular users, so there is
    no longer a separate code path (or guard) for admin settings.
    """
    user = get_user(username)
    if not user:
        raise ValueError(f"User or admin '{username}' not found.")
    # ``ui_language`` is a personal preference set by the user's own language
    # selector (PUT /api/auth/preferences), not an admin-managed capability.
    # The admin roster form doesn't carry it, so preserve the existing value
    # when a roster edit omits it — otherwise an admin saving the form would
    # silently reset the user's language.
    if "ui_language" not in new_settings:
        old = user.get("settings") or {}
        if old.get("ui_language") is not None:
            new_settings = dict(new_settings)
            new_settings["ui_language"] = old["ui_language"]
    user["settings"] = new_settings
    await _db_save_user_row(username, settings=new_settings)
    logger.info(f"Updated settings for '{username}'.")


async def write_group_file(
    group_name: str,
    settings_dict: dict,
    roles: Optional[list] = None,
    permissions: Optional[list] = None,
):
    """Create or update a group and persist it to the configuration database.

    ``roles`` / ``permissions`` are optional; when provided they replace the
    group's role/permission lists (which drive permission inheritance for
    every member), otherwise the existing values are kept.
    """
    if group_name not in GROUP_DATA:
        GROUP_DATA[group_name] = {
            "name": group_name,
            "settings": {},
            "roles": [],
            "permissions": [],
        }
    GROUP_DATA[group_name]["settings"] = settings_dict
    if roles is not None:
        GROUP_DATA[group_name]["roles"] = list(roles)
    if permissions is not None:
        GROUP_DATA[group_name]["permissions"] = list(permissions)
    await _db_save_group(
        group_name,
        settings_dict,
        roles=GROUP_DATA[group_name].get("roles") or [],
        permissions=GROUP_DATA[group_name].get("permissions") or [],
    )
    logger.info(f"Wrote group '{group_name}'.")


async def delete_group(group_name: str):
    """Delete a group and detach it from every user.

    Deleting a group also removes it from each member's multi-group list and
    clears it as their primary group (so its quota/settings overrides stop
    applying), then re-derives each affected user's is_admin flag.
    """
    if group_name not in GROUP_DATA:
        raise ValueError(f"Group '{group_name}' not found.")
    del GROUP_DATA[group_name]
    await _db_delete_group(group_name)
    for username, user in USER_DATA.items():
        groups = user.get("groups") or []
        settings_dict = user.get("settings") or {}
        changed = False
        if group_name in groups:
            user["groups"] = [g for g in groups if g != group_name]
            changed = True
        if settings_dict.get("group") == group_name:
            settings_dict["group"] = "none"
            user["settings"] = settings_dict
            changed = True
        if changed:
            await _db_save_user_row(
                username, groups=user["groups"], settings=user["settings"]
            )
            await recompute_is_admin(username)
    logger.info(f"Deleted group '{group_name}'.")


def get_home_dirs(username: str) -> List[str]:
    """Lists home directories for a given user."""
    user_storage_path = os.path.join(settings.storage_path, username)
    if not os.path.isdir(user_storage_path):
        return []
    try:
        return sorted(
            [
                d
                for d in os.listdir(user_storage_path)
                if os.path.isdir(os.path.join(user_storage_path, d))
            ]
        )
    except OSError as e:
        logger.error(f"Error listing home directories for {username}: {e}")
        return []


def create_home_dir(username: str, home_name: str):
    """Creates a new home directory for a user."""
    if not re.match(r"^[a-zA-Z0-9_-]+$", home_name):
        raise ValueError(
            "Invalid home directory name. Use only letters, numbers, underscore, or hyphen."
        )

    user_storage_path = os.path.join(settings.storage_path, username)
    new_home_path = os.path.join(user_storage_path, home_name)

    if os.path.exists(new_home_path):
        raise ValueError(
            f"Home directory '{home_name}' already exists for user '{username}'."
        )

    try:
        os.makedirs(new_home_path, exist_ok=True, mode=0o755)
        os.makedirs(
            os.path.join(new_home_path, "Desktop", "files"), exist_ok=True, mode=0o755
        )
        logger.info(f"Created home directory '{home_name}' for user '{username}'.")
    except OSError as e:
        logger.error(f"Failed to create home directory for {username}: {e}")
        raise


def delete_home_dir(username: str, home_name: str):
    """Deletes a home directory for a user."""
    if not re.match(r"^[a-zA-Z0-9_-]+$", home_name):
        raise ValueError("Invalid home directory name.")

    home_path = os.path.join(settings.storage_path, username, home_name)

    if not os.path.isdir(home_path):
        raise ValueError(
            f"Home directory '{home_name}' not found for user '{username}'."
        )

    try:
        shutil.rmtree(home_path)
        logger.info(f"Deleted home directory '{home_name}' for user '{username}'.")
    except OSError as e:
        logger.error(f"Failed to delete home directory for {username}: {e}")
        raise


# ---------------------------------------------------------------------------
# Password-based web login
#
# Web-login passwords are stored as an argon2id hash in the users table
# (password_hash column) and mirrored into the in-memory USER_DATA so that
# login verification never needs a database round-trip. This lets any user or
# admin — including key-only admin accounts that carry no settings — get a web
# password without changing how the extension-based auth works.
# ---------------------------------------------------------------------------

async def set_password(username: str, password: str) -> None:
    """Sets (or overwrites) the web-login password for an existing user/admin."""
    if not _HAS_ARGON2:
        raise RuntimeError(
            "Password authentication is unavailable: the 'argon2-cffi' "
            "dependency is not installed on the server."
        )
    if not re.match(r"^[a-zA-Z0-9_-]+$", username):
        raise ValueError(
            "Invalid username. Use only letters, numbers, underscore, or hyphen."
        )
    if username not in USER_DATA:
        raise ValueError(f"User or admin '{username}' not found.")
    if not password or len(password) < 8:
        raise ValueError("Password must be at least 8 characters long.")

    hashed = _password_hasher.hash(password)
    USER_DATA[username]["password_hash"] = hashed
    await _db_set_password(username, hashed)
    logger.info(f"Set web-login password for '{username}'.")


def has_password(username: str) -> bool:
    """Returns True if a web-login password is set for this user/admin."""
    user = USER_DATA.get(username)
    return bool(user and user.get("password_hash"))


def verify_password(username: str, password: str) -> bool:
    """Returns True if the password matches. False if unset or wrong."""
    if not _HAS_ARGON2:
        return False
    user = USER_DATA.get(username)
    if not user:
        return False
    stored_hash = user.get("password_hash")
    if not stored_hash:
        return False
    try:
        return _password_hasher.verify(stored_hash, password)
    except (VerifyMismatchError, InvalidHashError):
        return False
    except Exception as e:
        logger.error(f"Failed to verify password for '{username}': {e}")
        return False


async def delete_password(username: str) -> None:
    """Removes the web-login password for a user/admin, if present."""
    if username in USER_DATA:
        USER_DATA[username]["password_hash"] = None
    await _db_set_password(username, None)
    logger.info(f"Deleted web-login password for '{username}'.")


async def mark_user_sso(username: str) -> None:
    """Flag a user as SSO/OIDC-provisioned.

    SSO users authenticate through the identity provider, so their password is
    managed externally and the local change-password control is hidden for
    them. Set whenever a user logs in via SSO (covers both auto-provisioned
    accounts and pre-existing accounts that later adopt SSO). Idempotent: a
    no-op if the account is already flagged.
    """
    user = USER_DATA.get(username)
    if not user:
        return
    if user.get("is_sso"):
        return
    user["is_sso"] = True
    await _db_save_user_row(username, is_sso=True)
    logger.info(f"Marked '{username}' as an SSO-provisioned user.")


async def set_sso_groups(username: str, groups: list) -> None:
    """Record the full list of provider (IdP) groups a user belongs to.

    Called on every SSO login with the groups from the id_token, so the admin
    UI can show which SSO groups a user is in (and which of them grant admin).
    Unlike ``set_user_group`` (a single derived primary group), this stores the
    complete list. Idempotent: a no-op if the list is unchanged.
    """
    user = USER_DATA.get(username)
    if not user:
        return
    groups = [str(g) for g in (groups or []) if str(g).strip()]
    if user.get("sso_groups") == groups:
        return
    user["sso_groups"] = groups
    await _db_save_user_row(username, sso_groups=groups)
    logger.info(f"Recorded {len(groups)} SSO group(s) for '{username}'.")


# ---------------------------------------------------------------------------
# Role management (RBAC)
#
# Roles are named bundles of permissions that can be assigned to users and to
# groups. The built-in roles (admin / operator / user) are seeded on first
# run but are fully editable; custom roles can be created and deleted. The
# in-memory ROLE_DATA dictionary is the runtime source of truth; these
# functions keep the database in sync with it.
# ---------------------------------------------------------------------------
async def _db_save_role(name: str, role: Dict) -> None:
    """Insert or update a role row from the in-memory ROLE_DATA entry."""
    async with db.async_session_factory() as session:
        row = (
            await session.execute(select(db.Role).where(db.Role.name == name))
        ).scalar_one_or_none()
        if row is None:
            row = db.Role(name=name)
            session.add(row)
        row.description = role.get("description") or ""
        row.permissions = role.get("permissions") or []
        row.is_builtin = bool(role.get("is_builtin"))
        await session.commit()


async def create_role(name: str, description: str, permissions: list) -> Dict:
    """Create a new (custom) role. The name must be unique and valid."""
    if not re.match(r"^[a-zA-Z0-9_-]+$", name):
        raise ValueError(
            "Invalid role name. Use only letters, numbers, underscore, or hyphen."
        )
    if name in ROLE_DATA:
        raise ValueError(f"Role '{name}' already exists.")
    from .permissions import is_valid_permission

    for p in permissions or []:
        if not is_valid_permission(p):
            raise ValueError(f"Unknown permission '{p}'.")
    ROLE_DATA[name] = {
        "name": name,
        "description": description or "",
        "permissions": list(permissions or []),
        "is_builtin": False,
    }
    await _db_save_role(name, ROLE_DATA[name])
    logger.info(f"Created role '{name}'.")
    return ROLE_DATA[name]


async def update_role(
    name: str,
    description: Optional[str] = None,
    permissions: Optional[list] = None,
) -> Dict:
    """Update a role's description and/or permissions (built-ins included).

    Changing a role's permissions changes the effective permissions of every
    user (directly or via a group) that carries it, so all users' is_admin
    flags are re-derived afterwards (only changed values are persisted).
    """
    role = ROLE_DATA.get(name)
    if not role:
        raise ValueError(f"Role '{name}' not found.")
    from .permissions import is_valid_permission

    if description is not None:
        role["description"] = description
    if permissions is not None:
        for p in permissions:
            if not is_valid_permission(p):
                raise ValueError(f"Unknown permission '{p}'.")
        role["permissions"] = list(permissions)
    await _db_save_role(name, role)
    for username in USER_DATA:
        await recompute_is_admin(username)
    logger.info(f"Updated role '{name}'.")
    return role


async def delete_role(name: str) -> None:
    """Delete a role. Refused for built-in roles, or while any user or group
    still carries it."""
    if name not in ROLE_DATA:
        raise ValueError(f"Role '{name}' not found.")
    if ROLE_DATA[name].get("is_builtin"):
        raise ValueError(
            f"Role '{name}' is a built-in role and cannot be deleted."
        )
    used_by = []
    for username, user in USER_DATA.items():
        if name in (user.get("roles") or []):
            used_by.append(username)
    for group_name, group in GROUP_DATA.items():
        if name in (group.get("roles") or []):
            used_by.append(f"group:{group_name}")
    if used_by:
        raise ValueError(
            f"Role '{name}' is still assigned to: {', '.join(used_by)}. "
            "Remove it from those accounts first."
        )
    del ROLE_DATA[name]
    async with db.async_session_factory() as session:
        await session.execute(delete(db.Role).where(db.Role.name == name))
        await session.commit()
    logger.info(f"Deleted role '{name}'.")


async def set_user_access(
    username: str,
    roles: Optional[list] = None,
    permissions: Optional[list] = None,
    groups: Optional[list] = None,
) -> Dict:
    """Set a user's role/permission/group assignments (partial update).

    Each provided argument replaces the corresponding list; omitted arguments
    are left unchanged. When ``groups`` is provided, its first entry becomes
    the user's primary group (``settings.group``), which drives quota and
    settings overrides. After the update the user's is_admin flag is
    re-derived from the resulting effective permissions.
    """
    user = get_user(username)
    if not user:
        raise ValueError(f"User or admin '{username}' not found.")
    from .permissions import is_valid_permission

    # Validate everything and resolve the resulting roles BEFORE mutating any
    # in-memory state, so a rejected request never leaves the account
    # half-updated (the in-memory dict is the runtime source of truth).
    resulting_roles = list(roles) if roles is not None else list(user.get("roles") or [])
    if roles is not None:
        for r in roles:
            if r not in ROLE_DATA:
                raise ValueError(f"Unknown role '{r}'.")
    if permissions is not None:
        for p in permissions:
            if not is_valid_permission(p):
                raise ValueError(f"Unknown permission '{p}'.")
    # The bootstrap account must always keep the admin role, or everyone
    # would be locked out of the admin UI. Checked pre-mutation (above) so a
    # rejected request cannot strip the role from the live in-memory state.
    if username == await _bootstrap_username() and "admin" not in resulting_roles:
        raise ValueError(f"The root '{username}' account must keep the 'admin' role.")

    if roles is not None:
        user["roles"] = list(roles)
    if permissions is not None:
        user["permissions"] = list(permissions)
    if groups is not None:
        for g in groups:
            if g and g != "none" and g not in GROUP_DATA:
                await ensure_group(g)
        user["groups"] = [g for g in groups if g and g != "none"]
        settings_dict = dict(user.get("settings") or DEFAULT_USER_SETTINGS.copy())
        settings_dict["group"] = user["groups"][0] if user["groups"] else "none"
        user["settings"] = settings_dict
    await _db_save_user_row(
        username,
        roles=user.get("roles"),
        permissions=user.get("permissions"),
        groups=user.get("groups"),
        settings=user.get("settings"),
    )
    await recompute_is_admin(username)
    logger.info(f"Updated access for '{username}'.")
    return user
