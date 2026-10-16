//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Web-login authentication: opaque tokens, cookies, Bearer, lifecycle.
//!
//! Port of `tests/test_auth`.

mod common;

use common::{build_state, build_state_with, SecureClient, TempDir, TestClient};
use serde_json::json;
use vreckan::user_manager;

const ADMIN: (&str, &str) = ("admin", "admin1234");

#[tokio::test]
async fn test_login_success() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    let body = resp.json();
    assert_eq!(body["username"], "admin");
    assert_eq!(body["is_admin"], true);
    assert_eq!(body["active"], true);
    assert_eq!(body["has_password"], true);
    // The credential is issued both as an HttpOnly cookie and a header.
    let set_cookie = resp.header("set-cookie").unwrap_or_default();
    assert!(set_cookie.to_lowercase().contains("httponly"));
    assert!(resp.header("x-auth-token").is_some());
}

#[tokio::test]
async fn test_login_wrong_password() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": "admin", "password": "nope" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_login_unknown_user() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": "ghost", "password": "whatever" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_me_with_cookie() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.login(ADMIN.0, ADMIN.1).await.status.as_u16(), 200);
    let resp = sc.http.get("/api/auth/me").await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json()["username"], "admin");
}

#[tokio::test]
async fn test_me_with_bearer_token() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let login = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    let token = login.header("x-auth-token").unwrap();
    // Drop the cookie: authenticate via the header only.
    sc.http.auth_cookie = None;
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    let resp = sc.http.get_with("/api/auth/me", &headers).await;
    assert_eq!(resp.status.as_u16(), 200);
    assert_eq!(resp.json()["username"], "admin");
}

#[tokio::test]
async fn test_me_without_credentials() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_me_with_garbage_bearer() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), "Bearer not-a-issued-token".to_string());
    assert_eq!(sc.http.get_with("/api/auth/me", &headers).await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_logout_revokes_the_token() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    sc.http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(sc.http.post_json("/api/auth/logout", &json!({})).await.status.as_u16(), 200);
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 401);
}

#[tokio::test]
async fn test_change_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    assert!(user_manager::verify_password(&sc.state.db, "admin", "freshpass1")
        .await
        .unwrap_or(false));
    assert!(!user_manager::verify_password(&sc.state.db, "admin", ADMIN.1)
        .await
        .unwrap_or(false));
}

#[tokio::test]
async fn test_change_password_wrong_old_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": "wrong", "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    assert_eq!(resp.json()["detail"], "Current password is incorrect.");
}

#[tokio::test]
async fn test_change_password_short_new_password() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "short" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 422);
}

#[tokio::test]
async fn test_change_password_keeps_session_valid() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": ADMIN.1, "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    // The opaque token is not password-bound: the live session survives.
    assert_eq!(sc.http.get("/api/auth/me").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_set_password_requires_admin() {
    // No credentials at all: the auth guard rejects before the admin check.
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc
        .http
        .post_json("/api/auth/set_password", &json!({ "password": "somepass123" }))
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_set_password_for_other_user() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    user_manager::create_user(&sc.state.db, "bob", None).await.unwrap();
    sc.http.login(ADMIN.0, ADMIN.1).await;
    let resp = sc
        .http
        .post_json(
            "/api/auth/set_password?target=bob",
            &json!({ "password": "bobpass123" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    assert!(user_manager::verify_password(&sc.state.db, "bob", "bobpass123")
        .await
        .unwrap_or(false));
    // bob can now log in with the assigned password.
    assert_eq!(
        sc.http
            .post_json("/api/auth/login", &json!({ "username": "bob", "password": "bobpass123" }))
            .await
            .status
            .as_u16(),
        200
    );
}

#[tokio::test]
async fn test_inactive_user_cannot_login() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    // Deactivate the admin account.
    let user = user_manager::get_user(&sc.state.db, "admin").await.unwrap().unwrap();
    let mut settings = user.settings.unwrap_or_else(|| json!({}));
    settings["active"] = json!(false);
    user_manager::update_user_settings(&sc.state.db, "admin", settings).await.unwrap();
    let resp = sc
        .http
        .post_json("/api/auth/login", &json!({ "username": ADMIN.0, "password": ADMIN.1 }))
        .await;
    assert_eq!(resp.status.as_u16(), 403);
}

#[tokio::test]
async fn test_admin_only_endpoint_rejects_regular_user() {
    // /api/admin/* (unlike /api/admin/status) sits behind verify_admin.
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    user_manager::create_user(&sc.state.db, "bob", None).await.unwrap();
    user_manager::set_password(&sc.state.db, "bob", "bobpass123").await.unwrap();
    // Switch the cookie to the regular user's token.
    let resp = sc.http.login("bob", "bobpass123").await;
    assert_eq!(resp.status.as_u16(), 200);
    sc.handshake().await;
    let (status, _) = sc.call("POST", "/api/admin/data", Some(&json!({}))).await;
    assert_eq!(status.as_u16(), 403);
}

// ---------------------------------------------------------------------------
// P1-2: bootstrap admin must-change flag + default-password policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_bootstrap_admin_flag_on_first_login() {
    let _root = TempDir::new();
    let mut tc = TestClient::new(build_state(_root.path()).await);
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
    let me = tc.get("/api/auth/me").await;
    assert_eq!(me.status.as_u16(), 200);
    assert_eq!(me.json()["must_change_password"], true);
}

#[tokio::test]
async fn test_change_password_clears_must_change_flag() {
    let _root = TempDir::new();
    let mut tc = TestClient::new(build_state(_root.path()).await);
    tc.login("admin", "admin1234").await;
    let resp = tc
        .post_json(
            "/api/auth/change_password",
            &json!({ "old_password": "admin1234", "new_password": "freshpass1" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
    let me = tc.get("/api/auth/me").await;
    assert_eq!(me.status.as_u16(), 200);
    assert_eq!(me.json()["must_change_password"], false);
    // The changed password works on a fresh session too.
    tc.post_json("/api/auth/logout", &json!({})).await;
    tc.auth_cookie = None;
    assert_eq!(tc.login("admin", "freshpass1").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_default_password_policy_reject() {
    let _root = TempDir::new();
    let tc = TestClient::new(
        build_state_with(_root.path(), |s| {
            s.default_password_policy = "reject".to_string();
        })
        .await,
    );
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    // Under the default (warn) default-DB-creds policy the 401 body is the
    // generic one: it does not disclose that the default bootstrap password
    // is still in use (login-body oracle, ANT-276).
    assert_eq!(resp.json()["detail"], "Invalid username or password.");
}

#[tokio::test]
async fn test_default_password_policy_reject_specific_body_under_silent() {
    // The operator explicitly opted into running on defaults (silent): the
    // specific body is kept so the user is told exactly what to change.
    let _root = TempDir::new();
    let tc = TestClient::new(
        build_state_with(_root.path(), |s| {
            s.default_password_policy = "reject".to_string();
            s.default_db_creds_policy = vreckan::settings::DefaultDbCredsPolicy::Silent;
        })
        .await,
    );
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    assert_eq!(
        resp.json()["detail"],
        "Still using the default bootstrap password; change it."
    );
}

#[tokio::test]
async fn test_default_password_policy_allow() {
    let _root = TempDir::new();
    let tc = TestClient::new(
        build_state_with(_root.path(), |s| {
            s.default_password_policy = "allow".to_string();
        })
        .await,
    );
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
}

#[tokio::test]
async fn test_custom_bootstrap_password_not_flagged_by_policy() {
    // The policy only fires on the *literal* default password; a configured
    // bootstrap password logs in with the must-change flag set but policy-warn
    // semantics (200 under the default "warn" policy).
    let _root = TempDir::new();
    let tc = TestClient::new(
        build_state_with(_root.path(), |s| {
            s.bootstrap_admin_password = "seededpw123".to_string();
        })
        .await,
    );
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "seededpw123" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200);
}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_login_rate_limit_locks_after_max_failures() {
    let _root = TempDir::new();
    let tc = TestClient::new(build_state(_root.path()).await);
    // Default limiter: 5 failed attempts within the window.
    for _ in 0..5 {
        let resp = tc
            .post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // The next attempt fails closed even with the correct password.
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 429);
}

#[tokio::test]
async fn test_login_rate_limit_success_resets_counter() {
    let _root = TempDir::new();
    let mut tc = TestClient::new(build_state(_root.path()).await);
    for _ in 0..4 {
        assert_eq!(
            tc.post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" })
            )
            .await
            .status
            .as_u16(),
            401
        );
    }
    // A successful login clears the pair's counters ...
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
    // ... so four more misses do not trigger a lockout.
    for _ in 0..4 {
        assert_eq!(
            tc.post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" })
            )
            .await
            .status
            .as_u16(),
            401
        );
    }
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_login_rate_limit_buckets_per_username() {
    let _root = TempDir::new();
    let tc = TestClient::new(build_state(_root.path()).await);
    for _ in 0..5 {
        tc.post_json(
            "/api/auth/login",
            &json!({ "username": "ghost1", "password": "nope" }),
        )
        .await;
    }
    // ghost1 is now locked out (even with the right password it would 429).
    // Four misses for a *different* username must not be affected ...
    for _ in 0..4 {
        assert_eq!(
            tc.post_json(
                "/api/auth/login",
                &json!({ "username": "ghost2", "password": "nope" })
            )
            .await
            .status
            .as_u16(),
            401
        );
    }
    // ... and the fifth miss records the pair at the threshold (a plain 401,
    // like any other miss) ...
    assert_eq!(
        tc.post_json(
            "/api/auth/login",
            &json!({ "username": "ghost2", "password": "nope" })
        )
        .await
        .status
        .as_u16(),
        401
    );
    // ... after which ghost2 is locked out on its own, not via ghost1's bucket.
    assert_eq!(
        tc.post_json(
            "/api/auth/login",
            &json!({ "username": "ghost2", "password": "nope" })
        )
        .await
        .status
        .as_u16(),
        429
    );
}

#[tokio::test]
async fn test_login_rate_limit_buckets_per_ip() {
    let _root = TempDir::new();
    // The direct peer is a configured trusted proxy, so the X-Forwarded-For
    // values are honoured for bucketing (ANT-276).
    let tc = TestClient::new(
        build_state_with(_root.path(), |s| {
            s.trusted_proxies = "10.0.0.1".to_string();
        })
        .await,
    );
    let peer = std::net::SocketAddr::from(([10u8, 0, 0, 1], 443));
    let mut h1 = std::collections::HashMap::new();
    h1.insert("X-Forwarded-For".to_string(), "1.2.3.4".to_string());
    let mut h2 = std::collections::HashMap::new();
    h2.insert("X-Forwarded-For".to_string(), "5.6.7.8".to_string());
    // Five misses from the first address lock only that (ip, username) pair;
    // the same username from a second address stays under the threshold.
    for _ in 0..5 {
        tc.request_from(
            "POST",
            "/api/auth/login",
            Some(json!({ "username": "admin", "password": "nope" }).to_string().into_bytes()),
            Some("application/json"),
            &h1,
            peer,
        )
        .await;
    }
    // Four misses from the second address: still under the five-attempt
    // threshold, so every response is a plain 401.
    for _ in 0..4 {
        let resp = tc
            .request_from(
                "POST",
                "/api/auth/login",
                Some(json!({ "username": "admin", "password": "nope" }).to_string().into_bytes()),
                Some("application/json"),
                &h2,
                peer,
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // The first pair is now locked out (even the correct password), while the
    // second pair is still under the threshold.
    let resp = tc
        .request_from(
            "POST",
            "/api/auth/login",
            Some(json!({ "username": "admin", "password": "admin1234" }).to_string().into_bytes()),
            Some("application/json"),
            &h1,
            peer,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 429);
    let resp = tc
        .request_from(
            "POST",
            "/api/auth/login",
            Some(json!({ "username": "admin", "password": "nope" }).to_string().into_bytes()),
            Some("application/json"),
            &h2,
            peer,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
}

#[tokio::test]
async fn test_login_rate_limit_ignores_xff_from_untrusted_peer() {
    let _root = TempDir::new();
    // No trusted proxies configured: the XFF header is attacker-rotatable, so
    // it is ignored and every client on the same direct peer shares one
    // bucket (ANT-276).
    let tc = TestClient::new(build_state(_root.path()).await);
    let mut h1 = std::collections::HashMap::new();
    h1.insert("X-Forwarded-For".to_string(), "1.2.3.4".to_string());
    let mut h2 = std::collections::HashMap::new();
    h2.insert("X-Forwarded-For".to_string(), "5.6.7.8".to_string());
    for _ in 0..5 {
        tc.request(
            "POST",
            "/api/auth/login",
            Some(json!({ "username": "admin", "password": "nope" }).to_string().into_bytes()),
            Some("application/json"),
            &h1,
        )
        .await;
    }
    // The sixth miss arrives with a different XFF value, but the untrusted
    // direct peer is the same bucket — it fails closed with 429.
    let resp = tc
        .request(
            "POST",
            "/api/auth/login",
            Some(json!({ "username": "admin", "password": "nope" }).to_string().into_bytes()),
            Some("application/json"),
            &h2,
        )
        .await;
    assert_eq!(resp.status.as_u16(), 429);
}

#[tokio::test]
async fn test_login_rate_limit_key_is_case_insensitive_username() {
    let _root = TempDir::new();
    let tc = TestClient::new(build_state(_root.path()).await);
    // Misses under a different capitalisation of the same account share the
    // lockout budget with the canonical username (ANT-276).
    for _ in 0..5 {
        tc.post_json(
            "/api/auth/login",
            &json!({ "username": "Admin", "password": "nope" }),
        )
        .await;
    }
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 429);
}

#[tokio::test]
async fn test_login_rate_limit_window_resets_not_compounds() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let ref_state = state.clone();
    let mut tc = TestClient::new(state);
    // Four failures inside the window (the default threshold is five).
    for _ in 0..4 {
        let resp = tc
            .post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // Simulate the window expiring: rewind the pair's window start.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    {
        let mut map = ref_state.login_attempts.write().await;
        let w = map
            .get_mut("unknown:admin")
            .expect("the pair was recorded");
        w.window_start = now - 10_000.0;
    }
    // The window resets rather than compounding: four more failures (eight
    // in total) still sit below the threshold ...
    for _ in 0..4 {
        let resp = tc
            .post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // ... so the pair is not locked out and the correct password logs in.
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_login_rate_limit_lockout_expires() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let ref_state = state.clone();
    let mut tc = TestClient::new(state);
    // Lock the pair out: five failures (the default threshold).
    for _ in 0..5 {
        let resp = tc
            .post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // While locked out the pair fails closed: even the correct password
    // gets 429.
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "admin1234" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 429);
    // Simulate the lockout elapsing: the lock is over and the failure
    // window is stale.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    {
        let mut map = ref_state.login_attempts.write().await;
        let w = map
            .get_mut("unknown:admin")
            .expect("the pair was recorded");
        w.locked_until = now - 1.0;
        w.window_start = now - 10_000.0;
    }
    // The pair is evaluated again: a miss is a plain 401, not 429 ...
    let resp = tc
        .post_json(
            "/api/auth/login",
            &json!({ "username": "admin", "password": "nope" }),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 401);
    // ... and the correct password logs in.
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
}

#[tokio::test]
async fn test_login_rate_limit_disabled_via_zero() {
    let _root = TempDir::new();
    let mut tc = TestClient::new(
        build_state_with(
            _root.path(),
            |s| {
                s.login_max_attempts = 0;
            },
        )
        .await,
    );
    // More misses than the default threshold never lock the pair out ...
    for _ in 0..7 {
        let resp = tc
            .post_json(
                "/api/auth/login",
                &json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(resp.status.as_u16(), 401);
    }
    // ... and the correct password still logs in.
    assert_eq!(tc.login("admin", "admin1234").await.status.as_u16(), 200);
}
