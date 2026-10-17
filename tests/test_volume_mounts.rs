//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the admin volume-mount endpoints.
//!
//! Ports `tests/test_volume_mounts`. A volume mount binds a directory that
//! exists on the Docker host into the app containers spawned for a single user
//! (`scope=user`) or for every member of a group (`scope=group`);
//! `scope=none` stores the mount but never binds it. All endpoints live on the
//! encrypted admin router, so the tests drive them through the E2EE-capable
//! `SecureClient`. One test additionally verifies that writes reach the
//! configuration database by reading the row back via the entity.

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use sea_orm::{EntityTrait};
use serde_json::json;
use vreckan::entities;

const MOUNT_BODY: &str = r#"{"name": "media", "host_path": "/mnt/media", "container_path": "/media"}"#;

/// Replicates `volume_mount_manager.get_mounts_for_user`: the mounts that
/// apply to `username` — those assigned to the user directly (scope=user) plus
/// those assigned to the user's group (scope=group). scope=none is inactive.
async fn get_mounts_for_user(sc: &SecureClient, username: &str, group: &str) -> Vec<serde_json::Value> {
    let rows = entities::volume_mount::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap();
    let mut result = Vec::new();
    for m in rows {
        let applies = (m.scope == "user" && m.target == username)
            || (m.scope == "group" && !m.target.is_empty() && m.target == group);
        if applies {
            result.push(json!({
                "name": m.name,
                "host_path": m.host_path,
                "container_path": m.container_path,
                "read_only": m.read_only,
                "scope": m.scope,
                "target": m.target,
            }));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Listing / creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_mounts_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("GET", "/api/admin/volume_mounts", None).await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(body, Some(json!([])));
}

#[tokio::test]
async fn test_create_mount_scope_none() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", body);
    // Missing optional fields fall back to their defaults.
    assert_eq!(
        body,
        Some(json!({
            "name": "media",
            "host_path": "/mnt/media",
            "container_path": "/media",
            "read_only": false,
            "scope": "none",
            "target": "",
        }))
    );
    let exists = entities::volume_mount::Entity::find_by_id("media".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .is_some();
    assert!(exists);
}

#[tokio::test]
async fn test_create_mount_duplicate() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "media", "host_path": "/mnt/other", "container_path": "/other" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["detail"], "Volume mount 'media' already exists.");
}

#[tokio::test]
async fn test_create_mount_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "bad name!", "host_path": "/mnt/x", "container_path": "/x" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("Invalid request body"));
}

#[tokio::test]
async fn test_create_mount_invalid_scope() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "everyone" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body.unwrap()["detail"], "scope must be 'none', 'user', or 'group'.");
}

#[tokio::test]
async fn test_create_mount_user_scope_unknown_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "user", "target": "ghost" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User 'ghost' not found.");
}

#[tokio::test]
async fn test_create_mount_group_scope_unknown_group() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "group", "target": "ghost" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Group 'ghost' not found.");
}

#[tokio::test]
async fn test_create_mount_user_scope() {
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
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "user", "target": "alice" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", body);
    assert_eq!(body.as_ref().unwrap()["scope"], "user");
    assert_eq!(body.as_ref().unwrap()["target"], "alice");
    // The mount applies to alice ...
    assert_eq!(get_mounts_for_user(&sc, "alice", "none").await, vec![body.clone().unwrap()]);
    // ... but not to anyone else.
    assert_eq!(get_mounts_for_user(&sc, "bob", "none").await, Vec::<serde_json::Value>::new());
}

#[tokio::test]
async fn test_create_mount_group_scope() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/groups", Some(&json!({ "name": "devs", "settings": {} })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/volume_mounts",
            Some(&json!({ "name": "depot", "host_path": "/mnt/depot", "container_path": "depot", "scope": "group", "target": "devs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{:?}", body);
    assert_eq!(body.as_ref().unwrap()["scope"], "group");
    assert_eq!(body.as_ref().unwrap()["target"], "devs");
    // A member of 'devs' sees the mount; a member of another group does not.
    assert_eq!(get_mounts_for_user(&sc, "alice", "devs").await, vec![body.clone().unwrap()]);
    assert_eq!(get_mounts_for_user(&sc, "alice", "other").await, Vec::<serde_json::Value>::new());
}

// ---------------------------------------------------------------------------
// Updating
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_update_mount() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/volume_mounts/media",
            Some(&json!({ "host_path": "/mnt/new", "container_path": "/media2", "read_only": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", body);
    assert_eq!(body.as_ref().unwrap()["host_path"], "/mnt/new");
    assert_eq!(body.as_ref().unwrap()["container_path"], "/media2");
    assert_eq!(body.as_ref().unwrap()["read_only"], true);
    // Fields omitted from the update keep their previous values.
    assert_eq!(body.as_ref().unwrap()["scope"], "none");
    assert_eq!(body.as_ref().unwrap()["target"], "");
}

#[tokio::test]
async fn test_update_mount_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/volume_mounts/nope",
            Some(&json!({ "host_path": "/mnt/x", "container_path": "/x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Volume mount 'nope' not found.");
}

#[tokio::test]
async fn test_update_mount_invalid_scope() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/volume_mounts/media",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "everyone" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body.unwrap()["detail"], "scope must be 'none', 'user', or 'group'.");
}

#[tokio::test]
async fn test_update_mount_unknown_target() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/volume_mounts/media",
            Some(&json!({ "name": "media", "host_path": "/mnt/media", "container_path": "/media", "scope": "user", "target": "ghost" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "User 'ghost' not found.");
}

// ---------------------------------------------------------------------------
// Deletion / persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_mount() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/volume_mounts/media", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(entities::volume_mount::Entity::find_by_id("media".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .is_none());
    let (status, listed) = sc.call("GET", "/api/admin/volume_mounts", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, Some(json!([])));
}

#[tokio::test]
async fn test_delete_mount_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("DELETE", "/api/admin/volume_mounts/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Volume mount 'nope' not found.");
}

#[tokio::test]
async fn test_mount_persists_to_database() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, _) = sc
        .call("POST", "/api/admin/volume_mounts", Some(&serde_json::from_str(MOUNT_BODY).unwrap()))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // The row is committed and visible from the entity.
    let m = entities::volume_mount::Entity::find_by_id("media".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.name, "media");
    assert_eq!(m.host_path, "/mnt/media");
    assert_eq!(m.container_path, "/media");
    assert_eq!(m.scope, "none");
    assert_eq!(m.target, "");

    // Deleting removes the row as well.
    let (status, _) = sc.call("DELETE", "/api/admin/volume_mounts/media", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let rows = entities::volume_mount::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap();
    assert!(rows.is_empty());
}
