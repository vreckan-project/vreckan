//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Throwaway QA repro (Rob's ANT-945 question on ANT-884): do 429 probes
//! during a share lockout re-extend `locked_until` (sliding window), or is
//! the expiry fixed at the moment the budget is exhausted?

mod common;

use common::{build_state_with, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use std::collections::HashMap;
use vreckan_auth::user_manager;

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

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
async fn test_lockout_probes_do_not_extend_expiry() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(
        build_state_with(_root.path(), |s: &mut vreckan_core::settings::Settings| {
            s.share_password_lockout_seconds = 10;
        })
        .await,
    );
    sc.http.login("admin", "admin1234").await;
    sc.handshake().await;
    seed_shareable_file(&sc, "secret.txt", "top secret").await;
    let share_id = create_password_share(&sc, "hunter2").await;

    // 1. Trigger the lockout with five wrong guesses.
    for i in 0..5 {
        let resp = post_share_password(&sc, &share_id, &format!("wrong-{i}")).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "guess {i}");
    }

    // 2. Capture the exact expiry the server recorded when the lock started.
    let locked_until_at_lock = {
        let map = sc.state.share_password_attempts.read().await;
        let w = map.get(&share_id).expect("lockout entry must exist");
        w.locked_until
    };
    assert!(
        locked_until_at_lock > now_secs(),
        "share must be locked (locked_until={locked_until_at_lock})"
    );

    // 3. Rob's scenario: three more guesses while locked, fast, all 429.
    let t_first_probe = now_secs();
    for i in 0..3 {
        let resp = post_share_password(&sc, &share_id, "probe").await;
        assert_eq!(resp.status, StatusCode::TOO_MANY_REQUESTS, "locked probe {i}");
    }
    let t_last_probe = now_secs();
    println!(
        "probes spanned {} ms ({} -> {})",
        (t_last_probe - t_first_probe) * 1000.0,
        t_first_probe,
        t_last_probe
    );

    // 4. Deterministic check: the 429 path must not move the expiry.
    let locked_until_after_probes = {
        let map = sc.state.share_password_attempts.read().await;
        let w = map.get(&share_id).expect("lockout entry must exist");
        w.locked_until
    };
    assert_eq!(
        locked_until_at_lock, locked_until_after_probes,
        "429 probes must not re-extend locked_until (fixed expiry); \
         at_lock={locked_until_at_lock} after_probes={locked_until_after_probes}"
    );

    // 5. Wall-clock confirmation: the 429 -> 401 transition lands at the
    //    recorded expiry (not at last_probe + window).
    let t_first_401 = loop {
        let t_send = now_secs();
        let resp = post_share_password(&sc, &share_id, "poll").await;
        if resp.status == StatusCode::UNAUTHORIZED {
            break t_send;
        }
        assert_eq!(resp.status, StatusCode::TOO_MANY_REQUESTS);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let drift = (t_first_401 - locked_until_at_lock).abs();
    println!(
        "locked_until={locked_until_at_lock} first_401_send={t_first_401} drift_s={drift}"
    );
    assert!(
        drift < 1.0,
        "429->401 transition must land at the recorded expiry (drift {drift}s)"
    );
}
