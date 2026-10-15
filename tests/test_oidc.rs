//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tests for the OIDC / SSO authorization-code flow and session revocation.
//!
//! Ports `tests/test_oidc`. A small in-process mock identity provider
//! (`MockIdp`) stands in for a real OIDC provider (such as authentik). It
//! serves:
//!
//! * `GET /.well-known/openid-configuration` — the discovery document,
//! * `GET /jwks` — a JWKS exposing the RSA public key (`n`/`e`, base64url),
//! * `POST /token` — the authorization-code token endpoint (form-encoded),
//!
//! and signs `id_token`s with a real RSA key (RS256 + `kid`) so the server's
//! full JWKS signature-verification path is exercised end to end. The mock
//! binds to a unique localhost port per test, so the discovery cache never
//! collides across tests.
//!
//! The mock runs in a dedicated OS thread with its own tokio runtime, so the
//! test's `#[tokio::test]` runtime is free to drain (the server's reqwest
//! calls reach the mock over loopback).

mod common;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use pkcs8_010::EncodePrivateKey;
use rand_core::OsRng;
use rsa::{RsaPrivateKey, traits::PublicKeyParts};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vreckan::user_manager;

const CLIENT_ID: &str = "vreckan-test-client";
const CLIENT_SECRET: &str = "test-client-secret";

/// Base64url-encode a big-endian byte slice without padding.
fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A minimal OIDC authorization-code identity provider for tests.
struct MockIdp {
    /// The key whose public half is published in the JWKS (set once).
    jwk_key: RsaPrivateKey,
    kid: String,
    /// The key actually used to sign id_tokens. Defaults to the JWKS key; the
    /// bad-signature test swaps this for a different key so the server's JWKS
    /// verification rejects the token. Shared so the server thread sees swaps.
    sign_key: Arc<Mutex<RsaPrivateKey>>,
    /// Whether the discovery doc advertises a `jwks_uri`. Shared so the server
    /// thread sees the disable_jwks() toggle.
    serve_jwks_uri: Arc<Mutex<bool>>,
    /// Authorization code -> claims. Shared so codes issued by the test are
    /// visible to the server thread's token endpoint.
    codes: Arc<Mutex<HashMap<String, Value>>>,
    issuer: String,
    port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockIdp {
    fn new() -> Self {
        let jwk_key = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
        let sign_key = Arc::new(Mutex::new(jwk_key.clone()));
        Self {
            jwk_key,
            kid: "test-key-1".to_string(),
            sign_key,
            serve_jwks_uri: Arc::new(Mutex::new(true)),
            codes: Arc::new(Mutex::new(HashMap::new())),
            issuer: String::new(),
            port: 0,
            shutdown_tx: None,
            handle: None,
        }
    }

    /// Start the mock HTTP server on a fresh localhost port. The server thread
    /// owns its own tokio runtime and binds the listener inside it (a std
    /// listener can't be registered with a runtime via `from_std`), reporting
    /// the chosen port back over a channel before serving.
    fn start(&mut self) {
        let jwk_key = self.jwk_key.clone();
        let kid = self.kid.clone();
        let sign_key = self.sign_key.clone();
        let serve_jwks_uri = self.serve_jwks_uri.clone();
        let codes = self.codes.clone();
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
                let issuer = format!("http://127.0.0.1:{port}");
                let mut shutdown_rx = shutdown_rx;
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _)) => {
                                tokio::spawn(handle_conn(
                                    jwk_key.clone(),
                                    kid.clone(),
                                    sign_key.clone(),
                                    serve_jwks_uri.clone(),
                                    codes.clone(),
                                    issuer.clone(),
                                    stream,
                                ));
                            }
                            Err(_) => break,
                        },
                    }
                }
            });
        });
        // Wait for the server thread to report its port.
        self.port = port_rx.recv().unwrap();
        self.issuer = format!("http://127.0.0.1:{}", self.port);
        self.handle = Some(handle);
    }

    fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Record a claims set behind a fresh authorization code.
    fn issue_code(&self, claims: Value) -> String {
        let code = uuid::Uuid::new_v4().to_string();
        self.codes.lock().unwrap().insert(code.clone(), claims);
        code
    }

    /// Make the token endpoint sign with a key that does NOT match the JWKS.
    fn use_bad_signature(&mut self) {
        *self.sign_key.lock().unwrap() = RsaPrivateKey::new(&mut OsRng, 2048).unwrap();
    }

    /// Omit `jwks_uri` from the discovery doc (exercises the claims-only
    /// verification fallback in the server).
    fn disable_jwks(&mut self) {
        *self.serve_jwks_uri.lock().unwrap() = false;
    }
}

impl Drop for MockIdp {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Parse a form-encoded body into a map of single values.
fn parse_form(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Find the end of the HTTP headers (the byte after the blank line).
fn header_end(raw: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(raw).ok()?;
    s.find("\r\n\r\n").map(|i| i + 4)
}

/// The declared Content-Length of the request, if any.
fn content_length(raw: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(raw).ok()?;
    for line in s.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().ok();
            }
        }
    }
    None
}

/// Sign an id_token (RS256) with the given key and kid.
fn sign_id_token(key: &RsaPrivateKey, kid: &str, claims: &Value) -> String {
    let encoding_key = EncodingKey::from_rsa_pem(
        key.to_pkcs8_pem(pkcs8_010::LineEnding::LF)
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    jsonwebtoken::encode(&header, claims, &encoding_key).unwrap()
}

/// Handle a single connection: read the request, route it, write the response.
async fn handle_conn(
    jwk_key: RsaPrivateKey,
    kid: String,
    sign_key: Arc<Mutex<RsaPrivateKey>>,
    serve_jwks_uri: Arc<Mutex<bool>>,
    codes: Arc<Mutex<HashMap<String, Value>>>,
    issuer: String,
    mut stream: tokio::net::TcpStream,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Read the full request (headers + declared body).
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if let Some(end) = header_end(&buf) {
            match content_length(&buf) {
                Some(cl) if buf.len() < end + cl => continue,
                _ => break,
            }
        } else {
            break;
        }
    }
    let raw = String::from_utf8_lossy(&buf).to_string();
    let mut lines = raw.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let body_start = raw.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let body = &raw[body_start..];
    let method = request_line.split_whitespace().next().unwrap_or("");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .map(|p| p.split('?').next().unwrap_or(p))
        .unwrap_or("");

    let (status, obj) = if method == "GET" && path == "/.well-known/openid-configuration" {
        let mut doc = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
        });
        if *serve_jwks_uri.lock().unwrap() {
            doc["jwks_uri"] = json!(format!("{issuer}/jwks"));
        }
        (200, doc)
    } else if method == "GET" && path == "/jwks" {
        let pub_key = jwk_key.to_public_key();
        (
            200,
            json!({
                "keys": [{
                    "kty": "RSA",
                    "kid": kid,
                    "use": "sig",
                    "alg": "RS256",
                    "n": b64u(&pub_key.n().to_bytes_be()),
                    "e": b64u(&pub_key.e().to_bytes_be()),
                }]
            }),
        )
    } else if method == "POST" && path == "/token" {
        let form = parse_form(body);
        let grant = form.get("grant_type").cloned().unwrap_or_default();
        let code = form.get("code").cloned().unwrap_or_default();
        let claims = codes.lock().unwrap().remove(&code);
        if grant != "authorization_code" || claims.is_none() {
            (400, json!({ "error": "invalid_grant" }))
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            let mut full = claims.unwrap();
            if let Some(o) = full.as_object_mut() {
                o.insert("iss".into(), json!(issuer));
                o.insert("aud".into(), json!(CLIENT_ID));
                o.insert("iat".into(), json!(now));
                o.insert("exp".into(), json!(now + 3600));
            }
            let token = sign_id_token(&sign_key.lock().unwrap(), &kid, &full);
            (
                200,
                json!({
                    "access_token": "opaque-access-token",
                    "id_token": token,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                }),
            )
        }
    } else {
        (404, json!({ "error": "not_found" }))
    };

    let body = obj.to_string();
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

/// Point the app's OIDC settings at a live mock IdP for this test. Returns the
/// (state, idp) pair; the caller must keep `idp` alive for the duration of the
/// test (it owns the server thread).
async fn setup_oidc(root: &std::path::Path) -> (vreckan::state::AppState, MockIdp) {
    let mut idp = MockIdp::new();
    idp.start();
    let mut state = build_state(root).await;
    // Clear the process-wide discovery cache so the mock's discovery is
    // fetched fresh (not reused from a prior test).
    vreckan::auth::oidc_discovery_cache().lock().unwrap().clear();
    state.settings.oidc_enabled = true;
    state.settings.oidc_issuer = idp.issuer.clone();
    state.settings.oidc_client_id = CLIENT_ID.to_string();
    state.settings.oidc_client_secret = CLIENT_SECRET.to_string();
    state.settings.oidc_scopes = "openid email profile".to_string();
    state.settings.oidc_allow_signup = true;
    state.settings.oidc_state_ttl_seconds = 600;
    state.settings.oidc_admin_groups = String::new();
    state.settings.oidc_user_groups = String::new();
    state.settings.oidc_redirect_uri = String::new();
    (state, idp)
}

/// Headers that make the server derive an `http://testserver` base URL
/// (mirrors the Python test client's `http://testserver` scheme).
fn testserver_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert("host".to_string(), "testserver".to_string());
    h.insert("x-forwarded-proto".to_string(), "http".to_string());
    h
}

/// Start the flow and return the generated `state` from the redirect.
async fn state_from_authorize(client: &common::TestClient) -> String {
    let auth = client.get_with("/api/auth/oidc/authorize", &testserver_headers()).await;
    assert_eq!(auth.status, StatusCode::FOUND, "{:?}", auth.body);
    let location = auth.header("location").unwrap();
    let url = url::Url::parse(&location).unwrap();
    url.query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.to_string())
        .unwrap()
}

// ---------------------------------------------------------------------------
// /oidc/status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_oidc_status_disabled_by_default() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let client = common::TestClient::new(state);
    let resp = client.get("/api/auth/oidc/status").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["enabled"], false);
    assert_eq!(body["configured"], false);
}

#[tokio::test]
async fn test_oidc_status_enabled() {
    let _root = TempDir::new();
    let (state, _idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state);
    let resp = client.get("/api/auth/oidc/status").await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["enabled"], true);
    assert_eq!(body["configured"], true);
}

// ---------------------------------------------------------------------------
// /oidc/authorize
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_authorize_400_when_disabled() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let client = common::TestClient::new(state);
    let resp = client.get("/api/auth/oidc/authorize").await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert!(resp.json()["detail"].as_str().unwrap().contains("not enabled"));
}

#[tokio::test]
async fn test_authorize_redirects_to_provider() {
    let _root = TempDir::new();
    let (state, idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    let resp = client.get_with("/api/auth/oidc/authorize", &testserver_headers()).await;
    assert_eq!(resp.status, StatusCode::FOUND);
    let location = resp.header("location").unwrap();
    let parsed = url::Url::parse(&location).unwrap();
    // Points at the mock provider's authorization endpoint.
    assert_eq!(parsed.host_str(), Some("127.0.0.1"));
    assert_eq!(parsed.port(), Some(idp.port));
    assert_eq!(parsed.path(), "/authorize");
    let params: HashMap<String, String> = parsed
        .query_pairs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(params.get("response_type").map(|s| s.as_str()), Some("code"));
    assert_eq!(params.get("client_id").map(|s| s.as_str()), Some(CLIENT_ID));
    // redirect_uri is derived from the test client's public base URL.
    assert_eq!(
        params.get("redirect_uri").map(|s| s.as_str()),
        Some("http://testserver/api/auth/oidc/callback")
    );
    assert_eq!(params.get("scope").map(|s| s.as_str()), Some("openid email profile"));
    // A one-time state was generated and stored server-side.
    let state_str = params.get("state").cloned().unwrap();
    let states = state.oidc_states.read().await;
    assert!(states.contains_key(&state_str));
    assert!(states[&state_str].expires_at
        > std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64());
}

// ---------------------------------------------------------------------------
// /oidc/callback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_callback_provisions_user() {
    let _root = TempDir::new();
    let (state, idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({
        "sub": "auth0|abc123",
        "preferred_username": "sstest",
        "email": "sstest@example.com",
        "name": "SS Test",
        "groups": ["vdi-users"],
    }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FOUND, "{:?}", resp.body);
    assert_eq!(resp.header("location").as_deref(), Some("/"));
    // The web token cookie was set.
    assert!(resp.header("set-cookie").is_some());
    // The identity was provisioned and flagged as SSO (not admin).
    let user = user_manager::get_user(&state.db, "sstest").await.unwrap().unwrap();
    assert!(user.is_sso);
    assert!(!user.is_admin);
    // The issued cookie authenticates against /me.
    let set_cookie = resp.header("set-cookie").unwrap();
    let cookie = set_cookie.split(';').next().unwrap().trim();
    let value = cookie.split_once('=').map(|(_, v)| v.to_string()).unwrap();
    let mut me_client = common::TestClient::new(state.clone());
    me_client.auth_cookie = Some(value);
    let me = me_client.get("/api/auth/me").await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json()["username"], "sstest");
    assert_eq!(me.json()["is_sso"], true);
}

#[tokio::test]
async fn test_callback_invalid_state() {
    let _root = TempDir::new();
    let (state, idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    let code = idp.issue_code(json!({ "preferred_username": "sstest", "sub": "x" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state=not-a-real-state"))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert!(resp.json()["detail"].as_str().unwrap().contains("CSRF"));
}

#[tokio::test]
async fn test_callback_expired_state() {
    let _root = TempDir::new();
    let (state, idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    // Force the stored state to be past its expiry. The write guard must be
    // dropped (block scope) BEFORE the callback request, because the handler
    // acquires the same `oidc_states` write lock on the shared runtime.
    {
        let mut states = state.oidc_states.write().await;
        states.get_mut(&state_str).unwrap().expires_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() - 10.0;
    }
    let code = idp.issue_code(json!({ "preferred_username": "sstest", "sub": "x" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    assert!(resp.json()["detail"].as_str().unwrap().contains("CSRF"));
}

#[tokio::test]
async fn test_callback_unknown_user_signup_disabled() {
    let _root = TempDir::new();
    let (mut state, idp) = setup_oidc(_root.path()).await;
    state.settings.oidc_allow_signup = false;
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({ "preferred_username": "brand-new-user", "sub": "xyz" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert!(resp.json()["detail"].as_str().unwrap().contains("auto-signup is disabled"));
    // No account was created.
    assert!(user_manager::get_user(&state.db, "brand-new-user").await.unwrap().is_none());
}

#[tokio::test]
async fn test_callback_admin_group_promotion() {
    let _root = TempDir::new();
    let (mut state, idp) = setup_oidc(_root.path()).await;
    state.settings.oidc_admin_groups = "vdi-admins".to_string();
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({
        "preferred_username": "ssadmin-test",
        "sub": "admin-sub",
        "groups": ["vdi-admins"],
    }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FOUND, "{:?}", resp.body);
    let user = user_manager::get_user(&state.db, "ssadmin-test").await.unwrap().unwrap();
    assert!(user.is_admin);
    assert!(user.is_sso);
    // The full SSO group list is now persisted on the account.
    assert_eq!(user.sso_groups, vec!["vdi-admins".to_string()]);
}

#[tokio::test]
async fn test_callback_user_group_assignment() {
    let _root = TempDir::new();
    let (mut state, idp) = setup_oidc(_root.path()).await;
    state.settings.oidc_user_groups = "vdi-users".to_string();
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({
        "preferred_username": "sstest",
        "sub": "user-sub",
        "groups": ["other-group", "vdi-users"],
    }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FOUND, "{:?}", resp.body);
    let user = user_manager::get_user(&state.db, "sstest").await.unwrap().unwrap();
    assert!(!user.is_admin);
    assert!(user.is_sso);
    // The configured user group (vdi-users) is preferred over the other group.
    let effective = user_manager::get_effective_settings(&state.db, "sstest").await.unwrap();
    assert_eq!(effective["group"], "vdi-users");
    // The full SSO group list is persisted on the account.
    assert_eq!(user.sso_groups, vec!["other-group".to_string(), "vdi-users".to_string()]);
}

#[tokio::test]
async fn test_callback_inactive_user_rejected() {
    let _root = TempDir::new();
    let (state, idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    // Seed an already-provisioned user with a deactivated account.
    let mut settings = user_manager::default_user_settings().clone();
    settings["active"] = json!(false);
    let fields = user_manager::UserFields {
        is_admin: Some(false),
        is_sso: Some(false),
        settings: Some(settings),
        ..Default::default()
    };
    user_manager::db_save_user_row(&state.db.conn, "deactivated", &fields).await.unwrap();
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({ "preferred_username": "deactivated", "sub": "d1" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert!(resp.json()["detail"].as_str().unwrap().contains("inactive"));
}

#[tokio::test]
async fn test_callback_bad_signature_rejected() {
    let _root = TempDir::new();
    let (state, mut idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    // The provider signs the id_token with a key that does not match its JWKS.
    idp.use_bad_signature();
    let code = idp.issue_code(json!({ "preferred_username": "sstest", "sub": "x" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_GATEWAY);
    assert!(resp.json()["detail"].as_str().unwrap().contains("verification failed"));
}

#[tokio::test]
async fn test_callback_no_jwks_fails_closed() {
    let _root = TempDir::new();
    let (state, mut idp) = setup_oidc(_root.path()).await;
    let client = common::TestClient::new(state.clone());
    // Default: a signature is required, so an unreachable JWKS fails the
    // login (502) rather than accepting an unsigned token.
    idp.disable_jwks();
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({ "preferred_username": "sstest", "sub": "x" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_GATEWAY, "{:?}", resp.body);
    assert!(resp.json()["detail"].as_str().unwrap().contains("JWKS"));
}

#[tokio::test]
async fn test_callback_no_jwks_claims_only_when_disabled() {
    let _root = TempDir::new();
    let (mut state, mut idp) = setup_oidc(_root.path()).await;
    // With oidc_require_signature off, the legacy claims-only fallback is
    // used when the JWKS is unavailable.
    state.settings.oidc_require_signature = false;
    idp.disable_jwks();
    let client = common::TestClient::new(state.clone());
    let state_str = state_from_authorize(&client).await;
    let code = idp.issue_code(json!({ "preferred_username": "sstest", "sub": "x" }));
    let resp = client
        .get(&format!("/api/auth/oidc/callback?code={code}&state={state_str}"))
        .await;
    assert_eq!(resp.status, StatusCode::FOUND, "{:?}", resp.body);
    assert!(user_manager::get_user(&state.db, "sstest").await.unwrap().is_some());
}

// ---------------------------------------------------------------------------
// Session revocation (self-service + admin force-logout)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_revoke_own_sessions() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    // The bootstrap admin is logged in (cookie set).
    assert!(sc.http.auth_cookie.is_some());
    let resp = sc.http.post_json("/api/auth/revoke-sessions", &json!({})).await;
    assert_eq!(resp.status, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["username"], "admin");
    assert!(body["revoked"].as_u64().unwrap() >= 1);
    // The cookie is cleared (Max-Age=0), so the session is gone.
    let set_cookie = resp.header("set-cookie").unwrap();
    assert!(set_cookie.contains("Max-Age=0"));
    // Subsequent authenticated calls now fail.
    let me = sc.http.get("/api/auth/me").await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_revoke_own_sessions_rejects_other_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let resp = sc
        .http
        .post_json("/api/auth/revoke-sessions", &json!({ "username": "someone-else" }))
        .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    assert!(resp.json()["detail"].as_str().unwrap().contains("only invalidate your own"));
    // The admin's own session is untouched.
    let me = sc.http.get("/api/auth/me").await;
    assert_eq!(me.status, StatusCode::OK);
}

#[tokio::test]
async fn test_admin_force_revoke() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // Give a second user an active web token (as if they had signed in) and a
    // matching account, then have the admin force-log them out.
    let victim_token = "victim-token-abc123";
    sc.state.auth_tokens.write().await.insert(
        victim_token.to_string(),
        vreckan::state::AuthTokenRecord {
            username: "sstest".to_string(),
            expires_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64() + 3600.0,
        },
    );
    let fields = user_manager::UserFields {
        is_admin: Some(false),
        is_sso: Some(true),
        settings: Some(user_manager::default_user_settings().clone()),
        ..Default::default()
    };
    user_manager::db_save_user_row(&sc.state.db.conn, "sstest", &fields).await.unwrap();
    let (status, body) = sc
        .call("POST", "/api/admin/oidc/revoke", Some(&json!({ "username": "sstest" })))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body.as_ref().unwrap()["username"], "sstest");
    assert_eq!(body.as_ref().unwrap()["revoked"], 1);
    assert!(!sc.state.auth_tokens.read().await.contains_key(victim_token));
}

#[tokio::test]
async fn test_admin_force_revoke_unknown_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    let (status, body) = sc
        .call("POST", "/api/admin/oidc/revoke", Some(&json!({ "username": "ghost-user" })))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.as_ref().unwrap()["detail"].as_str().unwrap().contains("not found"));
}
