//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! RBAC: the role/permission/group access-control model.
//!
//! Ports `tests/test_rbac`. Covers the permission catalog, role CRUD,
//! effective-permission resolution (direct + roles + groups, union semantics,
//! admin super-permission), the derived `is_admin` flag (including its
//! persistence), capability derivation from permissions, per-endpoint 403s for
//! users holding only some admin permissions, SSO group-join + admin
//! promotion, the SSO/OIDC configuration endpoints, and the bootstrap-admin
//! guard.
//!
//! The Python tests drive the in-memory `USER_DATA` / `GROUP_DATA` /
//! `ROLE_DATA` dicts directly; the Rust port drives the equivalent database
//! rows through the same `user_manager` functions, so the assertions are
//! 1:1.

mod common;

use base64::Engine;
use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use sea_orm::ConnectionTrait;
use pkcs8::EncodePrivateKey;
use rsa::{RsaPrivateKey, traits::PublicKeyParts};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use vreckan::permissions;
use vreckan::user_manager;

const CLIENT_ID: &str = "vreckan-test-client";

/// Base64url-encode a big-endian byte slice without padding.
fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A minimal OIDC authorization-code identity provider for tests (copied from
/// `test_oidc.rs`). Only the discovery endpoint is exercised by the SSO
/// "test connection" test here, but the full mock is reused for consistency.
struct MockIdp {
    jwk_key: RsaPrivateKey,
    kid: String,
    sign_key: Arc<Mutex<RsaPrivateKey>>,
    serve_jwks_uri: Arc<Mutex<bool>>,
    codes: Arc<Mutex<HashMap<String, Value>>>,
    issuer: String,
    port: u16,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockIdp {
    fn new() -> Self {
        let jwk_key = RsaPrivateKey::new(&mut rand::rng(), 2048).unwrap();
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
        key.to_pkcs8_pem(pkcs8::LineEnding::LF)
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
                    "n": b64u(&pub_key.n().to_be_bytes_trimmed_vartime()),
                    "e": b64u(&pub_key.e().to_be_bytes_trimmed_vartime()),
                }]
            }),
        )
    } else if method == "POST" && path == "/token" {
        let form = parse_form(body);
        let grant = form.get("grant_type").cloned().unwrap_or_default();
        let code = form.get("code").cloned().unwrap_or_default();
        let claims = codes.lock().unwrap().remove(&code);
        match (grant.as_str(), claims) {
            ("authorization_code", Some(mut full)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
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
            _ => (400, json!({ "error": "invalid_grant" })),
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

/// A `SecureClient` logged in as the given (password-authenticated) user, with
/// the E2EE handshake performed. Mirrors the Python `make_logged_in_client`.
async fn make_logged_in_client(
    state: vreckan::state::AppState,
    username: &str,
    password: &str,
) -> SecureClient {
    let mut sc = SecureClient::new(state);
    let resp = sc.login(username, password).await;
    assert_eq!(resp.status, StatusCode::OK, "login failed: {:?}", resp.body);
    sc.handshake().await;
    sc
}

// ---------------------------------------------------------------------------
// Permission catalog
// ---------------------------------------------------------------------------

#[test]
fn test_catalog_shape() {
    assert!(permissions::ADMIN_PERMISSIONS.contains(&"admin"));
    let all = permissions::all_permissions();
    assert!(all.contains(&"admin"));
    assert_eq!(
        all.len(),
        permissions::ADMIN_PERMISSIONS.len() + permissions::USER_PERMISSIONS.len()
    );
    // No duplicates in the catalog.
    let mut seen = std::collections::HashSet::new();
    for p in &all {
        assert!(seen.insert(*p), "duplicate permission {p}");
    }
    for p in &all {
        assert!(permissions::is_valid_permission(p));
    }
    assert!(!permissions::is_valid_permission("admin.nonexistent"));
}

#[test]
fn test_admin_super_permission_expands_to_whole_catalog() {
    let expanded = permissions::expand_permissions(&["admin".to_string()]);
    let all: std::collections::HashSet<String> = permissions::all_permissions()
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(expanded, all);
}

#[test]
fn test_union_permissions_most_permissive_wins() {
    let all: std::collections::HashSet<String> = permissions::all_permissions()
        .into_iter()
        .map(String::from)
        .collect();
    // A single "admin" in any source grants the whole catalog.
    assert_eq!(
        permissions::union_permissions(&[
            vec!["user.storage".to_string()],
            vec!["admin".to_string()],
        ]),
        all
    );
    // Plain union otherwise.
    assert_eq!(
        permissions::union_permissions(&[
            vec!["user.gpu".to_string()],
            vec!["user.storage".to_string()],
        ]),
        std::collections::HashSet::from(["user.gpu".to_string(), "user.storage".to_string()])
    );
    // Unknown permissions are dropped.
    assert!(permissions::union_permissions(&[vec!["bogus.perm".to_string()]]).is_empty());
}

// ---------------------------------------------------------------------------
// Role CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_builtin_roles_seeded_on_load() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let roles = user_manager::get_all_roles(&state.db).await.unwrap();
    let names: std::collections::HashSet<&str> =
        roles.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, std::collections::HashSet::from(["admin", "operator", "user"]));
    let admin = roles.iter().find(|r| r.name == "admin").unwrap();
    assert_eq!(admin.permissions, vec!["admin".to_string()]);
    assert!(admin.is_builtin);
    let user = roles.iter().find(|r| r.name == "user").unwrap();
    assert_eq!(
        user.permissions,
        vec!["user.storage".to_string(), "user.gpu".to_string()]
    );
}

#[tokio::test]
async fn test_create_role() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let role = user_manager::create_role(&state.db, "editors", "Can edit", &["user.sharing".to_string()])
        .await
        .unwrap();
    assert_eq!(role.name, "editors");
    assert_eq!(role.permissions, vec!["user.sharing".to_string()]);
    assert!(!role.is_builtin);
    assert!(user_manager::get_role(&state.db, "editors").await.unwrap().is_some());
}

#[tokio::test]
async fn test_create_role_duplicate_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    assert!(user_manager::create_role(&state.db, "editors", "", &[]).await.is_err());
}

#[tokio::test]
async fn test_create_role_invalid_name_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::create_role(&state.db, "bad name!", "", &[]).await.is_err());
}

#[tokio::test]
async fn test_create_role_unknown_permission_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::create_role(&state.db, "editors", "", &["admin.bogus".to_string()])
        .await
        .is_err());
}

#[tokio::test]
async fn test_update_role() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "old", &["user.sharing".to_string()])
        .await
        .unwrap();
    let role = user_manager::update_role(&state.db, "editors", Some("new"), Some(&["user.gpu".to_string()]))
        .await
        .unwrap();
    assert_eq!(role.description, "new");
    assert_eq!(role.permissions, vec!["user.gpu".to_string()]);
}

#[tokio::test]
async fn test_update_role_unknown_permission_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    assert!(user_manager::update_role(&state.db, "editors", None, Some(&["nope.nope".to_string()]))
        .await
        .is_err());
}

#[tokio::test]
async fn test_update_role_missing_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::update_role(&state.db, "ghost", Some("x"), None).await.is_err());
}

#[tokio::test]
async fn test_delete_role() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    user_manager::delete_role(&state.db, "editors").await.unwrap();
    assert!(user_manager::get_role(&state.db, "editors").await.unwrap().is_none());
}

#[tokio::test]
async fn test_delete_role_missing_rejected() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::delete_role(&state.db, "ghost").await.is_err());
}

#[tokio::test]
async fn test_delete_role_refused_while_assigned() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["user".to_string(), "editors".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    // While assigned, deletion is refused.
    assert!(user_manager::delete_role(&state.db, "editors").await.is_err());
    // After removing the role from the user, deletion succeeds.
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["user".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    user_manager::delete_role(&state.db, "editors").await.unwrap();
    assert!(user_manager::get_role(&state.db, "editors").await.unwrap().is_none());
}

#[tokio::test]
async fn test_delete_builtin_role_refused() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    // Built-in roles are always assigned (admin to the bootstrap admin, user
    // to every user), so they can never be deleted.
    assert!(user_manager::delete_role(&state.db, "user").await.is_err());
}

// ---------------------------------------------------------------------------
// Effective permission resolution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_direct_permissions() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        None,
        Some(vec!["user.sharing".to_string()]),
        None,
    )
    .await
    .unwrap();
    let perms = user_manager::get_effective_permissions(&state.db, "alice").await.unwrap();
    assert!(perms.contains(&"user.sharing".to_string()));
    // The user role's permissions are still there (union).
    assert!(perms.contains(&"user.storage".to_string()));
    assert!(perms.contains(&"user.gpu".to_string()));
}

#[tokio::test]
async fn test_role_permissions() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["user".to_string(), "editors".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    let perms = user_manager::get_effective_permissions(&state.db, "alice").await.unwrap();
    assert!(perms.contains(&"user.sharing".to_string()));
}

#[tokio::test]
async fn test_group_permissions_and_roles() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_role(&state.db, "editors", "", &["user.sharing".to_string()])
        .await
        .unwrap();
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::ensure_group(&state.db, "writers").await.unwrap();
    // The group grants a permission directly and a role indirectly.
    user_manager::db_save_group(
        &state.db.conn,
        "writers",
        &json!({}),
        Some(&["editors".to_string()]),
        Some(&["user.harden_container".to_string()]),
    )
    .await
    .unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        None,
        None,
        Some(vec!["writers".to_string()]),
    )
    .await
    .unwrap();
    let perms = user_manager::get_effective_permissions(&state.db, "alice").await.unwrap();
    assert!(perms.contains(&"user.harden_container".to_string())); // from the group directly
    assert!(perms.contains(&"user.sharing".to_string())); // via the group's 'editors' role
    assert!(perms.contains(&"user.storage".to_string())); // still via the user's own 'user' role
}

#[tokio::test]
async fn test_admin_super_permission_in_effective_set() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    // The bootstrap admin has the admin role -> the whole catalog. The
    // effective set is returned sorted (mirrors the Python `sorted(...)`).
    let perms = user_manager::get_effective_permissions(&state.db, "admin").await.unwrap();
    let mut all: Vec<String> = permissions::all_permissions().into_iter().map(String::from).collect();
    all.sort();
    assert_eq!(perms, all);
}

#[tokio::test]
async fn test_unknown_roles_and_permissions_ignored() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["user".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    // Append unknown role/permission entries directly to the row.
    let user = user_manager::get_user(&state.db, "alice").await.unwrap().unwrap();
    let mut roles = user.roles;
    roles.push("ghost-role".to_string());
    let mut perms = user.permissions;
    perms.push("ghost.perm".to_string());
    user_manager::db_save_user_row(
        &state.db.conn,
        "alice",
        &user_manager::UserFields {
            roles: Some(roles),
            permissions: Some(perms),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    // No crash, and the unknown entries contribute nothing.
    let perms = user_manager::get_effective_permissions(&state.db, "alice").await.unwrap();
    assert!(!perms.contains(&"ghost.perm".to_string()));
    assert!(perms.contains(&"user.storage".to_string()));
}

// ---------------------------------------------------------------------------
// is_admin derivation + persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_is_admin_derived_from_permissions() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    assert!(!user_manager::get_user(&state.db, "alice").await.unwrap().unwrap().is_admin);
    // Granting the admin role makes her an admin.
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["admin".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(user_manager::get_user(&state.db, "alice").await.unwrap().unwrap().is_admin);
    // Removing it demotes her.
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["user".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!user_manager::get_user(&state.db, "alice").await.unwrap().unwrap().is_admin);
}

#[tokio::test]
async fn test_is_admin_persisted_to_db() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec!["admin".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    // The flag is persisted to the DB row, so a fresh read sees it.
    assert!(user_manager::get_user(&state.db, "alice").await.unwrap().unwrap().is_admin);
}

#[tokio::test]
async fn test_promote_and_demote() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    user_manager::promote_user_to_admin(&state.db, "alice").await.unwrap();
    let u = user_manager::get_user(&state.db, "alice").await.unwrap().unwrap();
    assert!(u.is_admin);
    assert!(u.roles.contains(&"admin".to_string()));
    user_manager::demote_admin_to_user(&state.db, &state.settings, "alice").await.unwrap();
    let u = user_manager::get_user(&state.db, "alice").await.unwrap().unwrap();
    assert!(!u.is_admin);
    // Demotion leaves the account with the built-in user role.
    assert_eq!(u.roles, vec!["user".to_string()]);
}

#[tokio::test]
async fn test_bootstrap_admin_cannot_be_demoted() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::demote_admin_to_user(&state.db, &state.settings, "admin").await.is_err());
}

#[tokio::test]
async fn test_bootstrap_admin_must_keep_admin_role() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::set_user_access(
        &state.db,
        &state.settings,
        "admin",
        Some(vec!["user".to_string()]),
        None,
        None,
    )
    .await
    .is_err());
    // The guard did not mutate the account.
    let u = user_manager::get_user(&state.db, "admin").await.unwrap().unwrap();
    assert!(u.roles.contains(&"admin".to_string()));
}

// ---------------------------------------------------------------------------
// Capability derivation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_capabilities_derived_from_permissions() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    let s = user_manager::get_effective_settings(&state.db, "alice").await.unwrap();
    // The 'user' role grants user.storage + user.gpu but not user.sharing.
    assert_eq!(s["persistent_storage"], true);
    assert_eq!(s["gpu"], true);
    assert_eq!(s["public_sharing"], false);
    // Granting user.sharing flips the capability on.
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        None,
        Some(vec!["user.sharing".to_string()]),
        None,
    )
    .await
    .unwrap();
    let s = user_manager::get_effective_settings(&state.db, "alice").await.unwrap();
    assert_eq!(s["public_sharing"], true);
}

#[tokio::test]
async fn test_capabilities_off_without_permissions() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "alice", None).await.unwrap();
    // Strip every role/permission: no capabilities at all.
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "alice",
        Some(vec![]),
        Some(vec![]),
        None,
    )
    .await
    .unwrap();
    let s = user_manager::get_effective_settings(&state.db, "alice").await.unwrap();
    assert_eq!(s["persistent_storage"], false);
    assert_eq!(s["public_sharing"], false);
    assert_eq!(s["gpu"], false);
}

// ---------------------------------------------------------------------------
// Per-endpoint 403s
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_partial_admin_permissions_gate_endpoints() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    // The built-in 'operator' role grants admin.apps/admin.stores/
    // admin.sessions but NOT admin.accounts.
    user_manager::create_user(&state.db, "bob", None).await.unwrap();
    user_manager::set_password(&state.db, "bob", "bobpass123").await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "bob",
        Some(vec!["operator".to_string()]),
        None,
        None,
    )
    .await
    .unwrap();
    let sc = make_logged_in_client(state.clone(), "bob", "bobpass123").await;
    // Allowed: operator holds admin.apps.
    let (status, _) = sc.call("GET", "/api/admin/apps/installed", None).await;
    assert_eq!(status, StatusCode::OK);
    // Denied: operator does not hold admin.accounts.
    let (status, _) = sc.call("GET", "/api/admin/people/bob/homedirs", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Denied: operator does not hold admin.roles.
    let (status, _) = sc.call("GET", "/api/admin/roles", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Denied: operator does not hold admin.global_settings.
    let (status, _) = sc.call("GET", "/api/admin/global-settings", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_plain_user_denied_all_admin_endpoints() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    user_manager::create_user(&state.db, "carol", None).await.unwrap();
    user_manager::set_password(&state.db, "carol", "carolpass1").await.unwrap();
    let sc = make_logged_in_client(state.clone(), "carol", "carolpass1").await;
    // /data is a POST (the aggregate management payload); the rest are GETs.
    let (status, _) = sc.call("POST", "/api/admin/data", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    for path in [
        "/api/admin/apps/installed",
        "/api/admin/roles",
        "/api/admin/global-settings",
        "/api/admin/sessions",
    ] {
        let (status, _) = sc.call("GET", path, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} should be 403 for a plain user");
    }
}

#[tokio::test]
async fn test_full_admin_allowed_everywhere() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    // /data is a POST (the aggregate management payload); the rest are GETs.
    let (status, _) = sc.call("POST", "/api/admin/data", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    for path in [
        "/api/admin/apps/installed",
        "/api/admin/roles",
        "/api/admin/permissions",
        "/api/admin/global-settings",
        "/api/admin/sso-groups",
        "/api/admin/sessions",
        "/api/admin/volume_mounts",
        "/api/admin/backup",
    ] {
        let (status, _) = sc.call("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path} should be 200 for the full admin");
    }
}

// ---------------------------------------------------------------------------
// Roles API (through the encrypted client)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_roles_api_crud() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    // Create.
    let (status, role) = sc
        .call(
            "POST",
            "/api/admin/roles",
            Some(&json!({ "name": "editors", "description": "d", "permissions": ["user.sharing"] })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(role.unwrap()["name"], "editors");
    // Duplicate -> 409.
    let (status, _) = sc
        .call("POST", "/api/admin/roles", Some(&json!({ "name": "editors", "permissions": [] })))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // List.
    let (status, roles) = sc.call("GET", "/api/admin/roles", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: std::collections::HashSet<String> = roles
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        std::collections::HashSet::from(["admin".to_string(), "operator".to_string(), "user".to_string(), "editors".to_string()])
    );
    // Update.
    let (status, role) = sc
        .call(
            "PUT",
            "/api/admin/roles/editors",
            Some(&json!({ "description": "new", "permissions": ["user.gpu"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(role.unwrap()["permissions"], json!(["user.gpu"]));
    // Delete (nothing is assigned to it).
    let (status, _) = sc.call("DELETE", "/api/admin/roles/editors", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_status, roles) = sc.call("GET", "/api/admin/roles", None).await;
    let names: std::collections::HashSet<String> = roles
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        std::collections::HashSet::from(["admin".to_string(), "operator".to_string(), "user".to_string()])
    );
}

#[tokio::test]
async fn test_permissions_catalog_endpoint() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    let (status, data) = sc.call("GET", "/api/admin/permissions", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    let admin_names: std::collections::HashSet<String> = data["admin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    let user_names: std::collections::HashSet<String> = data["user"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    let expected_admin: std::collections::HashSet<String> =
        permissions::ADMIN_PERMISSIONS.iter().map(|s| s.to_string()).collect();
    let expected_user: std::collections::HashSet<String> =
        permissions::USER_PERMISSIONS.iter().map(|s| s.to_string()).collect();
    assert_eq!(admin_names, expected_admin);
    assert_eq!(user_names, expected_user);
}

#[tokio::test]
async fn test_people_access_endpoint() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    // Create a user, then grant them a role + permission + group via /access.
    let (status, _) = sc
        .call("POST", "/api/admin/people", Some(&json!({ "username": "dave" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, user) = sc
        .call(
            "POST",
            "/api/admin/people/dave/access",
            Some(&json!({ "roles": ["operator"], "permissions": ["user.sharing"], "groups": ["writers"] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let user = user.unwrap();
    assert_eq!(user["roles"], json!(["operator"]));
    assert_eq!(user["permissions"], json!(["user.sharing"]));
    assert_eq!(user["groups"], json!(["writers"]));
    // The first group becomes the primary group.
    assert_eq!(user["settings"]["group"], "writers");
    // The bootstrap admin cannot be stripped of the admin role.
    let (status, _) = sc
        .call("POST", "/api/admin/people/admin/access", Some(&json!({ "roles": ["user"] })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// SSO group-join + admin promotion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_oidc_admin_group_promotes_to_admin() {
    let _root = TempDir::new();
    let mut state = build_state(_root.path()).await;
    // Point the admin-groups list at 'vdi-admins' (read at call time).
    state.settings.oidc_admin_groups = "vdi-admins".to_string();
    state.settings.oidc_user_groups = "vdi-users".to_string();
    user_manager::create_user(&state.db, "sstest", None).await.unwrap();
    let cfg = vreckan::auth::oidc_config(&state).await;
    vreckan::auth::oidc_apply_groups(&state, &cfg, "sstest", &json!({ "groups": ["vdi-admins", "vdi-users"] }))
        .await;
    let user = user_manager::get_user(&state.db, "sstest").await.unwrap().unwrap();
    // Joined both provider groups.
    let groups: std::collections::HashSet<&str> = user.groups.iter().map(String::as_str).collect();
    assert_eq!(groups, std::collections::HashSet::from(["vdi-admins", "vdi-users"]));
    // The admin-list group carries the admin role -> the user is an admin.
    let admin_group = user_manager::get_group(&state.db, "vdi-admins").await.unwrap().unwrap();
    let user_group = user_manager::get_group(&state.db, "vdi-users").await.unwrap().unwrap();
    assert!(admin_group.roles.contains(&"admin".to_string()));
    assert!(user_group.roles.contains(&"user".to_string()));
    assert!(user.is_admin);
    // The primary group is the one in the user-groups list.
    let settings = user.settings.unwrap();
    assert_eq!(settings["group"], "vdi-users");
}

#[tokio::test]
async fn test_oidc_non_admin_group_does_not_promote() {
    let _root = TempDir::new();
    let mut state = build_state(_root.path()).await;
    state.settings.oidc_admin_groups = "vdi-admins".to_string();
    state.settings.oidc_user_groups = "vdi-users".to_string();
    user_manager::create_user(&state.db, "plain", None).await.unwrap();
    let cfg = vreckan::auth::oidc_config(&state).await;
    vreckan::auth::oidc_apply_groups(&state, &cfg, "plain", &json!({ "groups": ["vdi-users"] }))
        .await;
    let user = user_manager::get_user(&state.db, "plain").await.unwrap().unwrap();
    assert!(user.groups.contains(&"vdi-users".to_string()));
    assert!(!user.is_admin);
    // Still a normal user with the user role's capabilities.
    let perms = user_manager::get_effective_permissions(&state.db, "plain").await.unwrap();
    assert!(perms.contains(&"user.storage".to_string()));
}

// ---------------------------------------------------------------------------
// SSO / OIDC configuration (editable from the UI)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_sso_get_reports_values_and_sources() {
    let _root = TempDir::new();
    let mut state = build_state(_root.path()).await;
    // A DB value (as if saved on the SSO page) overrides the environment.
    state
        .db
        .set_app_setting("VRECKAN_OIDC_ISSUER", "https://db.example/o/app/")
        .await
        .unwrap();
    state.db.set_app_setting("VRECKAN_OIDC_ENABLED", "true").await.unwrap();
    // Env values (struct fields) — set before cloning into the client.
    state.settings.oidc_issuer = "https://env.example/o/app/".to_string();
    state.settings.oidc_client_id = "env-client".to_string();
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    let (status, data) = sc.call("GET", "/api/admin/sso", None).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    // DB wins over env for the issuer; env is used for the client id (no DB row).
    assert_eq!(data["values"]["oidc_issuer"], "https://db.example/o/app/");
    assert_eq!(data["values"]["oidc_client_id"], "env-client");
    assert_eq!(data["sources"]["oidc_issuer"], "db");
    assert_eq!(data["sources"]["oidc_client_id"], "env");
    // The effective config resolves the same way.
    let cfg = vreckan::auth::oidc_config(&state).await;
    assert_eq!(cfg.issuer, "https://db.example/o/app/");
    assert_eq!(cfg.client_id, "env-client");
    assert!(cfg.enabled);
}

#[tokio::test]
async fn test_sso_put_persists_and_overrides_env() {
    let _root = TempDir::new();
    let mut state = build_state(_root.path()).await;
    state.settings.oidc_admin_groups = "env-admin".to_string();
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    let (status, _) = sc
        .call(
            "PUT",
            "/api/admin/sso",
            Some(&json!({
                "oidc_enabled": true,
                "oidc_issuer": "https://idp.example/o/app/",
                "oidc_client_id": "cid",
                "oidc_client_secret": "",
                "oidc_redirect_uri": "",
                "oidc_scopes": "openid email profile",
                "oidc_allow_signup": true,
                "oidc_state_ttl_seconds": 600,
                "oidc_admin_groups": ["vdi-admins"],
                "oidc_user_groups": ["vdi-users"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // The saved values are now the effective ones (DB overrides the env).
    let cfg = vreckan::auth::oidc_config(&state).await;
    assert_eq!(cfg.issuer, "https://idp.example/o/app/");
    assert_eq!(cfg.admin_groups, "vdi-admins");
    assert_eq!(cfg.user_groups, "vdi-users");
    // The source badge now reports db for the edited fields.
    let (_status, data) = sc.call("GET", "/api/admin/sso", None).await;
    let data = data.unwrap();
    assert_eq!(data["sources"]["oidc_admin_groups"], "db");
    assert_eq!(data["values"]["oidc_admin_groups"], json!(["vdi-admins"]));
}

#[tokio::test]
async fn test_sso_test_connection() {
    let _root = TempDir::new();
    // A live mock IdP stands in for the provider's discovery endpoint.
    let mut idp = MockIdp::new();
    idp.start();
    let state = build_state(_root.path()).await;
    // Point the issuer at the mock (via DB, as if saved on the SSO page), with
    // a trailing slash to exercise the endpoint's slash-stripping.
    state
        .db
        .set_app_setting("VRECKAN_OIDC_ISSUER", &format!("{}/", idp.issuer))
        .await
        .unwrap();
    // Clear the process-wide discovery cache so the mock is fetched fresh.
    vreckan::auth::oidc_discovery_cache().lock().unwrap().clear();
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    let (status, data) = sc.call("POST", "/api/admin/sso/test", Some(&json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let data = data.unwrap();
    assert_eq!(data["ok"], true);
    // The endpoint strips the trailing slash before discovery, so the echoed
    // issuer is the normalized (slash-free) form.
    assert_eq!(data["issuer"], idp.issuer);
    assert_eq!(
        data["authorization_endpoint"],
        format!("{}{}", idp.issuer, "/authorize")
    );
}

#[tokio::test]
async fn test_sso_endpoint_requires_admin_sso() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    // A user with only admin.groups can see the (groups) sso-groups endpoint
    // but not the SSO config endpoint.
    user_manager::create_user(&state.db, "ssoonly", None).await.unwrap();
    user_manager::set_password(&state.db, "ssoonly", "ssoonly1").await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "ssoonly",
        Some(vec![]),
        Some(vec!["admin.groups".to_string()]),
        Some(vec![]),
    )
    .await
    .unwrap();
    let sc = make_logged_in_client(state.clone(), "ssoonly", "ssoonly1").await;
    let (status, _) = sc.call("GET", "/api/admin/sso", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = sc.call("GET", "/api/admin/sso-groups", None).await;
    assert_eq!(status, StatusCode::OK);
    // A user with admin.sso can reach the SSO config endpoint.
    user_manager::create_user(&state.db, "ssoadmin", None).await.unwrap();
    user_manager::set_password(&state.db, "ssoadmin", "ssoadmin1").await.unwrap();
    user_manager::set_user_access(
        &state.db,
        &state.settings,
        "ssoadmin",
        Some(vec![]),
        Some(vec!["admin.sso".to_string()]),
        Some(vec![]),
    )
    .await
    .unwrap();
    let sc2 = make_logged_in_client(state.clone(), "ssoadmin", "ssoadmin1").await;
    let (status, _) = sc2.call("GET", "/api/admin/sso", None).await;
    assert_eq!(status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Bootstrap admin (configurable via environment)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bootstrap_admin_configurable() {
    let _root = TempDir::new();
    let mut state = build_state(_root.path()).await;
    // Point the bootstrap username/password at custom values (read at seed
    // time). The lifespan already ran with the defaults, so the DB has no
    // admin yet and generate_default_admin will create the custom one.
    state.settings.bootstrap_admin_username = "rooty".to_string();
    state.settings.bootstrap_admin_password = "s3cret".to_string();
    // The lifespan already seeded the default 'admin'; wipe the users table so
    // generate_default_admin runs its create path with the custom values.
    let backend = state.db.conn.get_database_backend();
    state
        .db
        .conn
        .execute_raw(sea_orm::Statement::from_string(backend, "DELETE FROM users"))
        .await
        .unwrap();
    user_manager::generate_default_admin(&state.db, &state.settings).await.unwrap();
    user_manager::load_users_and_groups(&state.db, &state.settings).await.unwrap();
    // The account is created with the configured username and password.
    let rooty = user_manager::get_user(&state.db, "rooty").await.unwrap();
    assert!(rooty.is_some());
    assert!(rooty.unwrap().is_admin);
    assert!(user_manager::verify_password(&state.db, "rooty", "s3cret").await.unwrap());
    assert!(!user_manager::verify_password(&state.db, "rooty", "admin1234").await.unwrap());
    // The protection guards follow the configured name, not the literal
    // 'admin': the bootstrap account can't be deleted or demoted, and a
    // non-bootstrap admin can.
    assert!(user_manager::delete_person(&state.db, &state.settings, "rooty").await.is_err());
    assert!(user_manager::demote_admin_to_user(&state.db, &state.settings, "rooty").await.is_err());
    user_manager::create_user(&state.db, "otheradmin", None).await.unwrap();
    user_manager::promote_user_to_admin(&state.db, "otheradmin").await.unwrap();
    user_manager::demote_admin_to_user(&state.db, &state.settings, "otheradmin").await.unwrap();
    user_manager::delete_person(&state.db, &state.settings, "otheradmin").await.unwrap();
}

#[tokio::test]
async fn test_admin_sso_in_catalog() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let sc = make_logged_in_client(state.clone(), "admin", "admin1234").await;
    let (status, data) = sc.call("GET", "/api/admin/permissions", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: std::collections::HashSet<String> = data
        .unwrap()
        ["admin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains("admin.sso"));
}
