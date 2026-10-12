//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Pinned behaviours (/api/pinned): per-user saved launch-option presets.
//! Rows live in the pinned_behaviors table (no in-memory cache), so "other
//! user" rows can be seeded with a direct insert.
//!
//! Port of `tests/test_pinned`.

mod common;

use common::{build_state, SecureClient, TempDir};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use vreckan::entities;

const PINNED_BODY: &str = r#"{"name": "Work Firefox", "application_id": "app-1"}"#;

/// Insert a pinned_behaviors row directly (e.g. for another user).
async fn seed_pinned_row(
    state: &vreckan::state::AppState,
    username: &str,
    pin_id: &str,
    created_at: f64,
    name: &str,
) {
    let data = json!({ "name": name, "application_id": "app-1" });
    let am = entities::pinned_behavior::ActiveModel {
        id: sea_orm::ActiveValue::Set(pin_id.to_string()),
        username: sea_orm::ActiveValue::Set(username.to_string()),
        created_at: sea_orm::ActiveValue::Set(created_at),
        data: sea_orm::ActiveValue::Set(data),
    };
    entities::pinned_behavior::Entity::insert(am)
        .exec(&state.db.conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_pinned_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.get("/api/pinned").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_pinned_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("GET", "/api/pinned", None).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(data, Some(json!([])));
}

#[tokio::test]
async fn test_create_pinned() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/pinned", Some(&serde_json::from_str(PINNED_BODY).unwrap()))
        .await;
    assert_eq!(status.as_u16(), 201);
    let data = data.unwrap();
    assert_eq!(data["name"], "Work Firefox");
    assert_eq!(data["application_id"], "app-1");
    assert!(data["id"].is_string());
    // Model defaults are applied.
    assert_eq!(data["wayland_mode"], true);
    assert_eq!(data["trigger_type"], "manual");
    assert_eq!(data["is_default"], false);
    let (status, listing) = sc.call("GET", "/api/pinned", None).await;
    assert_eq!(status.as_u16(), 200);
    let listing = listing.unwrap();
    let ids: Vec<String> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec![data["id"].as_str().unwrap().to_string()]);
}

#[tokio::test]
async fn test_create_pinned_malformed() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/pinned", Some(&json!({ "name": "" })))
        .await;
    assert_eq!(status.as_u16(), 422);
    let detail = data
        .and_then(|d| d.get("detail").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(detail.contains("Invalid request body"));
}

#[tokio::test]
async fn test_create_pinned_default_clears_previous() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    sc.call(
        "POST",
        "/api/pinned",
        Some(&json!({ "name": "A", "application_id": "app-1", "is_default": true })),
    )
    .await;
    sc.call(
        "POST",
        "/api/pinned",
        Some(&json!({ "name": "B", "application_id": "app-1", "is_default": true })),
    )
    .await;
    let (status, listing) = sc.call("GET", "/api/pinned", None).await;
    assert_eq!(status.as_u16(), 200);
    let listing = listing.unwrap();
    let by_name: std::collections::HashMap<&str, &serde_json::Value> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap(), p))
        .collect();
    assert_eq!(by_name["A"]["is_default"], false);
    assert_eq!(by_name["B"]["is_default"], true);
}

#[tokio::test]
async fn test_set_default_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("POST", "/api/pinned/nope/set-default", None).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(data, Some(json!({ "detail": "Pinned behaviour not found." })));
}

#[tokio::test]
async fn test_set_default() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let a = sc
        .call(
            "POST",
            "/api/pinned",
            Some(&json!({ "name": "A", "application_id": "app-1", "is_default": true })),
        )
        .await
        .1
        .unwrap();
    let b = sc
        .call("POST", "/api/pinned", Some(&json!({ "name": "B", "application_id": "app-1" })))
        .await
        .1
        .unwrap();
    let (status, data) = sc
        .call("POST", &format!("/api/pinned/{}/set-default", b["id"].as_str().unwrap()), None)
        .await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(data, Some(json!({ "ok": true })));
    let (status, listing) = sc.call("GET", "/api/pinned", None).await;
    let listing = listing.unwrap();
    let by_id: std::collections::HashMap<&str, &serde_json::Value> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    assert_eq!(by_id[a["id"].as_str().unwrap()]["is_default"], false);
    assert_eq!(by_id[b["id"].as_str().unwrap()]["is_default"], true);
}

#[tokio::test]
async fn test_delete_pinned_unknown() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("DELETE", "/api/pinned/nope", None).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(data, Some(json!({ "detail": "Pinned behaviour not found." })));
}

#[tokio::test]
async fn test_delete_pinned_other_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_pinned_row(&sc.state, "ghost", "pin-ghost", 1234.0, "Ghost Pin").await;
    let (status, data) = sc.call("DELETE", "/api/pinned/pin-ghost", None).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(data, Some(json!({ "detail": "Pinned behaviour not found." })));
    // The other user's row is untouched.
    let count = entities::pinned_behavior::Entity::find()
        .filter(entities::pinned_behavior::Column::Id.eq("pin-ghost"))
        .all(&sc.state.db.conn)
        .await
        .unwrap()
        .len();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_delete_pinned() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let created = sc
        .call("POST", "/api/pinned", Some(&serde_json::from_str(PINNED_BODY).unwrap()))
        .await
        .1
        .unwrap();
    eprintln!("CREATED id: {:?}", created["id"]);
    let rows = entities::pinned_behavior::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap();
    for r in &rows {
        eprintln!("DB row id: {:?}", r.id);
    }
    let (status, _) = sc
        .call("DELETE", &format!("/api/pinned/{}", created["id"].as_str().unwrap()), None)
        .await;
    assert_eq!(status.as_u16(), 204);
    let (status, listing) = sc.call("GET", "/api/pinned", None).await;
    assert_eq!(listing, Some(json!([])));
}

#[tokio::test]
async fn test_pinned_list_newest_first() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_pinned_row(&sc.state, "admin", "pin-old", 1000.0, "Old").await;
    seed_pinned_row(&sc.state, "admin", "pin-new", 2000.0, "New").await;
    let (status, listing) = sc.call("GET", "/api/pinned", None).await;
    assert_eq!(status.as_u16(), 200);
    let listing = listing.unwrap();
    let ids: Vec<&str> = listing
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["pin-new", "pin-old"]);
    assert_eq!(listing.as_array().unwrap()[0]["created_at"], 2000.0);
}
