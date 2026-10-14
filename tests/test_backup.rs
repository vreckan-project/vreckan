//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the admin backup / restore endpoints.
//!
//! Ports `tests/test_backup`. A backup is a `tar.gz` of the persistent data
//! root (including a logical JSON dump of the configuration database) written
//! into the backups dir. Restore either re-extracts a stored archive or
//! reassembles one uploaded in base64 chunks, then restores the database and
//! reloads the durable in-memory state (users, groups, app configs,
//! templates, public shares).
//!
//! Every endpoint lives on the encrypted admin router, so the tests drive them
//! through the E2EE-capable `SecureClient`. The `build_state` harness already
//! re-points `data_root` / `backups_path` at the per-test sandbox, so no test
//! can touch the real data directory.

mod common;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use sea_orm::EntityTrait;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use vreckan::backup_manager;
use vreckan::user_manager;

/// Split `data` into at most `n` roughly-equal chunks (mirrors `_split`).
fn split(data: &[u8], n: usize) -> Vec<Vec<u8>> {
    let size = (data.len() + n - 1) / n;
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let end = std::cmp::min(i + size, data.len());
        out.push(data[i..end].to_vec());
        i = end;
    }
    out
}

/// A valid gzip tarball whose single member points outside the data root
/// (mirrors `_malicious_archive`).
fn malicious_archive() -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        {
            let mut builder = tar::Builder::new(&mut gz);
            let mut header = tar::Header::new_gnu();
            // `set_path` rejects `..`; set a valid path first (which also
            // computes a checksum), then overwrite the raw name field with the
            // traversal component and recompute the checksum (mirrors the
            // Python `TarInfo("../evil.txt")`).
            header.set_path("placeholder.txt").unwrap();
            // Zero the name field, then write the traversal path (the
            // placeholder is longer, so leftover bytes would otherwise leak).
            header.as_old_mut().name.fill(0);
            let name = b"../evil.txt";
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_size(5);
            header.set_mtime(0);
            // Recompute the checksum LAST (after every other field is set).
            header.set_cksum();
            builder.append(&header, &b"pwned"[..]).unwrap();
            builder.finish().unwrap();
        }
        gz.finish().unwrap();
    }
    buf
}

/// The on-disk path of a backup archive in the sandboxed backups dir.
fn backup_path(sc: &SecureClient, name: &str) -> std::path::PathBuf {
    std::path::Path::new(&sc.state.settings.backups_path).join(name)
}

// ---------------------------------------------------------------------------
// Listing / creation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_backups_empty() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc.call("GET", "/api/admin/backup", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, Some(json!([])));
}

#[tokio::test]
async fn test_create_and_list_backup() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/backup/create", Some(&json!({ "include_user_data": false })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let name = body.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    // The name matches the expected shape.
    assert!(name.starts_with("backup_") && name.ends_with(".tar.gz"));
    assert!(body.as_ref().unwrap()["size"].as_u64().unwrap() > 0);
    assert_eq!(body.as_ref().unwrap()["include_user_data"], false);
    // The archive really exists on disk in the sandboxed backups dir ...
    assert!(backup_path(&sc, &name).is_file());
    // ... and it is a valid gzip tarball carrying the database dump.
    let raw = std::fs::read(backup_path(&sc, &name)).unwrap();
    let mut gz = flate2::read::GzDecoder::new(std::io::Cursor::new(raw));
    let mut archive = tar::Archive::new(&mut gz);
    let names: Vec<String> = archive.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string()).collect();
    assert!(names.contains(&"db_dump.json".to_string()), "{names:?}");
    // The listing reports it.
    let (status, listed) = sc.call("GET", "/api/admin/backup", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = listed.as_ref().unwrap().as_array().unwrap();
    let names_listed: Vec<String> = arr.iter().map(|b| b["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names_listed, vec![name]);
}

#[tokio::test]
async fn test_create_backup_with_user_data() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/backup/create", Some(&json!({ "include_user_data": true })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let name = body.as_ref().unwrap()["name"].as_str().unwrap();
    assert!(name.contains("_ud"));
    assert_eq!(body.as_ref().unwrap()["include_user_data"], true);
}

#[tokio::test]
async fn test_create_backup_same_second_collision() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Two creations in the same second must not clobber each other: the
    // second gets a numeric suffix.
    let (_, first) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let (_, second) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let first_name = first.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let second_name = second.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    assert_ne!(first_name, second_name);
    let (status, listed) = sc.call("GET", "/api/admin/backup", None).await;
    assert_eq!(status, StatusCode::OK);
    let arr = listed.as_ref().unwrap().as_array().unwrap();
    let names: std::collections::HashSet<String> =
        arr.iter().map(|b| b["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, std::collections::HashSet::from([first_name, second_name]));
}

// ---------------------------------------------------------------------------
// Download (chunked)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_download_backup_reassembles_file() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (_, created) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let name = created.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let on_disk = std::fs::read(backup_path(&sc, &name)).unwrap();
    // The test archive is far smaller than one 2 MiB chunk.
    let (status, chunk0) = sc
        .call("GET", &format!("/api/admin/backup/{name}/download?chunk_index=0"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chunk0.as_ref().unwrap()["filename"], name);
    assert_eq!(chunk0.as_ref().unwrap()["is_last_chunk"], true);
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(chunk0.as_ref().unwrap()["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, on_disk);
    // Asking for the chunk past the end yields an empty final chunk.
    let (status, chunk1) = sc
        .call("GET", &format!("/api/admin/backup/{name}/download?chunk_index=1"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chunk1.as_ref().unwrap()["is_last_chunk"], true);
    let decoded1 = base64::engine::general_purpose::STANDARD
        .decode(chunk1.as_ref().unwrap()["chunk_data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded1, Vec::<u8>::new());
}

#[tokio::test]
async fn test_download_backup_invalid_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("GET", "/api/admin/backup/bogus.tar.gz/download", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("Invalid backup name"));
}

#[tokio::test]
async fn test_download_backup_unknown_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Valid name shape, but no such archive exists.
    let (status, body) = sc
        .call("GET", "/api/admin/backup/backup_20200101_000000.tar.gz/download", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_resolve_backup_rejects_traversal() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    // Unit-level: the name guard must reject anything that could escape the
    // backups dir, independent of HTTP routing / URL normalisation.
    assert!(backup_manager::resolve_backup(&state.settings, "../vreckan.db").is_err());
    assert!(backup_manager::resolve_backup(&state.settings, "/etc/passwd").is_err());
    // Valid shape but missing on disk.
    assert!(backup_manager::resolve_backup(&state.settings, "backup_20200101_000000.tar.gz").is_err());
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_delete_backup() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (_, created) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let name = created.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let (status, _) = sc.call("DELETE", &format!("/api/admin/backup/{name}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!backup_path(&sc, &name).exists());
    let (status, listed) = sc.call("GET", "/api/admin/backup", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed, Some(json!([])));
    // Deleting again is a 404.
    let (status, _) = sc.call("DELETE", &format!("/api/admin/backup/{name}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Restore (stored + chunked upload)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_restore_from_stored_rolls_back_changes() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Snapshot the current state (bootstrap admin only).
    let (_, created) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let name = created.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    // Drift: provision a user after the snapshot was taken.
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "temp-user" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(user_manager::get_user(&sc.state.db, "temp-user").await.unwrap().is_some());
    // Restore the snapshot: the database is rolled back and the in-memory
    // user cache is reloaded from it.
    let (status, body) = sc
        .call("POST", "/api/admin/backup/restore", Some(&json!({ "name": name })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body, Some(json!({ "source": name, "restored": true })));
    assert!(user_manager::get_user(&sc.state.db, "temp-user").await.unwrap().is_none());
    // The pre-snapshot admin is still present.
    assert!(user_manager::get_user(&sc.state.db, "admin").await.unwrap().is_some());
}

#[tokio::test]
async fn test_restore_missing_name() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/backup/restore", Some(&json!({})))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("name"));
}

#[tokio::test]
async fn test_restore_unknown_backup() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/backup/restore",
            Some(&json!({ "name": "backup_20200101_000000.tar.gz" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_restore_uploaded_archive_chunked() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Snapshot, then drift.
    let (_, created) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let name = created.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let archive = std::fs::read(backup_path(&sc, &name)).unwrap();
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "temp-user" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Upload the same archive back through the chunked upload flow.
    let (status, initiated) = sc
        .call("POST", "/api/admin/backup/restore/initiate", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let upload_id = initiated.as_ref().unwrap()["upload_id"].as_str().unwrap().to_string();
    let chunks = split(&archive, 2);
    assert_eq!(chunks.len(), 2);
    for (i, chunk) in chunks.iter().enumerate() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
        let (status, body) = sc
            .call(
                "POST",
                "/api/admin/backup/restore/chunk",
                Some(&json!({
                    "upload_id": upload_id,
                    "chunk_index": i,
                    "chunk_data_b64": b64,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body:?}");
        assert_eq!(body.as_ref().unwrap()["chunk_index"], i as u64);
    }
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/finalize",
            Some(&json!({ "upload_id": upload_id, "total_chunks": 2 })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body, Some(json!({ "source": "upload", "restored": true })));
    // The restore rolled the drift back again.
    assert!(user_manager::get_user(&sc.state.db, "temp-user").await.unwrap().is_none());
    // The temporary upload dir is cleaned up.
    let upload_dir = std::path::Path::new(&sc.state.settings.backups_path).join(format!("restore_{upload_id}"));
    assert!(!upload_dir.is_dir());
}

#[tokio::test]
async fn test_restore_chunk_unknown_session() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"x");
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/chunk",
            Some(&json!({
                "upload_id": "no-such-session",
                "chunk_index": 0,
                "chunk_data_b64": b64,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("not found"));
}

#[tokio::test]
async fn test_restore_finalize_missing_chunk() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, initiated) = sc
        .call("POST", "/api/admin/backup/restore/initiate", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let upload_id = initiated.as_ref().unwrap()["upload_id"].as_str().unwrap().to_string();
    // Upload only the first of the two promised chunks.
    let b64 = base64::engine::general_purpose::STANDARD.encode(b"partial");
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/chunk",
            Some(&json!({
                "upload_id": upload_id,
                "chunk_index": 0,
                "chunk_data_b64": b64,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/finalize",
            Some(&json!({ "upload_id": upload_id, "total_chunks": 2 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("Missing chunk 1"));
    // The half-finished upload dir is cleaned up even on failure.
    let upload_dir = std::path::Path::new(&sc.state.settings.backups_path).join(format!("restore_{upload_id}"));
    assert!(!upload_dir.is_dir());
}

#[tokio::test]
async fn test_restore_rejects_path_traversal_archive() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, initiated) = sc
        .call("POST", "/api/admin/backup/restore/initiate", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let upload_id = initiated.as_ref().unwrap()["upload_id"].as_str().unwrap().to_string();
    let b64 = base64::engine::general_purpose::STANDARD.encode(malicious_archive());
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/chunk",
            Some(&json!({
                "upload_id": upload_id,
                "chunk_index": 0,
                "chunk_data_b64": b64,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/backup/restore/finalize",
            Some(&json!({ "upload_id": upload_id, "total_chunks": 1 })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("traversal"));
    // Nothing was written outside the sandboxed data root.
    let evil = std::path::Path::new(_root.path()).join("evil.txt");
    assert!(!evil.exists());
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_backup_retention_prunes_old_backups() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Enable retention (keep only the single newest archive) and rebuild the
    // router so the handler's state clone sees the new setting.
    sc.state.settings.backup_retention = 1;
    let cookie = sc.http.auth_cookie.clone();
    sc.http = common::TestClient::new(sc.state.clone());
    sc.http.auth_cookie = cookie;
    let (_, first) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (_, second) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (_, third) = sc.call("POST", "/api/admin/backup/create", Some(&json!({}))).await;
    let first_name = first.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let second_name = second.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let third_name = third.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    assert!(first_name < second_name && second_name < third_name);
    let (status, listed) = sc.call("GET", "/api/admin/backup", None).await;
    assert_eq!(status, StatusCode::OK);
    // Retention keeps only the single newest archive.
    let arr = listed.as_ref().unwrap().as_array().unwrap();
    let names: Vec<String> = arr.iter().map(|b| b["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec![third_name.clone()]);
    assert!(backup_path(&sc, &third_name).exists());
    assert!(!backup_path(&sc, &first_name).exists());
}

// ---------------------------------------------------------------------------
// P2-4: backup secrets handling
// ---------------------------------------------------------------------------

/// Extract the `db_dump.json` member of a backup archive and parse it.
fn read_dump_from_archive(raw: &[u8]) -> serde_json::Value {
    let mut gz = flate2::read::GzDecoder::new(std::io::Cursor::new(raw));
    let mut archive = tar::Archive::new(&mut gz);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().file_name().unwrap() == "db_dump.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).unwrap();
            return serde_json::from_slice(&buf).unwrap();
        }
    }
    panic!("db_dump.json not in archive");
}

#[tokio::test]
async fn test_backup_archive_is_0600() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/backup/create", Some(&json!({ "include_user_data": false })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let name = body.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let mode = std::fs::metadata(backup_path(&sc, &name))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn test_backup_dump_excludes_auth_tokens() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // A live login leaves an auth_tokens row in the configuration DB.
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/backup/create", Some(&json!({ "include_user_data": false })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let name = body.as_ref().unwrap()["name"].as_str().unwrap().to_string();
    let dump = read_dump_from_archive(&std::fs::read(backup_path(&sc, &name)).unwrap());
    let tables = dump["tables"].as_object().expect("tables object");
    // Live session tokens are excluded from the snapshot ...
    assert!(!tables.contains_key("auth_tokens"), "{tables:?}");
    // ... while the rest of the schema (including users) is preserved.
    assert!(tables.contains_key("users"));
    // The in-archive dump itself is owner-only too.
    let mut gz = flate2::read::GzDecoder::new(std::io::Cursor::new(
        std::fs::read(backup_path(&sc, &name)).unwrap(),
    ));
    let mut archive = tar::Archive::new(&mut gz);
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        if entry.path().unwrap().file_name().unwrap() == "db_dump.json" {
            let mode = entry.header().mode().map(|m| m & 0o777).unwrap_or(0);
            assert_eq!(mode, 0o600);
        }
    }
}

#[tokio::test]
async fn test_db_dump_restore_preserves_live_auth_tokens() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    // The login persisted an auth_tokens row.
    let token_rows =
        vreckan::entities::auth_token::Entity::find()
            .all(&sc.state.db.conn)
            .await
            .unwrap();
    assert!(!token_rows.is_empty());
    // Round-trip a dump: the new-style dump excludes auth_tokens, and the
    // restore must leave the live token rows untouched.
    let dump_path = sc.state.settings.data_root.clone() + "/dump_roundtrip.json";
    sc.state.db.dump(&dump_path).await.unwrap();
    let before = vreckan::entities::auth_token::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap()
        .len();
    sc.state.db.restore(&dump_path).await.unwrap();
    let after = vreckan::entities::auth_token::Entity::find()
        .all(&sc.state.db.conn)
        .await
        .unwrap();
    assert_eq!(after.len(), before);
    assert_eq!(
        after.iter().map(|r| r.token.clone()).collect::<Vec<_>>(),
        token_rows.iter().map(|r| r.token.clone()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_file(&dump_path);
}
