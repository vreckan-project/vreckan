//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Launch spec assembly: env layering, volumes, GPUs, room mode, access.
//!
//! Ports `tests/test_launch`. These tests drive `launch_common` directly
//! (no TestClient) with a fake provider (the `fake_provider` test hook on
//! `AppState`), so they assert on the exact kwargs the Docker provider would
//! have received.

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::{json, Value};
use std::collections::HashMap;
use vreckan::catalog;
use vreckan::user_manager;

/// The default provider config used by the launch tests (mirrors
/// `DEFAULT_PROVIDER_CONFIG`).
const DEFAULT_PROVIDER_CONFIG: &str = r#"{
    "image": "img:latest",
    "port": 3000,
    "nvidia_support": false,
    "dri3_support": true,
    "type": "browser",
    "url_support": true,
    "open_support": false,
    "extensions": [],
    "env": [{"name": "APP_ENV", "value": "1"}],
    "docker_overrides": {"devices": ["/dev/snd"]}
}"#;

/// Build an InstalledApp from the default config, applying `overrides` (a JSON
/// object merged over the base), and insert it into `state.installed_apps`.
/// Mirrors `make_app`.
async fn make_app(sc: &SecureClient, overrides: Option<&Value>) -> Value {
    let mut data = json!({
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
    if let Some(ov) = overrides {
        if let (Some(d), Some(o)) = (data.as_object_mut(), ov.as_object()) {
            for (k, v) in o {
                d.insert(k.clone(), v.clone());
            }
        }
    }
    let app: catalog::InstalledApp = serde_json::from_value(data.clone()).unwrap();
    sc.state.installed_apps.write().await.insert(app.id.clone(), app);
    data
}

/// The parameters for a `launch_common` invocation (mirrors `run_launch`).
struct LaunchArgs {
    home_name: Option<&'static str>,
    language: Option<&'static str>,
    selected_gpu: Option<&'static str>,
    env_vars: HashMap<String, String>,
    app_overrides: Option<Value>,
    launch_in_room_mode: bool,
    session_name: Option<&'static str>,
    check_access: bool,
}

impl Default for LaunchArgs {
    fn default() -> Self {
        Self {
            home_name: None,
            language: None,
            selected_gpu: None,
            env_vars: HashMap::new(),
            app_overrides: None,
            launch_in_room_mode: false,
            session_name: None,
            check_access: true,
        }
    }
}

/// Drive `launch_common` with the fake provider and return the launch result
/// plus the recorded fake-provider kwargs. Mirrors `run_launch`.
async fn run_launch(sc: &mut SecureClient, args: LaunchArgs) -> (catalog::LaunchResponse, Value) {
    // The bootstrap admin + roles are already seeded by build_state.
    make_app(sc, args.app_overrides.as_ref()).await;
    if let Some(h) = args.home_name {
        if h.to_lowercase() != "cleanroom" {
            user_manager::create_home_dir(&sc.state.settings, "admin", h).unwrap();
        }
    }
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    // Enable the fake provider so launch_common skips the real Docker daemon.
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        args.home_name,
        args.env_vars,
        args.language,
        args.selected_gpu,
        None,
        None,
        true,
        None,
        args.launch_in_room_mode,
        true,
        args.session_name,
        args.check_access,
    )
    .await;
    let result = result.expect("launch_common failed");
    let record = sc.state.fake_provider_record.lock().unwrap().clone().unwrap();
    (result, record)
}

// ---------------------------------------------------------------------------
// Env / volumes / title
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_env_volumes_and_title() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let (result, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            session_name: Some("My Session"),
            ..Default::default()
        },
    )
    .await;
    assert!(result.session_url.starts_with(&format!("/api/apps/session/{}/", result.session_id)));
    let env = &record["launch_kwargs"]["env_vars"];
    // Base runtime layer.
    assert!(env["SUBFOLDER"].as_str().unwrap().starts_with(&format!("/api/apps/session/{}/", result.session_id)));
    assert_eq!(env["PUID"].as_str().unwrap(), "1000");
    assert_eq!(env["PGID"].as_str().unwrap(), "1000");
    assert!(env["CUSTOM_USER"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
    assert!(env["PASSWORD"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
    assert_eq!(env["TZ"].as_str().unwrap(), "America/Phoenix");
    assert_eq!(env["SELKIES_ALLOWED_ORIGINS"].as_str().unwrap(), "*");
    assert_eq!(env["PIXELFLUX_WAYLAND"].as_str().unwrap(), "true");
    // The app's own env layer.
    assert_eq!(env["APP_ENV"].as_str().unwrap(), "1");
    // The optional session name wins the TITLE layer.
    assert_eq!(env["TITLE"].as_str().unwrap(), "Firefox - My Session");
    // Volumes: the persistent home + the shared-files bind.
    let volumes = &record["launch_kwargs"]["volumes"];
    let storage = &sc.state.settings.storage_path;
    let home = format!("{storage}/admin/default");
    let shared = format!("{storage}/admin/_vreckan_shared_files");
    let container_config = &sc.state.settings.container_config_path;
    assert_eq!(volumes[&home]["bind"].as_str().unwrap(), container_config);
    assert_eq!(volumes[&home]["mode"].as_str().unwrap(), "rw");
    assert_eq!(volumes[&shared]["bind"].as_str().unwrap(), &format!("{container_config}/Desktop/files"));
    assert_eq!(volumes[&shared]["mode"].as_str().unwrap(), "rw");
    assert!(record["launch_kwargs"]["gpu_config"].is_null());
    // The session is registered for the proxy + persistence.
    let sessions = sc.state.sessions.read().await;
    let record_session = sessions.get(&result.session_id).unwrap();
    assert_eq!(record_session["username"], "admin");
    assert_eq!(record_session["name"], "My Session");
    assert_eq!(record_session["container_registry"]["app-1"]["instance_id"], "ctr-1");
}

#[tokio::test]
async fn test_launch_cleanroom_uses_ephemeral_storage() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let (result, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("cleanroom"),
            ..Default::default()
        },
    )
    .await;
    let volumes = &record["launch_kwargs"]["volumes"];
    let ephemeral_base = format!("{}/vreckan_ephemeral", sc.state.settings.storage_path);
    let keys: Vec<&String> = volumes.as_object().unwrap().keys().collect();
    let host_mount = keys
        .iter()
        .find(|p| p.starts_with(&ephemeral_base) && !p.contains(&result.session_id))
        .cloned()
        .unwrap();
    let shared = keys.iter().find(|p| **p != host_mount).cloned().unwrap();
    assert!(shared.starts_with(&ephemeral_base));
    assert!(shared.contains(&result.session_id));
}

#[tokio::test]
async fn test_launch_language_sets_lc_all() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            language: Some("de_DE.UTF-8"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(record["launch_kwargs"]["env_vars"]["LC_ALL"].as_str().unwrap(), "de_DE.UTF-8");
}

#[tokio::test]
async fn test_launch_english_language_leaves_lc_all_unset() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            language: Some("en_us.utf-8"),
            ..Default::default()
        },
    )
    .await;
    assert!(record["launch_kwargs"]["env_vars"].get("LC_ALL").is_none());
}

// ---------------------------------------------------------------------------
// GPU selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_selected_dri3_gpu_applied() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.state.available_gpus.write().await.push(vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD128".into(),
        driver: "amdgpu".into(),
        r#type: Some("dri3".into()),
        index: None,
    });
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            selected_gpu: Some("/dev/dri/renderD128"),
            ..Default::default()
        },
    )
    .await;
    let env = &record["launch_kwargs"]["env_vars"];
    assert_eq!(env["DRI_NODE"].as_str().unwrap(), "/dev/dri/renderD128");
    assert_eq!(env["DRINODE"].as_str().unwrap(), "/dev/dri/renderD128");
    assert_eq!(record["launch_kwargs"]["gpu_config"]["device"].as_str().unwrap(), "/dev/dri/renderD128");
}

#[tokio::test]
async fn test_launch_nvidia_gpu_with_wayland_sets_dri_node() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let mut pc = serde_json::from_str::<Value>(DEFAULT_PROVIDER_CONFIG).unwrap();
    pc["nvidia_support"] = json!(true);
    let app_overrides = json!({ "provider_config": pc });
    sc.state.available_gpus.write().await.push(vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD129".into(),
        driver: "nvidia".into(),
        r#type: Some("nvidia".into()),
        index: Some(0),
    });
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            selected_gpu: Some("/dev/dri/renderD129"),
            app_overrides: Some(app_overrides),
            ..Default::default()
        },
    )
    .await;
    let env = &record["launch_kwargs"]["env_vars"];
    assert_eq!(env["DRI_NODE"].as_str().unwrap(), "/dev/dri/renderD129");
    assert_eq!(env["DRINODE"].as_str().unwrap(), "/dev/dri/renderD129");
}

#[tokio::test]
async fn test_launch_unknown_gpu_rejected() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    make_app(&sc, None).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    sc.state.available_gpus.write().await.push(vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD128".into(),
        driver: "amdgpu".into(),
        r#type: Some("dri3".into()),
        index: None,
    });
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        Some("default"),
        HashMap::new(),
        None,
        Some("/dev/dri/renderD999"),
        None,
        None,
        true,
        None,
        false,
        true,
        None,
        true,
    )
    .await;
    let (status, _) = result.unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_launch_nvidia_gpu_rejected_when_unsupported() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    make_app(&sc, None).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    sc.state.available_gpus.write().await.push(vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD129".into(),
        driver: "nvidia".into(),
        r#type: Some("nvidia".into()),
        index: Some(0),
    });
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    // The default app has nvidia_support=False.
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        Some("default"),
        HashMap::new(),
        None,
        Some("/dev/dri/renderD129"),
        None,
        None,
        true,
        None,
        false,
        true,
        None,
        true,
    )
    .await;
    let (status, _) = result.unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_launch_global_default_gpu_fallback() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.state.available_gpus.write().await.push(vreckan::docker_provider::GpuInfo {
        device: "/dev/dri/renderD128".into(),
        driver: "amdgpu".into(),
        r#type: Some("dri3".into()),
        index: None,
    });
    *sc.state.global_default_gpu.write().await = Some("/dev/dri/renderD128".into());
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(record["launch_kwargs"]["gpu_config"]["device"].as_str().unwrap(), "/dev/dri/renderD128");
}

// ---------------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_denied_without_access() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    make_app(&sc, Some(&json!({ "users": ["alice"] }))).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        Some("default"),
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
    .await;
    let (status, _) = result.unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_launch_missing_app_404() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "nope",
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
    .await;
    let (status, _) = result.unwrap_err();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Room mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_room_mode() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let (result, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            launch_in_room_mode: true,
            ..Default::default()
        },
    )
    .await;
    let env = &record["launch_kwargs"]["env_vars"];
    assert!(env["SELKIES_MASTER_TOKEN"].as_str().map(|s| !s.is_empty()).unwrap_or(false));
    let kwargs = &record["launch_kwargs"];
    assert_eq!(kwargs["is_collaboration"], true);
    assert_eq!(kwargs["master_token"], env["SELKIES_MASTER_TOKEN"]);
    let initial_tokens = kwargs["initial_tokens"].as_object().unwrap();
    assert_eq!(initial_tokens.len(), 1);
    let (controller_token, info) = initial_tokens.iter().next().unwrap();
    assert_eq!(info["role"], "controller");
    let sessions = sc.state.sessions.read().await;
    let record_session = sessions.get(&result.session_id).unwrap();
    assert_eq!(record_session["is_collaboration"], true);
    assert_eq!(record_session["master_token"], env["SELKIES_MASTER_TOKEN"]);
    assert_eq!(record_session["controller_token"].as_str().unwrap(), controller_token.as_str());
    assert!(result.session_url.starts_with(&format!("/room/{}?", result.session_id)));
}

// ---------------------------------------------------------------------------
// Schema / template env layering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_base_layer_schema_env() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    // Set admin base-layer overrides on the in-memory schema.
    {
        let mut schema = sc.state.template_schema.write().await;
        for s in schema["settings"].as_array_mut().unwrap() {
            if s["name"] == "SELKIES_ENCODER" {
                s["current"] = json!("h264enc");
            } else if s["name"] == "DOCKER_SHM_SIZE" {
                s["current"] = json!("2g");
            }
        }
    }
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            ..Default::default()
        },
    )
    .await;
    let env = &record["launch_kwargs"]["env_vars"];
    assert_eq!(env["SELKIES_ENCODER"].as_str().unwrap(), "h264enc");
    let overrides = &record["app_config"]["provider_config"]["docker_overrides"];
    assert_eq!(overrides["shm_size"].as_str().unwrap(), "2g");
    // The app's own overrides coexist with the base layer.
    assert_eq!(overrides["devices"], json!(["/dev/snd"]));
}

#[tokio::test]
async fn test_launch_template_settings_layers() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.state.app_templates.write().await.insert(
        "Default".into(),
        json!({
            "name": "Default",
            "settings": {
                "TITLE": "From Template",
                "DOCKER_DEVICES": "/dev/dri/renderD128",
                "DOCKER_SHM_SIZE": "1g",
            },
        }),
    );
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            ..Default::default()
        },
    )
    .await;
    let env = &record["launch_kwargs"]["env_vars"];
    assert_eq!(env["TITLE"].as_str().unwrap(), "From Template");
    let overrides = &record["app_config"]["provider_config"]["docker_overrides"];
    // The app's own devices list is extended with the template's.
    assert_eq!(overrides["devices"], json!(["/dev/snd", "/dev/dri/renderD128"]));
    assert_eq!(overrides["shm_size"].as_str().unwrap(), "1g");
}

#[tokio::test]
async fn test_launch_explicit_env_vars_win_over_template() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    sc.state.app_templates.write().await.insert(
        "Default".into(),
        json!({ "name": "Default", "settings": { "CUSTOM_VAR": "from-template" } }),
    );
    let mut env_vars = HashMap::new();
    env_vars.insert("CUSTOM_VAR".into(), "from-request".into());
    let (_, record) = run_launch(
        &mut sc,
        LaunchArgs {
            home_name: Some("default"),
            env_vars,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(record["launch_kwargs"]["env_vars"]["CUSTOM_VAR"].as_str().unwrap(), "from-request");
}

// ---------------------------------------------------------------------------
// URL env var mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_launch_url_env_var_mapping() {
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let mut pc = serde_json::from_str::<Value>(DEFAULT_PROVIDER_CONFIG).unwrap();
    pc["url_env_var"] = json!("FIREFOX_CLI");
    make_app(&sc, Some(&json!({ "provider_config": pc }))).await;
    user_manager::create_home_dir(&sc.state.settings, "admin", "default").unwrap();
    let settings = user_manager::get_effective_settings(&sc.state.db, "admin").await.unwrap();
    let mut env_vars = HashMap::new();
    env_vars.insert("VRECKAN_URL".into(), "https://example.com".into());
    sc.state.fake_provider.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = catalog::launch_common(
        &sc.state,
        "app-1",
        "admin",
        &settings,
        Some("default"),
        env_vars,
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
    .unwrap();
    let record = sc.state.fake_provider_record.lock().unwrap().clone().unwrap();
    let env = &record["launch_kwargs"]["env_vars"];
    assert_eq!(env["VRECKAN_URL"].as_str().unwrap(), "https://example.com");
    assert_eq!(env["FIREFOX_CLI"].as_str().unwrap(), "https://example.com");
    let sessions = sc.state.sessions.read().await;
    let session = sessions.get(&result.session_id).unwrap();
    assert_eq!(
        session["launch_context"],
        json!({ "type": "url", "value": "https://example.com" })
    );
}

// ---------------------------------------------------------------------------
// Docker overrides parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_extract_docker_overrides_parsing() {
    let result = catalog::extract_docker_overrides(&json!({
        "DOCKER_PRIVILEGED": "true",
        "DOCKER_CAP_ADD": "SYS_ADMIN, NET_ADMIN",
        "DOCKER_ENV": "A=1,B=2",
        "DOCKER_CPU_SHARES": "notanumber",
        "NOT_DOCKER": "x",
    }));
    let result = Value::Object(result.into_iter().collect());
    assert_eq!(
        result,
        json!({
            "privileged": true,
            "cap_add": ["SYS_ADMIN", "NET_ADMIN"],
            "environment": { "A": "1", "B": "2" },
        })
    );
}
