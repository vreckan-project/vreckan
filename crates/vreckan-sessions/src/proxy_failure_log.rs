//! Rate limiting for the session proxy's upstream-connect failure logs
//! (ANT-1214, follow-up to the 2026-11-17 staging incident ANT-1211).
//!
//! In the incident a client held against a dead upstream (selkies
//! crash-looping) retried ~1/sec, and every failed upstream connect logged a
//! WARN — one line per second, indefinitely. These helpers move both
//! upstream-connect failure sites (the HTTP and the WebSocket leg of
//! `session_proxy`) behind a per-session, per-error-class limiter:
//!
//! - the **first** failure of a class logs at full detail (the original
//!   `warn!` shape);
//! - repeats of the same class are **suppressed**;
//! - one **cumulative** line is emitted per `CUMULATIVE_WINDOW` (~30s):
//!   `upstream not ready: N failed connects since <first> (last: <detail>)`;
//! - the next **success** logs one recovery line with the total count and
//!   resets (removes) the entry.
//!
//! The upstream **404** gets its own class and an explicit message: it is the
//! missing-`50x.html` artifact (the container's nginx 502s the dead selkies,
//! and the LSIO image's `error_page 502 ... /50x.html` points at a file that
//! doesn't exist in the image, so the 502 surfaces as a 404). What the client
//! receives is unchanged (still 502 / a closed WS) — only the logging changes.
//!
//! State lives in `AppState.proxy_upstream_failure_logs` (in-memory only,
//! session id -> error class -> accumulator); `prune_session` removes a
//! session's entries on kill so the map cannot grow unbounded.

use std::time::{Duration, Instant};

use vreckan_core::shared::ProxyFailureLogEntry;
use vreckan_core::state::AppState;

/// How often a suppressed failure stream emits its cumulative line.
pub const CUMULATIVE_WINDOW: Duration = Duration::from_secs(30);

/// Classify an upstream-connect failure into the limiter's error class
/// (ANT-1214): a transport error (no HTTP status was received — connect
/// refused/timeout), the upstream 404 artifact, or any other upstream
/// status.
pub fn failure_class(status: Option<u16>) -> &'static str {
    match status {
        None => "transport",
        Some(404) => "upstream-404",
        Some(_) => "upstream-other-status",
    }
}

/// The human-readable detail for a failure (ANT-1214): the 404 class is
/// labeled as the masquerading-502 artifact; everything else keeps the raw
/// detail. Log text only — the client-visible response is untouched.
pub fn failure_detail(status: Option<u16>, detail: &str) -> String {
    if status == Some(404) {
        format!("{detail} — upstream not ready (nginx 502 masquerading as 404 — the LSIO image has no 50x.html)")
    } else {
        detail.to_string()
    }
}

/// Render the cumulative line for an accumulator: how many failures of this
/// class since the first, how long the upstream has been failing, and the
/// last detail ("since Ns" = the first failure was Ns ago; the log line's
/// own timestamp makes it recoverable without a wall clock here).
pub fn cumulative_line(entry: &ProxyFailureLogEntry) -> String {
    format!(
        "upstream not ready: {} failed connects since {}s (last: {})",
        entry.count,
        entry.first_at.elapsed().as_secs(),
        entry.last_detail
    )
}

/// Record one upstream-connect failure for `session_id` (ANT-1214).
/// `status` is the upstream's HTTP status when one was received (the
/// WebSocket path extracts it from tungstenite's `Error::Http`), `None` for
/// a transport error; `detail` is the raw error text; `first_failure_label`
/// is the leg wording the original warns used ("Proxy connect" for the HTTP
/// path, "WebSocket connect" for the WS path); `now` is the observation
/// time (injected so tests control the clock).
///
/// Returns the `warn!` line to emit, or `None` when the failure is
/// suppressed (a repeat of a class inside the window). The first failure of
/// a class always logs at full detail in the original warn shape
/// (`[{session_id}] {label} to {host} failed: {detail}`); a cumulative line
/// is emitted at most once per `CUMULATIVE_WINDOW`.
pub async fn record_failure(
    state: &AppState,
    session_id: &str,
    upstream_host: &str,
    status: Option<u16>,
    detail: &str,
    first_failure_label: &str,
    now: Instant,
) -> Option<String> {
    let class = failure_class(status);
    let message = failure_detail(status, detail);
    let mut logs = state.proxy_upstream_failure_logs.write().await;
    let entries = logs.entry(session_id.to_string()).or_default();
    match entries.get_mut(class) {
        None => {
            let line =
                format!("[{session_id}] {first_failure_label} to {upstream_host} failed: {message}");
            entries.insert(
                class.to_string(),
                ProxyFailureLogEntry {
                    class: class.to_string(),
                    count: 1,
                    first_at: now,
                    last_logged_at: now,
                    last_detail: message,
                },
            );
            Some(line)
        }
        Some(entry) => {
            entry.count += 1;
            entry.last_detail = message;
            if now.duration_since(entry.last_logged_at) >= CUMULATIVE_WINDOW {
                let line = cumulative_line(entry);
                entry.last_logged_at = now;
                Some(format!("[{session_id}] {line}"))
            } else {
                None
            }
        }
    }
}

/// Record a successful upstream connect for `session_id` (ANT-1214): log one
/// recovery line with the total count per class, and reset (remove) the
/// entries so a later failure starts fresh and the map cannot grow
/// unbounded. Returns the `info!` line to emit, or `None` when there was
/// nothing to recover from.
pub async fn record_success(state: &AppState, session_id: &str) -> Option<String> {
    let mut logs = state.proxy_upstream_failure_logs.write().await;
    let entries = logs.remove(session_id)?;
    let total: u64 = entries.values().map(|e| e.count).sum();
    let oldest = entries.values().map(|e| e.first_at).min()?;
    let detail: Vec<String> = entries.values().map(|e| e.last_detail.clone()).collect();
    Some(format!(
        "[{session_id}] upstream connect recovered after {} failed connect(s) (first failure {}s ago): {}",
        total,
        oldest.elapsed().as_secs(),
        detail.join("; ")
    ))
}

/// Remove a session's limiter entries (ANT-1214). Called on session kill so
/// a killed session cannot leave accumulators behind; also the second
/// pruning path besides `record_success`.
pub async fn prune_session(state: &AppState, session_id: &str) {
    state
        .proxy_upstream_failure_logs
        .write()
        .await
        .remove(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_state() -> AppState {
        let root = std::env::temp_dir().join(format!("vreckan-proxylog-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut settings = vreckan_core::settings::Settings::from_env();
        settings.database_url = format!("sqlite://{}/test.db?mode=rwc", root.display());
        settings.storage_path = root.join("storage").to_string_lossy().to_string();
        let db = vreckan_core::db::connect(&settings.database_url).await.unwrap();
        vreckan_core::state::AppState::new(settings, db)
    }


    #[tokio::test]
    async fn first_failure_logs_full_detail() {
        let state = test_state().await;
        let t0 = Instant::now();
        let line = record_failure(
            &state,
            "s-1",
            "10.0.0.5:3000",
            Some(404),
            "404 Not Found",
            "Proxy connect",
            t0,
        )
        .await
        .expect("first failure must log");
        assert!(line.contains("[s-1]"), "line: {line}");
        assert!(line.contains("10.0.0.5:3000"), "line: {line}");
        assert!(line.contains("404 Not Found"), "line: {line}");
        assert!(
            line.contains("nginx 502 masquerading as 404"),
            "the 404 artifact must be identified: {line}"
        );
    }

    #[tokio::test]
    async fn repeats_within_the_window_are_suppressed() {
        let state = test_state().await;
        let t0 = Instant::now();
        for offset in [0u64, 5, 10, 20] {
            let got = record_failure(
                &state,
                "s-1",
                "10.0.0.5:3000",
                None,
                "connection refused",
                "Proxy connect",
                t0.checked_add(Duration::from_secs(offset)).unwrap(),
            )
            .await;
            if offset == 0 {
                assert!(got.is_some(), "first failure logs");
            } else {
                assert!(got.is_none(), "repeat within the window is suppressed (at +{offset}s)");
            }
        }
    }

    #[tokio::test]
    async fn cumulative_line_after_the_window() {
        let state = test_state().await;
        let t0 = Instant::now();
        // 4 failures: first at t0 (logs), then at t+10, t+20 (suppressed),
        // t+31 (past the 30s window: cumulative line).
        for (i, offset) in [0u64, 10, 20, 31].into_iter().enumerate() {
            let got = record_failure(
                &state,
                "s-1",
                "10.0.0.5:3000",
                Some(502),
                "502 Bad Gateway",
                "Proxy connect",
                t0.checked_add(Duration::from_secs(offset)).unwrap(),
            )
            .await;
            match i {
                0 => assert!(got.is_some(), "first failure logs"),
                1 | 2 => assert!(got.is_none(), "suppressed at +{offset}s"),
                _ => {
                    let line = got.expect("cumulative line at +31s");
                    // The count includes the failure that triggered the line
                    // (4 total: t0, t+10, t+20, t+31).
                    assert!(line.contains("4 failed connects since"), "line: {line}");
                    assert!(line.contains("502 Bad Gateway"), "line: {line}");
                    assert!(line.contains("upstream not ready"), "line: {line}");
                }
            }
        }
    }

    #[tokio::test]
    async fn distinct_error_classes_track_independently() {
        let state = test_state().await;
        let t0 = Instant::now();
        // The first failure of each class logs at full detail...
        let t404 = record_failure(&state, "s-1", "h:3000", Some(404), "404", "Proxy connect", t0).await;
        let t_transport = record_failure(&state, "s-1", "h:3000", None, "refused", "WebSocket connect", t0).await;
        assert!(t404.is_some() && t_transport.is_some(), "first of each class logs");
        // ...and the next failure of a class already seen is suppressed even
        // though another class is still "new".
        let again_404 = record_failure(&state, "s-1", "h:3000", Some(404), "404", "Proxy connect", t0.checked_add(Duration::from_secs(5)).unwrap()).await;
        assert!(again_404.is_none());
    }

    #[tokio::test]
    async fn success_logs_recovery_and_resets() {
        let state = test_state().await;
        let t0 = Instant::now();
        record_failure(&state, "s-1", "h:3000", None, "refused", "Proxy connect", t0).await;
        record_failure(&state, "s-1", "h:3000", None, "refused", "Proxy connect", t0.checked_add(Duration::from_secs(5)).unwrap()).await;
        let recovery = record_success(&state, "s-1").await.expect("recovery line");
        assert!(recovery.contains("recovered"), "line: {recovery}");
        assert!(recovery.contains("2"), "line: {recovery}");
        // The map is reset: nothing left to recover from...
        assert!(record_success(&state, "s-1").await.is_none());
        // ...and a later failure starts a fresh counter (first-failure log).
        let fresh = record_failure(&state, "s-1", "h:3000", None, "refused", "Proxy connect", t0.checked_add(Duration::from_secs(40)).unwrap()).await;
        assert!(fresh.is_some(), "failure after recovery logs at full detail again");
        assert!(fresh.unwrap().contains("Proxy connect to h:3000 failed"), "fresh first-failure shape");
    }

    #[tokio::test]
    async fn success_without_prior_failures_is_silent() {
        let state = test_state().await;
        assert!(record_success(&state, "s-2").await.is_none());
    }

    #[tokio::test]
    async fn prune_session_removes_entries() {
        let state = test_state().await;
        let t0 = Instant::now();
        record_failure(&state, "s-1", "h:3000", None, "refused", "WebSocket connect", t0).await;
        prune_session(&state, "s-1").await;
        assert!(state.proxy_upstream_failure_logs.read().await.get("s-1").is_none());
        // After a prune, the next failure logs at full detail again.
        let line = record_failure(&state, "s-1", "h:3000", None, "refused", "WebSocket connect", t0.checked_add(Duration::from_secs(1)).unwrap())
            .await
            .expect("post-prune failure logs at full detail");
        assert!(line.contains("WebSocket connect to h:3000 failed"), "line: {line}");
    }

    #[test]
    fn failure_classes_are_stable() {
        assert_eq!(failure_class(None), "transport");
        assert_eq!(failure_class(Some(404)), "upstream-404");
        assert_eq!(failure_class(Some(502)), "upstream-other-status");
        assert_eq!(failure_class(Some(301)), "upstream-other-status");
    }
}
