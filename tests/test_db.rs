//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Configuration database: init/migrate, dump/restore, persistence round-trips.
//! Ports `tests/test_db`.

mod common;

use common::{build_state, connect_db, test_settings, TempDir};
use sea_orm::ConnectionTrait;
use vreckan::db;
use vreckan::user_manager;

#[tokio::test]
async fn test_init_and_migrate_are_idempotent() {
    let root = TempDir::new();
    let d = connect_db(root.path()).await;
    d.init().await.unwrap();
    d.init().await.unwrap();
    // init() is idempotent (schema-sync); running it twice must not fail.
    d.init().await.unwrap();
}

#[tokio::test]
async fn test_dump_restore_roundtrip() {
    let root = TempDir::new();
    let settings = test_settings(root.path());
    let d = connect_db(root.path()).await;
    let state = vreckan::state::AppState::new(settings, d.clone());

    // Seed an admin, a user, and a group.
    user_manager::load_users_and_groups(&state.db, &state.settings)
        .await
        .unwrap();
    user_manager::create_user(&state.db, "alice", Some(&serde_json::json!({ "public_sharing": true })))
        .await
        .unwrap();
    user_manager::set_password(&state.db, "alice", "alicepass1").await.unwrap();
    user_manager::write_group_file(&state.db, "ops", serde_json::json!({ "gpu": false }), None, None)
        .await
        .unwrap();

    let dump_path = root.path().join("backup.json");
    state.db.dump(dump_path.to_str().unwrap()).await.unwrap();

    // Wipe everything, then restore from the dump.
    wipe_db(&state.db).await;
    state.db.restore(dump_path.to_str().unwrap()).await.unwrap();

    // `get_all_users` returns non-admins and `get_all_admins` returns admins
    // (mirroring the Python split), so combine both for the full set.
    let mut names: std::collections::HashSet<String> = user_manager::get_all_users(&state.db)
        .await
        .unwrap()
        .into_iter()
        .map(|u| u.username)
        .collect();
    names.extend(
        user_manager::get_all_admins(&state.db)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.username),
    );
    assert_eq!(
        names,
        ["admin".to_string(), "alice".to_string()].into_iter().collect()
    );

    let alice = user_manager::get_user(&state.db, "alice").await.unwrap().unwrap();
    assert!(!alice.is_admin);
    assert!(alice.password_hash.as_deref().unwrap_or("").starts_with("$argon2"));
    let ops = user_manager::get_group(&state.db, "ops").await.unwrap().unwrap();
    assert_eq!(ops.settings, serde_json::json!({ "gpu": false }));
}

#[tokio::test]
async fn test_auth_tokens_prune_expired() {
    let root = TempDir::new();
    let state = build_state(root.path()).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    state
        .auth_tokens
        .write()
        .await
        .insert("fresh".to_string(), vreckan::state::AuthTokenRecord { username: "admin".into(), expires_at: now + 3600.0 });
    state
        .auth_tokens
        .write()
        .await
        .insert("stale".to_string(), vreckan::state::AuthTokenRecord { username: "admin".into(), expires_at: now - 10.0 });
    vreckan::auth::save_auth_tokens(&state).await.unwrap();
    vreckan::auth::load_auth_tokens(&state).await.unwrap();
    let tokens = state.auth_tokens.read().await;
    let keys: Vec<&str> = tokens.keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, vec!["fresh"]);
}

/// Delete every row from every table (mirrors the Python `_wipe_db`).
async fn wipe_db(d: &db::Db) {
    let backend = d.conn.get_database_backend();
    for table in db::TABLE_ORDER {
        let _ = d
            .conn
            .execute_raw(sea_orm::Statement::from_string(backend, &format!("DELETE FROM {table}")))
            .await;
    }
}

/// (retired key, successor key, seed value) for every ANT-128 rename pair.
const LEGACY_LE_PAIRS: [(&str, &str, &str); 9] = [
    ("VRECKAN_LE_ENABLED", "VRECKAN_ACME_ENABLED", "true"),
    ("VRECKAN_LE_DOMAINS", "VRECKAN_ACME_DOMAINS", "example.com,www.example.com"),
    ("VRECKAN_LE_AUTH", "VRECKAN_ACME_AUTH", "dns-cloudflare"),
    ("VRECKAN_LE_CLOUDFLARE_EMAIL", "VRECKAN_ACME_CLOUDFLARE_EMAIL", "admin@example.com"),
    ("VRECKAN_LE_CLOUDFLARE_TOKEN", "VRECKAN_ACME_CLOUDFLARE_TOKEN", "cf-token-123"),
    ("VRECKAN_LE_STAGING", "VRECKAN_ACME_STAGING", "true"),
    ("VRECKAN_LE_AUTO_RENEW", "VRECKAN_ACME_AUTO_RENEW", "true"),
    ("VRECKAN_LE_RENEW_CHECK_HOURS", "VRECKAN_ACME_RENEW_CHECK_HOURS", "12"),
    ("VRECKAN_LE_RENEW_BEFORE_DAYS", "VRECKAN_ACME_RENEW_BEFORE_DAYS", "45"),
];

/// ANT-252: Python-era `VRECKAN_LE_*` app_settings rows are migrated to their
/// `VRECKAN_ACME_*` successors at startup — values copied, retired rows
/// removed, and a second run is a no-op.
#[tokio::test]
async fn test_migrate_legacy_le_app_settings() {
    let root = TempDir::new();
    let d = connect_db(root.path()).await;
    for (legacy_key, _, value) in LEGACY_LE_PAIRS {
        d.set_app_setting(legacy_key, value).await.unwrap();
    }

    let moved = d.migrate_legacy_le_app_settings().await.unwrap();
    assert_eq!(moved.len(), 9);

    for (legacy_key, successor_key, value) in LEGACY_LE_PAIRS {
        assert_eq!(
            d.get_app_setting(successor_key).await.unwrap().as_deref(),
            Some(value),
            "successor {successor_key} must hold the migrated value"
        );
        assert_eq!(
            d.get_app_setting(legacy_key).await.unwrap(),
            None,
            "retired row {legacy_key} must be removed"
        );
    }

    // Idempotent: a second run moves nothing and changes nothing.
    assert!(
        d.migrate_legacy_le_app_settings().await.unwrap().is_empty(),
        "second migration run must be a no-op"
    );
    assert_eq!(
        d.get_app_setting("VRECKAN_ACME_RENEW_BEFORE_DAYS").await.unwrap().as_deref(),
        Some("45")
    );
}

/// ANT-252: when both a retired and a successor row exist (the admin page
/// saved the new name after the upgrade), the successor value wins and only
/// the retired row is removed.
#[tokio::test]
async fn test_migrate_legacy_le_keeps_existing_successor() {
    let root = TempDir::new();
    let d = connect_db(root.path()).await;
    d.set_app_setting("VRECKAN_LE_DOMAINS", "old.example.com").await.unwrap();
    d.set_app_setting("VRECKAN_ACME_DOMAINS", "new.example.com").await.unwrap();

    let moved = d.migrate_legacy_le_app_settings().await.unwrap();
    assert_eq!(moved.len(), 1);
    assert_eq!(moved[0].0, "VRECKAN_LE_DOMAINS");
    assert_eq!(
        d.get_app_setting("VRECKAN_ACME_DOMAINS").await.unwrap().as_deref(),
        Some("new.example.com")
    );
    assert_eq!(d.get_app_setting("VRECKAN_LE_DOMAINS").await.unwrap(), None);
}

/// ANT-252: `Settings::from_env` honours a retired `VRECKAN_LE_*` env var as
/// a fallback when the current `VRECKAN_ACME_*` var is unset, and the
/// current name wins when both are set.
#[tokio::test]
async fn test_from_env_honours_legacy_le_vars() {
    const NEW_VAR: &str = "VRECKAN_ACME_DOMAINS";
    const OLD_VAR: &str = "VRECKAN_LE_DOMAINS";
    let saved_new = std::env::var(NEW_VAR).ok();
    let saved_old = std::env::var(OLD_VAR).ok();

    std::env::remove_var(NEW_VAR);
    std::env::remove_var(OLD_VAR);

    // Neither set: code default (empty).
    let s = vreckan::settings::Settings::from_env();
    assert_eq!(s.acme_domains, "");

    // Only the retired name set: fallback applies.
    std::env::set_var(OLD_VAR, "legacy.example.com");
    let s = vreckan::settings::Settings::from_env();
    assert_eq!(s.acme_domains, "legacy.example.com");

    // Both set: the current name wins.
    std::env::set_var(NEW_VAR, "current.example.com");
    let s = vreckan::settings::Settings::from_env();
    assert_eq!(s.acme_domains, "current.example.com");

    if let Some(v) = saved_new {
        std::env::set_var(NEW_VAR, v);
    }
    if let Some(v) = saved_old {
        std::env::set_var(OLD_VAR, v);
    }
}
