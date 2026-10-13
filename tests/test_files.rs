//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tier 4 — files API.
//!
//! Ports `tests/test_files`. Covers `/api/files`: chunked download, folder
//! creation, asynchronous deletion (+ status polling), directory listing,
//! in-home uploads, public shares (create/list/delete) and launch-from-storage.
//!
//! All tests are Docker-free: launch-from-storage is exercised only up to its
//! pre-launch checks (unknown app / access denied), which raise before any
//! container work.

mod common;

use std::collections::HashSet;
use std::path::Path;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use sea_orm::{ActiveModelTrait, EntityTrait, IntoActiveModel};
use serde_json::{json, Value};
use vreckan::catalog;
use vreckan::files::PublicShareMetadata;
use vreckan::user_manager;

/// Mirrors `api_module.CHUNK_SIZE`.
const CHUNK_SIZE: usize = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Fixtures / helpers
// ---------------------------------------------------------------------------

/// The admin's `default` home dir with a known layout. The skeleton
/// (Desktop/files) comes from `create_home_dir`; the rest is created here so
/// listing tests can assert exact output. Returns the home root path.
fn home(sc: &SecureClient) -> std::path::PathBuf {
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let root = Path::new(&sc.state.settings.storage_path).join("admin").join("default");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(root.join("docs").join("inner.txt"), "inner").unwrap();
    std::fs::write(root.join("a.txt"), "alpha").unwrap();
    std::fs::write(root.join("B.txt"), "bravo").unwrap();
    std::fs::write(root.join("c.txt"), "charlie").unwrap();
    std::fs::write(root.join("d.txt"), "delta").unwrap();
    root
}

/// Update the admin's settings via the admin endpoint (mirrors
/// `set_admin_settings`).
async fn set_admin_settings(sc: &SecureClient, overrides: Value) {
    let (status, data) = sc
        .call("PUT", "/api/admin/people/admin", Some(&json!({ "settings": overrides })))
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
}

/// `POST /api/upload/initiate` — returns the new upload id.
async fn initiate_upload(sc: &SecureClient, filename: &str) -> String {
    let (status, data) = sc
        .call(
            "POST",
            "/api/upload/initiate",
            Some(&json!({ "filename": filename, "total_size": 100 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    data.unwrap()["upload_id"].as_str().unwrap().to_string()
}

/// `POST /api/upload/chunk` — send one Base64-encoded chunk.
async fn send_chunk(sc: &SecureClient, upload_id: &str, chunk_index: i64, payload: &[u8]) {
    let (status, data) = sc
        .call(
            "POST",
            "/api/upload/chunk",
            Some(&json!({
                "upload_id": upload_id,
                "chunk_index": chunk_index,
                "chunk_data_b64": base64::engine::general_purpose::STANDARD.encode(payload),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
}

/// Poll the deletion task until it reaches a terminal state.
async fn wait_for_task(sc: &SecureClient, task_id: &str) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, data) = sc
            .call("GET", &format!("/api/files/delete_status/{task_id}"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{:?}", data);
        let data = data.expect("body");
        if matches!(data["status"].as_str(), Some("completed") | Some("error")) {
            return data;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Deletion task did not finish: {data}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The share ids currently persisted in the `public_shares` table.
async fn share_rows(sc: &SecureClient) -> Vec<String> {
    let rows = vreckan::entities::public_share::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap();
    rows.into_iter().map(|r| r.share_id).collect()
}

/// Strip the admin's roles and direct permissions (bypassing the bootstrap
/// "must keep the admin role" guard) so a capability is lost. Mirrors the
/// Python test's direct `USER_DATA` mutation.
async fn set_admin_roles_perms(sc: &SecureClient, roles: Value, permissions: Value) {
    let model = vreckan::entities::user::Entity::find_by_id("admin".to_string())
        .one(&sc.state.db.conn)
        .await
        .unwrap()
        .unwrap();
    let mut am = model.into_active_model();
    am.roles = sea_orm::ActiveValue::Set(Some(roles));
    am.permissions = sea_orm::ActiveValue::Set(Some(permissions));
    am.update(&sc.state.db.conn).await.unwrap();
}

/// A `SecureClient` logged in as the bootstrap admin with the E2EE handshake
/// performed.
async fn admin_client(state: vreckan::state::AppState) -> SecureClient {
    let mut sc = SecureClient::new(state);
    let resp = sc.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "login failed: {:?}", resp.body);
    sc.handshake().await;
    sc
}

// ---------------------------------------------------------------------------
// /download/chunk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_download_chunk_missing_path() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("GET", "/api/files/download/chunk/default?path=nope.txt&chunk_index=0", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Path not found.");
}

#[tokio::test]
async fn test_download_chunk_path_is_directory() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("GET", "/api/files/download/chunk/default?path=docs&chunk_index=0", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "File not found or is a directory.");
}

#[tokio::test]
async fn test_download_chunk_small_file() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("GET", "/api/files/download/chunk/default?path=a.txt&chunk_index=0", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, b"alpha");
    assert_eq!(data["is_last_chunk"], true);
}

#[tokio::test]
async fn test_download_chunk_multi_chunk() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    let root = home(&sc);
    // Exactly 2 chunks worth of data: the third read is empty and last.
    std::fs::write(root.join("big.bin"), vec![b'x'; 2 * CHUNK_SIZE]).unwrap();
    let (status, c0) = sc
        .call("GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=0", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let c0 = c0.unwrap();
    let d0 = base64::engine::general_purpose::STANDARD
        .decode(c0["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(d0.len(), CHUNK_SIZE);
    assert_eq!(c0["is_last_chunk"], false);

    let (status, c1) = sc
        .call("GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=1", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let c1 = c1.unwrap();
    let d1 = base64::engine::general_purpose::STANDARD
        .decode(c1["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(d1.len(), CHUNK_SIZE);
    assert_eq!(c1["is_last_chunk"], false);

    let (status, c2) = sc
        .call("GET", "/api/files/download/chunk/default?path=big.bin&chunk_index=2", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let c2 = c2.unwrap();
    let d2 = base64::engine::general_purpose::STANDARD
        .decode(c2["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(d2, b"");
    assert_eq!(c2["is_last_chunk"], true);
}

#[tokio::test]
async fn test_download_chunk_negative_index() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, _) = sc
        .call("GET", "/api/files/download/chunk/default?path=a.txt&chunk_index=-1", None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_download_chunk_traversal() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "GET",
            "/api/files/download/chunk/default?path=../../etc&chunk_index=0",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "Directory traversal attempt detected.");
}

// ---------------------------------------------------------------------------
// /create_folder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_folder_malformed_body() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("POST", "/api/files/create_folder/default", Some(&json!({ "path": "/" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_create_folder_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    let root = home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "/", "folder_name": "notes" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.unwrap()["message"], "Folder 'notes' created successfully.");
    assert!(root.join("notes").is_dir());
}

#[tokio::test]
async fn test_create_folder_duplicate() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "/", "folder_name": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(data.unwrap()["detail"], "Folder 'docs' already exists.");
}

#[tokio::test]
async fn test_create_folder_invalid_name() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, _) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "/", "folder_name": "a/b" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_create_folder_parent_missing() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "nope", "folder_name": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Path not found.");
}

#[tokio::test]
async fn test_create_folder_parent_is_file() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "a.txt", "folder_name": "x" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Path is not a valid directory.");
}

// ---------------------------------------------------------------------------
// /delete + /delete_status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_malformed_body() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("POST", "/api/files/delete/default", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_delete_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    let root = home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/delete/default",
            Some(&json!({ "paths": ["a.txt", "docs"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    assert_eq!(data["message"], "Deletion task started.");
    let task = wait_for_task(&sc, data["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "completed");
    assert_eq!(task["message"], "Successfully deleted 2 items.");
    assert!(!root.join("a.txt").exists());
    assert!(!root.join("docs").exists());
}

#[tokio::test]
async fn test_delete_missing_path() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/delete/default",
            Some(&json!({ "paths": ["nope.txt"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let task = wait_for_task(&sc, data.unwrap()["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "error");
    assert_eq!(task["message"], "Path not found.");
}

#[tokio::test]
async fn test_delete_traversal_path() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/delete/default",
            Some(&json!({ "paths": ["../../etc"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let task = wait_for_task(&sc, data.unwrap()["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "error");
    assert_eq!(task["message"], "Directory traversal attempt detected.");
}

#[tokio::test]
async fn test_delete_invalid_home() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    // A home dir name failing the charset check is rejected inside the
    // background task, so the endpoint still reports "task started".
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/delete/bad%20name",
            Some(&json!({ "paths": ["a.txt"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let task = wait_for_task(&sc, data.unwrap()["task_id"].as_str().unwrap()).await;
    assert_eq!(task["status"], "error");
    assert_eq!(task["message"], "Invalid home directory name.");
}

#[tokio::test]
async fn test_delete_status_unknown() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc.call("GET", "/api/files/delete_status/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Task not found.");
}

// ---------------------------------------------------------------------------
// /list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.http.get("/api/files/list/default").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["detail"], "Not authenticated.");
}

#[tokio::test]
async fn test_list_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc.call("GET", "/api/files/list/default", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let names: Vec<String> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    // Directories first, then files, case-insensitive.
    assert_eq!(names, vec!["Desktop", "docs", "a.txt", "B.txt", "c.txt", "d.txt"]);
    let paths: Vec<String> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(paths, vec!["/Desktop", "/docs", "/a.txt", "/B.txt", "/c.txt", "/d.txt"]);
    assert_eq!(data["total"], 6);
    assert_eq!(data["page"], 1);
    assert_eq!(data["per_page"], 50);
    assert_eq!(data["path"], "/");
    let by_name: std::collections::HashMap<&str, &Value> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["name"].as_str().unwrap(), i))
        .collect();
    assert_eq!(by_name["docs"]["is_dir"], true);
    assert_eq!(by_name["a.txt"]["is_dir"], false);
    assert_eq!(by_name["a.txt"]["size"], 5);
    assert!(by_name["a.txt"]["mtime"].is_number());
}

#[tokio::test]
async fn test_list_pagination() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("GET", "/api/files/list/default?per_page=2&page=2", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let names: Vec<String> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["a.txt", "B.txt"]);
    assert_eq!(data["total"], 6);
    assert_eq!(data["page"], 2);
    assert_eq!(data["per_page"], 2);
}

#[tokio::test]
async fn test_list_subdirectory() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc.call("GET", "/api/files/list/default?path=docs", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let names: Vec<String> = data["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["inner.txt"]);
    // The response echoes the raw query parameter (item paths are the
    // normalized "/..." form).
    assert_eq!(data["path"], "docs");
    assert_eq!(data["total"], 1);
}

#[tokio::test]
async fn test_list_path_not_directory() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc.call("GET", "/api/files/list/default?path=a.txt", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Path is not a valid directory.");
}

#[tokio::test]
async fn test_list_missing_path() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc.call("GET", "/api/files/list/default?path=nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Path not found.");
}

// ---------------------------------------------------------------------------
// /upload_to_dir
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_upload_to_dir_malformed_body() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call("POST", "/api/files/upload_to_dir/default", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_upload_to_dir_dest_missing() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "nope",
                "filename": "x.txt",
                "upload_id": upload_id,
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Path not found.");
}

#[tokio::test]
async fn test_upload_to_dir_dest_is_file() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "a.txt",
                "filename": "x.txt",
                "upload_id": upload_id,
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Destination path is not a valid directory.");
}

#[tokio::test]
async fn test_upload_to_dir_unknown_upload() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "docs",
                "filename": "x.txt",
                "upload_id": "nope",
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Upload session not found.");
}

#[tokio::test]
async fn test_upload_to_dir_missing_chunk() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "docs",
                "filename": "x.txt",
                "upload_id": upload_id,
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Missing chunk 0 for upload.");
}

#[tokio::test]
async fn test_upload_to_dir_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    let root = home(&sc);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    send_chunk(&sc, &upload_id, 0, b"part one ").await;
    send_chunk(&sc, &upload_id, 1, b"part two").await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "docs",
                "filename": "report.pdf",
                "upload_id": upload_id,
                "total_chunks": 2,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data.unwrap()["message"], "File uploaded successfully.");
    assert_eq!(
        std::fs::read(root.join("docs").join("report.pdf")).unwrap(),
        b"part one part two"
    );
    // The upload session dir is cleaned up.
    assert!(!Path::new(&sc.state.settings.upload_dir).join(&upload_id).exists());
}

#[tokio::test]
async fn test_upload_to_dir_unique_filename() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    let root = home(&sc);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    send_chunk(&sc, &upload_id, 0, b"one").await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "docs",
                "filename": "report.pdf",
                "upload_id": upload_id,
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let upload_id = initiate_upload(&sc, "report.pdf").await;
    send_chunk(&sc, &upload_id, 0, b"two").await;
    let (status, _) = sc
        .call(
            "POST",
            "/api/files/upload_to_dir/default",
            Some(&json!({
                "path": "docs",
                "filename": "report.pdf",
                "upload_id": upload_id,
                "total_chunks": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(std::fs::read(root.join("docs").join("report.pdf")).unwrap(), b"one");
    assert_eq!(std::fs::read(root.join("docs").join("report-1.pdf")).unwrap(), b"two");
}

// ---------------------------------------------------------------------------
// /share + /shares
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_share_disabled_by_default() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    // Capabilities are permission-driven: drop the admin's roles (which grant
    // user.sharing) but keep user.storage, so the 403 is specifically about
    // public sharing, not about persistent storage.
    set_admin_roles_perms(&sc, json!([]), json!(["user.storage"])).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "Public file sharing is disabled for this account.");
}

#[tokio::test]
async fn test_share_malformed_body() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, data) = sc
        .call("POST", "/api/files/share", Some(&json!({ "home_dir": "default" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_share_path_not_file() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "docs" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Path does not point to a file.");
}

#[tokio::test]
async fn test_share_path_missing() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "nope.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Path not found.");
}

#[tokio::test]
async fn test_share_home_denied() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "other", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "Access to home directory 'other' denied.");
}

#[tokio::test]
async fn test_share_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({
                "home_dir": "default",
                "path": "a.txt",
                "password": "hunter2",
                "expiry_hours": 24,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", info);
    let info = info.unwrap();
    let share_id = info["share_id"].as_str().unwrap().to_string();
    assert_eq!(info["original_filename"], "a.txt");
    assert_eq!(info["size_bytes"], 5);
    assert_eq!(info["has_password"], true);
    assert_eq!(info["url"], format!("/public/{share_id}"));
    let expiry = info["expiry_timestamp"].as_f64().expect("expiry");
    assert!(before + 86400.0 - 5.0 < expiry && expiry < before + 86400.0 + 5.0);
    // The file is copied into the public storage area...
    let public_copy = Path::new(&sc.state.settings.public_storage_path).join(&share_id);
    assert_eq!(std::fs::read(&public_copy).unwrap(), b"alpha");
    // ...and the metadata is persisted to the database.
    assert!(share_rows(&sc).await.contains(&share_id));
}

#[tokio::test]
async fn test_share_no_password() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let info = info.unwrap();
    assert_eq!(info["has_password"], false);
    assert!(info["expiry_timestamp"].is_null());
}

#[tokio::test]
async fn test_shares_list() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, first) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let first_id = first.unwrap()["share_id"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    // A share owned by someone else must never appear in this user's list...
    let mut shares = sc.state.public_shares.write().await;
    shares.insert(
        "ghost-share".to_string(),
        PublicShareMetadata {
            owner_username: "ghost".to_string(),
            original_filename: "g.txt".to_string(),
            created_at: now + 100.0,
            size_bytes: 1,
            password_hash: None,
            expiry_timestamp: None,
        },
    );
    // ...and a newer own share sorts first.
    shares.insert(
        "newer-share".to_string(),
        PublicShareMetadata {
            owner_username: "admin".to_string(),
            original_filename: "n.txt".to_string(),
            created_at: now + 50.0,
            size_bytes: 1,
            password_hash: None,
            expiry_timestamp: None,
        },
    );
    drop(shares);
    let (status, data) = sc.call("GET", "/api/files/shares", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let ids: Vec<String> = data
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["share_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["newer-share".to_string(), first_id.clone()]);
    let entry = &data.as_array().unwrap()[1];
    // No internal fields (password hash, owner) leak into the response.
    let keys: HashSet<String> = entry
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        keys,
        HashSet::from([
            "share_id".to_string(),
            "original_filename".to_string(),
            "size_bytes".to_string(),
            "created_at".to_string(),
            "expiry_timestamp".to_string(),
            "has_password".to_string(),
            "url".to_string(),
        ])
    );
    assert_eq!(entry["url"], format!("/public/{first_id}"));
    assert_eq!(entry["has_password"], false);
}

#[tokio::test]
async fn test_shares_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.http.get("/api/files/shares").await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["detail"], "Not authenticated.");
}

#[tokio::test]
async fn test_delete_share_success() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({ "home_dir": "default", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let share_id = info.unwrap()["share_id"].as_str().unwrap().to_string();
    let (status, _) = sc.call("DELETE", &format!("/api/files/share/{share_id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!sc.state.public_shares.read().await.contains_key(&share_id));
    assert!(!Path::new(&sc.state.settings.public_storage_path).join(&share_id).exists());
    assert!(!share_rows(&sc).await.contains(&share_id));
}

#[tokio::test]
async fn test_delete_share_unknown() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let (status, data) = sc.call("DELETE", "/api/files/share/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Share not found or permission denied.");
}

#[tokio::test]
async fn test_delete_share_other_user() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    set_admin_settings(&sc, json!({ "public_sharing": true })).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let mut shares = sc.state.public_shares.write().await;
    shares.insert(
        "ghost-share".to_string(),
        PublicShareMetadata {
            owner_username: "ghost".to_string(),
            original_filename: "g.txt".to_string(),
            created_at: now,
            size_bytes: 1,
            password_hash: None,
            expiry_timestamp: None,
        },
    );
    drop(shares);
    let (status, data) = sc.call("DELETE", "/api/files/share/ghost-share", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "Share not found or permission denied.");
    // The other user's share is untouched.
    assert!(sc.state.public_shares.read().await.contains_key("ghost-share"));
}

// ---------------------------------------------------------------------------
// /launch_from_storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_from_storage_requires_auth() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json(
            "/api/files/launch_from_storage",
            &json!({ "home_dir": "default", "path": "a.txt", "application_id": "x" }),
        )
        .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    assert_eq!(resp.json()["detail"], "Not authenticated.");
}

#[tokio::test]
async fn test_launch_from_storage_malformed_body() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({ "home_dir": "default", "path": "a.txt" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(data.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body:"));
}

#[tokio::test]
async fn test_launch_from_storage_path_not_file() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({
                "home_dir": "default",
                "path": "docs",
                "application_id": "whatever",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(data.unwrap()["detail"], "Only files can be opened in an application.");
}

#[tokio::test]
async fn test_launch_from_storage_unknown_app() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({
                "home_dir": "default",
                "path": "a.txt",
                "application_id": "ghost-app",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(data.unwrap()["detail"], "Application with ID 'ghost-app' not found.");
}

#[tokio::test]
async fn test_launch_from_storage_app_not_accessible() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    // An app restricted to another user denies even admins (no bypass).
    let app: catalog::InstalledApp = serde_json::from_value(json!({
        "id": "app-restricted",
        "name": "Restricted",
        "logo": "https://example.invalid/restricted.png",
        "url": "https://example.invalid/restricted",
        "source": "Test Store",
        "source_app_id": "restricted",
        "provider": "docker",
        "home_directories": true,
        "users": ["someone-else"],
        "groups": [],
        "provider_config": {
            "image": "lscr.io/linuxserver/firefox:latest",
            "port": 3000,
            "nvidia_support": true,
            "dri3_support": true,
            "type": "browser",
            "url_support": true,
            "open_support": false,
            "extensions": ["html", "pdf"],
            "autostart": false,
        },
    }))
    .unwrap();
    sc.state.installed_apps.write().await.insert(app.id.clone(), app);
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({
                "home_dir": "default",
                "path": "a.txt",
                "application_id": "app-restricted",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "You do not have access to 'Restricted'.");
}

#[tokio::test]
async fn test_launch_from_storage_persistent_storage_off() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    // Capabilities are permission-driven: strip the admin's roles and direct
    // permissions so user.storage (persistent storage) is lost.
    set_admin_roles_perms(&sc, json!([]), json!([])).await;
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({
                "home_dir": "default",
                "path": "a.txt",
                "application_id": "whatever",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(data.unwrap()["detail"], "Persistent storage is disabled for this account.");
}

// ---------------------------------------------------------------------------
// /list pagination bounds (client-reachable panic guard)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_files_list_pagination_bounds_no_panic() {
    let _root = TempDir::new();
    let sc = admin_client(build_state(_root.path()).await).await;
    home(&sc);
    // Negative/zero/out-of-range `page`/`per_page` must be rejected with 400,
    // not panic (the unclamped slice indexing panicked before the fix).
    for qs in ["page=0", "page=-3", "per_page=0", "per_page=-2", "per_page=201"] {
        let (status, data) = sc
            .call("GET", &format!("/api/files/list/default?path=/&{qs}"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{qs}: {data:?}");
    }
    // A large but valid page is an empty page (200), not a panic.
    let (status, data) = sc
        .call("GET", "/api/files/list/default?path=/&page=999999", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    assert!(data.unwrap()["items"].as_array().unwrap().is_empty());
}
