//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! ANT-443 in-memory map prune + re-push generation (ANT-470, finding F5).
//!
//! `selkies_client_conns` and `last_pushed_token_tables` were inserted per
//! session but never removed on `stop_session`, so they grew one small entry
//! per session over a process lifetime. And concurrent connect/disconnect
//! refreshes each re-derived and POSTed independently, so an interleaving
//! could let an older derivation land after a newer one, reverting the table
//! until the next connection event. The fix prunes both maps in
//! `stop_session` and stamps every token-table derivation with a
//! per-session generation counter (taken before the connection snapshot is
//! read) so an older derivation is dropped when a newer one has already been
//! pushed.
//!
//! These tests drive the plain-session refresh phases directly against a
//! fake app container that records the token-table POSTs it receives.

mod common;

use common::{build_state, TempDir};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use vreckan::collaboration::{
    derive_plain_session_token_table, push_plain_session_token_table, PushedTokenTable,
};

/// Records the token-table POST bodies a fake app container receives.
#[derive(Clone)]
struct UpstreamCapture {
    tables: Arc<Mutex<Vec<Value>>>,
}

/// A fake app container: records every token-table POST body under the
/// selkies token-API path.
async fn spawn_capture_upstream() -> (String, UpstreamCapture) {
    use axum::{Json, Router};

    let cap = UpstreamCapture {
        tables: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new().route(
        "/api/apps/session/{*path}",
        axum::routing::post({
            let cap = cap.clone();
            move |Json(body): Json<Value>| {
                let cap = cap.clone();
                async move {
                    cap.tables.lock().await.push(body);
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("127.0.0.1:{}", addr.port()), cap)
}

/// A plain (non-collab) session in selkies secure mode: owner addressed by
/// the access token, one controller-role joiner in the viewers list.
async fn seed_plain_session(state: &vreckan::state::AppState, upstream: &str) -> String {
    let (ip, port) = upstream.rsplit_once(':').unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let session = json!({
        "instance_id": "ctr-1",
        "ip": ip,
        "port": port.parse::<i64>().unwrap(),
        "created_at": 1.0,
        "access_token": "at-1",
        "master_token": "mt-1",
        "is_collaboration": false,
        "provider_app_id": "ghost-app",
        "username": "admin",
        "app_name": "Test App",
        "app_logo": "x",
        "viewers": [{
            "token": "vb-1",
            "role": "controller",
            "permission": "participant",
            "username": "bob",
            "slot": null,
            "mk_control": false
        }],
        "container_registry": {},
    });
    state.sessions.write().await.insert(sid.clone(), session);
    sid
}

#[tokio::test]
async fn test_stop_session_prunes_ant443_maps() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let (upstream, _cap) = spawn_capture_upstream().await;
    let sid = seed_plain_session(&state, &upstream).await;

    // Simulate a session that carried traffic: every per-session map the
    // ANT-443 refresh keeps holds an entry for it.
    state
        .selkies_client_conns
        .write()
        .await
        .insert(sid.clone(), vec![("at-1".to_string(), 1)]);
    state.last_pushed_token_tables.write().await.insert(
        sid.clone(),
        PushedTokenTable {
            generation: 1,
            table: json!({
                "at-1": { "role": "controller", "slot": null, "mk_control": true }
            }),
        },
    );
    state
        .token_table_generations
        .write()
        .await
        .insert(sid.clone(), 1);

    vreckan::catalog::stop_session(&state, &sid).await;

    assert!(
        !state.sessions.read().await.contains_key(&sid),
        "the session itself must be removed"
    );
    assert!(
        !state.selkies_client_conns.read().await.contains_key(&sid),
        "selkies_client_conns must be pruned on stop_session"
    );
    assert!(
        !state
            .last_pushed_token_tables
            .read()
            .await
            .contains_key(&sid),
        "last_pushed_token_tables must be pruned on stop_session"
    );
    assert!(
        !state
            .token_table_generations
            .read()
            .await
            .contains_key(&sid),
        "token_table_generations must be pruned on stop_session"
    );
}

#[tokio::test]
async fn test_stale_derivation_dropped_after_newer_push() {
    let _root = TempDir::new();
    let state = build_state(_root.path()).await;
    let (upstream, cap) = spawn_capture_upstream().await;
    let sid = seed_plain_session(&state, &upstream).await;

    // The older refresh starts while only the owner is connected and
    // derives from that snapshot.
    state
        .selkies_client_conns
        .write()
        .await
        .insert(sid.clone(), vec![("at-1".to_string(), 1)]);
    let stale = derive_plain_session_token_table(&state, &sid)
        .await
        .expect("derive (stale)");
    assert_eq!(
        stale.table["at-1"]["mk_control"], true,
        "with only the owner connected the owner holds mk"
    );
    assert_eq!(
        stale.table["vb-1"]["mk_control"], false,
        "the unconnected joiner keeps the static default"
    );

    // A newer refresh starts after the joiner connects and pushes first.
    state.selkies_client_conns.write().await.insert(
        sid.clone(),
        vec![("at-1".to_string(), 1), ("vb-1".to_string(), 2)],
    );
    let fresh = derive_plain_session_token_table(&state, &sid)
        .await
        .expect("derive (fresh)");
    assert!(
        fresh.generation > stale.generation,
        "a later refresh must carry the larger generation"
    );
    assert_eq!(
        fresh.table["vb-1"]["mk_control"], true,
        "the newest connected controller holds mk"
    );
    assert_eq!(
        fresh.table["at-1"]["mk_control"], false,
        "mk left the owner on the hand-off"
    );
    assert!(
        push_plain_session_token_table(&state, &sid, &fresh).await,
        "the newer derivation must be pushed"
    );
    assert_eq!(
        cap.tables.lock().await.len(),
        1,
        "the newer push must reach the container"
    );

    // The older generation's push now lands: it must be dropped so the
    // container keeps the newer table.
    assert!(
        !push_plain_session_token_table(&state, &sid, &stale).await,
        "an older-generation derivation must be dropped after a newer push"
    );
    let tables = cap.tables.lock().await.clone();
    assert_eq!(
        tables.len(),
        1,
        "the stale re-derive must not POST a second table"
    );
    assert_eq!(
        tables[0]["vb-1"]["mk_control"], true,
        "the container must still hold the newer table"
    );

    // The recorded push must carry the newer generation, not the stale one.
    let pushed_guard = state.last_pushed_token_tables.read().await;
    let pushed = pushed_guard
        .get(&sid)
        .cloned()
        .expect("last_pushed_token_tables entry");
    assert_eq!(pushed.generation, fresh.generation);
    assert_eq!(pushed.table["vb-1"]["mk_control"], true);
}
