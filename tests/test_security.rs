//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Security: argon2 passwords, upload-id validation, path traversal, share
//! passwords, and the per-response CSP / security headers.
//!
//! Ports `tests/test_security`. The raw-client tests drive the axum router
//! directly (no E2EE); the encrypted tests use the `SecureClient` handshake.

mod common;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use base64::Engine;
use common::{build_state, build_state_with, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use vreckan::security;
use vreckan::user_manager;

/// Start a throwaway TCP server that answers every request with a small 200
/// HTML body. Used to stand in for the app container that the session proxy
/// reverse-proxies to (the Python test monkeypatches `httpx2.AsyncClient`;
/// here we point the session's upstream at a real local socket).
async fn spawn_fake_upstream() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let body = b"<html>upstream</html>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).unwrap()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Enable the `public_sharing` capability for the bootstrap admin (it is off
/// by default), mirroring the Python tests' `get_user(...)["settings"][...]`.
async fn enable_public_sharing(sc: &SecureClient) {
    let user = user_manager::get_user(&sc.state.db, "admin")
        .await
        .unwrap()
        .unwrap();
    let mut settings = user.settings.unwrap_or_else(|| json!({}));
    settings["public_sharing"] = json!(true);
    user_manager::update_user_settings(&sc.state.db, "admin", settings)
        .await
        .unwrap();
}

/// Create the `default` home dir for admin and write a file into it.
fn seed_home_file(sc: &SecureClient, filename: &str, contents: &str) {
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let home = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("default");
    std::fs::create_dir_all(home.join("Desktop").join("files")).unwrap();
    std::fs::write(home.join("Desktop").join("files").join(filename), contents).unwrap();
}

// ---------------------------------------------------------------------------
// Password hashing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_argon2_password_roundtrip() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    assert!(user_manager::verify_password(&state.db, "admin", "admin1234").await.unwrap());
    assert!(!user_manager::verify_password(&state.db, "admin", "wrong-password").await.unwrap());
    assert!(!user_manager::verify_password(&state.db, "ghost", "admin1234").await.unwrap());
    // The stored hash is an argon2 hash.
    let user = user_manager::get_user(&state.db, "admin").await.unwrap().unwrap();
    let hash = user.password_hash.as_deref().unwrap();
    assert!(hash.starts_with("$argon2"), "hash: {hash}");
}

#[tokio::test]
async fn test_short_password_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    let resp = sc
        .http
        .post_json("/api/auth/set_password?target=admin", &json!({ "password": "short" }))
        .await;
    assert_eq!(resp.status, StatusCode::UNPROCESSABLE_ENTITY); // pydantic min_length=8
}

// ---------------------------------------------------------------------------
// Upload-id validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_upload_id_validation() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;

    let (status, data) = sc
        .call("POST", "/api/upload/initiate", Some(&json!({ "filename": "a.txt", "total_size": 5 })))
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    let good_id = data.as_ref().unwrap()["upload_id"].as_str().unwrap().to_string();

    // A valid id accepts chunks.
    let (status, _) = sc
        .call(
            "POST",
            "/api/upload/chunk",
            Some(&json!({
                "upload_id": good_id,
                "chunk_index": 0,
                "chunk_data_b64": base64::engine::general_purpose::STANDARD.encode(b"hello"),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Traversal / unknown ids are rejected (the id must name an existing dir).
    for bad in ["../etc", "a/b", "abc"] {
        let (status, _) = sc
            .call(
                "POST",
                "/api/upload/chunk",
                Some(&json!({
                    "upload_id": bad,
                    "chunk_index": 0,
                    "chunk_data_b64": "aGk=",
                })),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// Path traversal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_path_traversal_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_home_file(&sc, "note.txt", "hi");

    // A valid listing works.
    let (status, data) = sc
        .call("GET", "/api/files/list/default?path=Desktop/files", None)
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", data);
    let names: Vec<&str> = data
        .as_ref()
        .unwrap()
        .get("items")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["note.txt"]);

    // Traversal in the sub-path is rejected.
    let (status, _) = sc
        .call("GET", "/api/files/list/default?path=../../etc", None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Traversal in the home-dir name is rejected.
    let (status, _) = sc.call("GET", "/api/files/list/..%2Fetc", None).await;
    assert!(matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND), "{status}");
}

// ---------------------------------------------------------------------------
// Share password flow
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_share_password_flow() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    enable_public_sharing(&sc).await;
    seed_home_file(&sc, "secret.txt", "top secret");

    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({
                "home_dir": "default",
                "path": "Desktop/files/secret.txt",
                "password": "hunter2",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", info);
    let share_id = info.as_ref().unwrap()["share_id"].as_str().unwrap().to_string();
    assert_eq!(info.as_ref().unwrap()["has_password"], json!(true));
    // New shares are stored as an argon2id hash (memory-hard).
    let hash = sc
        .state
        .public_shares
        .read()
        .await
        .get(&share_id)
        .unwrap()
        .password_hash
        .clone()
        .unwrap();
    assert!(hash.starts_with("$argon2"), "hash: {hash}");

    // Wrong password -> 401.
    let resp = sc
        .http
        .request(
            "POST",
            &format!("/public/{share_id}"),
            Some(b"password=wrong".to_vec()),
            Some("application/x-www-form-urlencoded"),
            &HashMap::new(),
        )
        .await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);

    // Correct password -> one-time download token.
    let resp = sc
        .http
        .request(
            "POST",
            &format!("/public/{share_id}"),
            Some(b"password=hunter2".to_vec()),
            Some("application/x-www-form-urlencoded"),
            &HashMap::new(),
        )
        .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);
    let location = resp.header("location").unwrap();
    assert!(location.starts_with("/public/download/"), "location: {location}");

    let download = sc.http.get(&location).await;
    assert_eq!(download.status, StatusCode::OK);
    assert_eq!(download.body, b"top secret");

    // The token is single-use.
    let download2 = sc.http.get(&location).await;
    assert_eq!(download2.status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Security headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_security_headers_present() {
    // The default mode is now loose (ANT-653); this test pins the strict
    // header set, which strict mode must reproduce exactly.
    let _root = TempDir::new();
    let sc = SecureClient::new(
        build_state_with(_root.path(), |s| s.cors_mode = "strict".to_string()).await,
    );
    let resp = sc.http.get("/").await;
    assert_eq!(resp.status, StatusCode::OK);
    // CSP: strict by default — no 'unsafe-inline'.
    let csp = resp.header("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'self'"), "csp: {csp}");
    assert!(!csp.contains("'unsafe-inline'"), "csp: {csp}");
    assert!(csp.contains("worker-src 'self' blob:"), "csp: {csp}");
    assert!(csp.contains("script-src 'self' blob:"), "csp: {csp}");
    assert!(csp.contains("frame-ancestors 'self'"), "csp: {csp}");
    assert!(csp.contains("img-src 'self' data: blob:"), "csp: {csp}");
    assert!(csp.contains("https://raw.githubusercontent.com"), "csp: {csp}");
    // HSTS / nosniff / clickjacking.
    assert!(resp
        .header("strict-transport-security")
        .unwrap()
        .starts_with("max-age=63072000"));
    assert_eq!(resp.header("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(resp.header("x-frame-options").unwrap(), "SAMEORIGIN");
    // The CO* + Permissions-Policy headers.
    assert!(resp.header("permissions-policy").is_some());
    assert_eq!(resp.header("cross-origin-embedder-policy").unwrap(), "require-corp");
    assert_eq!(resp.header("cross-origin-opener-policy").unwrap(), "same-origin");
    assert_eq!(resp.header("cross-origin-resource-policy").unwrap(), "same-origin");
}

// ---------------------------------------------------------------------------
// CORS (ANT-653)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cors_loose_is_default() {
    // Out of the box the server runs loose CORS: a request carrying an
    // Origin gets it echoed back with credentials.
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let mut headers = HashMap::new();
    headers.insert("origin".to_string(), "https://other.example.com".to_string());
    let resp = sc.http.get_with("/", &headers).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header("access-control-allow-origin").as_deref(),
        Some("https://other.example.com")
    );
    assert_eq!(
        resp.header("access-control-allow-credentials").as_deref(),
        Some("true")
    );
    assert!(
        resp.header("vary")
            .map(|v| v.contains("Origin"))
            .unwrap_or(false),
        "vary: {:?}",
        resp.header("vary")
    );
    // The cross-origin-relevant headers are relaxed in loose mode.
    assert_eq!(
        resp.header("cross-origin-opener-policy").as_deref(),
        Some("same-origin-allow-popups")
    );
    assert_eq!(
        resp.header("cross-origin-resource-policy").as_deref(),
        Some("cross-origin")
    );
    let csp = resp.header("content-security-policy").unwrap();
    assert!(csp.contains("connect-src 'self' https: http: ws: wss:"), "csp: {csp}");
    // The rest of the header set is unchanged.
    assert_eq!(
        resp.header("cross-origin-embedder-policy").as_deref(),
        Some("require-corp")
    );
    assert_eq!(resp.header("x-content-type-options").as_deref(), Some("nosniff"));
    assert_eq!(resp.header("x-frame-options").as_deref(), Some("SAMEORIGIN"));
    assert!(resp
        .header("strict-transport-security")
        .unwrap()
        .starts_with("max-age=63072000"));
}

#[tokio::test]
async fn test_cors_loose_no_origin_no_headers() {
    // A request without an Origin header (same-origin, non-browser) is
    // served the mode's security headers but no CORS headers.
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.http.get("/").await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.header("access-control-allow-origin").is_none());
    assert!(resp.header("access-control-allow-credentials").is_none());
    assert!(resp.header("vary").is_none());
}

#[tokio::test]
async fn test_cors_loose_preflight_short_circuits() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let mut headers = HashMap::new();
    headers.insert("origin".to_string(), "https://other.example.com".to_string());
    headers.insert(
        "access-control-request-method".to_string(),
        "POST".to_string(),
    );
    headers.insert(
        "access-control-request-headers".to_string(),
        "X-Session-ID, Content-Type".to_string(),
    );
    // An E2EE-gated admin route: without the preflight short-circuit the
    // unauthenticated OPTIONS would reach the handler and fail the E2EE
    // body gate (400 with an encrypted body).
    let resp = sc
        .http
        .request(
            "OPTIONS",
            "/api/admin/global-settings",
            None,
            None,
            &headers,
        )
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    assert!(resp.body.is_empty(), "body: {:?}", String::from_utf8_lossy(&resp.body));
    assert_eq!(
        resp.header("access-control-allow-origin").as_deref(),
        Some("https://other.example.com")
    );
    assert_eq!(
        resp.header("access-control-allow-credentials").as_deref(),
        Some("true")
    );
    let methods = resp.header("access-control-allow-methods").unwrap();
    for m in ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
        assert!(methods.contains(m), "methods {methods} missing {m}");
    }
    let allowed_headers = resp.header("access-control-allow-headers").unwrap();
    assert!(allowed_headers.contains("X-Session-ID"), "{allowed_headers}");
    assert!(allowed_headers.contains("Content-Type"), "{allowed_headers}");
    assert!(resp.header("access-control-max-age").is_some());
    // The preflight response still carries the security headers.
    assert!(resp.header("content-security-policy").is_some());
    assert!(resp.header("strict-transport-security").is_some());
}

#[tokio::test]
async fn test_cors_loose_allowlist_env() {
    let _root = TempDir::new();
    let sc = SecureClient::new(
        build_state_with(_root.path(), |s| {
            s.cors_allowed_origins = "https://a.example.com".to_string()
        })
        .await,
    );
    // A listed origin is echoed.
    let mut headers = HashMap::new();
    headers.insert("origin".to_string(), "https://a.example.com".to_string());
    let resp = sc.http.get_with("/", &headers).await;
    assert_eq!(
        resp.header("access-control-allow-origin").as_deref(),
        Some("https://a.example.com")
    );
    // Origin comparison is case-insensitive.
    let mut headers_upper = HashMap::new();
    headers_upper.insert("origin".to_string(), "HTTPS://A.example.com".to_string());
    let resp_upper = sc.http.get_with("/", &headers_upper).await;
    assert_eq!(
        resp_upper.header("access-control-allow-origin").as_deref(),
        Some("https://a.example.com")
    );
    // An unlisted origin gets no ACAO.
    let mut headers_other = HashMap::new();
    headers_other.insert("origin".to_string(), "https://b.example.com".to_string());
    let resp_other = sc.http.get_with("/", &headers_other).await;
    assert!(resp_other.header("access-control-allow-origin").is_none());
    // ... and its preflight falls through to the route without ACAO.
    let mut preflight = HashMap::new();
    preflight.insert("origin".to_string(), "https://b.example.com".to_string());
    preflight.insert("access-control-request-method".to_string(), "POST".to_string());
    let resp_pf = sc
        .http
        .request("OPTIONS", "/api/admin/global-settings", None, None, &preflight)
        .await;
    assert!(resp_pf.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn test_cors_db_override_takes_effect_immediately() {
    // The env layer is loose (the default); a DB row saved on the admin
    // page flips the effective mode with no restart.
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let mut headers = HashMap::new();
    headers.insert("origin".to_string(), "https://other.example.com".to_string());
    sc.state
        .db
        .set_app_setting("VRECKAN_CORS_MODE", "strict")
        .await
        .unwrap();
    let strict = sc.http.get_with("/", &headers).await;
    assert!(strict.header("access-control-allow-origin").is_none());
    assert_eq!(
        strict.header("cross-origin-opener-policy").as_deref(),
        Some("same-origin")
    );
    assert_eq!(
        strict.header("cross-origin-resource-policy").as_deref(),
        Some("same-origin")
    );
    assert_eq!(
        strict.header("content-security-policy").as_deref(),
        Some(security::SECURITY_HEADERS[0].1)
    );
    sc.state
        .db
        .set_app_setting("VRECKAN_CORS_MODE", "loose")
        .await
        .unwrap();
    let loose = sc.http.get_with("/", &headers).await;
    assert_eq!(
        loose.header("access-control-allow-origin").as_deref(),
        Some("https://other.example.com")
    );
}

#[tokio::test]
async fn test_cors_strict_mode_parity() {
    // Strict mode reproduces the pre-ANT-653 behavior exactly: no CORS
    // headers on any response, even when an Origin is present, and
    // preflights reach the routes.
    let _root = TempDir::new();
    let sc = SecureClient::new(
        build_state_with(_root.path(), |s| s.cors_mode = "strict".to_string()).await,
    );
    let mut headers = HashMap::new();
    headers.insert("origin".to_string(), "https://other.example.com".to_string());
    let resp = sc.http.get_with("/", &headers).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert!(resp.header("access-control-allow-origin").is_none());
    assert!(resp.header("access-control-allow-credentials").is_none());
    assert!(resp.header("vary").is_none());
    let mut preflight = headers.clone();
    preflight.insert(
        "access-control-request-method".to_string(),
        "POST".to_string(),
    );
    let resp_pf = sc
        .http
        .request("OPTIONS", "/api/health", None, None, &preflight)
        .await;
    assert!(resp_pf.header("access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------------------
// Login cookie SameSite policy (ANT-653)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_login_cookie_same_site_none_in_loose_mode() {
    // Default (loose) mode over the default https scheme: the login cookie
    // is SameSite=None AND Secure, so it survives the cross-site hop the
    // CORS layer exists for.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.http.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "{:?}", resp.body);
    let cookie = resp.header("set-cookie").unwrap();
    assert!(cookie.contains("SameSite=None"), "cookie: {cookie}");
    assert!(cookie.contains("Secure"), "cookie: {cookie}");
    assert!(cookie.contains("HttpOnly"), "cookie: {cookie}");
}

#[tokio::test]
async fn test_login_cookie_same_site_lax_in_strict_mode() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(
        build_state_with(_root.path(), |s| s.cors_mode = "strict".to_string()).await,
    );
    let resp = sc.http.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "{:?}", resp.body);
    let cookie = resp.header("set-cookie").unwrap();
    assert!(cookie.contains("SameSite=Lax"), "cookie: {cookie}");
    assert!(!cookie.contains("SameSite=None"), "cookie: {cookie}");
}

#[tokio::test]
async fn test_login_cookie_same_site_none_never_without_secure() {
    // SameSite=None is only valid together with Secure: behind a trusted
    // proxy reporting a plain-http scheme, the cookie must fall back to
    // Lax even in loose mode (ANT-346's trusted-proxy gate decides
    // `secure`).
    let _root = TempDir::new();
    let sc = SecureClient::new(
        build_state_with(_root.path(), |s| {
            s.trusted_proxies = "10.0.0.1".to_string()
        })
        .await,
    );
    let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 51234);
    let mut headers = HashMap::new();
    headers.insert("x-forwarded-proto".to_string(), "http".to_string());
    let resp = sc
        .http
        .request_from(
            "POST",
            "/api/auth/login",
            Some(
                serde_json::json!({ "username": "admin", "password": "admin1234" })
                    .to_string()
                    .into_bytes(),
            ),
            Some("application/json"),
            &headers,
            remote,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{:?}", resp.body);
    let cookie = resp.header("set-cookie").unwrap();
    assert!(cookie.contains("SameSite=Lax"), "cookie: {cookie}");
    assert!(!cookie.contains("SameSite=None"), "cookie: {cookie}");
    assert!(!cookie.contains("Secure"), "cookie: {cookie}");
}

// ---------------------------------------------------------------------------
// Vendored Font Awesome (ANT-570)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_room_font_awesome_vendored() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);

    // The room page links the vendored stylesheet, not the cdnjs copy that
    // the strict CSP (`style-src 'self'`) used to block.
    let page = sc.http.get("/collaboration/room.html").await;
    assert_eq!(page.status, StatusCode::OK);
    let html = String::from_utf8_lossy(&page.body);
    assert!(!html.contains("cdnjs.cloudflare.com"), "room.html: {html}");
    assert!(
        html.contains(
            r#"<link rel="stylesheet" href="/collaboration/font-awesome/css/all.min.css">"#
        ),
        "room.html: {html}"
    );

    // The stylesheet is served from the static tree ('self' under both CSPs).
    let css = sc
        .http
        .get("/collaboration/font-awesome/css/all.min.css")
        .await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.header("content-type").unwrap(), "text/css");
    assert!(String::from_utf8_lossy(&css.body).contains("Font Awesome Free 6.4.0"));

    // The solid webfont resolves from the CSS's relative `../webfonts/` path
    // and is served as `font/woff2` ('self' under default-src).
    let font = sc
        .http
        .get("/collaboration/font-awesome/webfonts/fa-solid-900.woff2")
        .await;
    assert_eq!(font.status, StatusCode::OK);
    assert_eq!(font.header("content-type").unwrap(), "font/woff2");
    assert_eq!(&font.body[..4], b"wOF2");
}

// ---------------------------------------------------------------------------
// Password page strict CSP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_password_page_strict_csp() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    enable_public_sharing(&sc).await;
    seed_home_file(&sc, "secret.txt", "top secret");

    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({
                "home_dir": "default",
                "path": "Desktop/files/secret.txt",
                "password": "pw",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", info);
    let share_id = info.as_ref().unwrap()["share_id"].as_str().unwrap().to_string();

    let resp = sc.http.get(&format!("/public/{share_id}")).await;
    assert_eq!(resp.status, StatusCode::OK);
    let csp = resp.header("content-security-policy").unwrap();
    // Strict: script-src has a nonce and NO 'unsafe-inline' before style-src.
    let before_style = csp.split("style-src").next().unwrap();
    assert!(!before_style.contains("'unsafe-inline'"), "csp: {csp}");
    assert!(csp.contains("script-src 'self' 'nonce-"), "csp: {csp}");
    // The nonce in the header matches the one injected into the page.
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap()
        .to_string();
    let body = String::from_utf8_lossy(&resp.body).to_string();
    assert!(body.contains(&format!("nonce=\"{nonce}\"")), "nonce not in page");
    assert!(body.contains("<style nonce="), "no <style nonce=");
    assert!(body.contains("<script nonce="), "no <script nonce=");
}

// ---------------------------------------------------------------------------
// Session proxy loose CSP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_session_proxy_gets_loose_csp() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);

    // Stand up a fake upstream and point the session at it.
    let upstream_host = spawn_fake_upstream().await.0;
    let (ip, port) = upstream_host.rsplit_once(':').unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "access_token": "tok-123",
        "username": "admin",
    });
    sc.state.sessions.write().await.insert(sid.clone(), session);

    // First hit with the access token: 303 redirect + session cookie.
    let resp = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?access_token=tok-123"))
        .await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER);

    // Seed the session cookie manually to exercise the proxied (cookie) path.
    let cookie_name = format!("{}_{}", sc.state.settings.session_cookie_name, sid);
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), format!("{cookie_name}=tok-123"));

    // Second hit with the cookie: reverse-proxied, and must carry the loose
    // session CSP (with 'unsafe-inline'), not the strict default.
    let resp = sc
        .http
        .request("GET", &format!("/api/apps/session/{sid}/"), None, None, &headers)
        .await;
    assert_eq!(resp.status, StatusCode::OK);
    let csp = resp.header("content-security-policy").unwrap();
    assert_eq!(csp, security::SESSION_CSP);
    assert!(csp.contains("'unsafe-inline'"));

    // The selkies audio pipeline loads its Playback AudioWorklet from a
    // data:text/javascript URL, so the session CSP must allow data: in both
    // script-src and worker-src (ANT-537) or the worklet is blocked and
    // in-container audio dies.
    let directive = |name: &str| {
        let rest = csp.split(name).nth(1).unwrap_or("");
        rest.split(';').next().unwrap_or("")
    };
    assert!(
        directive("script-src").contains("data:"),
        "session CSP script-src must allow data: (got: {csp})"
    );
    assert!(
        directive("worker-src").contains("data:"),
        "session CSP worker-src must allow data: (got: {csp})"
    );
}

// ---------------------------------------------------------------------------
// E2EE request decryption: IV length guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2ee_malformed_iv_returns_400_not_500() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    // The decoded IV must be exactly 12 bytes; any other length must be a
    // 400 (before the fix, `nonce.copy_from_slice(&iv)` panicked -> 500).
    for len in [0usize, 1, 11, 13, 16, 64] {
        let resp = sc
            .post_with_iv_len(
                "/api/files/create_folder/default",
                &json!({ "path": "/", "folder_name": "x" }),
                len,
            )
            .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "iv_len={len}: {:?}", resp.body);
    }
    // The happy path is unaffected (home dir created first, as in test_files).
    vreckan::user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let (status, data) = sc
        .call(
            "POST",
            "/api/files/create_folder/default",
            Some(&json!({ "path": "/", "folder_name": "ok-folder" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{data:?}");
}
