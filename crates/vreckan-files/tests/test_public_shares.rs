//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tier 4 — public shares, audit finding 15 (ANT-886): the share size is
//! capped at creation, the anonymous download routes stream the file from
//! disk instead of buffering it whole, and `launch_from_storage` copies a
//! stored file disk-to-disk instead of reading it into memory.
//!
//! Drives the real axum router (via the `common` harness); the launch test
//! uses the `fake_provider` hook so `launch_common` skips the Docker daemon.

mod common;

use std::io::Write;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::{json, Value};
use vreckan_auth::user_manager;
use vreckan_core::shared::InstalledApp;
use vreckan_core::state::DownloadTokenRecord;
use vreckan_files::files::MAX_PUBLIC_SHARE_BYTES;

/// A SecureClient logged in as the bootstrap admin with a completed E2EE
/// handshake (the bootstrap `admin` permission expands to the full
/// capability catalog, so `public_sharing` and `persistent_storage` are on).
async fn admin_client(root: &std::path::Path) -> SecureClient {
    let mut sc = SecureClient::new(build_state(root).await);
    sc.login("admin", "admin1234").await;
    sc.handshake().await;
    sc
}

/// The default provider config used by the launch test (mirrors the
/// `DEFAULT_PROVIDER_CONFIG` of the catalog launch tests).
const DEFAULT_PROVIDER_CONFIG: &str = r#"{
    "image": "img:latest",
    "port": 3000,
    "nvidia_support": false,
    "dri3_support": true,
    "type": "browser",
    "url_support": true,
    "open_support": false,
    "extensions": [],
    "env": [{"name": "APP_ENV", "value": "1"}],
    "docker_overrides": {"devices": ["/dev/snd"]}
}"#;

/// Seed an installed app into `state.installed_apps` (mirrors `make_app` in
/// the catalog launch tests) so `launch_common` can resolve it.
async fn seed_app(sc: &SecureClient) {
    let data = json!({
        "id": "app-1",
        "name": "Firefox",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": "firefox",
        "provider": "docker",
        "home_directories": true,
        "users": ["all"],
        "groups": [],
        "app_template": "Default",
        "provider_config": serde_json::from_str::<Value>(DEFAULT_PROVIDER_CONFIG).unwrap(),
    });
    let app: InstalledApp = serde_json::from_value(data).unwrap();
    sc.state
        .installed_apps
        .write()
        .await
        .insert(app.id.clone(), app);
}

/// Deterministic byte pattern: byte `i` is `i % 251` (251 prime keeps the
/// pattern unambiguous for truncated or shifted copies).
fn pattern_byte(i: u64) -> u8 {
    (i % 251) as u8
}

/// Write `size` patterned bytes to `path` without holding the file in memory
/// (the at-cap fixture is a full GiB).
fn write_patterned(path: &std::path::Path, size: u64) {
    let mut w = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut written = 0u64;
    let mut chunk = vec![0u8; 4 * 1024 * 1024];
    while written < size {
        let n = std::cmp::min(chunk.len() as u64, size - written);
        for (i, b) in chunk.iter_mut().take(n as usize).enumerate() {
            *b = pattern_byte(written + i as u64);
        }
        w.write_all(&chunk[..n as usize]).unwrap();
        written += n;
    }
    w.flush().unwrap();
}

/// The expected patterned bytes for small fixtures (test-side memory only).
fn expected_pattern(size: usize) -> Vec<u8> {
    (0..size as u64).map(pattern_byte).collect()
}

/// Stream-compare two files byte-for-byte without reading either whole.
fn assert_same_bytes(a: &std::path::Path, b: &std::path::Path) {
    let (mut fa, mut fb) = (std::fs::File::open(a).unwrap(), std::fs::File::open(b).unwrap());
    let mut ba = Vec::new();
    let mut bb = Vec::new();
    loop {
        let (na, nb) = (
            std::io::Read::read(&mut fa, &mut ba).unwrap(),
            std::io::Read::read(&mut fb, &mut bb).unwrap(),
        );
        assert_eq!(na, nb, "files differ in length");
        if na == 0 {
            break;
        }
        assert_eq!(&ba[..na], &bb[..nb], "files differ in content");
    }
}

/// `POST /api/files/share` with `home_dir`/`path` (plus optional extras).
async fn share(sc: &SecureClient, path: &str, extra: Value) -> (StatusCode, Option<Value>) {
    let body = match extra {
        Value::Object(mut m) => {
            m.insert("home_dir".into(), json!("work"));
            m.insert("path".into(), json!(path));
            Value::Object(m)
        }
        other => other,
    };
    sc.call("POST", "/api/files/share", Some(&body)).await
}

// ---------------------------------------------------------------------------
// Share size cap at creation
// ---------------------------------------------------------------------------

/// One byte over the cap is rejected with 413 before any copy happens: no
/// share row is persisted and nothing lands in the public storage area.
#[tokio::test]
async fn test_share_oversized_rejected_413() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let big = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("work")
        .join("big.bin");
    write_patterned(&big, MAX_PUBLIC_SHARE_BYTES + 1);

    let (status, data) = share(&sc, "big.bin", json!({})).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{data:?}");
    let data = data.unwrap();
    let detail = data["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("public shares are limited to"),
        "detail: {detail}"
    );

    // Nothing persisted: no share row, no copied file.
    assert!(
        sc.state.public_shares.read().await.is_empty(),
        "rejected share must not persist a share row"
    );
    let public = std::path::Path::new(&sc.state.settings.public_storage_path);
    assert_eq!(public.read_dir().unwrap().count(), 0);
}

/// A file of exactly the cap is accepted and copied; the persisted metadata
/// carries the real size.
#[tokio::test]
async fn test_share_at_cap_accepted() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let at_cap = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("work")
        .join("at-cap.bin");
    write_patterned(&at_cap, MAX_PUBLIC_SHARE_BYTES);

    let (status, data) = share(&sc, "at-cap.bin", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let data = data.unwrap();
    let share_id = data["share_id"].as_str().unwrap().to_string();
    assert_eq!(data["size_bytes"].as_u64().unwrap(), MAX_PUBLIC_SHARE_BYTES);

    let copied = std::path::Path::new(&sc.state.settings.public_storage_path).join(&share_id);
    assert_eq!(copied.metadata().unwrap().len(), MAX_PUBLIC_SHARE_BYTES);
    assert_same_bytes(&at_cap, &copied);
    let shares = sc.state.public_shares.read().await;
    let meta = shares.get(&share_id).unwrap();
    assert_eq!(meta.size_bytes, MAX_PUBLIC_SHARE_BYTES);
}

// ---------------------------------------------------------------------------
// Anonymous download routes stream from disk
// ---------------------------------------------------------------------------

/// A multi-MiB share downloads byte-for-byte through both anonymous routes,
/// each with its on-disk size advertised as `Content-Length`.
#[tokio::test]
async fn test_share_download_streams_both_routes() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let size = 5 * 1024 * 1024;
    let source = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("work")
        .join("video.bin");
    write_patterned(&source, size as u64);

    let (status, data) = share(&sc, "video.bin", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let share_id = data.unwrap()["share_id"].as_str().unwrap().to_string();
    let expected = expected_pattern(size);
    let len = size.to_string();

    // Route 1: the direct share URL (no password → file straight).
    let resp = sc.http.get(&format!("/public/{share_id}")).await;
    assert_eq!(resp.status, StatusCode::OK, "direct: {:?}", resp.body);
    assert_eq!(resp.header("content-length").as_deref(), Some(len.as_str()));
    assert!(
        resp
            .header("content-disposition")
            .unwrap()
            .contains("filename=\"video.bin\""),
        "disposition: {:?}",
        resp.headers
    );
    assert_eq!(resp.body, expected, "direct share must stream byte-for-byte");

    // Route 2: the one-shot download token (issued by the password flow).
    let token = "ant886-download-token";
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    sc.state.download_tokens.write().await.insert(
        token.to_string(),
        DownloadTokenRecord {
            share_id: share_id.clone(),
            expires_at: now + 3600.0,
        },
    );
    let resp = sc.http.get(&format!("/public/download/{token}")).await;
    assert_eq!(resp.status, StatusCode::OK, "token: {:?}", resp.body);
    assert_eq!(resp.header("content-length").as_deref(), Some(len.as_str()));
    assert_eq!(resp.body, expected, "token download must stream byte-for-byte");
}

/// A share whose file was deleted out from under it answers 404 on the
/// anonymous routes (the stream's open probe fails), not a partial download.
#[tokio::test]
async fn test_share_download_missing_file_404() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let source = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("work")
        .join("gone.bin");
    write_patterned(&source, 4096);

    let (status, data) = share(&sc, "gone.bin", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let share_id = data.unwrap()["share_id"].as_str().unwrap().to_string();
    std::fs::remove_file(
        std::path::Path::new(&sc.state.settings.public_storage_path).join(&share_id),
    )
    .unwrap();

    let resp = sc.http.get(&format!("/public/{share_id}")).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{:?}", resp.body);
}

// ---------------------------------------------------------------------------
// launch_from_storage copies disk-to-disk
// ---------------------------------------------------------------------------

/// Launching a multi-MiB stored file in an app copies it into the
/// shared-files area (the fake provider stands in for the container); the
/// source file is left untouched and the copy is byte-identical.
#[tokio::test]
async fn test_launch_from_storage_copies_stored_file() {
    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    seed_app(&sc).await;
    sc.state
        .fake_provider
        .store(true, std::sync::atomic::Ordering::Relaxed);
    user_manager::create_home_dir(&sc.state.settings, "admin", "work").unwrap();
    let size = 5 * 1024 * 1024;
    let source = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("work")
        .join("doc.pdf");
    write_patterned(&source, size as u64);

    let (status, data) = sc
        .call(
            "POST",
            "/api/files/launch_from_storage",
            Some(&json!({
                "home_dir": "work",
                "path": "doc.pdf",
                "application_id": "app-1",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");

    // The launch actually went through launch_common (the fake provider
    // recorded the container kwargs).
    assert!(
        sc.state.fake_provider_record.lock().unwrap().is_some(),
        "fake provider must have recorded the launch"
    );
    // The file landed in the shared-files area, byte-identical to the source.
    let shared = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("_vreckan_shared_files")
        .join("doc.pdf");
    assert!(shared.is_file(), "launch must copy the file into the shared-files area");
    assert_eq!(
        std::fs::read(&shared).unwrap(),
        expected_pattern(size),
        "shared copy must be byte-identical to the stored file"
    );
    assert_eq!(
        std::fs::read(&source).unwrap(),
        expected_pattern(size),
        "the stored source must be left untouched"
    );
}
