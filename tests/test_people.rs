//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the people (users/admins), groups, and home-directory endpoints.
//!
//! Ports `tests/test_people`. All of these live on the encrypted admin
//! router, so the tests drive them through the E2EE-capable `SecureClient`.
//! Every test starts from the bootstrap `admin` account alone (a fresh
//! database per test via `build_state`).
//!
//! Notable status-code semantics covered here:
//!
//! * `POST /people` maps a validation failure to 409 — so an *invalid*
//!   username is a 409 (the username pattern is enforced in the manager),
//!   while a *missing* username is a 422 (model validation).
//! * `DELETE /people/admin` is a 403 (the root account is protected); every
//!   other unknown-account error is a 404.
//! * Home-directory endpoints are 403 when the account's effective
//!   `persistent_storage` setting is off.

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::{json, Value};
use vreckan::user_manager;

/// Fetch the management roster (`POST /api/admin/data`).
async fn roster(sc: &SecureClient) -> Value {
    let (status, body) = sc.call("POST", "/api/admin/data", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    body.unwrap()
}

// ---------------------------------------------------------------------------
// Creating people
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let user = &body.unwrap()["user"];
    assert_eq!(user["username"], "alice");
    assert_eq!(user["is_admin"], false);
    assert_eq!(user["is_sso"], false);
    assert_eq!(user["has_password"], false);
    // New accounts are always created active, with complete default settings.
    assert_eq!(user["settings"]["active"], true);
    assert_eq!(user["settings"]["persistent_storage"], true);
    // The account is really in the database.
    assert!(user_manager::get_user(&sc.state.db, "alice").await.unwrap().is_some());
}

#[tokio::test]
async fn test_create_admin() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bob", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body.unwrap()["user"]["is_admin"], true);
    assert!(user_manager::get_user(&sc.state.db, "bob").await.unwrap().is_some());
}

#[tokio::test]
async fn test_create_user_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["detail"], "User 'alice' already exists.");
}

#[tokio::test]
async fn test_create_admin_collides_with_user() {
    // The roster is unified: an admin cannot reuse an existing user's name.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "alice", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body.unwrap()["detail"],
        "User or admin 'alice' already exists."
    );
}

#[tokio::test]
async fn test_create_person_invalid_username() {
    // The username pattern is enforced in the manager (-> 409), not in the
    // request model — so a malformed name is a 409, not a 422.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bad name!" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body.unwrap()["detail"],
        "Invalid username. Use only letters, numbers, underscore, or hyphen."
    );
}

#[tokio::test]
async fn test_create_person_missing_username() {
    // A missing username *is* a model-level validation error -> 422.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/people", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

// ---------------------------------------------------------------------------
// Updating people
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_update_user_settings() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Settings are replaced wholesale; missing keys fall back to defaults.
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/people/alice",
            Some(&json!({ "settings": { "active": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.as_ref().unwrap()["username"], "alice");
    assert_eq!(body.as_ref().unwrap()["settings"]["active"], false);
    assert_eq!(body.as_ref().unwrap()["settings"]["gpu"], true); // untouched key keeps its default
    // The roster reflects the change.
    let r = roster(&sc).await;
    let alice = r["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "alice")
        .unwrap();
    assert_eq!(alice["settings"]["active"], false);
}

#[tokio::test]
async fn test_update_user_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/people/ghost",
            Some(&json!({ "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User or admin 'ghost' not found.");
}

#[tokio::test]
async fn test_update_user_missing_settings() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call("PUT", "/api/admin/people/alice", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

// ---------------------------------------------------------------------------
// Deleting people
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/people/alice", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(user_manager::get_user(&sc.state.db, "alice").await.unwrap().is_none());
    let r = roster(&sc).await;
    let all: Vec<&Value> = r["users"]
        .as_array()
        .unwrap()
        .iter()
        .chain(r["admins"].as_array().unwrap().iter())
        .collect();
    assert!(all.iter().all(|u| u["username"] != "alice"));
}

#[tokio::test]
async fn test_delete_root_admin_forbidden() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("DELETE", "/api/admin/people/admin", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body.unwrap()["detail"], "The root 'admin' account cannot be deleted.");
    // ...and it is still there.
    assert!(user_manager::get_user(&sc.state.db, "admin").await.unwrap().is_some());
}

#[tokio::test]
async fn test_delete_unknown_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("DELETE", "/api/admin/people/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User or admin 'ghost' not found.");
}

#[tokio::test]
async fn test_delete_other_admin() {
    // Only the root 'admin' account is protected; other admins are deletable.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bob", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/people/bob", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(user_manager::get_user(&sc.state.db, "bob").await.unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Changing admin status (promote / demote)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_promote_user_to_admin() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/alice/admin-status",
            Some(&json!({ "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["is_admin"], true);
    // The account moved from the users list to the admins list.
    let r = roster(&sc).await;
    assert!(r["users"]
        .as_array()
        .unwrap()
        .iter()
        .all(|u| u["username"] != "alice"));
    assert!(r["admins"]
        .as_array()
        .unwrap()
        .iter()
        .any(|u| u["username"] == "alice"));
}

#[tokio::test]
async fn test_demote_admin_to_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bob", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/bob/admin-status",
            Some(&json!({ "is_admin": false })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap()["is_admin"], false);
    let r = roster(&sc).await;
    assert!(r["admins"]
        .as_array()
        .unwrap()
        .iter()
        .all(|u| u["username"] != "bob"));
    assert!(r["users"]
        .as_array()
        .unwrap()
        .iter()
        .any(|u| u["username"] == "bob"));
}

#[tokio::test]
async fn test_set_admin_status_idempotent() {
    // Promoting an account that is already an admin is a no-op (200, unchanged).
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bob", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/bob/admin-status",
            Some(&json!({ "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.unwrap()["is_admin"], true);
}

#[tokio::test]
async fn test_demote_root_admin_forbidden() {
    // The bootstrap 'admin' account cannot be demoted (would lock everyone out).
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/admin/admin-status",
            Some(&json!({ "is_admin": false })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("cannot be demoted"));
    // ...and it is still an admin.
    assert!(user_manager::get_user(&sc.state.db, "admin")
        .await
        .unwrap()
        .unwrap()
        .is_admin);
}

#[tokio::test]
async fn test_set_admin_status_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/ghost/admin-status",
            Some(&json!({ "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User 'ghost' not found.");
}

#[tokio::test]
async fn test_set_admin_status_missing_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people/alice/admin-status",
            Some(&json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------
// Home directories
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_homedirs_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc.call("GET", "/api/admin/people/alice/homedirs", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.unwrap(), json!({ "home_dirs": [] }));
}

#[tokio::test]
async fn test_list_homedirs_unknown_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("GET", "/api/admin/people/ghost/homedirs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User or admin 'ghost' not found.");
}

#[tokio::test]
async fn test_homedirs_disabled_when_persistent_storage_off() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "carol", "settings": { "persistent_storage": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Capabilities are permission-driven: carol's built-in 'user' role grants
    // user.storage, so strip her roles to drop the capability.
    let fields = user_manager::UserFields {
        roles: Some(vec![]),
        ..Default::default()
    };
    user_manager::db_save_user_row(&sc.state.db.conn, "carol", &fields).await.unwrap();
    let (status, body) = sc.call("GET", "/api/admin/people/carol/homedirs", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body.unwrap()["detail"],
        "Persistent storage is disabled for this account."
    );
}

#[tokio::test]
async fn test_create_home_dir() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/alice/homedirs",
            Some(&json!({ "home_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body.unwrap(), json!({ "status": "success" }));
    // The directory skeleton really exists under the user's storage dir.
    let home = std::path::Path::new(&sc.state.settings.storage_path).join("alice").join("docs");
    assert!(home.join("Desktop").join("files").is_dir());
    // ...and the listing reports it.
    let (status, listed) = sc.call("GET", "/api/admin/people/alice/homedirs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.unwrap(), json!({ "home_dirs": ["docs"] }));
}

#[tokio::test]
async fn test_create_home_dir_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people/alice/homedirs",
            Some(&json!({ "home_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/alice/homedirs",
            Some(&json!({ "home_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body.unwrap()["detail"],
        "Home directory 'docs' already exists for user 'alice'."
    );
}

#[tokio::test]
async fn test_create_home_dir_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // The home-name pattern lives in the model -> 422 (unlike usernames).
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/alice/homedirs",
            Some(&json!({ "home_name": "bad name!" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

#[tokio::test]
async fn test_create_home_dir_unknown_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/people/ghost/homedirs",
            Some(&json!({ "home_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User or admin 'ghost' not found.");
}

#[tokio::test]
async fn test_delete_home_dir() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people/alice/homedirs",
            Some(&json!({ "home_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/people/alice/homedirs/docs", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // The directory is really gone from disk.
    let home = std::path::Path::new(&sc.state.settings.storage_path).join("alice").join("docs");
    assert!(!home.exists());
    let (_status, listed) = sc.call("GET", "/api/admin/people/alice/homedirs", None).await;
    assert_eq!(listed.unwrap(), json!({ "home_dirs": [] }));
}

#[tokio::test]
async fn test_delete_home_dir_missing() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc.call("DELETE", "/api/admin/people/alice/homedirs/docs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body.unwrap()["detail"],
        "Home directory 'docs' not found for user 'alice'."
    );
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_group() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body.unwrap()["name"], "devs");
    assert!(user_manager::get_group(&sc.state.db, "devs").await.unwrap().is_some());
}

#[tokio::test]
async fn test_create_group_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["detail"], "Group 'devs' already exists.");
}

#[tokio::test]
async fn test_create_group_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "bad name!", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

#[tokio::test]
async fn test_update_group() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/groups/devs",
            Some(&json!({ "settings": { "gpu": false } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.as_ref().unwrap()["name"], "devs");
    assert_eq!(body.as_ref().unwrap()["settings"]["gpu"], false);
}

#[tokio::test]
async fn test_update_group_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/groups/ghost",
            Some(&json!({ "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Group 'ghost' not found.");
}

#[tokio::test]
async fn test_delete_group() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/groups/devs", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(user_manager::get_group(&sc.state.db, "devs").await.unwrap().is_none());
}

#[tokio::test]
async fn test_delete_group_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("DELETE", "/api/admin/groups/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Group 'ghost' not found.");
}

// ---------------------------------------------------------------------------
// Management roster
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_roster_reflects_people_and_groups() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "alice" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/people",
            Some(&json!({ "username": "bob", "is_admin": true })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/groups",
            Some(&json!({ "name": "devs", "settings": {} })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let r = roster(&sc).await;
    let admin_names: Vec<&str> = r["admins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    let user_names: Vec<&str> = r["users"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert!(admin_names.contains(&"admin"));
    assert!(admin_names.contains(&"bob"));
    assert_eq!(user_names, vec!["alice"]);
    let group_names: Vec<&str> = r["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(group_names, vec!["devs"]);
    assert_eq!(r["volume_mounts"], json!([]));

    // The bootstrap admin has a local web password; API-created accounts do
    // not (their password_hash is NULL).
    let admin_entry = r["admins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "admin")
        .unwrap();
    assert_eq!(admin_entry["has_password"], true);
    let alice = r["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "alice")
        .unwrap();
    assert_eq!(alice["has_password"], false);
    // Roster settings are always complete (defaults fill any missing keys).
    let alice_keys: std::collections::HashSet<&str> = alice["settings"]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    let default_keys: std::collections::HashSet<&str> = user_manager::default_user_settings()
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(alice_keys, default_keys);
}
