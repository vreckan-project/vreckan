//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Session proxy request-body streaming (audit finding 15, ANT-886): the
//! proxy forwards client request bodies as a stream to the app container
//! instead of collecting them into a 1 GiB buffer. The loopback upstream
//! echoes the body back verbatim and reports how the request arrived
//! (chunked stream vs length-framed), so the test proves the round-trip is
//! byte-for-byte and the framing is a stream.

mod common;

use std::collections::HashMap;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use vreckan_core::state::AppState;

/// A throwaway upstream that echoes the request body back verbatim and
/// reports the request framing via `X-Saw-Framing`: `chunked` when the
/// request arrived as a chunked stream (no Content-Length),
/// `content-length` when it was length-framed, `none` otherwise. A
/// pre-chunked request body is read chunk-by-chunk; a length-framed one by
/// its declared length; both paths terminate the connection after the
/// response (the proxy's reqwest client opens a fresh connection either way).
async fn spawn_echo_upstream() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        loop {
            let (socket, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            tokio::spawn(async move {
                let mut io = BufReader::new(socket);
                // Read the request head (request line + headers).
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                        break;
                    }
                    head.push_str(&line);
                    if line.trim_end().is_empty() {
                        break;
                    }
                }
                let head_l = head.to_ascii_lowercase();
                let framing = if head_l.contains("transfer-encoding: chunked") {
                    "chunked"
                } else if head_l.contains("content-length:") {
                    "content-length"
                } else {
                    "none"
                };
                let mut body = Vec::new();
                if framing == "chunked" {
                    loop {
                        let mut size_line = String::new();
                        if io.read_line(&mut size_line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                        if size == 0 {
                            break;
                        }
                        let mut chunk = vec![0u8; size];
                        if io.read_exact(&mut chunk).await.is_err() {
                            break;
                        }
                        let mut crlf = [0u8; 2];
                        let _ = io.read_exact(&mut crlf).await;
                        body.extend_from_slice(&chunk);
                    }
                    // Consume the (empty) trailer section.
                    loop {
                        let mut line = String::new();
                        if io.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        if line.trim_end().is_empty() {
                            break;
                        }
                    }
                } else if framing == "content-length" {
                    let len = head_l
                        .split("content-length:")
                        .nth(1)
                        .and_then(|s| s.split_whitespace().next())
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut buf = vec![0u8; len];
                    if io.read_exact(&mut buf).await.is_ok() {
                        body.extend_from_slice(&buf);
                    }
                }
                let header = format!(
                    "HTTP/1.1 200 OK\r\nX-Saw-Framing: {framing}\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                );
                let mut resp = header.into_bytes();
                resp.extend_from_slice(&body);
                let mut out = io.into_inner();
                let _ = out.write_all(&resp).await;
                let _ = out.shutdown().await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), handle)
}

/// Seed a plain (non-collaboration) session owned by the bootstrap admin,
/// pointed at `upstream` (host:port).
async fn seed_plain_session(state: &AppState, upstream: &str) -> (String, String) {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": access_token,
        "is_collaboration": false,
        "username": "admin",
        "app_name": "Test App",
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    (sid, access_token)
}

/// A multi-KiB POST body round-trips the session proxy byte-for-byte, and
/// the upstream received it as a chunked stream rather than a collected,
/// length-framed buffer (audit finding 15).
#[tokio::test]
async fn test_proxy_request_body_is_streamed() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.login("admin", "admin1234").await;
    sc.handshake().await;
    let (upstream, _u) = spawn_echo_upstream().await;
    let (sid, access_token) = seed_plain_session(&sc.state, &upstream).await;
    // Owner traffic is admitted via the per-session cookie.
    let cookie = format!(
        "{}_{sid}={access_token}",
        sc.state.settings.session_cookie_name
    );
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), cookie);

    let size = 1024 * 1024;
    let payload: Vec<u8> = (0..size as u64).map(|i| (i % 251) as u8).collect();
    let resp = sc
        .http
        .request(
            "POST",
            &format!("/api/apps/session/{sid}/api/upload"),
            Some(payload.clone()),
            Some("application/octet-stream"),
            &headers,
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "proxy: {:?}", &resp.body[..64.min(resp.body.len())]);
    assert_eq!(
        resp.body, payload,
        "the proxied request body must round-trip byte-for-byte"
    );
    assert_eq!(
        resp.header("x-saw-framing").as_deref(),
        Some("chunked"),
        "the request body must be forwarded as a stream, not a collected buffer: {:?}",
        resp.headers
    );
}
