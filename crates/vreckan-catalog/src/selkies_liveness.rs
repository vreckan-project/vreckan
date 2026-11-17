//! Data-plane liveness check for freshly launched selkies sessions
//! (ANT-1214, follow-up to the 2026-11-17 staging incident ANT-1211).
//!
//! The capability probe (`catalog::probe_selkies_capability`) resolves a
//! *crash-looping* selkies as `Secure`: step 1 (`GET /api/sessions`) answers
//! 404 text/html while selkies is down (inconclusive), and step 2
//! (`selkies --version`) passes whenever the *binary* exists — it proves
//! nothing about the *process*. In the incident that combination marked the
//! session `ready` while the container's nginx turned every WebSocket
//! reconnect into a 404 (the LSIO image's `error_page 502 ... /50x.html`
//! points at a file that doesn't exist in the image, so the 502 for the dead
//! upstream surfaces as a 404), and clients looped on it indefinitely.
//!
//! A freshly launched plain session whose probe resolved `Secure` must
//! therefore prove its data plane is serving before the readiness task marks
//! the session ready: a server-side WebSocket upgrade to selkies'
//! `/api/websockets?token={master_token}` must complete (101) within a short
//! retry budget.

use std::time::{Duration, Instant};

use base64::Engine;
use tokio_tungstenite::connect_async;

use vreckan_core::shared::SelkiesLivenessHook;
use vreckan_core::state::AppState;

/// The outcome of one WebSocket upgrade attempt (ANT-1214).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessOutcome {
    /// The upstream answered the upgrade request with an HTTP response.
    /// `status == 101` means the upgrade completed; anything else is the
    /// non-101 response tungstenite surfaces in its `Error::Http` variant.
    Response {
        status: u16,
        content_type: Option<String>,
    },
    /// The connection could not be established at all (refused, timeout,
    /// protocol failure during the handshake, ...).
    Transport(String),
}

/// The verdict of one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LivenessVerdict {
    /// selkies is serving the data plane: the upgrade completed (101), or
    /// selkies' own token API answered (401/403 with a JSON or plain-text
    /// body — a token-table race; the push is idempotent and
    /// `apply_selkies_probe` already retried it).
    Up,
    /// selkies is not serving: the nginx 502 masquerading as a 404
    /// text/html (the LSIO image has no `50x.html`), a 502/503/504, or a
    /// transport error. Carries a short human-readable reason.
    NotUp(String),
}

/// Classify one attempt's outcome (see the module docs for the incident that
/// motivated the rules).
pub fn classify_liveness(outcome: &LivenessOutcome) -> LivenessVerdict {
    match outcome {
        LivenessOutcome::Response { status, content_type } => {
            let ct = content_type.as_deref().unwrap_or("");
            match status {
                101 => LivenessVerdict::Up,
                // selkies' token API is answering: a JSON 401/403 is the
                // token-table race (the owner entry has not landed yet) and a
                // text/plain 401 is selkies' own secure-mode "Unauthorized".
                // Either way the desktop service is up.
                401 | 403 if ct.contains("application/json") || ct.contains("text/plain") => {
                    LivenessVerdict::Up
                }
                // Any other 401/403 is nginx's (basic-auth failures answer
                // text/html): the container's HTTP layer, not selkies.
                401 | 403 => {
                    LivenessVerdict::NotUp(format!("upstream {status} (nginx, not selkies)"))
                }
                // The missing-50x.html artifact: nginx's `error_page 502 ...
                // /50x.html` renders a 404 when the upstream is down.
                404 => LivenessVerdict::NotUp(
                    if ct.contains("text/html") {
                        "upstream 404 text/html (nginx 502 masquerading as 404 — the LSIO image has no 50x.html)"
                            .to_string()
                    } else {
                        format!("upstream {status} on the WebSocket upgrade")
                    },
                ),
                500..=599 => LivenessVerdict::NotUp(format!("upstream {status} on the WebSocket upgrade")),
                other => LivenessVerdict::NotUp(format!("upstream {other} on the WebSocket upgrade")),
            }
        }
        LivenessOutcome::Transport(detail) => LivenessVerdict::NotUp(format!("transport error: {detail}")),
    }
}

/// The per-attempt ceiling: a single upgrade attempt must not hang the
/// readiness task (the overall budget below bounds the loop too).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);

/// One WebSocket upgrade attempt against the session's selkies data plane.
/// Builds the handshake exactly the way the session proxy builds its
/// upstream leg (the five headers tungstenite's `generate_request` requires)
/// and adds the container's nginx basic-auth credential when set.
async fn liveness_attempt(
    ip: &str,
    port: i64,
    session_id: &str,
    master_token: &str,
    auth_header: Option<&str>,
) -> LivenessOutcome {
    let url = format!(
        "ws://{ip}:{port}/api/apps/session/{session_id}/api/websockets?token={master_token}"
    );
    let mut request = http::Request::builder()
        .method(http::Method::GET)
        .uri(&url)
        .body(())
        .unwrap_or_else(|_| http::Request::new(()));
    let headers = request.headers_mut();
    if let Ok(host) = http::HeaderValue::from_str(&format!("{ip}:{port}")) {
        headers.insert(http::header::HOST, host);
    }
    let ws_key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
    if let Ok(key_value) = http::HeaderValue::from_str(&ws_key) {
        headers.insert(http::header::SEC_WEBSOCKET_KEY, key_value);
    }
    headers.insert(http::header::CONNECTION, http::HeaderValue::from_static("upgrade"));
    headers.insert(http::header::UPGRADE, http::HeaderValue::from_static("websocket"));
    headers.insert(http::header::SEC_WEBSOCKET_VERSION, http::HeaderValue::from_static("13"));
    if let Some(auth) = auth_header {
        if let Ok(auth_value) = http::HeaderValue::from_str(auth) {
            headers.insert(http::header::AUTHORIZATION, auth_value);
        }
    }

    match tokio::time::timeout(ATTEMPT_TIMEOUT, connect_async(request)).await {
        Ok(Ok((_ws, _response))) => LivenessOutcome::Response {
            status: 101,
            content_type: None,
        },
        // A non-101 answer comes back boxed in tungstenite's `Error::Http`;
        // unwrap it to the `http::Response` it carries.
        Ok(Err(tokio_tungstenite::tungstenite::Error::Http(boxed))) => LivenessOutcome::Response {
            status: boxed.status().as_u16(),
            content_type: boxed
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        },
        Ok(Err(e)) => LivenessOutcome::Transport(e.to_string()),
        Err(_) => LivenessOutcome::Transport("WebSocket upgrade attempt timed out".to_string()),
    }
}

/// Wait for the session's selkies data plane to serve the WebSocket upgrade,
/// retrying with short backoff (0.5s doubling to a 2s cap) until `budget`
/// elapses. Returns the attempt number at which the plane came up, or `None`
/// when the budget is exhausted without an `Up` verdict.
///
/// Tests set the `fake_selkies_liveness` hook on `state` to record an
/// outcome in place of the live dial (mirrors the `fake_selkies_probe`
/// hook): the recorded `up_after` is returned as-is, so a recorded failure
/// returns immediately instead of burning the real budget.
pub async fn wait_for_data_plane(
    state: &AppState,
    ip: &str,
    port: i64,
    session_id: &str,
    master_token: &str,
    auth_header: Option<&str>,
    budget: Duration,
) -> Option<usize> {
    if let Some(hook) = state.fake_selkies_liveness.read().await.clone() {
        return hook_from_record(session_id, hook);
    }

    let start = Instant::now();
    let mut attempt = 0usize;
    let mut backoff = Duration::from_millis(500);
    while start.elapsed() < budget {
        attempt += 1;
        let outcome = liveness_attempt(ip, port, session_id, master_token, auth_header).await;
        match classify_liveness(&outcome) {
            LivenessVerdict::Up => {
                if attempt > 1 {
                    tracing::info!(
                        "[{session_id}] Data-plane liveness check passed on attempt {attempt}."
                    );
                }
                return Some(attempt);
            }
            LivenessVerdict::NotUp(detail) => {
                tracing::info!(
                    "[{session_id}] Data-plane liveness attempt {attempt} not up: {detail}"
                );
                let wait = backoff;
                backoff = (backoff * 2).min(Duration::from_secs(2));
                // A final attempt that would overshoot the budget is not
                // worth its sleep.
                if start.elapsed() + wait >= budget {
                    break;
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
    tracing::warn!(
        "[{session_id}] Data-plane liveness check exhausted its {}s budget after {attempt} attempt(s); selkies is not serving.",
        budget.as_secs()
    );
    None
}

/// Map a recorded hook onto the loop's return shape (test seam).
fn hook_from_record(session_id: &str, hook: SelkiesLivenessHook) -> Option<usize> {
    match hook.up_after {
        Some(attempts) => {
            tracing::info!(
                "[{session_id}] Data-plane liveness check (recorded hook): up after {attempts} attempt(s)."
            );
            Some(attempts)
        }
        None => {
            let detail = hook
                .detail
                .unwrap_or_else(|| "data-plane liveness check failed (recorded)".to_string());
            tracing::warn!("[{session_id}] Data-plane liveness check (recorded hook): {detail}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn response(status: u16, content_type: &str) -> LivenessOutcome {
        LivenessOutcome::Response {
            status,
            content_type: Some(content_type.to_string()),
        }
    }

    #[test]
    fn classifies_upgrade_success_as_up() {
        assert_eq!(
            classify_liveness(&LivenessOutcome::Response {
                status: 101,
                content_type: None
            }),
            LivenessVerdict::Up
        );
    }

    #[test]
    fn classifies_json_auth_replies_as_token_table_race() {
        for status in [401u16, 403] {
            assert_eq!(classify_liveness(&response(status, "application/json")), LivenessVerdict::Up);
            // Some stacks declare the media type with a charset.
            assert_eq!(
                classify_liveness(&response(status, "application/json; charset=utf-8")),
                LivenessVerdict::Up
            );
        }
    }

    #[test]
    fn classifies_selkies_plain_text_401_as_up() {
        // selkies' own secure-mode 401 answers text/plain ("Unauthorized").
        assert_eq!(classify_liveness(&response(401, "text/plain")), LivenessVerdict::Up);
    }

    #[test]
    fn classifies_nginx_auth_replies_as_not_up() {
        // nginx's basic-auth failure answers text/html — the container's HTTP
        // layer, not selkies.
        for status in [401u16, 403] {
            assert!(matches!(
                classify_liveness(&response(status, "text/html")),
                LivenessVerdict::NotUp(_)
            ));
        }
    }

    #[test]
    fn classifies_missing_50x_html_404_as_not_up() {
        // The incident artifact: the dead-selkies 502 masquerading as a 404.
        let verdict = classify_liveness(&response(404, "text/html"));
        match verdict {
            LivenessVerdict::NotUp(detail) => assert!(
                detail.contains("masquerading as 404"),
                "expected the masquerading-502 explanation, got: {detail}"
            ),
            other => panic!("expected NotUp, got {other:?}"),
        }
    }

    #[test]
    fn classifies_5xx_as_not_up() {
        for status in [502u16, 503, 504] {
            assert!(
                matches!(classify_liveness(&response(status, "text/html")), LivenessVerdict::NotUp(_))
            );
        }
    }

    #[test]
    fn classifies_transport_errors_as_not_up() {
        let verdict = classify_liveness(&LivenessOutcome::Transport("connection refused".into()));
        assert!(matches!(verdict, LivenessVerdict::NotUp(ref d) if d.contains("connection refused")));
    }

    /// A tiny in-test WebSocket server: for the first `failures` connections
    /// it answers the upgrade request with a raw 404 text/html (the
    /// missing-50x.html artifact), then accepts real upgrades
    /// (`accept_async`) until `total` connections have been served.
    async fn spawn_flaky_ws_server(
        failures: usize,
        total: usize,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                served += 1;
                if served <= failures {
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 14\r\nConnection: close\r\n\r\n404 Not Found\n",
                        )
                        .await;
                } else {
                    // The handshake bytes are flushed before `accept_async`
                    // returns, so the client has already completed its side;
                    // dropping the upgraded stream just closes the socket.
                    if let Ok(upgraded) = tokio_tungstenite::accept_async(stream).await {
                        drop(upgraded);
                    }
                }
                // Serve exactly `total` connections, then stop: a break before
                // serving would drop the client's `total`-th (success) connect,
                // and a bare `accept()` after it would park forever.
                if served >= total {
                    break;
                }
            }
        });
        (addr, handle)
    }

    async fn test_state() -> AppState {
        let root = std::env::temp_dir().join(format!("vreckan-liveness-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = vreckan_core::settings::Settings::from_env();
        settings.database_url = format!("sqlite://{}/test.db?mode=rwc", root.display());
        settings.storage_path = root.join("storage").to_string_lossy().to_string();
        let db = vreckan_core::db::connect(&settings.database_url).await.unwrap();
        vreckan_core::state::AppState::new(settings, db)
    }

    #[tokio::test]
    async fn hook_up_after_returns_immediately() {
        let state = test_state().await;
        *state
            .fake_selkies_liveness
            .write()
            .await = Some(SelkiesLivenessHook {
            up_after: Some(2),
            detail: None,
        });
        let started = Instant::now();
        let got = wait_for_data_plane(&state, "127.0.0.1", 1, "s-1", "tok", None, Duration::from_secs(15)).await;
        assert_eq!(got, Some(2));
        assert!(started.elapsed() < Duration::from_millis(500), "hook path must not burn the budget");
    }

    #[tokio::test]
    async fn hook_exhausted_returns_none_immediately() {
        let state = test_state().await;
        *state
            .fake_selkies_liveness
            .write()
            .await = Some(SelkiesLivenessHook {
            up_after: None,
            detail: Some("upstream 404".into()),
        });
        let got = wait_for_data_plane(&state, "127.0.0.1", 1, "s-1", "tok", None, Duration::from_secs(15)).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn retries_through_404s_until_the_plane_comes_up() {
        let state = test_state().await;
        let (addr, handle) = spawn_flaky_ws_server(2, 3).await;
        let got =
            wait_for_data_plane(&state, "127.0.0.1", addr.port() as i64, "s-1", "tok", None, Duration::from_secs(10)).await;
        assert_eq!(got, Some(3), "up on the 3rd attempt (two 404s first)");
        // Abort the server: it is now parked on `accept()` waiting for more
        // connections than the client will make, so a bare await would hang.
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn exhausted_budget_returns_none() {
        let state = test_state().await;
        let (addr, handle) = spawn_flaky_ws_server(10, 10).await;
        let started = Instant::now();
        let got =
            wait_for_data_plane(&state, "127.0.0.1", addr.port() as i64, "s-1", "tok", None, Duration::from_millis(900)).await;
        assert_eq!(got, None);
        assert!(started.elapsed() < Duration::from_secs(3), "budget must bound the loop");
        // The client stops after a couple of attempts, well short of the 10
        // connections the server would serve, so it parks on `accept()` —
        // abort it rather than await it into a hang.
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn connection_refused_counts_as_not_up() {
        let state = test_state().await;
        // Bind then drop a listener to find a port nothing is listening on.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let got =
            wait_for_data_plane(&state, "127.0.0.1", port as i64, "s-1", "tok", None, Duration::from_millis(700)).await;
        assert_eq!(got, None);
    }
}
