"""The Vreckan permission catalog and the helpers that resolve them.

Vreckan's access control is role- and group-based. A *permission* is a single
fine-grained capability (``admin.accounts``, ``user.gpu``, ...). Permissions
are granted in three places, and a user's effective permission set is the
**union** (most-permissive-wins) of all three:

  * the permissions assigned directly to the user,
  * the permissions of the user's roles,
  * the permissions (and roles) of every group the user belongs to.

The ``admin`` permission is a *super-permission*: it expands to *every*
permission (the full catalog), so granting it (directly, via a role, or via a
group) makes the user a full-access administrator. ``is_admin`` is therefore
*derived* — a user is an admin exactly when their effective permissions
include ``admin`` — rather than being a separately-stored flag.

The catalog is code-defined (not database-defined) so the set of valid
permissions is fixed and versioned with the application; roles and the
user/group assignments that reference them are what live in the database.
"""
from typing import Iterable, List, Set

# ---------------------------------------------------------------------------
# The permission catalog
# ---------------------------------------------------------------------------
# Admin permissions gate the individual admin-panel sections (and their API
# endpoints). The "admin" super-permission implies all of them.
ADMIN_PERMISSIONS = [
    "admin",
    "admin.accounts",
    "admin.groups",
    "admin.roles",
    "admin.apps",
    "admin.stores",
    "admin.templates",
    "admin.mounts",
    "admin.backup",
    "admin.sessions",
    "admin.laboratory",
    "admin.global_settings",
    "admin.sso",
]

# User permissions gate the features a user can use in the app. These map onto
# the per-user capability flags (see user_manager.get_effective_settings).
USER_PERMISSIONS = [
    "user.storage",
    "user.sharing",
    "user.gpu",
    "user.harden_container",
    "user.harden_openbox",
]

# The complete set of valid permissions.
ALL_PERMISSIONS: List[str] = ADMIN_PERMISSIONS + USER_PERMISSIONS

# The super-permission. Granting ``admin`` grants *every* permission (the full
# admin scope plus all user capabilities), so an admin is a full-access user.
SUPER_PERMISSION = "admin"


def is_valid_permission(perm: str) -> bool:
    """True if ``perm`` is a known permission in the catalog."""
    return perm in ALL_PERMISSIONS


def expand_permissions(perms: Iterable[str]) -> Set[str]:
    """Expand a permission list into the full set it grants.

    The ``admin`` super-permission expands to *every* permission (the whole
    catalog), so a set containing ``admin`` grants full access. Other known
    permissions are added as-is; unknown permissions are dropped.
    """
    result: Set[str] = set()
    for p in perms:
        if not p or p not in ALL_PERMISSIONS:
            continue
        if p == SUPER_PERMISSION:
            result.update(ALL_PERMISSIONS)
        else:
            result.add(p)
    return result


def union_permissions(*sources: Iterable[str]) -> Set[str]:
    """The union (most-permissive-wins) of several permission lists, expanded.

    Each source is a list of permission names (callers expand roles to
    permissions before calling this). The result is the expanded union, so a
    single ``admin`` in any source grants the whole admin scope.
    """
    result: Set[str] = set()
    for source in sources:
        result |= expand_permissions(source)
    return result
