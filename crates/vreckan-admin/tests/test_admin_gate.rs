//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The admin-route access gate: `/api/admin/*` (unlike `/api/admin/status`)
//! sits behind `verify_admin`, so a regular user's token is rejected with
//! 403 before the handler runs. Split out of the auth suite in ANT-810:
//! this test drives an admin-crate route, so the rest of the auth tests
//! link only `vreckan-auth` + `vreckan-acme` + `vreckan-core`.

mod common;

use common::{build_state, SecureClient, TempDir};
use serde_json::json;
use vreckan_auth::user_manager;

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
