//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Session-share links (ANT-305, WP2) end-to-end: a real selkies app
//! container (`lscr.io/linuxserver/firefox:latest`) verifies the lazy
//! secure-mode promotion — the visitor's per-visit token must actually be
//! accepted by the container's token control plane and admitted through the
//! proxy. Skipped when Docker (or the image, or a usable network) is
//! unavailable, so non-docker runners are unaffected.
//!
//! The router-level suite (`test_session_shares.rs`) covers the legacy
//! `?role=` fallback with fake upstreams; here the container is the real
//! thing.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::{build_state, SecureClient, TempDir};
use http::StatusCode;
use serde_json::json;

const FIREFOX_IMAGE: &str = "lscr.io/linuxserver/firefox:latest";
const SELKIES_PORT: u16 = 3000;

// ---------------------------------------------------------------------------
// Docker helpers
// ---------------------------------------------------------------------------

async fn docker_available() -> Option<bollard::Docker> {
    match bollard::Docker::connect_with_local_defaults() {
        Ok(c) if c.ping().await.is_ok() => Some(c),
        _ => None,
    }
}

/// Pull the image when missing; false when the daemon has no image and
/// cannot pull it (offline runner).
async fn ensure_image(client: &bollard::Docker) -> bool {
    if client.inspect_image(FIREFOX_IMAGE).await.is_ok() {
        return true;
    }
    use futures_util::StreamExt;
    let options = bollard::query_parameters::CreateImageOptions {
        from_image: Some(FIREFOX_IMAGE.to_string()),
        ..Default::default()
    };
    let mut stream = client.create_image(Some(options), None, None);
    let mut ok = true;
    while let Some(event) = stream.next().await {
        if event.is_err() {
            ok = false;
            break;
        }
    }
    ok
}

/// Inspect our own container (by hostname, as `inspect_self_container` does)
/// and report its primary network plus the gateway on it.
async fn self_network_info(client: &bollard::Docker) -> Option<(String, String)> {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()?
        .trim()
        .to_string();
    if hostname.is_empty() {
        return None;
    }
    let c = client.inspect_container(&hostname, None).await.ok()?;
    let networks = c
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?
        .iter()
        .collect::<Vec<_>>();
    let (name, ep) = networks.first()?;
    let gateway = ep
        .gateway
        .clone()
        .filter(|g| !g.is_empty())
        .unwrap_or_default();
    Some((name.to_string(), gateway))
}

/// Launch a firefox container and return `(container id, "host:port")` that
/// the test process can reach. Prefers joining our own docker network and
/// addressing the container by its IP; falls back to publishing a host port
/// and addressing the docker-host gateway (default-bridge case); finally,
/// host networking.
async fn launch_firefox(
    client: &bollard::Docker,
    session_id: &str,
    master_token: &str,
) -> Option<(String, String)> {
    use bollard::models::{ContainerCreateBody, HostConfig, PortBinding};

    let (self_net, self_gw) = self_network_info(client).await.unwrap_or_default();

    // Decide how the test will reach the container, and how to wire it.
    let join_network = !self_net.is_empty()
        && self_net != "bridge"
        && self_net != "host"
        && self_net != "none";
    let publish = !join_network;

    let mut bindings = HashMap::new();
    if publish {
        bindings.insert(
            "3000/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("0.0.0.0".to_string()),
                host_port: Some("0".to_string()),
            }]),
        );
    }
    let body = ContainerCreateBody {
        image: Some(FIREFOX_IMAGE.to_string()),
        env: Some(vec![
            "TZ=America/Phoenix".to_string(),
            "PUID=1000".to_string(),
            "PGID=1000".to_string(),
            "SELKIES_ALLOWED_ORIGINS=*".to_string(),
            // Launch in Selkies "secure mode" exactly as vreckan does for a
            // room: the master token is known to us, and the SUBFOLDER is the
            // session's proxy prefix. This makes the `/api/tokens` push path
            // deterministic against the real container.
            format!("SELKIES_MASTER_TOKEN={master_token}"),
            format!("SUBFOLDER=/api/apps/session/{session_id}/"),
        ]),
        host_config: Some(HostConfig {
            port_bindings: if publish { Some(bindings) } else { None },
            network_mode: if join_network {
                Some(self_net.clone())
            } else if self_net == "host" {
                Some("host".to_string())
            } else {
                None
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    let cid = client
        .create_container(None, body)
        .await
        .ok()?
        .id;
    if client.start_container(&cid, None).await.is_err() {
        let _ = client.remove_container(&cid, None).await;
        return None;
    }

    // Resolve the address the test process can use to reach port 3000.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(info) = client.inspect_container(&cid, None).await {
            let settings = info.network_settings.as_ref();
            let target = if join_network {
                // Same network as us: address the container by its IP.
                settings
                    .and_then(|n| n.networks.as_ref())
                    .and_then(|ns| ns.get(&self_net))
                    .and_then(|e| e.ip_address.clone())
                    .filter(|ip| !ip.is_empty())
                    .map(|ip| format!("{ip}:{SELKIES_PORT}"))
            } else if self_net == "host" {
                Some(format!("127.0.0.1:{SELKIES_PORT}"))
            } else {
                // Default bridge: reach the published port via the docker-host
                // gateway as seen from our (bridge) network namespace.
                let gw = if self_gw.is_empty() { None } else { Some(self_gw.clone()) };
                let port = settings
                    .and_then(|n| n.ports.as_ref())
                    .and_then(|m| m.get("3000/tcp"))
                    .and_then(|v| v.as_ref())
                    .and_then(|v| v.first())
                    .and_then(|b| b.host_port.as_deref())
                    .filter(|p| !p.is_empty())
                    .map(|p| p.to_string());
                match (gw, port) {
                    (Some(g), Some(p)) => Some(format!("{g}:{p}")),
                    _ => None,
                }
            };
            if let Some(target) = target {
                return Some((cid, target));
            }
        }
        if Instant::now() > deadline {
            let _ = client.remove_container(&cid, None).await;
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Poll the container root until it answers 2xx (selkies is up).
async fn wait_container_ready(base_url: &str, seconds: u64) -> bool {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if client
            .get(format!("{base_url}/"))
            .send()
            .await
            .ok()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Stop and remove a firefox container, awaiting inline so the teardown is
/// guaranteed to run before the test's runtime shuts down (a `tokio::spawn`
/// here would be dropped and leak the container, especially in CI).
async fn cleanup_firefox(client: bollard::Docker, cid: String) {
    let _ = client
        .stop_container(
            &cid,
            Some(bollard::query_parameters::StopContainerOptions {
                signal: None,
                t: Some(5),
            }),
        )
        .await;
    let _ = client.remove_container(&cid, None).await;
}

// ---------------------------------------------------------------------------
// Shared helpers (mirror test_session_shares.rs)
// ---------------------------------------------------------------------------

async fn admin_client(root: &std::path::Path) -> SecureClient {
    let mut sc = SecureClient::new(build_state(root).await);
    sc.login("admin", "admin1234").await;
    sc.handshake().await;
    sc
}

fn location(resp: &common::RawResponse) -> String {
    resp.header("location").unwrap_or_default()
}

async fn seed_session(
    state: &vreckan::state::AppState,
    upstream: &str,
    sid: &str,
    master_token: &str,
) -> String {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let access_token = format!("access-{}", uuid::Uuid::new_v4());
    let session = json!({
        "instance_id": "ctr-e2e",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": access_token,
        "master_token": master_token,
        "is_collaboration": false,
        "username": "admin",
        "app_name": "Firefox",
        "viewers": [],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.to_string(), session);
    access_token
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_share_visit_e2e_real_selkies_container() {
    let client = match docker_available().await {
        Some(c) => c,
        None => {
            eprintln!("skip: docker daemon unavailable");
            return;
        }
    };
    if !ensure_image(&client).await {
        eprintln!("skip: {FIREFOX_IMAGE} not present and pull failed");
        return;
    }
    // Pre-generate the session id and master token: the container must be
    // launched with the SUBFOLDER (which embeds the id) and the SELKIES
    // master token that vreckan will use for the `/api/tokens` push.
    let sid = uuid::Uuid::new_v4().to_string();
    let master = format!("master-{}", uuid::Uuid::new_v4().simple());
    let (cid, upstream) = match launch_firefox(&client, &sid, &master).await {
        Some(v) => v,
        None => {
            eprintln!("skip: firefox container failed to start/address");
            return;
        }
    };
    if !wait_container_ready(&format!("http://{upstream}"), 120).await {
        cleanup_firefox(client.clone(), cid.clone()).await;
        eprintln!("skip: firefox container never became ready");
        return;
    }

    let _root = TempDir::new();
    let sc = admin_client(_root.path()).await;
    seed_session(&sc.state, &upstream, &sid, &master).await;

    // A full share on a plain session.
    let (status, body) = sc
        .call(
            "POST",
            &format!("/api/sessions/{sid}/shares"),
            Some(&json!({ "role": "full" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "create share: {body:?}");
    let share_id = body
        .expect("body")
        .get("share_id")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    // The public entry must promote the real container (its selkies token
    // endpoint answers 2xx) and redirect with the per-visit token.
    let grant = sc.http.get(&format!("/share/{share_id}")).await;
    assert_eq!(
        grant.status,
        StatusCode::SEE_OTHER,
        "grant: {:?}",
        grant.body
    );
    let loc = location(&grant);
    let token = loc
        .strip_prefix(&format!("/api/apps/session/{sid}/?token="))
        .expect(&format!("promoted redirect: {loc}"))
        .split('&')
        .next()
        .expect("token")
        .to_string();
    let session = sc
        .state
        .sessions
        .read()
        .await
        .get(&sid)
        .cloned()
        .expect("session");
    // The push reached the real selkies (its `/api/tokens` answered 2xx), so
    // the master token vreckan minted is retained — i.e. the session is now
    // in secure mode. If the push had failed, `public_share_get` would have
    // undone the mint.
    assert_eq!(
        session
            .get("master_token")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        master.as_str(),
        "session should have been promoted to secure mode (master token retained)"
    );
    let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
    assert!(
        viewers
            .iter()
            .any(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str())),
        "visitor must be in the session's viewers list"
    );

    // The proxy admits the visitor token and reaches the real container.
    let page = sc
        .http
        .get(&format!("/api/apps/session/{sid}/?token={token}"))
        .await;
    assert_eq!(
        page.status,
        StatusCode::OK,
        "session page: {}",
        String::from_utf8_lossy(&page.body)
    );
    let set_cookie = page.header("set-cookie").unwrap_or_default();
    assert!(
        set_cookie.contains("collab_token_"),
        "visitor cookie must be set: {set_cookie}"
    );

    // ...and the cookie alone (no query token) keeps working afterwards.
    let via_cookie = sc
        .http
        .get_with(
            &format!("/api/apps/session/{sid}/"),
            &{
                let mut h = HashMap::new();
                h.insert("cookie".to_string(), set_cookie);
                h
            },
        )
        .await;
    assert_eq!(
        via_cookie.status,
        StatusCode::OK,
        "cookie session page: {}",
        String::from_utf8_lossy(&via_cookie.body)
    );

    cleanup_firefox(client, cid).await;
}
