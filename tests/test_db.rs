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
            .execute_raw(sea_orm::Statement::from_string(backend, format!("DELETE FROM {table}")))
            .await;
    }
}
