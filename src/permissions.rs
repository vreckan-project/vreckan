//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The Vreckan permission catalog and the helpers that resolve them.
//!
//! Ports `server/permissions`. Vreckan's access control is role- and
//! group-based. A *permission* is a single fine-grained capability.
//! Permissions are granted in three places, and a user's effective permission
//! set is the **union** (most-permissive-wins) of all three:
//!
//!   * the permissions assigned directly to the user,
//!   * the permissions of the user's roles,
//!   * the permissions (and roles) of every group the user belongs to.
//!
//! The `admin` permission is a *super-permission*: it expands to *every*
//! permission (the full catalog).

use std::collections::HashSet;

/// Admin permissions gate the individual admin-panel sections.
pub const ADMIN_PERMISSIONS: &[&str] = &[
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
    "admin.certificates",
];

/// User permissions gate the features a user can use in the app.
pub const USER_PERMISSIONS: &[&str] = &[
    "user.storage",
    "user.sharing",
    "user.gpu",
    "user.harden_container",
    "user.harden_openbox",
];

/// The complete set of valid permissions.
pub fn all_permissions() -> Vec<&'static str> {
    let mut v = Vec::with_capacity(ADMIN_PERMISSIONS.len() + USER_PERMISSIONS.len());
    v.extend_from_slice(ADMIN_PERMISSIONS);
    v.extend_from_slice(USER_PERMISSIONS);
    v
}

/// The super-permission. Granting `admin` grants *every* permission.
pub const SUPER_PERMISSION: &str = "admin";

/// True if `perm` is a known permission in the catalog.
pub fn is_valid_permission(perm: &str) -> bool {
    all_permissions().iter().any(|p| *p == perm)
}

/// Expand a permission list into the full set it grants.
///
/// The `admin` super-permission expands to *every* permission. Other known
/// permissions are added as-is; unknown permissions are dropped.
pub fn expand_permissions(perms: &[String]) -> HashSet<String> {
    let mut result = HashSet::new();
    for p in perms {
        if p.is_empty() || !is_valid_permission(p) {
            continue;
        }
        if p == SUPER_PERMISSION {
            for ap in all_permissions() {
                result.insert(ap.to_string());
            }
        } else {
            result.insert(p.clone());
        }
    }
    result
}

/// The union (most-permissive-wins) of several permission lists, expanded.
pub fn union_permissions(sources: &[Vec<String>]) -> HashSet<String> {
    let mut result = HashSet::new();
    for source in sources {
        result.extend(expand_permissions(source));
    }
    result
}
