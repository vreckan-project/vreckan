//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Tier 5: real Docker integration tests.
//!
//! Ports `tests/test_docker_launch`. Every other tier uses the
//! `fake_provider` test hook so the suite stays hermetic and fast. This tier
//! does the opposite: it lets the *real* provider launch and stop actual
//! containers on the local Docker daemon, exercising the full
//! launch -> ready -> stop lifecycle over the compose network (the same bridge
//! network the API container sits on, so the API can reach the launched
//! container's IP directly).
//!
//! The probe app is backed by `python:3.12-slim` running a catch-all HTTP
//! server, so both the launch readiness probe (SUBFOLDER path + Basic auth)
//! and the session status endpoint (root path, no auth) get a 200. The app
//! declares no GPU support, so no device passthrough is attempted and the
//! tests run on any host.
//!
//! These tests are skipped (not failed) when no Docker daemon is reachable,
//! so the suite remains runnable on machines without Docker.

mod common;

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;
use std::sync::{Arc, Mutex};
use vreckan::catalog;

const APP_ID: &str = "app-docker-probe";

/// Catch-all HTTP server: 200 for any path/method, silent. Satisfies both the
/// launch readiness probe and the session status endpoint.
const CATCH_ALL: &str = r#"from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def _ok(self):
        self.send_response(200)
        self.send_header('Content-Type', 'text/plain')
        self.end_headers()
        self.wfile.write(b'ok')
    do_GET = do_POST = do_PUT = do_DELETE = _ok
    def log_message(self, *a): pass
HTTPServer(('0.0.0.0', 8080), H).serve_forever()
"#;

/// Is a Docker daemon reachable? Mirrors the module-level
/// `_DOCKER_AVAILABLE = bool(docker.from_env().ping())`.
async fn docker_available() -> bool {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(c) => c.ping().await.is_ok(),
        Err(_) => false,
    }
}

/// Register a no-GPU probe app backed by `python:3.12-slim`. Mirrors
/// `_seed_probe_app`.
async fn seed_probe_app(sc: &SecureClient) -> String {
    let app: catalog::InstalledApp = serde_json::from_value(json!({
        "id": APP_ID,
        "name": "Docker Probe",
        "logo": "x",
        "url": "x",
        "source": "Test Store",
        "source_app_id": APP_ID,
        "provider": "docker",
        // home_directories=false -> cleanroom/ephemeral: no home-dir dependency
        "home_directories": false,
        "users": ["*"],
        "groups": [],
        "app_template": "Default",
        "provider_config": {
            "image": "python:3.12-slim",
            "port": 8080,
            "nvidia_support": false,
            "dri3_support": false,
            "type": "docker",
            "url_support": false,
            "open_support": false,
            "extensions": [],
            "docker_overrides": { "command": ["python", "-c", CATCH_ALL] },
        },
    }))
    .unwrap();
    let id = app.id.clone();
    sc.state.installed_apps.write().await.insert(id.clone(), app);
    id
}

/// Safety net: stop+remove any container a test launched but didn't stop.
/// Mirrors `_force_cleanup`. A failed launch (504) already self-cleans inside
/// the provider, so this only matters when a test fails *after* a successful
/// launch.
async fn force_cleanup(client: &bollard::Docker, instance_ids: &[String]) {
    for cid in instance_ids {
        if cid.is_empty() {
            continue;
        }
        if let Ok(ci) = client.inspect_container(cid, None).await {
            let running = ci
                .state
                .as_ref()
                .and_then(|s| s.running)
                .unwrap_or(false);
            if running {
                let _ = client
                    .stop_container(
                        cid,
                        Some(bollard::query_parameters::StopContainerOptions {
                            signal: None,
                            t: Some(2),
                        }),
                    )
                    .await;
            }
            let _ = client.remove_container(cid, None).await;
        }
    }
}

/// Restore the real Docker client for the duration of a test. In the Rust
/// port the real provider is used by default (no fake is installed), so this
/// only needs to re-run the self-container inspection now that the real
/// client is reachable, so launched containers land on the same network as
/// the test container (and are therefore reachable by the readiness probe).
/// Mirrors the `real_docker` fixture.
async fn real_docker(sc: &SecureClient) {
    catalog::inspect_self_container(&sc.state).await;
}

/// Launch a real container, confirm it is running + ready, then stop it.
#[tokio::test]
async fn test_launch_stop_lifecycle() {
    if !docker_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "login failed: {:?}", resp.body);
    sc.handshake().await;
    let app_id = seed_probe_app(&sc).await;
    real_docker(&sc).await;

    let client = bollard::Docker::connect_with_local_defaults().unwrap();
    let launched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let launched2 = launched.clone();
    let result: Result<(), String> = (|| async {
        let (status, body) = sc
            .call("POST", "/api/launch/simple", Some(&json!({ "application_id": app_id })))
            .await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        let body = body.unwrap();
        let session_id = body["session_id"].as_str().unwrap().to_string();
        assert!(body["session_url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("/api/apps/session/{session_id}/")));

        // The session is recorded in the in-memory DB.
        let session = sc.state.sessions.read().await.get(&session_id).cloned().unwrap();
        assert_eq!(session["username"], "admin");
        assert_eq!(session["provider_app_id"], app_id);
        let instance_id = session["instance_id"].as_str().unwrap().to_string();
        launched2.lock().unwrap().push(instance_id.clone());

        // A real container is running on the daemon.
        let container = client.inspect_container(&instance_id, None).await.unwrap();
        assert!(container
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false));

        // The env layering reached the real container.
        let env = container
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .cloned()
            .unwrap_or_default();
        assert!(env
            .iter()
            .any(|e| e.starts_with(&format!("SUBFOLDER=/api/apps/session/{session_id}/"))));

        // The status endpoint agrees the upstream is ready.
        let (status2, status_body) = sc
            .call("GET", &format!("/api/sessions/{session_id}/status"), None)
            .await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(status_body.unwrap()["ready"], true);

        // Stop: 204, session gone, container removed.
        let (status3, _) = sc
            .call("DELETE", &format!("/api/sessions/{session_id}"), None)
            .await;
        assert_eq!(status3, StatusCode::NO_CONTENT);
        assert!(!sc.state.sessions.read().await.contains_key(&session_id));
        assert!(client.inspect_container(&instance_id, None).await.is_err());
        Ok(())
    })()
    .await;
    let ids = launched.lock().unwrap().clone();
    if let Err(e) = result {
        force_cleanup(&client, &ids).await;
        panic!("{e}");
    }
    force_cleanup(&client, &ids).await;
}

/// A no-GPU app must launch without any /dev device passthrough.
#[tokio::test]
async fn test_no_gpu_app_gets_no_device_passthrough() {
    if !docker_available().await {
        eprintln!("skipping: Docker daemon not reachable");
        return;
    }
    let _root = TempDir::new();
    let mut sc = SecureClient::new(build_state(_root.path()).await);
    let resp = sc.login("admin", "admin1234").await;
    assert_eq!(resp.status, StatusCode::OK, "login failed: {:?}", resp.body);
    sc.handshake().await;
    let app_id = seed_probe_app(&sc).await;
    real_docker(&sc).await;

    let client = bollard::Docker::connect_with_local_defaults().unwrap();
    let launched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let launched2 = launched.clone();
    let result: Result<(), String> = (|| async {
        let (status, body) = sc
            .call("POST", "/api/launch/simple", Some(&json!({ "application_id": app_id })))
            .await;
        assert_eq!(status, StatusCode::OK, "{:?}", body);
        let session_id = body.unwrap()["session_id"].as_str().unwrap().to_string();
        let instance_id = sc
            .state
            .sessions
            .read()
            .await
            .get(&session_id)
            .cloned()
            .unwrap()["instance_id"]
            .as_str()
            .unwrap()
            .to_string();
        launched2.lock().unwrap().push(instance_id.clone());

        let container = client.inspect_container(&instance_id, None).await.unwrap();
        let devices = container
            .host_config
            .as_ref()
            .and_then(|h| h.devices.as_ref())
            .cloned()
            .unwrap_or_default();
        assert!(devices.is_empty(), "unexpected device passthrough: {devices:?}");
        let env = container
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .cloned()
            .unwrap_or_default();
        assert!(!env.iter().any(|e| e.starts_with("DRI_NODE=")));
        Ok(())
    })()
    .await;
    let ids = launched.lock().unwrap().clone();
    if let Err(e) = result {
        force_cleanup(&client, &ids).await;
        panic!("{e}");
    }
    force_cleanup(&client, &ids).await;
}
