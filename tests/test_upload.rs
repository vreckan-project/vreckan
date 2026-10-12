//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tier 4 — upload flow: `/api/upload/{initiate,chunk,to_storage}`.
//!
//! Ports `tests/test_upload`. Covers the encrypted upload pipeline:
//! session initiation, chunked Base64 uploads, and reassembly into the user's
//! persistent storage. Also pins the upload-id containment rule: ids that
//! resolve outside `settings.upload_dir` (e.g. `..`) must be rejected as
//! unknown sessions rather than read from / written to elsewhere on disk.

mod common;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel};
use serde_json::json;
use vreckan::user_manager;

/// `POST /api/upload/initiate` — returns the new upload id.
async fn initiate(sc: &SecureClient, filename: &str, total_size: i64) -> String {
    let (status, data) = sc
        .call(
            "POST",
            "/api/upload/initiate",
            Some(&json!({ "filename": filename, "total_size": total_size })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    data.unwrap()["upload_id"].as_str().unwrap().to_string()
}

/// `POST /api/upload/chunk` — send one Base64-encoded chunk.
async fn send_chunk(sc: &SecureClient, upload_id: &str, chunk_index: i64, payload: &[u8]) -> (StatusCode, Option<serde_json::Value>) {
    sc.call(
        "POST",
        "/api/upload/chunk",
        Some(&json!({
            "upload_id": upload_id,
            "chunk_index": chunk_index,
            "chunk_data_b64": base64::engine::general_purpose::STANDARD.encode(payload),
        })),
    )
    .await
}

/// `POST /api/upload/to_storage` — reassemble the chunks into the user's
/// persistent storage.
async fn to_storage(
    sc: &SecureClient,
    upload_id: &str,
    home_name: &str,
    filename: &str,
    total_chunks: i64,
) -> (StatusCode, Option<serde_json::Value>) {
    sc.call(
        "POST",
        "/api/upload/to_storage",
        Some(&json!({
            "filename": filename,
            "upload_id": upload_id,
            "total_chunks": total_chunks,
            "home_name": home_name,
        })),
    )
    .await
}

/// Strip the admin's roles and direct permissions so the `user.storage`
/// capability (persistent storage) is lost. Mirrors the Python test's direct
/// `USER_DATA` mutation; here we update the DB row directly, bypassing
/// `set_user_access`'s "bootstrap account must keep the admin role" guard.
async fn strip_persistent_storage(sc: &SecureClient) {
    let model = vreckan::entities::user::Entity::find_by_id("admin".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .unwrap();
    let mut am = model.into_active_model();
    am.roles = sea_orm::ActiveValue::Set(Some(json!([])));
    am.permissions = sea_orm::ActiveValue::Set(Some(json!([])));
    am.update(&sc.state.db.conn).await.unwrap();
}

// ---------------------------------------------------------------------------
// /initiate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_initiate_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/upload/initiate", &json!({ "filename": "a.txt", "total_size": 1 }))
        .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["detail"], "Not authenticated.");
}

#[tokio::test]
async fn test_initiate_success() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let upload_id = initiate(&sc, "report.pdf", 1234).await;
    assert_eq!(upload_id.len(), 36); // uuid4
    let session_dir = std::path::Path::new(&sc.state.settings.upload_dir).join(&upload_id);
    assert!(session_dir.is_dir());
    let metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(session_dir.join("metadata.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["filename"], "report.pdf");
    assert_eq!(metadata["size"], 1234);
    assert!(metadata["started"].is_number());
}

#[tokio::test]
async fn test_initiate_malformed_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call("POST", "/api/upload/initiate", Some(&json!({ "filename": "a.txt" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

// ---------------------------------------------------------------------------
// /chunk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chunk_unknown_upload() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/upload/chunk",
            Some(&json!({ "upload_id": "nope", "chunk_index": 0, "chunk_data_b64": "aGk=" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Upload session not found.");
}

#[tokio::test]
async fn test_chunk_success() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    let (status, data) = send_chunk(&sc, &upload_id, 0, b"hello world").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "status": "ok", "chunk_index": 0 })));
    let written = std::fs::read(
        std::path::Path::new(&sc.state.settings.upload_dir)
            .join(&upload_id)
            .join("chunk_0"),
    )
    .unwrap();
    assert_eq!(written, b"hello world");
}

#[tokio::test]
async fn test_chunk_invalid_base64() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/upload/chunk",
            Some(&json!({
                "upload_id": upload_id,
                "chunk_index": 0,
                "chunk_data_b64": "not-valid-b64!!!",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid Base64 chunk data:"));
}

#[tokio::test]
async fn test_chunk_malformed_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("POST", "/api/upload/chunk", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_chunk_traversal_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let parent = std::path::Path::new(&sc.state.settings.upload_dir).parent().unwrap().to_path_buf();
    for bad in ["..", ".", ""] {
        let (status, data) = sc
            .call(
                "POST",
                "/api/upload/chunk",
                Some(&json!({ "upload_id": bad, "chunk_index": 0, "chunk_data_b64": "aGk=" })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
        assert_eq!(data.unwrap()["detail"], "Upload session not found.");
    }
    // Nothing may have been written outside the upload dir.
    assert!(!parent.join("chunk_0").exists());
}

// ---------------------------------------------------------------------------
// /to_storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_to_storage_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json(
            "/api/upload/to_storage",
            &json!({ "filename": "a.txt", "upload_id": "x", "total_chunks": 1, "home_name": "work" }),
        )
        .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["detail"], "Not authenticated.");
}

#[tokio::test]
async fn test_to_storage_persistent_storage_disabled() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    strip_persistent_storage(&sc).await;
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    let (status, data) = to_storage(&sc, &upload_id, "work", "report.pdf", 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        data,
        Some(json!({ "detail": "Persistent storage is disabled for this account." }))
    );
}

#[tokio::test]
async fn test_to_storage_malformed_body() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = sc.call("POST", "/api/upload/to_storage", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_to_storage_unknown_home() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, data) = to_storage(&sc, "whatever", "nope", "report.pdf", 1).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Home directory 'nope' not found for user.");
}

#[tokio::test]
async fn test_to_storage_unknown_upload() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let (status, data) = to_storage(&sc, "nope", "work", "report.pdf", 1).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Upload session not found.");
}

#[tokio::test]
async fn test_to_storage_missing_chunk() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let upload_id = initiate(&sc, "a.txt", 100).await;
    send_chunk(&sc, &upload_id, 0, b"data").await;
    let (status, data) = to_storage(&sc, &upload_id, "work", "a.txt", 2).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Missing chunk 1 for upload.");
}

#[tokio::test]
async fn test_to_storage_success() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    send_chunk(&sc, &upload_id, 0, b"part one ").await;
    send_chunk(&sc, &upload_id, 1, b"part two").await;
    let (status, data) = to_storage(&sc, &upload_id, "work", "report.pdf", 2).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.as_ref().unwrap()["status"], "success");
    assert_eq!(
        data.as_ref().unwrap()["message"],
        "File 'report.pdf' uploaded successfully."
    );
    let dest = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("_vreckan_shared_files")
        .join("report.pdf");
    assert_eq!(std::fs::read(&dest).unwrap(), b"part one part two");
    // The upload session is consumed and cleaned up.
    assert!(!std::path::Path::new(&sc.state.settings.upload_dir).join(&upload_id).exists());
}

#[tokio::test]
async fn test_to_storage_unique_filename() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let shared = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("_vreckan_shared_files");
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    send_chunk(&sc, &upload_id, 0, b"one").await;
    let (status, _) = to_storage(&sc, &upload_id, "work", "report.pdf", 1).await;
    assert_eq!(status, StatusCode::OK);
    let upload_id = initiate(&sc, "report.pdf", 100).await;
    send_chunk(&sc, &upload_id, 0, b"two").await;
    let (status, _) = to_storage(&sc, &upload_id, "work", "report.pdf", 1).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(std::fs::read(shared.join("report.pdf")).unwrap(), b"one");
    assert_eq!(std::fs::read(shared.join("report-1.pdf")).unwrap(), b"two");
}
