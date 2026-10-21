//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! The owner session URL carries `token` alongside `access_token` (ANT-449).
//!
//! The selkies 2.0.0 WebRTC client sources its `client_token` only from the
//! page URL's `?token=` param. The stock owner URL exposed only
//! `?access_token=`, so on a TURN-configured dual-mode image signaling
//! rejected the owner's HELLO (`missing or invalid token in secure mode`) and
//! input was dropped. The owner URL now carries the access token under both
//! params. These tests pin the shape at the single construction site
//! (`owner_session_url`) and end-to-end through the launch flow (the "stock
//! URL" the QA matrix exercises).

mod common;

use common::{build_state, SecureClient, TempDir};
use serde_json::{json, Value};
use std::collections::HashMap;
use vreckan::catalog;
use vreckan::user_manager;

const DEFAULT_PROVIDER_CONFIG: &str = r#"{
    "image": "img:latest",
    "port": 3000,
    "nvidia_support": false,
    "dri3_support": true,
    "type": "browser",
    "url_support": true,
    "open_support": false,
    "extensions": [],
    "env": [],
    "docker_overrides": {}
}"#;

async fn make_app(sc: &SecureClient) {
    let data = json!({
        "id": "app-1",
        "name": "Firefox",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": "firefox",
        "provider": "docker",
        "home_directories": true,
        "users": ["all"],
        "groups": [],
        "app_template": "Default",
        "provider_config": serde_json::from_str::<Value>(DEFAULT_PROVIDER_CONFIG).unwrap(),
    });
    let app: catalog::InstalledApp = serde_json::from_value(data.clone()).unwrap();
    sc.state.installed_apps.write().await.insert(app.id.clone(), app);
}

/// Unit: the owner URL carries the access token under both `access_token` and
/// `token` (the WebRTC client reads only `?token=`).
#[tokio::test]
async fn test_owner_session_url_carries_token_alongside_access_token() {
    assert_eq!(
        catalog::owner_session_url("sid-1", "at-1"),
        "/api/apps/session/sid-1/?access_token=at-1&token=at-1"
    );
}

/// E2E: a non-room (owner) launch returns a stock URL the WebRTC client can
/// source its client_token from — both params present and equal to the
/// session's access token.
#[tokio::test]
async fn test_launch_owner_url_carries_webrtc_token() {
    let _root = TempDir::new();
    let sc = SecureClient::new(build_state(_root.path()).await);
    make_app(&sc).await;
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    sc.state
        .fake_provider
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        None,
        HashMap::new(),
        None,
        None,
        None,
        None,
        true,
        None,
        false,
        true,
        None,
        true,
    )
    .await
    .expect("launch_common failed");
    let access_token = sc
        .state
        .sessions
        .read()
        .await
        .get(&result.session_id)
        .unwrap()["access_token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        result.session_url,
        catalog::owner_session_url(&result.session_id, &access_token)
    );
    assert!(
        result
            .session_url
            .contains(&format!("access_token={access_token}&token={access_token}")),
        "owner URL must carry token={access_token} alongside access_token: {}",
        result.session_url
    );
}
