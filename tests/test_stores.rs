//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the admin app-store, installed-app, and meta-app endpoints.
//!
//! Ports `tests/test_stores`. App stores are remote YAML catalogs
//! (`{apps: [...]}`) that admins register, browse, and install from. Catalogs
//! are cached on disk under `app_store_cache_path` and only re-fetched on an
//! explicit refresh, so most of these tests run fully offline: a dead URL plus
//! a pre-seeded cache proves the cache is honoured, and a tiny local HTTP
//! server (`StoreServer`) stands in for a real store when a network
//! round-trip is required.
//!
//! The happy paths (install, update, delete, schema) are already covered by
//! the Tier-1 smoke tests; this file focuses on the error paths and the
//! caching semantics those tests cannot reach. The launch endpoints themselves
//! are out of scope — they spawn real containers.

mod common;

use base64::Engine;
use common::{build_state, SecureClient, TempDir, DEAD_URL};
use http::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Dead local port: any fetch against this URL fails with an instant
/// connection refusal, which keeps the tests hermetic.
const STORE_URL: &str = "http://127.0.0.1:9/apps.yml";

/// A 1x1 transparent PNG, base64 — small enough to inline, valid enough for
/// the icon round-trip assertions.
const PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0CgAAAAASUVORK5CYII=";

/// The Firefox catalog entry (mirrors the Python `STORE_APP`).
fn store_app() -> Value {
    json!({
        "id": "firefox",
        "name": "Firefox",
        "logo": "https://example.invalid/firefox.png",
        "url": "https://example.invalid/firefox",
        "provider": "docker",
        "provider_config": {
            "image": "lscr.io/linuxserver/firefox:latest",
            "port": 3000,
            "nvidia_support": true,
            "dri3_support": true,
            "type": "browser",
            "url_support": true,
            "open_support": false,
            "extensions": [["html", "htm"], "pdf"],
            "autostart": false,
        }
    })
}

/// A throwaway HTTP server serving a fixed app-store YAML payload.
///
/// Mirrors the Python `StoreServer`: a tokio HTTP server on an ephemeral port
/// in a daemon thread, plus a hit counter so tests can prove whether — and how
/// often — the app actually fetched the URL.
struct StoreServer {
    content: Vec<u8>,
    hits: Arc<Mutex<usize>>,
    url: String,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StoreServer {
    fn new(content: &str) -> Self {
        Self {
            content: content.as_bytes().to_vec(),
            hits: Arc::new(Mutex::new(0)),
            url: String::new(),
            shutdown_tx: None,
            handle: None,
        }
    }

    fn start(&mut self) -> &mut Self {
        let content = self.content.clone();
        let hits = self.hits.clone();
        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        self.shutdown_tx = Some(shutdown_tx);
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                let port = listener.local_addr().unwrap().port();
                let _ = port_tx.send(port);
                let mut shutdown_rx = shutdown_rx;
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _)) => {
                                tokio::spawn(handle_store_conn(content.clone(), hits.clone(), stream));
                            }
                            Err(_) => break,
                        },
                    }
                }
            });
        });
        self.url = format!("http://127.0.0.1:{}/apps.yml", port_rx.recv().unwrap());
        self.handle = Some(handle);
        self
    }

    fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    fn hit_count(&self) -> usize {
        *self.hits.lock().unwrap()
    }
}

impl Drop for StoreServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read the request, count the hit, and serve the fixed YAML payload.
async fn handle_store_conn(
    content: Vec<u8>,
    hits: Arc<Mutex<usize>>,
    mut stream: tokio::net::TcpStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Read the request headers (we only need to consume the request; the
    // response is always the same fixed payload).
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    *hits.lock().unwrap() += 1;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-yaml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        content.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&content).await;
    let _ = stream.flush().await;
}

/// The install-app body (mirrors the Python `install_body`).
fn install_body(app_id: &str, name: &str, image: &str) -> Value {
    json!({
        "id": app_id,
        "name": name,
        "logo": "https://example.invalid/firefox.png",
        "url": "https://example.invalid/firefox",
        "source": "Test Store",
        "source_app_id": "firefox",
        "provider": "docker",
        "home_directories": true,
        "users": ["admin"],
        "groups": [],
        "app_template": "Default",
        "provider_config": {
            "image": image,
            "port": 3000,
            "nvidia_support": false,
            "dri3_support": false,
            "type": "browser",
            "url_support": true,
            "open_support": false,
            "extensions": ["html", "pdf"],
        }
    })
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

/// URL-encode a string for use in a query string. The test store names only
/// contain spaces, so a simple space→%20 replacement suffices.
fn quote(s: &str) -> String {
    s.replace(' ', "%20")
}

// ---------------------------------------------------------------------------
// Store CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_list_stores_seeds_default() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc.call("GET", "/api/admin/apps/stores", None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body, Some(json!([{"name": "Vreckan Apps", "url": STORE_URL}])));
}

#[tokio::test]
async fn test_add_store() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call("POST", "/api/admin/apps/stores", Some(&json!({ "name": "Test Store", "url": STORE_URL })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body, Some(json!({ "name": "Test Store", "url": STORE_URL })));
    let (status, stores) = sc.call("GET", "/api/admin/apps/stores", None).await;
    assert_eq!(status, StatusCode::OK);
    let stores = stores.unwrap();
    let names: Vec<&str> = stores
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Test Store"));
    assert!(names.contains(&"Vreckan Apps")); // the seeded default store is kept
}

#[tokio::test]
async fn test_add_store_duplicate() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, _) = sc
        .call("POST", "/api/admin/apps/stores", Some(&json!({ "name": "Test Store", "url": STORE_URL })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/stores",
            Some(&json!({ "name": "Test Store", "url": "https://other.invalid/apps.yml" })),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body.unwrap()["detail"], "App store with name 'Test Store' already exists.");
}

#[tokio::test]
async fn test_add_store_malformed_body() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    // Missing the required 'url' field: the unguarded-parse bug used to
    // surface as a 500; the fix maps it to a 422.
    let (status, body) = sc
        .call("POST", "/api/admin/apps/stores", Some(&json!({ "name": "X" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body"));
}

#[tokio::test]
async fn test_delete_store_unknown() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc.call("DELETE", "/api/admin/apps/stores/Ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "App store not found.");
}

#[tokio::test]
async fn test_delete_store() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, _) = sc
        .call("POST", "/api/admin/apps/stores", Some(&json!({ "name": "Test Store", "url": STORE_URL })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = sc.call("DELETE", "/api/admin/apps/stores/Test%20Store", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, stores) = sc.call("GET", "/api/admin/apps/stores", None).await;
    assert_eq!(status, StatusCode::OK);
    // The custom store is gone; the seeded default store remains.
    let stores = stores.unwrap();
    let names: Vec<&str> = stores
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["Vreckan Apps"]);
}

/// Register a "Test Store" whose cached catalog contains the Firefox entry
/// (mirrors the Python `store_with_firefox` fixture). The store URL is the
/// dead URL, so a fetch would fail — proving the cache is honoured.
async fn seed_cached_firefox_store(state: &vreckan::state::AppState) {
    let cache_dir = &state.settings.app_store_cache_path;
    std::fs::create_dir_all(cache_dir).unwrap();
    let yaml = serde_yaml::to_string(&json!({ "apps": [store_app()] })).unwrap();
    std::fs::write(format!("{cache_dir}/Test Store.yml"), yaml).unwrap();
    state
        .db
        .set_app_setting("VRECKAN_APP_STORE_TEST_STORE", STORE_URL)
        .await
        .ok();
    state
        .app_stores
        .write()
        .await
        .push(vreckan::catalog::AppStore {
            name: "Test Store".to_string(),
            url: STORE_URL.to_string(),
        });
}

// ---------------------------------------------------------------------------
// Available apps: caching and fetch semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_available_apps_refresh_bypasses_cache() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    seed_cached_firefox_store(&state).await;
    let sc = admin_client(state).await;
    // A cached copy exists, but refresh=true must ignore it and hit the
    // (dead) URL.
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=Test%20Store&refresh=true",
        quote(STORE_URL)
    );
    let (status, body) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.unwrap()["detail"].as_str().unwrap().starts_with("Failed to fetch app store from URL"));
}

#[tokio::test]
async fn test_available_apps_fetch_failure() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    // No cache file for this store: the fetch is attempted and fails.
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=No%20Cache",
        quote(STORE_URL)
    );
    let (status, body) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let detail = body.unwrap()["detail"].as_str().unwrap().to_string();
    assert!(detail.starts_with("Failed to fetch app store from URL"));
    assert!(detail.contains(STORE_URL));
}

#[tokio::test]
async fn test_available_apps_invalid_format() {
    let _root = TempDir::new();
    let mut server = StoreServer::new("just a plain string\n");
    server.start();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    // A well-formed HTTP response whose YAML is not a catalog: the
    // fresh-fetch path lets the parser's 500 propagate.
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=Broken%20Store",
        quote(&server.url)
    );
    let (status, body) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body.unwrap()["detail"], "App store YAML has an invalid format.");
}

#[tokio::test]
async fn test_available_apps_yaml_parse_error() {
    let _root = TempDir::new();
    let mut server = StoreServer::new("foo: [unclosed\n");
    server.start();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=Broken%20Store",
        quote(&server.url)
    );
    let (status, body) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.unwrap()["detail"].as_str().unwrap().starts_with("Failed to parse app store YAML:"));
}

#[tokio::test]
async fn test_available_apps_cache_prevents_refetch() {
    let _root = TempDir::new();
    let mut server = StoreServer::new(&serde_yaml::to_string(&json!({ "apps": [store_app()] })).unwrap());
    server.start();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=Local%20Store",
        quote(&server.url)
    );
    let (status, apps) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::OK, "{apps:?}");
    let apps = apps.unwrap();
    let ids: Vec<&str> = apps
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["firefox"]);
    // Second request without refresh must be served from the cache ...
    let (status, _) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::OK);
    // ... so the server was hit exactly once.
    assert_eq!(server.hit_count(), 1);
}

#[tokio::test]
async fn test_available_apps_poisoned_cache_refetches() {
    let _root = TempDir::new();
    let mut server = StoreServer::new("just a plain string\n");
    server.start();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    // A failed parse still writes the (bad) cache file. The cache-hit path
    // swallows the resulting 500 and falls through to a re-fetch, so a
    // poisoned cache is not sticky: the next request goes out to the network
    // again and fails the same way.
    let url = format!(
        "/api/admin/apps/available?url={}&store_name=Poisoned%20Store",
        quote(&server.url)
    );
    let (status, _) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = sc.call("GET", &url, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(server.hit_count(), 2);
}

// ---------------------------------------------------------------------------
// Installed apps: error paths (happy paths live in test_api_smoke)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_install_app_malformed_body() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let mut body = install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest");
    body["provider_config"]["image"] = Value::Null;
    let (status, resp) = sc
        .call("POST", "/api/admin/apps/installed", Some(&body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body"));
}

#[tokio::test]
async fn test_update_installed_app_unknown() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call(
            "PUT",
            "/api/admin/apps/installed/ghost",
            Some(&install_body("ghost", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Installed app not found.");
}

#[tokio::test]
async fn test_update_installed_app_id_mismatch() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // The path ID exists, but the body carries a different ID.
    let (status, resp) = sc
        .call(
            "PUT",
            "/api/admin/apps/installed/app-1",
            Some(&install_body("other", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(resp.unwrap()["detail"], "App ID in path does not match body.");
}

#[tokio::test]
async fn test_update_installed_app_malformed_body() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let mut body = install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest");
    body.as_object_mut().unwrap().remove("provider_config");
    let (status, resp) = sc
        .call("PUT", "/api/admin/apps/installed/app-1", Some(&body))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(resp.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body"));
}

#[tokio::test]
async fn test_delete_installed_app_unknown() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc.call("DELETE", "/api/admin/apps/installed/ghost", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Installed app not found.");
}

#[tokio::test]
async fn test_check_app_update_unknown() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc
        .call("POST", "/api/admin/apps/installed/ghost/check_update", None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Installed app not found.");
}

#[tokio::test]
async fn test_check_app_update_no_remote() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    // The image's registry cannot be resolved (.invalid is reserved for
    // exactly this), so the daemon reports no digest and the endpoint
    // surfaces a 502 instead of a bogus "up to date" answer.
    let image = "registry.invalid/ghost:latest";
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("app-1", "Firefox", image)),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call("POST", "/api/admin/apps/installed/app-1/check_update", None)
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains(image));
}

// ---------------------------------------------------------------------------
// Check-all / update-all (bulk) endpoints
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_check_all_updates() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    // Fake the Docker provider (deterministic digests) and no-op the pull.
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
    )
    .await;
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&install_body("app-2", "Jellyfin", "lscr.io/linuxserver/jellyfin:latest")),
    )
    .await;
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/check_all_updates", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let data = data.unwrap();
    assert_eq!(data["updates_available"], 1);
    let by_id: HashMap<String, Value> = data["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["app_id"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    let keys: std::collections::HashSet<String> = by_id.keys().cloned().collect();
    assert_eq!(keys, std::collections::HashSet::from(["app-1".to_string(), "app-2".to_string()]));
    assert_eq!(by_id["app-1"]["update_available"], false);
    assert_eq!(by_id["app-2"]["update_available"], true);
    assert_eq!(by_id["app-1"]["current_sha"], "local123");
}

#[tokio::test]
async fn test_check_all_updates_empty() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/check_all_updates", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data, Some(json!({ "results": [], "updates_available": 0 })));
}

#[tokio::test]
async fn test_pull_all_latest() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
    )
    .await;
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&install_body("app-2", "Jellyfin", "lscr.io/linuxserver/jellyfin:latest")),
    )
    .await;
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/pull_all_latest", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
    let data = data.unwrap();
    assert_eq!(data["started"], 2);
    let ids: std::collections::HashSet<String> = data["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["app_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, std::collections::HashSet::from(["app-1".to_string(), "app-2".to_string()]));
    assert!(data["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["status"] == "pulling"));
    // Every image was claimed for a background pull.
    let ps = sc.state.pull_status.read().await;
    assert_eq!(ps.get("lscr.io/linuxserver/firefox:latest").map(|s| s.as_str()), Some("queued"));
    assert_eq!(ps.get("lscr.io/linuxserver/jellyfin:latest").map(|s| s.as_str()), Some("queued"));
}

#[tokio::test]
async fn test_pull_all_latest_already_queued() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    sc.call(
        "POST",
        "/api/admin/apps/installed",
        Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
    )
    .await;
    // A pull for this image is already in flight.
    sc.state
        .pull_status
        .write()
        .await
        .insert("lscr.io/linuxserver/firefox:latest".to_string(), "pulling".to_string());
    let (status, data) = sc
        .call("POST", "/api/admin/apps/installed/pull_all_latest", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    assert_eq!(data["started"], 1);
    assert_eq!(data["results"][0]["status"], "pulling");
}

// ---------------------------------------------------------------------------
// Meta apps (per-user variants of an installed app)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_create_meta_app() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, base) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("base-app", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, meta) = sc
        .call(
            "POST",
            "/api/admin/apps/meta",
            Some(&json!({
                "name": "Firefox (admin)",
                "base_app_id": "base-app",
                "logo": PNG_B64,
                "users": ["admin"],
                "groups": [],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{meta:?}");
    let meta = meta.unwrap();
    assert_eq!(meta["is_meta_app"], true);
    assert_eq!(meta["base_app_id"], "base-app");
    assert_eq!(meta["name"], "Firefox (admin)");
    assert_eq!(
        meta["logo"],
        format!("/api/app_icon/{}", meta["id"].as_str().unwrap())
    );
    assert!(meta["home_template_name"].as_str().unwrap().starts_with("meta_"));
    assert_eq!(meta["auto_update"], false);
    assert_eq!(meta["source"], "Firefox");
    // The meta app is a full copy of the base app's provider config.
    assert_eq!(
        meta["provider_config"]["image"],
        base.unwrap()["provider_config"]["image"]
    );
}

#[tokio::test]
async fn test_create_meta_app_missing_base() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call(
            "POST",
            "/api/admin/apps/meta",
            Some(&json!({
                "name": "Orphan",
                "base_app_id": "ghost",
                "logo": PNG_B64,
                "users": ["admin"],
                "groups": [],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Base application with ID 'ghost' not found.");
}

#[tokio::test]
async fn test_create_meta_app_malformed_body() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call("POST", "/api/admin/apps/meta", Some(&json!({ "name": "X" })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().contains("base_app_id"));
}

#[tokio::test]
async fn test_create_meta_app_invalid_logo() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("base-app", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, meta) = sc
        .call(
            "POST",
            "/api/admin/apps/meta",
            Some(&json!({
                "name": "Firefox (admin)",
                "base_app_id": "base-app",
                "logo": "!!!not-base64!!!",
                "users": ["admin"],
                "groups": [],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{meta:?}");
    // Undecodable logos are kept verbatim (treated as a URL).
    assert_eq!(meta.unwrap()["logo"], "!!!not-base64!!!");
}

// ---------------------------------------------------------------------------
// App icons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_app_icon_invalid_id() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc.call("GET", "/api/app_icon/bad..id", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "Invalid application ID.");
}

#[tokio::test]
async fn test_app_icon_missing() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = admin_client(state).await;
    let (status, body) = sc.call("GET", "/api/app_icon/doesnotexist", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body.unwrap()["detail"], "Icon not found.");
}

#[tokio::test]
async fn test_app_icon_serves_saved_icon() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("base-app", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, meta) = sc
        .call(
            "POST",
            "/api/admin/apps/meta",
            Some(&json!({
                "name": "Firefox (admin)",
                "base_app_id": "base-app",
                "logo": PNG_B64,
                "users": ["admin"],
                "groups": [],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let meta_id = meta.unwrap()["id"].as_str().unwrap().to_string();
    let (status, body) = sc.call("GET", &format!("/api/app_icon/{meta_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let body = body.unwrap();
    let icon_b64 = body["icon_data_b64"].as_str().unwrap();
    let icon_bytes = base64::engine::general_purpose::STANDARD.decode(icon_b64).unwrap();
    let png_bytes = base64::engine::general_purpose::STANDARD.decode(PNG_B64).unwrap();
    assert_eq!(icon_bytes, png_bytes);
}

// ---------------------------------------------------------------------------
// Meta-app customization launch (pre-checks only — the success path spawns
// a real container and is out of scope for hermetic tests)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_meta_customize_unknown_app() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call("POST", "/api/admin/launch/meta_customize", Some(&json!({ "application_id": "ghost" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "This endpoint is only for customizing meta applications.");
}

#[tokio::test]
async fn test_launch_meta_customize_non_meta_app() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    sc.state.pull_noop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (status, _) = sc
        .call(
            "POST",
            "/api/admin/apps/installed",
            Some(&install_body("app-1", "Firefox", "lscr.io/linuxserver/firefox:latest")),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = sc
        .call("POST", "/api/admin/launch/meta_customize", Some(&json!({ "application_id": "app-1" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body.unwrap()["detail"], "This endpoint is only for customizing meta applications.");
}

#[tokio::test]
async fn test_launch_meta_customize_malformed_body() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let mut sc = admin_client(state).await;
    let (status, body) = sc
        .call("POST", "/api/admin/launch/meta_customize", Some(&json!({ "wrong_field": 1 })))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body.unwrap()["detail"].as_str().unwrap().starts_with("Invalid request body"));
}
