//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! `delete_person` storage-failure path (audit finding 12): a storage-removal
//! failure after the row is gone is logged rather than fatal.
//!
//! This test lives in its own file (its own process) because it drops the
//! effective uid to a non-root value to make `remove_dir_all` fail the way a
//! non-owner would; `seteuid` is process-global, so sharing a test binary with
//! other `delete_person` tests would let the dropped uid leak into them.

mod common;

use common::{connect_db, test_settings, TempDir};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use vreckan_auth::user_manager;

/// If the storage tree cannot be removed after the row is gone, the delete
/// still succeeds (log-and-continue): the account is removed and the leftover
/// directory is a harmless orphan, not a failed delete. The test process runs
/// as root, so it drops to a non-root effective uid — where a read-only,
/// root-owned storage tree is genuinely not removable — while the database is
/// left writable so the row delete itself succeeds.
#[tokio::test]
async fn storage_removal_failure_is_nonfatal() {
    let root = TempDir::new();
    let settings = test_settings(root.path());
    let db = connect_db(root.path()).await;
    std::fs::create_dir_all(Path::new(&settings.storage_path)).unwrap();

    user_manager::create_user(&db, "carol", None).await.unwrap();
    let storage_root = Path::new(&settings.storage_path);
    let storage = storage_root.join("carol");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::write(storage.join("file.txt"), b"data").unwrap();
    assert!(
        user_manager::get_user(&db, "carol").await.unwrap().is_some(),
        "precondition: account exists"
    );

    // Leave the database writable by a non-root uid (row DELETE will succeed)…
    for p in ["test.db", "test.db-wal", "test.db-shm"] {
        let f = root.path().join(p);
        if f.exists() {
            std::fs::set_permissions(&f, PermissionsExt::from_mode(0o666)).unwrap();
        }
    }
    // …and make the storage tree non-removable by a non-owner (read-only,
    // root-owned dirs: a non-owner without the write bit cannot unlink entries).
    std::fs::set_permissions(storage_root, PermissionsExt::from_mode(0o555)).unwrap();
    std::fs::set_permissions(&storage, PermissionsExt::from_mode(0o555)).unwrap();

    // Drop to a non-root effective uid (real uid stays 0, so we can restore).
    let nonroot: libc::uid_t = 1000;
    assert_eq!(unsafe { libc::setegid(nonroot as libc::gid_t) }, 0, "setegid failed");
    assert_eq!(unsafe { libc::seteuid(nonroot) }, 0, "seteuid failed");

    // The row write succeeds; the storage removal fails -> log-and-continue.
    let res = user_manager::delete_person(&db, &settings, "carol").await;

    // Restore full privileges before asserting/cleaning up.
    assert_eq!(unsafe { libc::setegid(0) }, 0, "setegid(0) failed");
    assert_eq!(unsafe { libc::seteuid(0) }, 0, "seteuid(0) failed");

    assert!(
        res.is_ok(),
        "a storage-removal failure must not fail the delete: {res:?}"
    );
    assert!(
        user_manager::get_user(&db, "carol").await.unwrap().is_none(),
        "account row must be removed even when the storage removal fails"
    );
    assert!(
        storage.is_dir(),
        "leftover storage is an orphan, not a failure"
    );
    assert!(
        storage.join("file.txt").exists(),
        "files survive a failed storage removal"
    );
}
