//! Shared test harness for the Vreckan Rust integration test suite.
//!
//! Ports the fixtures in `tests/conftest.py`. The server reads every
//! `VRECKAN_*` environment variable at startup and builds its database at
//! startup, so each test gets a throwaway directory tree and a fresh SQLite
//! database. The `TestClient` drives the real axum router directly (via
//! `tower::ServiceExt::oneshot`, no real socket), and the `SecureClient`
//! additionally logs in as the bootstrap admin and performs the E2EE
//! handshake, mirroring the browser client's crypto.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use http::header;
use http::{Request, StatusCode};
use pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey, sha2::Sha256};
use serde_json::Value;
use tower::ServiceExt;

use vreckan::app;
use vreckan::db;
use vreckan::settings::Settings;
use vreckan::state::AppState;
use vreckan::user_manager;

/// A dead local port: any store URL pointed here fails instantly with a
/// connection refusal instead of hanging on a real network fetch.
pub const DEAD_URL: &str = "http://127.0.0.1:9/";

/// Build a `Settings` pointed at a throwaway directory tree, mirroring the
/// `isolate` fixture's per-test sandbox.
pub fn test_settings(root: &std::path::Path) -> Settings {
    let mut s = Settings::from_env();
    s.data_root = root.join("data").to_string_lossy().to_string();
    s.backups_path = format!("{}/backups", s.data_root);
    s.storage_path = format!("{}/storage", s.data_root);
    s.upload_dir = format!("{}/storage/uploads", s.data_root);
    s.app_icons_path = format!("{}/storage/icons", s.data_root);
    s.home_templates_path = format!("{}/storage/home_templates", s.data_root);
    s.public_storage_path = format!("{}/storage/public", s.data_root);
    s.autostart_cache_path = format!("{}/config/autostart_cache", s.data_root);
    s.app_store_cache_path = format!("{}/config/app_stores_cache", s.data_root);
    s.server_private_key_path = format!("{}/ssl/server_key.pem", s.data_root);
    s.app_resource_path = format!("{DEAD_URL}apps.yml");
    // Point at the real bundled template schema so the template editor routes
    // serve the full baseline (mirrors the production layout). The path is
    // relative to the workdir, which is the repo root in a local checkout and
    // /app (with server/ copied in) in the CI test image; CI may override the
    // location with VRECKAN_TEST_TEMPLATE_SCHEMA_PATH.
    s.template_schema_path = std::env::var("VRECKAN_TEST_TEMPLATE_SCHEMA_PATH")
        .unwrap_or_else(|_| "server/template_schema.yml".to_string());
    s.database_url = format!("sqlite://{}?mode=rwc", root.join("test.db").to_string_lossy());
    s.oidc_enabled = false;
    s.oidc_admin_groups = String::new();
    s.oidc_user_groups = String::new();
    s.auto_update_apps = false;
    s.le_enabled = false;
    s
}

/// Connect to (and initialise) a fresh SQLite database under `root`.
pub async fn connect_db(root: &std::path::Path) -> vreckan::db::Db {
    let url = format!("sqlite://{}?mode=rwc", root.join("test.db").to_string_lossy());
    let d = db::connect(&url).await.expect("connect db");
    d.init().await.expect("init db");
    d
}

/// Build a fresh `AppState` for a test: throwaway dirs, fresh DB, bootstrap
/// admin + roles seeded (mirrors the Python lifespan startup).
pub async fn build_state(root: &std::path::Path) -> AppState {
    let settings = test_settings(root);
    // Mirror the lifespan startup: create the persistent data directories
    // (storage, public storage, uploads, icons, home templates, ...).
    for p in [
        &settings.storage_path,
        &settings.public_storage_path,
        &settings.upload_dir,
        &settings.app_icons_path,
        &settings.home_templates_path,
        &settings.app_store_cache_path,
        &settings.autostart_cache_path,
        &settings.backups_path,
    ] {
        std::fs::create_dir_all(p).ok();
    }
    let db = connect_db(root).await;
    let state = AppState::new(settings, db);
    // Mirror the lifespan startup: bootstrap admin + roles, then load the
    // durable stores.
    user_manager::load_users_and_groups(&state.db, &state.settings)
        .await
        .expect("load users/groups");
    // Initialise the E2EE server key (a process-wide OnceLock, so only the
    // first call actually loads/generates it).
    vreckan::e2ee::init_server_key(&state.settings).expect("init server key");
    // Seed the template schema from the bundled YAML baseline (mirrors the
    // lifespan startup).
    vreckan::catalog::load_template_schema(&state).await;
    // Load application templates (built-in YAML + DB overrides; seeds a blank
    // "Default" template when none exist). Mirrors the lifespan startup.
    vreckan::catalog::load_app_templates(&state).await;
    // Load the app stores (seeds the default "Vreckan Apps" store on first
    // run). Mirrors the lifespan startup.
    vreckan::catalog::load_app_stores(&state).await;
    state
}

/// The axum router for a test (the real app router, state-bound).
pub fn router(state: AppState) -> axum::Router {
    app::router(state)
}

/// A raw HTTP response from the router.
pub struct RawResponse {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: Vec<u8>,
}

impl RawResponse {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }
}

/// A test client that drives the real axum router directly.
pub struct TestClient {
    router: axum::Router,
    /// The auth cookie value (set by login), sent on subsequent requests.
    pub auth_cookie: Option<String>,
}

impl TestClient {
    pub fn new(state: AppState) -> Self {
        Self {
            router: router(state),
            auth_cookie: None,
        }
    }

    /// Perform a request against the router. `body` is the raw request body
    /// (already encoded); `content_type` sets the Content-Type header.
    pub async fn request(
        &self,
        method: &str,
        url: &str,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
        extra_headers: &HashMap<String, String>,
    ) -> RawResponse {
        let mut builder = Request::builder().method(method).uri(url);
        if let Some(ct) = content_type {
            builder = builder.header(header::CONTENT_TYPE, ct);
        }
        if let Some(cookie) = &self.auth_cookie {
            builder = builder.header(header::COOKIE, format!("{}={cookie}", "vreckan_auth"));
        }
        for (k, v) in extra_headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let body_bytes = body.unwrap_or_default();
        let req = builder
            .body(http_body_util::Full::new(body_bytes.into()))
            .expect("build request");
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router call");
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .expect("collect body")
            .to_bytes();
        RawResponse {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    pub async fn get(&self, url: &str) -> RawResponse {
        self.request("GET", url, None, None, &HashMap::new()).await
    }

    /// GET with extra headers (e.g. an explicit `Authorization` header).
    pub async fn get_with(&self, url: &str, extra_headers: &HashMap<String, String>) -> RawResponse {
        self.request("GET", url, None, None, extra_headers).await
    }

    pub async fn post_json(&self, url: &str, body: &Value) -> RawResponse {
        self.request(
            "POST",
            url,
            Some(body.to_string().into_bytes()),
            Some("application/json"),
            &HashMap::new(),
        )
        .await
    }

    pub async fn put_json(&self, url: &str, body: &Value) -> RawResponse {
        self.request(
            "PUT",
            url,
            Some(body.to_string().into_bytes()),
            Some("application/json"),
            &HashMap::new(),
        )
        .await
    }

    pub async fn delete(&self, url: &str) -> RawResponse {
        self.request("DELETE", url, None, None, &HashMap::new()).await
    }

    /// Log in as `username`/`password`; captures the auth cookie. Returns the
    /// raw response.
    pub async fn login(&mut self, username: &str, password: &str) -> RawResponse {
        let resp = self
            .post_json(
                "/api/auth/login",
                &serde_json::json!({ "username": username, "password": password }),
            )
            .await;
        if let Some(set_cookie) = resp.header("set-cookie") {
            // Extract the cookie value (name=value; ...).
            let name = format!("{}=", "vreckan_auth");
            if let Some(pos) = set_cookie.find(&name) {
                let value = &set_cookie[pos + name.len()..];
                let value = value.split(';').next().unwrap_or("").trim();
                self.auth_cookie = Some(value.to_string());
            }
        }
        resp
    }
}

/// A logged-in, E2EE-capable test client (mirrors the browser client).
pub struct SecureClient {
    pub http: TestClient,
    pub state: AppState,
    pub session_id: Option<String>,
    aes_key: Option<[u8; 32]>,
}

impl SecureClient {
    pub fn new(state: AppState) -> Self {
        Self {
            http: TestClient::new(state.clone()),
            state,
            session_id: None,
            aes_key: None,
        }
    }

    pub async fn login(&mut self, username: &str, password: &str) -> RawResponse {
        self.http.login(username, password).await
    }

    /// Perform the E2EE handshake: fetch the server public key, initiate,
    /// wrap a fresh AES-256 key with RSA-OAEP, and exchange it for a session
    /// id. Mirrors `SecureClient.handshake`.
    pub async fn handshake(&mut self) {
        // 1) Server public key (SPKI PEM).
        let pk = self.http.get("/api/handshake/public_key").await;
        let pem = pk
            .json()
            .get("server_public_key")
            .and_then(|v| v.as_str())
            .expect("server_public_key")
            .to_string();
        // 2) Initiate: nonce + RSA-PSS signature (we trust the server here).
        let _init = self.http.post_json("/api/handshake/initiate", &Value::Null).await;
        // 3) Wrap a fresh AES-256 key with RSA-OAEP.
        let pub_key = RsaPublicKey::from_public_key_pem(&pem).expect("parse SPKI");
        let mut aes_key = [0u8; 32];
        rand::rng().fill_bytes(&mut aes_key);
        let wrapped = pub_key
            .encrypt(&mut rand_core::OsRng, Oaep::new::<Sha256>(), &aes_key)
            .expect("OAEP encrypt");
        let b64 = base64::engine::general_purpose::STANDARD.encode(wrapped);
        let resp = self
            .http
            .post_json(
                "/api/handshake/exchange",
                &serde_json::json!({ "encrypted_session_key": b64 }),
            )
            .await;
        assert_eq!(resp.status, StatusCode::OK, "handshake failed: {:?}", resp.body);
        self.session_id = resp
            .json()
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        self.aes_key = Some(aes_key);
    }

    /// Perform an encrypted API call; returns (status, decrypted_json).
    pub async fn call(
        &self,
        method: &str,
        url: &str,
        body: Option<&Value>,
    ) -> (StatusCode, Option<Value>) {
        let mut headers = HashMap::new();
        if let Some(sid) = &self.session_id {
            headers.insert("x-session-id".to_string(), sid.clone());
        }
        let content = if let (Some(key), Some(body)) = (&self.aes_key, body) {
            let mut iv: [u8; 12] = [0u8; 12];
            rand::rng().fill_bytes(&mut iv);
            let aad = self
                .session_id
                .as_deref()
                .map(|s| s.as_bytes())
                .unwrap_or(&[] as &[u8]);
            let cipher = Aes256Gcm::new_from_slice(key).expect("aes");
            let mut nonce = Nonce::default();
            nonce.copy_from_slice(&iv);
            let ct = cipher
                .encrypt(&nonce, Payload { msg: body.to_string().as_bytes(), aad })
                .expect("encrypt");
            let payload = serde_json::json!({
                "iv": base64::engine::general_purpose::STANDARD.encode(iv),
                "ciphertext": base64::engine::general_purpose::STANDARD.encode(ct),
            });
            headers.insert("content-type".to_string(), "application/json".to_string());
            Some(payload.to_string().into_bytes())
        } else {
            None
        };
        let has_content = content.is_some();
        let resp = self
            .http
            .request(method, url, content, has_content.then_some("application/json"), &headers)
            .await;
        if resp.status == StatusCode::NO_CONTENT || resp.body.is_empty() {
            return (resp.status, None);
        }
        let payload = resp.json();
        if let Some(ct) = payload.get("ciphertext").and_then(|v| v.as_str()) {
            if let (Some(key), Some(sid)) = (&self.aes_key, &self.session_id) {
                let iv_b64 = payload.get("iv").and_then(|v| v.as_str()).unwrap_or("");
                let iv = base64::engine::general_purpose::STANDARD.decode(iv_b64).unwrap();
                let ct_b = base64::engine::general_purpose::STANDARD.decode(ct).unwrap();
                let cipher = Aes256Gcm::new_from_slice(key).expect("aes");
                let mut nonce = Nonce::default();
                nonce.copy_from_slice(&iv);
                let plain = cipher
                    .decrypt(&nonce, Payload { msg: &ct_b, aad: sid.as_bytes() })
                    .expect("decrypt");
                return (resp.status, Some(serde_json::from_slice(&plain).unwrap()));
            }
        }
        (resp.status, Some(payload))
    }
}

/// A temporary directory for a single test (removed on drop).
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        // VRECKAN_TEST_TMPDIR lets CI root the fixture tree in a directory
        // visible to the docker daemon: the test container's own /tmp is not
        // a valid bind source for the app containers the server launches
        // through the mounted socket (the daemon resolves the source on its
        // own host). Locally the default temp dir is used.
        let root = std::env::var("VRECKAN_TEST_TMPDIR")
            .ok()
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(std::env::temp_dir);
        let dir = root.join(format!(
            "vreckan-rust-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Re-export for convenience in test files.
pub use rand::RngCore;
