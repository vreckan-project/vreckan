//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tier 4 — anonymous share-password brute-force limiter (ANT-884, audit
//! finding 13).
//!
//! The `POST /public/{share_id}` route is anonymous: no auth, no caller
//! identity. Before the fix, every wrong guess ran a ~100-300 ms / ~19 MiB
//! Argon2id verification synchronously on a tokio worker thread, with no
//! bound — one leaked share link was a cheap availability attack on the whole
//! instance. These tests pin the fix: a per-share failure budget
//! (`share_password_max_attempts`) with a temporary lockout
//! (`share_password_lockout_seconds`), lockouts refused before the hash
//! verification runs, the budget cleared by a correct guess, `0` disabling
//! the limiter, and the lockout window resetting on expiry.

mod common;

use common::{build_state, build_state_with, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use std::collections::HashMap;
use vreckan_auth::user_manager;

/// Enable the `public_sharing` capability for the bootstrap admin (it is off
/// by default) and seed a shareable file into the `default` home dir.
async fn seed_shareable_file(sc: &SecureClient, filename: &str, contents: &str) {
    let user = user_manager::get_user(&sc.state.db, "admin")
        .await
        .unwrap()
        .unwrap();
    let mut settings = user.settings.unwrap_or_else(|| json!({}));
    settings["public_sharing"] = json!(true);
    user_manager::update_user_settings(&sc.state.db, "admin", settings)
        .await
        .unwrap();
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let home = std::path::Path::new(&sc.state.settings.storage_path)
        .join("admin")
        .join("default");
    std::fs::create_dir_all(home.join("Desktop").join("files")).unwrap();
    std::fs::write(home.join("Desktop").join("files").join(filename), contents).unwrap();
}

/// Create a password-protected share for the seeded file; returns the share id.
async fn create_password_share(sc: &SecureClient, password: &str) -> String {
    let (status, info) = sc
        .call(
            "POST",
            "/api/files/share",
            Some(&json!({
                "home_dir": "default",
                "path": "Desktop/files/secret.txt",
                "password": password,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{:?}", info);
    info.unwrap()["share_id"].as_str().unwrap().to_string()
}

/// POST a form-encoded password at the share's public entry point.
async fn post_share_password(
    sc: &SecureClient,
    share_id: &str,
    password: &str,
) -> common::RawResponse {
    sc.http
        .request(
            "POST",
            &format!("/public/{share_id}"),
            Some(format!("password={password}").into_bytes().to_vec()),
            Some("application/x-www-form-urlencoded"),
            &HashMap::new(),
        )
        .await
}

#[tokio::test]
async fn test_lockout_after_budget_exhausted() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_shareable_file(&sc, "secret.txt", "top secret").await;
    let share_id = create_password_share(&sc, "hunter2").await;

    // The default budget is 5 guesses per share.
    assert_eq!(sc.state.settings.share_password_max_attempts, 5);
    for i in 0..5 {
        let resp = post_share_password(&sc, &share_id, &format!("wrong-{i}")).await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "guess {i} must be a plain 401 while the budget lasts"
        );
    }
    // The 6th guess is refused before verification runs — even though it is
    // the correct password.
    let resp = post_share_password(&sc, &share_id, "hunter2").await;
    assert_eq!(resp.status, StatusCode::TOO_MANY_REQUESTS, "locked out");
    let html = String::from_utf8_lossy(&resp.body);
    assert!(
        html.contains("Too many failed password attempts"),
        "lockout page: {html}"
    );
}

#[tokio::test]
async fn test_lockout_expires_and_budget_resets() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_shareable_file(&sc, "secret.txt", "top secret").await;
    let share_id = create_password_share(&sc, "hunter2").await;

    for i in 0..5 {
        let resp = post_share_password(&sc, &share_id, "wrong").await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "guess {i}");
    }
    let resp = post_share_password(&sc, &share_id, "hunter2").await;
    assert_eq!(resp.status, StatusCode::TOO_MANY_REQUESTS, "locked out");

    // Age the lockout entry past the window (in production the wall clock
    // elapses instead): the next guess must be admitted again.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    {
        let mut map = sc.state.share_password_attempts.write().await;
        let w = map.get_mut(&share_id).expect("lockout entry must exist");
        w.window_start = now - 2.0 * sc.state.settings.share_password_lockout_seconds as f64;
        w.locked_until = now - 1.0;
    }
    let resp = post_share_password(&sc, &share_id, "hunter2").await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER, "lockout elapsed");
    let location = resp.header("location").unwrap();
    assert!(
        location.starts_with("/public/download/"),
        "location: {location}"
    );
}

#[tokio::test]
async fn test_success_clears_failure_budget() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_shareable_file(&sc, "secret.txt", "top secret").await;
    let share_id = create_password_share(&sc, "hunter2").await;

    // Three wrong guesses, then a correct one.
    for i in 0..3 {
        let resp = post_share_password(&sc, &share_id, &format!("wrong-{i}")).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "guess {i}");
    }
    let resp = post_share_password(&sc, &share_id, "hunter2").await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER, "correct guess");

    // The correct guess cleared the counter: a fresh, full budget of five
    // wrong guesses is allowed without a lockout (with the stale counter,
    // the third of these would be the fifth cumulative failure).
    for i in 0..5 {
        let resp = post_share_password(&sc, &share_id, &format!("again-{i}")).await;
        assert_eq!(
            resp.status,
            StatusCode::UNAUTHORIZED,
            "post-success guess {i} must not be locked out"
        );
    }
}

#[tokio::test]
async fn test_limiter_disabled_at_zero() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(
        build_state_with(_root.path(), |s: &mut vreckan_core::settings::Settings| {
            s.share_password_max_attempts = 0;
        })
        .await,
    );
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_shareable_file(&sc, "secret.txt", "top secret").await;
    let share_id = create_password_share(&sc, "hunter2").await;

    // 0 disables the limiter: more wrong guesses than the default budget, no
    // lockout.
    for i in 0..8 {
        let resp = post_share_password(&sc, &share_id, &format!("wrong-{i}")).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "guess {i}");
    }
    let resp = post_share_password(&sc, &share_id, "hunter2").await;
    assert_eq!(resp.status, StatusCode::SEE_OTHER, "limiter disabled");
}
