//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! `delete_person` ordering (audit finding 12): the DB row is removed before the
//! storage tree, so a failed delete never destroys the user's files, and a
//! storage-removal failure after the row is gone is logged rather than fatal.

mod common;

use common::{connect_db, test_settings, TempDir};
use sea_orm::ConnectionTrait;
use std::path::Path;
use vreckan_auth::user_manager;
use vreckan_core::db;

/// A successful delete removes both the account row and the storage tree.
#[tokio::test]
async fn delete_person_removes_row_and_storage() {
    let root = TempDir::new();
    let settings = test_settings(root.path());
    let db = connect_db(root.path()).await;
    std::fs::create_dir_all(Path::new(&settings.storage_path)).unwrap();

    user_manager::create_user(&db, "carol", None).await.unwrap();
    let storage = Path::new(&settings.storage_path).join("carol");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("file.txt"), b"data").unwrap();

    user_manager::delete_person(&db, &settings, "carol").await.unwrap();

    assert!(
        user_manager::get_user(&db, "carol").await.unwrap().is_none(),
        "account row must be removed"
    );
    assert!(!storage.exists(), "storage tree must be removed");
}

/// The bootstrap account is still protected (deleting it locks everyone out of
/// the admin UI). This guard must survive the reordering.
#[tokio::test]
async fn delete_person_bootstrap_account_is_protected() {
    let root = TempDir::new();
    let settings = test_settings(root.path());
    let db = connect_db(root.path()).await;

    let bootstrap = user_manager::bootstrap_username(&db, &settings).await.unwrap();
    user_manager::create_user(&db, &bootstrap, None).await.unwrap();

    let res = user_manager::delete_person(&db, &settings, &bootstrap).await;
    assert!(res.is_err(), "bootstrap account cannot be deleted");
    let msg = res.as_ref().unwrap_err().to_string();
    assert!(
        msg.contains("cannot be deleted"),
        "expected the bootstrap-protection message, got: {msg}"
    );
    // The account is untouched.
    assert!(user_manager::get_user(&db, &bootstrap).await.unwrap().is_some());
}

/// If the DB row delete fails, the storage tree must be left intact — no data
/// loss. A second connection holding an exclusive lock makes the `DELETE` fail
/// ("database is locked") while the `SELECT`s (`bootstrap_username`,
/// `get_user`) still succeed, so the failure lands specifically on the row
/// write — which the fix moves before the storage removal.
#[tokio::test]
async fn failed_row_delete_preserves_storage() {
    let root = TempDir::new();
    let settings = test_settings(root.path());
    let db = connect_db(root.path()).await;
    std::fs::create_dir_all(Path::new(&settings.storage_path)).unwrap();

    user_manager::create_user(&db, "carol", None).await.unwrap();
    let storage = Path::new(&settings.storage_path).join("carol");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("file.txt"), b"data").unwrap();

    // A second connection holding an exclusive lock: the row write below
    // fails ("database is locked") while reads still succeed (WAL permits
    // concurrent readers).
    let locker = db::connect(&format!(
        "sqlite://{}?mode=rwc",
        root.path().join("test.db").to_string_lossy()
    ))
    .await
    .unwrap();
    locker
        .conn
        .execute_raw(sea_orm::Statement::from_string(
            locker.conn.get_database_backend(),
            "BEGIN EXCLUSIVE;",
        ))
        .await
        .unwrap();
    // Precondition: reads still succeed while the write lock is held, so the
    // delete gets past bootstrap_username/get_user and fails at the row write.
    assert!(
        user_manager::get_user(&db, "carol").await.unwrap().is_some(),
        "reads must succeed while the write lock is held"
    );

    let err = user_manager::delete_person(&db, &settings, "carol").await.unwrap_err();
    assert!(
        matches!(err, user_manager::UserError::Db(_)),
        "expected a database (row-write) failure, got: {err}"
    );
    // No data loss: the account still exists and its files are intact, so the
    // delete is cleanly retryable.
    assert!(
        user_manager::get_user(&db, "carol").await.unwrap().is_some(),
        "account must survive a failed row delete (retryable)"
    );
    assert!(storage.is_dir(), "storage tree must survive a failed delete");
    assert!(
        storage.join("file.txt").exists(),
        "files must survive a failed delete"
    );
}
