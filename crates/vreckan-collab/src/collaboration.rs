//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Collaboration-room feature: the room HTML page, its static assets, and the
//! room WebSocket that routes messages between a controller and viewers.
//!
//! Ports `server/collaboration`. The room lets a controller share a running
//! app session with viewers; the controller can assign gamepad slots and
//! mouse/keyboard control, viewers can chat, and binary (input/audio) packets
//! are relayed to the app container.

use std::collections::HashMap;

use axum::extract::{
    ws::{Message, WebSocketUpgrade},
    Path as PathParam, Query, State,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use rand::RngExt;
use serde::Deserialize;
use serde_json::Value;
use tracing::{error, info, warn};

use vreckan_core::state::AppState;
use vreckan_core::tokens::opt_tokens_equal;
use vreckan_core::tokens::tokens_equal;
// Re-exported so the pre-split paths keep resolving from other crates
// (ANT-770).
pub use vreckan_core::shared::{ConnectionInfo, PushedTokenTable, RoomConnections};

// ---------------------------------------------------------------------------
// Room connection state (mirrors `collaboration.ROOM_CONNECTIONS`)
// ---------------------------------------------------------------------------



// ---------------------------------------------------------------------------
// Static room assets
// ---------------------------------------------------------------------------

fn serve_room_asset(filename: &str, mime: &str) -> Response {
    let path = vreckan_core::static_files::static_dir().join("collaboration").join(filename);
    match std::fs::read(&path) {
        Ok(data) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "no-store")
            .body(axum::body::Body::from(data))
            .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
        Err(_) => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

async fn get_room_css() -> Response {
    serve_room_asset("room.css", "text/css")
}

async fn get_room_js() -> Response {
    serve_room_asset("room.js", "application/javascript")
}

async fn get_translation_js() -> Response {
    serve_room_asset("translation.js", "application/javascript")
}

// ---------------------------------------------------------------------------
// Room page (GET /room/{session_id})
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RoomQuery {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// Render the collaboration room HTML page. Mirrors `collaborative_room`.
async fn collaborative_room(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(session_id): PathParam<String>,
    Query(q): Query<RoomQuery>,
) -> Response {
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(mut session_data) = session_data else {
        return (
            StatusCode::NOT_FOUND,
            "Collaboration room not found.",
        )
            .into_response();
    };
    if !session_data.get("is_collaboration").and_then(|v| v.as_bool()).unwrap_or(false) {
        return (
            StatusCode::NOT_FOUND,
            "Collaboration room not found.",
        )
            .into_response();
    }

    // Migrate the legacy viewer_token to participant_invite_token.
    if session_data.get("viewer_token").is_some()
        && session_data.get("participant_invite_token").is_none()
    {
        if let Some(vt) = session_data.get("viewer_token").cloned() {
            session_data["participant_invite_token"] = vt;
            session_data.as_object_mut().map(|o| o.remove("viewer_token"));
        }
    }
    if session_data.get("readonly_invite_token").is_none() {
        session_data["readonly_invite_token"] = Value::String(vreckan_core::shared::random_token(16));
    }

    let cookie_name = &state.settings.session_cookie_name;
    let main_access_token = q
        .access_token
        .clone()
        .or_else(|| cookie_from_headers(&headers, &format!("{cookie_name}_{session_id}")));
    let collab_token = q.token.clone();

    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let access_token = session_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_controller_by_session = main_access_token
        .as_deref()
        .is_some_and(|t| tokens_equal(t, access_token));
    let is_controller_by_collab = collab_token
        .as_deref()
        .is_some_and(|t| tokens_equal(t, controller_token));
    let is_viewer_by_collab = session_data
        .get("viewers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|v| {
                opt_tokens_equal(
                    v.get("token").and_then(|t| t.as_str()),
                    collab_token.as_deref(),
                )
            })
        })
        .unwrap_or(false);
    let is_new_participant_by_collab = opt_tokens_equal(
        collab_token.as_deref(),
        session_data
            .get("participant_invite_token")
            .and_then(|v| v.as_str()),
    );
    let is_new_readonly_by_collab = opt_tokens_equal(
        collab_token.as_deref(),
        session_data
            .get("readonly_invite_token")
            .and_then(|v| v.as_str()),
    );

    if !(is_controller_by_session
        || is_controller_by_collab
        || is_viewer_by_collab
        || is_new_participant_by_collab
        || is_new_readonly_by_collab)
    {
        return (
            StatusCode::UNAUTHORIZED,
            "Invalid or missing authentication token.",
        )
            .into_response();
    }

    let template_path = vreckan_core::static_files::static_dir()
        .join("collaboration")
        .join("room.html");
    let html_template = match std::fs::read_to_string(&template_path) {
        Ok(t) => t,
        Err(e) => {
            warn!("Collaboration room template not found: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Room UI template is missing.",
            )
                .into_response();
        }
    };

    // `user_role` and `user_token` are assigned in every non-returning branch
    // below (the final `else` returns), so definite assignment is enough.
    let user_role;
    let user_token;
    let mut user_permission = "participant";

    if is_controller_by_session || is_controller_by_collab {
        user_role = "controller";
        user_token = Some(controller_token.to_string());
    } else if is_viewer_by_collab {
        user_role = "viewer";
        user_token = collab_token.clone();
        if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
            if let Some(vd) = viewers.iter().find(|v| {
                opt_tokens_equal(
                    v.get("token").and_then(|t| t.as_str()),
                    collab_token.as_deref(),
                )
            }) {
                user_permission = vd
                    .get("permission")
                    .and_then(|p| p.as_str())
                    .unwrap_or("participant");
            }
        }
    } else if is_new_participant_by_collab || is_new_readonly_by_collab {
        let permission = if is_new_participant_by_collab {
            "participant"
        } else {
            "readonly"
        };
        let new_viewer_token = vreckan_core::shared::random_token(16);
        let new_username = format!("User-{}", rand::rng().random_range(100..999));
        // Add the viewer entry in one sessions write-lock critical section,
        // re-validating the redeemed token against the live session: two
        // visitors redeeming the same invite link at once each commit their
        // own entry instead of the later whole-object write-back clobbering
        // the earlier one (ANT-887, audit finding 16). The invite-token
        // migration applied to the first clone above is re-applied to the
        // live value for the same reason.
        //
        // Per-session push lock held from here through the push below
        // (ANT-927, same discipline as the slot/MK handlers): a viewer
        // join's push cannot be overtaken by a later-committed mutation's
        // push on this session, and vice versa. The guard lives to the end
        // of this branch (both exits return).
        let push_lock = session_push_lock(&state, &session_id);
        let _push = push_lock.lock().await;
        let committed = {
            let mut sessions = state.sessions.write().await;
            let Some(mut live) = sessions.get(&session_id).cloned() else {
                return (
                    StatusCode::NOT_FOUND,
                    "Collaboration room not found.",
                )
                    .into_response();
            };
            // Migrate before comparing: a legacy session carries the invite
            // under `viewer_token` until the rename lands, and the first
            // clone's admission check ran against the migrated value.
            if live.get("viewer_token").is_some()
                && live.get("participant_invite_token").is_none()
            {
                if let Some(vt) = live.get("viewer_token").cloned() {
                    live["participant_invite_token"] = vt;
                    live.as_object_mut().map(|o| o.remove("viewer_token"));
                }
            }
            if live.get("readonly_invite_token").is_none() {
                live["readonly_invite_token"] =
                    Value::String(vreckan_core::shared::random_token(16));
            }
            let invite_token = if is_new_participant_by_collab {
                live.get("participant_invite_token").and_then(|v| v.as_str())
            } else {
                live.get("readonly_invite_token").and_then(|v| v.as_str())
            };
            if opt_tokens_equal(collab_token.as_deref(), invite_token) {
                if let Some(viewers) = live.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                    viewers.push(serde_json::json!({
                        "token": new_viewer_token,
                        "slot": Value::Null,
                        "mk_control": false,
                        "username": new_username,
                        "permission": permission,
                    }));
                }
                sessions.insert(session_id.clone(), live.clone());
                Some(live)
            } else {
                None
            }
        };
        match committed {
            // The re-validation raced a session change between the first
            // read and the lock; treat it like the stale-link case below.
            None => {
                return (
                    StatusCode::FORBIDDEN,
                    Html("<h1>Invalid or expired collaboration link.</h1>"),
                )
                    .into_response();
            }
            Some(committed) => {
                // The write-back already landed and the write lock is
                // dropped, so the network push no longer spans the
                // read-modify-write.
                push_token_table(&state, &session_id, &committed).await;
                // WS/visit flow: in-memory state is authoritative and the
                // next session event retries the full replace (audit finding 1).
                if let Err(e) = vreckan_core::state::save_sessions(&state).await {
                    error!("[{session_id}] Failed to persist session after collab link commit: {e}");
                }
            }
        }
        // Redirect with the new viewer token. The Location carries the
        // visitor's per-visit token, so keep it out of any caching proxy's
        // store, as the share redirect does (ANT-322 F5; ANT-344, item 2).
        let redirect = format!("/room/{session_id}?token={new_viewer_token}");
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, redirect)
            .header(header::CACHE_CONTROL, "private, no-store")
            .body(axum::body::Body::empty())
            .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response());
    } else {
        return (
            StatusCode::FORBIDDEN,
            Html("<h1>Invalid or expired collaboration link.</h1>"),
        )
            .into_response();
    }

    let user_token = user_token.unwrap_or_default();
    let iframe_src = format!("/api/apps/session/{session_id}/?token={user_token}");

    // The live tokens are injected as static JS below. Any script execution
    // on the room page (XSS) could read them. Accepted under the v1
    // "viewers trust the server operator" premise (board decision D1,
    // ANT-310): the page ships a strict per-request nonce CSP, so only
    // operator-served same-origin scripts run. Hardening path: short-lived
    // one-time join tokens exchanged at the WS upgrade (ANT-344, item 4).
    let mut client_data = serde_json::json!({
        "sessionId": session_id,
        "userRole": user_role,
        "userToken": user_token,
        "userPermission": user_permission,
    });
    if user_role == "controller" {
        client_data["participantJoinUrl"] = Value::String(format!(
            "/room/{session_id}?token={}",
            session_data
                .get("participant_invite_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
        client_data["readonlyJoinUrl"] = Value::String(format!(
            "/room/{session_id}?token={}",
            session_data
                .get("readonly_invite_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
    } else if user_role == "viewer" && user_permission == "participant" {
        client_data["readonlyJoinUrl"] = Value::String(format!(
            "/room/{session_id}?token={}",
            session_data
                .get("readonly_invite_token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
    }

    let nonce = {
        let mut buf = [0u8; 16];
        rand::rng().fill_bytes(&mut buf);
        base64::engine::general_purpose::STANDARD.encode(buf)
    };
    let html = html_template
        .replace("{{IFRAME_SRC}}", &iframe_src)
        .replace(
            "<!-- CLIENT_DATA -->",
            &format!(
                "<script nonce=\"{nonce}\">window.COLLAB_DATA = {};</script>",
                client_data
            ),
        );

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, vreckan_core::shared::strict_csp(&nonce))
        .body(axum::body::Body::from(html))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response());

    // Set the session cookie (if an access token was provided) and the collab
    // token cookie (for the iframe).
    if let Some(auth_token) = &q.access_token {
        let set_cookie = format!(
            "{}_{session_id}={auth_token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite=lax",
            state.settings.session_cookie_name
        );
        if let Ok(hv) = HeaderValue::from_str(&set_cookie) {
            response.headers_mut().append(header::SET_COOKIE, hv);
        }
    }
    if !user_token.is_empty() {
        let set_cookie = format!(
            "collab_token_{session_id}={user_token}; Path=/api/apps/session/{session_id}; HttpOnly; Secure; SameSite=none"
        );
        if let Ok(hv) = HeaderValue::from_str(&set_cookie) {
            response.headers_mut().append(header::SET_COOKIE, hv);
        }
    }
    response
}

/// Extract a cookie value by name from the headers.
fn cookie_from_headers(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        if let Ok(s) = value.to_str() {
            for pair in s.split(';') {
                let pair = pair.trim();
                if let Some((k, v)) = pair.split_once('=') {
                    if k.trim() == name {
                        return Some(v.trim().to_string());
                    }
                }
            }
        }
    }
    None
}

/// The first 8 chars of a token: enough to tell two room users apart, too
/// short to recover the token (ANT-344, item 1). Char-boundary safe for any
/// string. Since ANT-882 it doubles as the room's non-secret identity key in
/// broadcast frames — a truncated prefix is not replayable for admission,
/// because `tokens_equal` is a length-checked full compare.
fn short_token(token: &str) -> &str {
    let mut end = token.len().min(8);
    while end > 0 && !token.is_char_boundary(end) {
        end -= 1;
    }
    &token[..end]
}

/// Resolve a room identity submitted by a room client — either a full token
/// or its non-secret 8-char broadcast form (ANT-882) — to the full token it
/// addresses: the controller token or a stored viewer token. Returns `None`
/// when the key matches no stored token (a stale or unknown key).
fn resolve_room_token(session_data: &Value, key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let controller = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !controller.is_empty()
        && (tokens_equal(key, controller) || short_token(controller) == key)
    {
        return Some(controller.to_string());
    }
    if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
        for v in viewers {
            let t = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            if !t.is_empty() && (tokens_equal(key, t) || short_token(t) == key) {
                return Some(t.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Broadcast helpers
// ---------------------------------------------------------------------------

/// Send a JSON payload to every live connection in the room. Mirrors
/// `broadcast_to_room`.
async fn broadcast_to_room(state: &AppState, session_id: &str, payload: &Value) {
    let conns = state.room_connections.read().await.get(session_id).cloned();
    let Some(conns) = conns else {
        return;
    };
    let mut senders: Vec<tokio::sync::mpsc::Sender<Message>> = Vec::new();
    if let Some(c) = &conns.controller {
        senders.push(c.sender.clone());
    }
    for v in conns.viewers.values() {
        senders.push(v.sender.clone());
    }
    let msg = Message::Text(payload.to_string().into());
    for s in senders {
        let m = msg.clone();
        let _ = s.send(m).await;
    }
}

/// Send a binary payload to every live connection except the sender. Mirrors
/// `broadcast_binary_to_room`.
async fn broadcast_binary_to_room(
    state: &AppState,
    session_id: &str,
    payload: &[u8],
    sender_token: &str,
    is_controller: bool,
) {
    let conns = state.room_connections.read().await.get(session_id).cloned();
    let Some(conns) = conns else {
        return;
    };
    let mut senders: Vec<tokio::sync::mpsc::Sender<Message>> = Vec::new();
    if let Some(c) = &conns.controller {
        if is_controller && tokens_equal(&c.token, sender_token) {
            // skip the controller sender (it's the originator)
        } else {
            senders.push(c.sender.clone());
        }
    }
    for v in conns.viewers.values() {
        if !tokens_equal(&v.token, sender_token) {
            senders.push(v.sender.clone());
        }
    }
    let msg = Message::Binary(payload.to_vec().into());
    for s in senders {
        let m = msg.clone();
        let _ = s.send(m).await;
    }
}

/// Build and broadcast the current room state (who's online, slots, MK).
/// Mirrors `broadcast_state`.
async fn broadcast_state(state: &AppState, session_id: &str) {
    let conns = state.room_connections.read().await.get(session_id).cloned();
    let session_data = state.sessions.read().await.get(session_id).cloned();
    let (Some(conns), Some(session_data)) = (conns, session_data) else {
        return;
    };

    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mk_owner = session_data.get("mk_owner_token").and_then(|v| v.as_str());
    let controller_online = conns.controller.is_some();
    let controller_public_id = conns
        .controller
        .as_ref()
        .map(|c| c.public_id.clone())
        .unwrap_or_default();

    let controller_public_id_value: Value = if controller_online {
        Value::String(controller_public_id)
    } else {
        Value::Null
    };
    // The frame carries each user's non-secret identity key (the 8-char
    // short form), never the full token: a viewer must not end up holding a
    // replayable control credential (finding 11, ANT-882). Admission still
    // requires the exact full token, so the short form grants nothing.
    let controller_info = serde_json::json!({
        "token": short_token(controller_token),
        "username": "Controller",
        "slot": session_data.get("controller_slot").cloned().unwrap_or(Value::Null),
        "online": controller_online,
        "has_mk": mk_owner.is_none() || mk_owner.is_some_and(|o| tokens_equal(o, controller_token)),
        "permission": "controller",
        "publicId": controller_public_id_value,
    });

    let online_viewer_tokens: std::collections::HashSet<String> =
        conns.viewers.keys().cloned().collect();
    let mut users_with_status = vec![controller_info];
    if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
        for v in viewers {
            let mut viewer_info = v.clone();
            let vtoken = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            let is_online = online_viewer_tokens.contains(vtoken);
            if let Some(obj) = viewer_info.as_object_mut() {
                // Replace the full token with its non-secret identity key
                // (finding 11, ANT-882).
                obj.insert("token".into(), serde_json::json!(short_token(vtoken)));
                obj.insert(
                    "has_mk".into(),
                    serde_json::json!(mk_owner.is_some_and(|o| tokens_equal(o, vtoken))),
                );
                obj.insert("online".into(), serde_json::json!(is_online));
                if is_online {
                    if let Some(conn) = conns.viewers.get(vtoken) {
                        obj.insert("publicId".into(), serde_json::json!(conn.public_id));
                    }
                }
            }
            users_with_status.push(viewer_info);
        }
    }
    // The stored speaker is a full token; the frame carries the same
    // non-secret 8-char identity key as the viewer list (finding 11,
    // ANT-882).
    let designated_speaker = session_data
        .get("designated_speaker")
        .and_then(|v| v.as_str())
        .map(|t| Value::String(short_token(t).to_string()))
        .unwrap_or(Value::Null);
    // Mixed-version tolerance (ANT-882): an un-upgraded client matches its
    // own list entry against the page-local full token and keys its own
    // stream on it, so each recipient gets its own entry back in the
    // full-token form. Every *other* entry stays in the non-secret 8-char
    // identity-key form — no client ever sees a foreign full token — and the
    // new client accepts both forms, so the frame is safe in either
    // deployment direction.
    let frame_for = |own_token: Option<&str>| {
        let mut viewers = users_with_status.clone();
        if let Some(own) = own_token.filter(|t| !t.is_empty()) {
            for v in viewers.iter_mut() {
                if v.get("token").and_then(|t| t.as_str()) == Some(short_token(own)) {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("token".into(), Value::String(own.to_string()));
                    }
                }
            }
        }
        serde_json::json!({
            "type": "state_update",
            "viewers": viewers,
            "designated_speaker": designated_speaker,
        })
    };
    if let Some(c) = &conns.controller {
        let _ = c
            .sender
            .send(Message::Text(frame_for(Some(controller_token)).to_string().into()))
            .await;
    }
    for v in conns.viewers.values() {
        let _ = v
            .sender
            .send(Message::Text(frame_for(Some(&v.token)).to_string().into()))
            .await;
    }
}

/// The token that addresses the session owner in the token table. A room
/// session carries a dedicated controller token; a plain (non-room) session
/// has none, so its owner is addressed by the session's access token
/// (relevant once a share session is promoted to secure mode, ANT-305).
fn session_owner_token(session_data: &Value) -> String {
    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if controller_token.is_empty() {
        session_data
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        controller_token.to_string()
    }
}

/// Build the full token table (owner entry + viewer entries) that gets
/// provisioned to the app container. The role/slot fields mirror the session
/// data; `owner_mk` and `viewer_mk` decide each entry's `mk_control`. A
/// viewer entry may carry an explicit `role` (used by session shares: a full
/// share or a self-serve join on a promoted plain session lists its visitor
/// as `controller`); entries without one default to `viewer`.
fn build_token_table(
    session_data: &Value,
    owner_mk: bool,
    viewer_mk: &impl Fn(&str) -> bool,
) -> Value {
    let owner_token = session_owner_token(session_data);
    let mut all_tokens = serde_json::Map::new();
    if !owner_token.is_empty() {
        all_tokens.insert(
            owner_token,
            serde_json::json!({
                "role": "controller",
                "slot": session_data.get("controller_slot").cloned().unwrap_or(Value::Null),
                "mk_control": owner_mk,
            }),
        );
    }
    if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
        for v in viewers {
            let vtoken = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            if vtoken.is_empty() {
                continue;
            }
            all_tokens.insert(
                vtoken.to_string(),
                serde_json::json!({
                    "role": v.get("role").and_then(|r| r.as_str()).unwrap_or("viewer"),
                    "slot": v.get("slot").cloned().unwrap_or(Value::Null),
                    "mk_control": viewer_mk(vtoken),
                }),
            );
        }
    }
    Value::Object(all_tokens)
}

/// Push the per-token state (slots + MK control) to the app container's token
/// endpoint. Mirrors `broadcast_token_state`.
///
/// Returns true when the table was actually accepted by the container (a 2xx
/// from its token endpoint).
pub async fn push_token_table(state: &AppState, session_id: &str, session_data: &Value) -> bool {
    let mk_owner = session_data.get("mk_owner_token").and_then(|v| v.as_str());
    let owner_token = session_owner_token(session_data);
    let table = build_token_table(
        session_data,
        if mk_owner.is_some() {
            mk_owner.is_some_and(|o| tokens_equal(o, &owner_token))
        } else {
            true
        },
        &|t: &str| mk_owner.is_some_and(|o| tokens_equal(o, t)),
    );
    // The manual mk-transfer push records the currently known generation
    // (no new derivation happens here, so no stamp is taken).
    let generation = state
        .token_table_generations
        .read()
        .await
        .get(session_id)
        .copied()
        .unwrap_or(0);
    post_token_table(state, session_id, session_data, &table, generation).await
}

/// The per-session token-table push lock (ANT-927). Collab mutations that
/// commit under the sessions write lock must acquire it FIRST and hold it
/// through the container POST: the sessions lock orders the commits and
/// this lock orders the pushes, so the last-write-wins container records
/// pushes in commit order and a table built from an older commit can no
/// longer land after a newer one (the flake behind
/// `concurrent_slot_assignments_do_not_clobber_each_other`). Every path
/// takes the locks in the same order (push lock -> sessions lock) and
/// nothing acquires the push lock while holding the sessions lock, so no
/// deadlock.
fn session_push_lock(
    state: &AppState,
    session_id: &str,
) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut locks = state
        .token_table_push_locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    locks
        .entry(session_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}


/// A derived plain-session token table plus the generation stamped when the
/// derivation started (ANT-470).
#[derive(Debug, Clone)]
pub struct PlainTokenTableDerivation {
    pub generation: u64,
    pub table: Value,
}

/// Derive the token table of a plain session from the current data-plane
/// connection snapshot, stamping the derivation with the session's next
/// refresh generation (ANT-470).
///
/// The stamp is taken BEFORE the snapshot is read: a refresh that starts
/// later reads a snapshot at least as new and therefore carries the larger
/// generation, so a push built from an older snapshot can be recognized as
/// stale at commit time. Returns `None` for sessions the refresh does not
/// manage (collab rooms, legacy sessions, sessions already gone).
pub async fn derive_plain_session_token_table(
    state: &AppState,
    session_id: &str,
) -> Option<PlainTokenTableDerivation> {
    let session = state.sessions.read().await.get(session_id).cloned()?;
    let is_collab = session
        .get("is_collaboration")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_master = session
        .get("master_token")
        .and_then(|v| v.as_str())
        .map(|m| !m.is_empty())
        .unwrap_or(false);
    if is_collab || !has_master {
        return None;
    }

    let generation = {
        let mut gens = state.token_table_generations.write().await;
        let entry = gens.entry(session_id.to_string()).or_insert(0);
        *entry += 1;
        *entry
    };

    let owner_token = session_owner_token(&session);
    let conns = state
        .selkies_client_conns
        .read()
        .await
        .get(session_id)
        .cloned()
        .unwrap_or_default();
    // The newest connected controller token. Viewers never claim the primary
    // display (the server takes their SETTINGS down the viewer path), so only
    // controller entries can be the active driver.
    let newest = conns
        .iter()
        .rev()
        .find(|(tok, _)| {
            if tokens_equal(tok, &owner_token) {
                return true;
            }
            session
                .get("viewers")
                .and_then(|v| v.as_array())
                .is_some_and(|viewers| {
                    viewers.iter().any(|v| {
                        v.get("token")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| tokens_equal(t, tok))
                            && v.get("role").and_then(|r| r.as_str()) == Some("controller")
                    })
                })
        })
        .map(|(tok, _)| tok.clone());

    let owner_mk: bool;
    let viewer_mk: Box<dyn Fn(&str) -> bool + Send>;
    match newest {
        Some(holder) => {
            owner_mk = tokens_equal(&holder, &owner_token);
            viewer_mk = Box::new(move |t: &str| tokens_equal(&holder, t));
        }
        None => {
            // No data-plane client connected: keep the static default (the
            // owner holds mk; an explicit mk owner, if any, wins).
            let mk_owner = session
                .get("mk_owner_token")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            owner_mk = match mk_owner.as_deref() {
                Some(o) => tokens_equal(o, &owner_token),
                None => true,
            };
            viewer_mk =
                Box::new(move |t: &str| mk_owner.as_deref().is_some_and(|o| tokens_equal(o, t)));
        }
    }

    let table = build_token_table(&session, owner_mk, &viewer_mk);
    Some(PlainTokenTableDerivation { generation, table })
}

/// Commit a derived plain-session token table: skip the POST when the
/// container already holds the table (the ANT-443 dedupe), drop the push
/// when a newer generation's derivation has already been pushed (ANT-470),
/// and record the accepted table on success. Returns true when a push went
/// out and was accepted (a 2xx).
pub async fn push_plain_session_token_table(
    state: &AppState,
    session_id: &str,
    derivation: &PlainTokenTableDerivation,
) -> bool {
    let session = state.sessions.read().await.get(session_id).cloned();
    let Some(session) = session else {
        return false;
    };
    let changed = state
        .last_pushed_token_tables
        .read()
        .await
        .get(session_id)
        .map(|last| last.table != derivation.table)
        .unwrap_or(true);
    if !changed {
        return false;
    }
    let latest = state
        .token_table_generations
        .read()
        .await
        .get(session_id)
        .copied()
        .unwrap_or(0);
    // The stamp was taken before the snapshot read, so a strictly newer stamp
    // belongs to a refresh that saw a newer (or equal) connection state; its
    // push must not be overwritten by this older derivation.
    if derivation.generation < latest {
        return false;
    }
    post_token_table(
        state,
        session_id,
        &session,
        &derivation.table,
        derivation.generation,
    )
    .await
}

/// Re-derive and re-push the token table of a plain session after a
/// data-plane connection change (ANT-443).
///
/// Selkies 2.0.0 keeps `active_mk_token` as the first `mk_control:true` entry
/// of the last-pushed table and never clears it when that token's client
/// disconnects. On the WebSockets "newest-controller-wins" hand-off the
/// surviving joiner therefore sits at `MK_ACCESS=false` — its own table entry
/// is irrelevant to the verdict; the killed owner's token still holds the
/// global mk grant. This refresh grants `mk_control` only to the most
/// recently connected controller, so the hand-off re-grants MK to the
/// survivor without a client reconnect; when no client is connected it keeps
/// the static default (owner holds mk). Room sessions keep their own
/// mk-transfer mechanism, and legacy (non-secure) sessions have no table.
/// Concurrent refreshes are ordered by the per-session generation counter
/// (ANT-470) so an older derivation cannot land after a newer push.
pub async fn refresh_plain_session_token_table(state: &AppState, session_id: &str) {
    let Some(derivation) = derive_plain_session_token_table(state, session_id).await else {
        return;
    };
    push_plain_session_token_table(state, session_id, &derivation).await;
}

/// POST a built token table to the container's token endpoint. Returns true
/// when the container accepted it (a 2xx). `generation` is the refresh
/// generation that produced the table (ANT-470) and is recorded with the
/// accepted table.
async fn post_token_table(
    state: &AppState,
    session_id: &str,
    session_data: &Value,
    table: &Value,
    generation: u64,
) -> bool {
    // Build the target host:port set (session ip + container registry).
    let mut targets: Vec<(String, Option<u16>)> = Vec::new();
    if let Some(ip) = session_data.get("ip").and_then(|v| v.as_str()) {
        let port = session_data
            .get("port")
            .and_then(|v| v.as_u64())
            .map(|p| p as u16);
        targets.push((ip.to_string(), port));
    }
    if let Some(registry) = session_data
        .get("container_registry")
        .and_then(|v| v.as_object())
    {
        for c in registry.values() {
            if let Some(ip) = c.get("ip").and_then(|v| v.as_str()) {
                let port = c.get("port").and_then(|v| v.as_u64()).map(|p| p as u16);
                if !targets.iter().any(|(h, _)| h == ip) {
                    targets.push((ip.to_string(), port));
                }
            }
        }
    }
    if targets.is_empty() {
        return false;
    }

    let master_token = session_data
        .get("master_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bearer = format!("Bearer {master_token}");
    let mut stacked = HashMap::new();
    stacked.insert("Selkies-Authorization".to_string(), bearer.clone());
    if let (Some(user), Some(pass)) = (
        session_data.get("custom_user").and_then(|v| v.as_str()),
        session_data.get("password").and_then(|v| v.as_str()),
    ) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        stacked.insert("Authorization".to_string(), format!("Basic {b64}"));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .unwrap_or_default();
    for (ip, port) in targets {
        let mut urls: Vec<String> = Vec::new();
        if let Some(p) = port {
            // Selkies serves its token API under the session's SUBFOLDER
            // (`/api/apps/session/{id}/`), so the endpoint is
            // `/api/apps/session/{id}/api/tokens`, not `/{id}/api/tokens`.
            urls.push(format!(
                "http://{ip}:{p}/api/apps/session/{session_id}/api/tokens"
            ));
        }
        urls.push(format!("http://{ip}:8083/tokens"));
        for url in urls {
            let is_api = url.ends_with("/api/tokens");
            let mut req = client.post(&url).json(table);
            if is_api {
                for (k, v) in &stacked {
                    req = req.header(k.as_str(), v.as_str());
                }
            } else {
                req = req.header("Authorization", bearer.clone());
            }
            // A 2xx means the container accepted the table. Other statuses
            // (e.g. 404 "not in secure mode" from selkies 3.x, or 401 from
            // an older image with a different master token) and send errors
            // try the next candidate URL.
            if let Ok(resp) = req.send().await {
                if resp.status().is_success() {
                    // Record for the connect/disconnect refresh's dedupe
                    // (ANT-443): an unchanged re-derivation skips the
                    // POST. The stamp lets a later refresh tell whether
                    // the container holds the newest push (ANT-470).
                    state
                        .last_pushed_token_tables
                        .write()
                        .await
                        .insert(
                            session_id.to_string(),
                            PushedTokenTable {
                                generation,
                                table: table.clone(),
                            },
                        );
                    return true;
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Slot / MK assignment handlers
// ---------------------------------------------------------------------------

/// Assign a gamepad slot to a user (controller or viewer). Mirrors
/// `handle_assign_slot`.
async fn handle_assign_slot(
    state: &AppState,
    session_id: &str,
    viewer_token: Option<&str>,
    slot: Option<i64>,
) {
    // Read, mutate, and commit in one sessions write-lock critical section:
    // a concurrent assignment that lands while the token-table push below is
    // in flight can no longer be clobbered by a stale whole-object write-back
    // (ANT-887, audit finding 16). The push, save, and broadcasts run after
    // the sessions lock is dropped.
    //
    // The per-session push lock is acquired BEFORE the sessions write lock
    // and held until the POST completes (ANT-927): commits are ordered by
    // the sessions lock and pushes by this lock, so the container records
    // this session's tables in commit order and a stale push can no longer
    // land after a newer one (lock order push -> sessions everywhere, so no
    // deadlock).
    let push_lock = session_push_lock(state, session_id);
    let _push = push_lock.lock().await;
    let (session_data, notifications) = {
        let mut sessions = state.sessions.write().await;
        let Some(mut session_data) = sessions.get(session_id).cloned() else {
            return;
        };
        let controller_token = session_data
            .get("controller_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let viewer_token = viewer_token.unwrap_or("");
        // The state frame carries the 8-char identity key, but mixed-version
        // clients may still submit the full token: normalize to the stored
        // full token before any compare or store below (ANT-882). An unknown
        // key resolves to itself, preserving the existing no-match behavior.
        let resolved_viewer_token = resolve_room_token(&session_data, viewer_token)
            .unwrap_or_else(|| viewer_token.to_string());
        let viewer_token = resolved_viewer_token.as_str();

        let mut target_username = "Unknown".to_string();
        let mut old_slot_for_target: Option<i64> = None;

        let mut notifications: Vec<String> = Vec::new();

            if tokens_equal(viewer_token, &controller_token) {
                target_username = "Controller".to_string();
                old_slot_for_target = session_data
                    .get("controller_slot")
                    .and_then(|v| v.as_i64());
            } else if let Some(viewers) =
                session_data.get_mut("viewers").and_then(|v| v.as_array_mut())
            {
                if let Some(v) = viewers.iter_mut().find(|v| {
                    v.get("token")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| tokens_equal(t, viewer_token))
                }) {
                    target_username = v
                        .get("username")
                        .and_then(|u| u.as_str())
                        .unwrap_or("Unnamed")
                        .to_string();
                    old_slot_for_target = v.get("slot").and_then(|s| s.as_i64());
                }
            }

            // Clear the slot from any previous owner.
            if let Some(slot) = slot {
                let mut previous_owner_cleared = false;
                if session_data.get("controller_slot").and_then(|v| v.as_i64()) == Some(slot)
                    && !tokens_equal(&controller_token, viewer_token)
                {
                    session_data["controller_slot"] = Value::Null;
                    notifications.push(format!(
                        "Controller was unassigned from Gamepad {slot}."
                    ));
                    previous_owner_cleared = true;
                }
                if !previous_owner_cleared {
                    if let Some(viewers) =
                        session_data.get_mut("viewers").and_then(|v| v.as_array_mut())
                    {
                        if let Some(v) = viewers.iter_mut().find(|v| {
                            v.get("slot").and_then(|s| s.as_i64()) == Some(slot)
                                && v.get("token")
                                    .and_then(|t| t.as_str())
                                    .is_none_or(|t| !tokens_equal(t, viewer_token))
                        }) {
                            v["slot"] = Value::Null;
                            notifications.push(format!(
                                "{} was unassigned from Gamepad {slot}.",
                                v.get("username").and_then(|u| u.as_str()).unwrap_or("Unnamed")
                            ));
                        }
                    }
                }
            }

            // Set the slot on the target.
            if tokens_equal(viewer_token, &controller_token) {
                session_data["controller_slot"] = slot.map(Value::from).unwrap_or(Value::Null);
            } else if let Some(viewers) =
                session_data.get_mut("viewers").and_then(|v| v.as_array_mut())
            {
                if let Some(v) = viewers.iter_mut().find(|v| {
                    v.get("token")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| tokens_equal(t, viewer_token))
                }) {
                    v["slot"] = slot.map(Value::from).unwrap_or(Value::Null);
                }
            }

            if let Some(slot) = slot {
                if old_slot_for_target != Some(slot) {
                    notifications.push(format!(
                        "Gamepad {slot} was assigned to {target_username}."
                    ));
                }
            } else if let Some(old) = old_slot_for_target {
                notifications.push(format!(
                    "{target_username} was unassigned from Gamepad {old}."
                ));
            }

            sessions.insert(session_id.to_string(), session_data.clone());
            (session_data, notifications)
        };
    // The write-back landed under the write lock above; the network push
    // runs after the sessions lock is dropped and no longer spans read and
    // write. The push lock stays held until the POST completes, so a
    // later-committed mutation's push cannot overtake this one (ANT-927).
    push_token_table(state, session_id, &session_data).await;
    drop(_push);
    // WS/visit flow: in-memory state is authoritative and the next session
    // event retries the full replace (audit finding 1).
    if let Err(e) = vreckan_core::state::save_sessions(state).await {
        error!("[{session_id}] Failed to persist session after gamepad change: {e}");
    }

    for msg in notifications {
        let now_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)) as i64;
        broadcast_to_room(
            state,
            session_id,
            &serde_json::json!({ "type": "gamepad_change", "message": msg, "timestamp": now_ms }),
        )
        .await;
    }
    broadcast_state(state, session_id).await;
}

/// Assign mouse/keyboard control to a user (or revert to controller). Mirrors
/// `handle_assign_mk`.
async fn handle_assign_mk(state: &AppState, session_id: &str, target_token: Option<&str>) {
    // Same read-mutate-commit critical section as `handle_assign_slot`
    // (ANT-887, audit finding 16): the mk transfer commits under the
    // sessions write lock before the token-table push, so a concurrent
    // slot/viewer mutation cannot be clobbered by a stale write-back.
    //
    // Same per-session push-lock discipline as `handle_assign_slot`
    // (ANT-927): acquired before the sessions write lock, held through the
    // POST, so this push cannot be overtaken by a later-committed one.
    let push_lock = session_push_lock(state, session_id);
    let _push = push_lock.lock().await;
    let (session_data, username) = {
        let mut sessions = state.sessions.write().await;
        let Some(mut session_data) = sessions.get(session_id).cloned() else {
            return;
        };
        let controller_token = session_data
            .get("controller_token")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mut target_token = target_token.map(|s| s.to_string());
        // The state frame carries the 8-char identity key, but mixed-version
        // clients may still submit the full token: normalize to the stored
        // full token before the controller/mk-owner/viewer compares below
        // (ANT-882). An unknown key resolves to itself, preserving the
        // existing no-match behavior.
        if let Some(t) = &target_token {
            if let Some(resolved) = resolve_room_token(&session_data, t) {
                target_token = Some(resolved);
            }
        }
        if target_token
            .as_deref()
            .is_some_and(|t| tokens_equal(t, controller_token))
        {
            target_token = None;
        }
        let current_owner = session_data.get("mk_owner_token").and_then(|v| v.as_str());
        if opt_tokens_equal(current_owner, target_token.as_deref()) {
            return;
        }
        session_data["mk_owner_token"] = target_token.clone().map(Value::from).unwrap_or(Value::Null);

        let mut username = "Controller".to_string();
        if let Some(tt) = &target_token {
            if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
                if let Some(v) = viewers.iter().find(|v| {
                    v.get("token")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| tokens_equal(t, tt))
                }) {
                    username = v
                        .get("username")
                        .and_then(|u| u.as_str())
                        .unwrap_or("User")
                        .to_string();
                }
            }
        }
        sessions.insert(session_id.to_string(), session_data.clone());
        (session_data, username)
    };
    // The write-back landed under the write lock above; the network push
    // runs after the sessions lock is dropped and no longer spans read and
    // write. The push lock stays held until the POST completes, so a
    // later-committed mutation's push cannot overtake this one (ANT-927).
    push_token_table(state, session_id, &session_data).await;
    drop(_push);
    // WS/visit flow: in-memory state is authoritative and the next session
    // event retries the full replace (audit finding 1).
    if let Err(e) = vreckan_core::state::save_sessions(state).await {
        error!("[{session_id}] Failed to persist session after collab mutation: {e}");
    }
    let now_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)) as i64;
    broadcast_to_room(
        state,
        session_id,
        &serde_json::json!({
            "type": "mk_change",
            "message": format!("Mouse & Keyboard control assigned to {username}."),
            "timestamp": now_ms,
        }),
    )
    .await;
    broadcast_state(state, session_id).await;
}

/// Broadcast session termination and clean up the room. Mirrors
/// `notify_session_ended`.
pub async fn notify_session_ended(state: &AppState, session_id: &str) {
    if state.room_connections.read().await.contains_key(session_id) {
        info!("[{session_id}] Broadcasting session termination to room.");
        broadcast_to_room(state, session_id, &serde_json::json!({ "type": "session_ended" })).await;
        state.room_connections.write().await.remove(session_id);
    }
}

// ---------------------------------------------------------------------------
// Room WebSocket (WS /ws/room/{session_id}?token=...)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WsQuery {
    #[serde(default)]
    token: Option<String>,
}

/// The room WebSocket bridge. Mirrors `room_websocket`.
async fn room_websocket(
    State(state): State<AppState>,
    PathParam(session_id): PathParam<String>,
    Query(q): Query<WsQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let token = q.token.clone().unwrap_or_default();
    let session_data = state.sessions.read().await.get(&session_id).cloned();
    let Some(session_data) = session_data else {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    };
    if !session_data.get("is_collaboration").and_then(|v| v.as_bool()).unwrap_or(false) {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    }
    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_controller = tokens_equal(&token, controller_token);
    let viewer_data_ref: Option<Value> = session_data
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter().find(|v| {
                v.get("token")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| tokens_equal(t, &token))
            })
        })
        .cloned();
    let is_viewer = viewer_data_ref.is_some();
    if !is_controller && !is_viewer {
        return upgrade.on_upgrade(|mut ws| async move {
            let _ = ws.close().await;
        });
    }

    let username = if is_controller {
        "Controller".to_string()
    } else {
        viewer_data_ref
            .as_ref()
            .and_then(|v| v.get("username").and_then(|u| u.as_str()))
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("User-{}", &token[..token.len().min(6)]))
    };
    let public_id = {
        let mut buf = [0u8; 4];
        rand::rng().fill_bytes(&mut buf);
        hex::encode(buf)
    };

    // Create the mpsc channel for pushing broadcasts to this client.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(64);
    let conn_info = ConnectionInfo {
        username: username.clone(),
        token: token.clone(),
        public_id: public_id.clone(),
        has_joined: false,
        sender: tx.clone(),
    };

    // Register the connection (closing any previous one with the same token).
    let previous: Option<ConnectionInfo> = {
        let mut rooms = state.room_connections.write().await;
        let room = rooms.entry(session_id.clone()).or_default();
        if is_controller {
            let prev = room.controller.take();
            room.controller = Some(conn_info.clone());
            prev
        } else {
            let prev = room.viewers.remove(&token);
            room.viewers.insert(token.clone(), conn_info.clone());
            prev
        }
    };
    if let Some(prev) = previous {
        let _ = prev.sender.send(Message::Close(None)).await;
    }

    // Broadcast the join, mark joined, and broadcast state.
    let now_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)) as i64;
    broadcast_to_room(
        &state,
        &session_id,
        &serde_json::json!({ "type": "user_joined", "username": username, "timestamp": now_ms }),
    )
    .await;
    {
        let mut rooms = state.room_connections.write().await;
        if let Some(room) = rooms.get_mut(&session_id) {
            if is_controller {
                if let Some(c) = room.controller.as_mut() {
                    c.has_joined = true;
                }
            } else if let Some(v) = room.viewers.get_mut(&token) {
                v.has_joined = true;
            }
        }
    }
    broadcast_state(&state, &session_id).await;

    // Send the app list to the controller on connect.
    if is_controller {
        send_app_list(&state, &session_id, &session_data, &tx).await;
    }

    upgrade.on_upgrade(move |client_ws| async move {
        // split() returns (write half, read half).
        let (mut ws_tx, mut ws_rx) = client_ws.split();

        // Task 1: forward broadcast-channel messages -> client WebSocket.
        let to_client = tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if ws_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Task 2: read client messages and route them.
        let state_clone = state.clone();
        let session_id_clone = session_id.clone();
        let token_clone = token.clone();
        let username_clone = username.clone();
        let is_controller_clone = is_controller;
        let viewer_perm = viewer_data_ref
            .as_ref()
            .and_then(|v| v.get("permission").and_then(|p| p.as_str()))
            .unwrap_or("participant")
            .to_string();
        let from_client = tokio::spawn(async move {
            while let Some(Ok(msg)) = ws_rx.next().await {
                match msg {
                    Message::Text(text) => {
                        let Ok(data) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        let mut data = data;
                        if let Some(obj) = data.as_object_mut() {
                            // Non-secret identity key, not the replayable full
                            // token: the `control` re-broadcast below embeds
                            // this field in every frame (finding 11, ANT-882).
                            // `route_message` identifies the sender through
                            // its own parameters, never this field.
                            obj.insert(
                                "sender_token".into(),
                                serde_json::json!(short_token(&token_clone)),
                            );
                        }
                        route_message(
                            &state_clone,
                            &session_id_clone,
                            &token_clone,
                            &username_clone,
                            is_controller_clone,
                            &viewer_perm,
                            &data,
                        )
                        .await;
                    }
                    Message::Binary(bytes) => {
                        // Read-only viewers don't send input.
                        if !is_controller_clone && viewer_perm == "readonly" {
                            continue;
                        }
                        if bytes.len() > 1024 * 1024 {
                            continue;
                        }
                        let session_data = state_clone
                            .sessions
                            .read()
                            .await
                            .get(&session_id_clone)
                            .cloned();
                        let designated_speaker = session_data
                            .as_ref()
                            .and_then(|s| s.get("designated_speaker").and_then(|v| v.as_str()));
                        let is_audio_packet = bytes.len() > 8 && bytes[8] == 0x02;
                        if let Some(ds) = designated_speaker {
                            if is_audio_packet && !tokens_equal(ds, &token_clone) {
                                continue;
                            }
                        }
                        broadcast_binary_to_room(
                            &state_clone,
                            &session_id_clone,
                            &bytes,
                            &token_clone,
                            is_controller_clone,
                        )
                        .await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        // The client disconnecting (the read half ending) is the real end
        // signal. Await it, then abort the broadcast->client task: it blocks on
        // `rx.recv()`, which only closes when the `tx` stored in
        // `room_connections` is dropped — and that happens in
        // `cleanup_disconnect` below. A plain `join!` would hang waiting for
        // `to_client` to finish on its own. (Same fix as the session proxy.)
        // The read task's own errors (a broken read half) already end the loop,
        // so an Err here only means the task panicked or was cancelled —
        // nothing more to do than drop it.
        let _ = from_client.await;
        to_client.abort();

        // Cleanup on disconnect (use the closure-owned originals; the `_clone`
        // values were moved into the `from_client` task above).
        cleanup_disconnect(
            &state,
            &session_id,
            &token,
            &username,
            is_controller,
            &public_id,
        )
        .await;
    })
}

/// Route an incoming text message from a room client. Mirrors the message
/// loop in `room_websocket`.
async fn route_message(
    state: &AppState,
    session_id: &str,
    token: &str,
    username: &str,
    is_controller: bool,
    _viewer_perm: &str,
    data: &Value,
) {
    let action = data.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let now_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)) as i64;

    match action {
        "assign_slot" if is_controller => {
            let viewer_token = data.get("viewer_token").and_then(|v| v.as_str());
            let slot = data.get("slot").and_then(|s| s.as_i64());
            handle_assign_slot(state, session_id, viewer_token, slot).await;
        }
        "assign_mk" if is_controller => {
            let target = data.get("token").and_then(|t| t.as_str());
            handle_assign_mk(state, session_id, target).await;
        }
        "set_designated_speaker" if is_controller => {
            let speaker_key = data.get("token").and_then(|t| t.as_str());
            let mut sessions = state.sessions.write().await;
            if let Some(s) = sessions.get_mut(session_id) {
                // Resolve the identity key (full or 8-char form, ANT-882) to
                // the stored full token before storing: the audio gate and
                // the disconnect cleanup compare `designated_speaker` with
                // full connection tokens, so the stored shape stays a full
                // token. A key that addresses nobody clears the designation
                // instead of storing an unmatchable value.
                let speaker = speaker_key.and_then(|k| resolve_room_token(s, k));
                s["designated_speaker"] = speaker.map(Value::from).unwrap_or(Value::Null);
            }
            drop(sessions);
            // WS flow: in-memory state is authoritative and the next session
            // event retries the full replace (audit finding 1).
            if let Err(e) = vreckan_core::state::save_sessions(state).await {
                error!("[{session_id}] Failed to persist session after speaker designation: {e}");
            }
            broadcast_state(state, session_id).await;
        }
        "set_username" if !is_controller => {
            let new_username = data.get("username").and_then(|u| u.as_str()).unwrap_or("").trim();
            if new_username.is_empty() || new_username.len() > 25 {
                return;
            }
            let mut sessions = state.sessions.write().await;
            if let Some(s) = sessions.get_mut(session_id) {
                if let Some(viewers) = s.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                    if let Some(v) = viewers.iter_mut().find(|v| {
                        v.get("token")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| tokens_equal(t, token))
                    }) {
                        let old = v.get("username").and_then(|u| u.as_str()).unwrap_or("").to_string();
                        if old == new_username {
                            return;
                        }
                        v["username"] = Value::String(new_username.to_string());
                    }
                }
            }
            drop(sessions);
            // WS flow: in-memory state is authoritative and the next session
            // event retries the full replace (audit finding 1).
            if let Err(e) = vreckan_core::state::save_sessions(state).await {
                error!("[{session_id}] Failed to persist session after username change: {e}");
            }
            broadcast_to_room(
                state,
                session_id,
                &serde_json::json!({
                    "type": "username_changed",
                    "old_username": username,
                    "new_username": new_username,
                    "timestamp": now_ms,
                }),
            )
            .await;
            broadcast_state(state, session_id).await;
        }
        "send_chat_message" => {
            let message_text = data.get("message").and_then(|m| m.as_str()).unwrap_or("").trim();
            if message_text.is_empty() || message_text.len() > 500 {
                return;
            }
            let message_id = format!("{now_ms}-{}", {
                let mut buf = [0u8; 4];
                rand::rng().fill_bytes(&mut buf);
                hex::encode(buf)
            });
            broadcast_to_room(
                state,
                session_id,
                &serde_json::json!({
                    "type": "chat_message",
                    "sender": username,
                    "message": message_text,
                    "timestamp": now_ms,
                    "messageId": message_id,
                    "replyTo": data.get("replyTo").cloned().unwrap_or(Value::Null),
                }),
            )
            .await;
        }
        "video_state" | "audio_state" | "force_cursor_render" => {
            broadcast_to_room(
                state,
                session_id,
                &serde_json::json!({ "type": "control", "payload": data }),
            )
            .await;
        }
        "get_apps" if is_controller => {
            // Re-send the app list to the controller.
            let session_data = state.sessions.read().await.get(session_id).cloned();
            if let Some(sd) = session_data {
                let conns = state.room_connections.read().await.get(session_id).cloned();
                if let Some(conns) = conns {
                    if let Some(c) = conns.controller {
                        send_app_list(state, session_id, &sd, &c.sender).await;
                    }
                }
            }
        }
        "request_resolutions" if is_controller => {
            broadcast_to_room(
                state,
                session_id,
                &serde_json::json!({ "type": "request_resolutions" }),
            )
            .await;
        }
        "client_resolution" => {
            let width = data.get("width").and_then(|w| w.as_u64());
            let height = data.get("height").and_then(|h| h.as_u64());
            if let (Some(w), Some(h)) = (width, height) {
                broadcast_to_room(
                    state,
                    session_id,
                    &serde_json::json!({
                        "type": "resolution_update",
                        // Identity key, not the replayable full token
                        // (finding 11, ANT-882).
                        "token": short_token(token),
                        "width": w,
                        "height": h,
                    }),
                )
                .await;
            }
        }
        // stop_app / restart_app / swap_app are controller actions that manage
        // containers; for now broadcast a control ack (full container mgmt is
        // handled by the session proxy + catalog).
        "stop_app" | "restart_app" | "swap_app" if is_controller => {
            let app_id = data.get("app_id").and_then(|a| a.as_str());
            info!(
                "[{session_id}] Controller {action} for app {app_id:?} (container mgmt)"
            );
            broadcast_to_room(
                state,
                session_id,
                &serde_json::json!({ "type": "control", "payload": data }),
            )
            .await;
        }
        _ => {}
    }
}

/// Send the list of launchable apps to a specific connection. Mirrors
/// `send_app_list`.
async fn send_app_list(
    state: &AppState,
    _session_id: &str,
    session_data: &Value,
    sender: &tokio::sync::mpsc::Sender<Message>,
) {
    let apps = state.installed_apps.read().await.clone();
    let session_username = session_data
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let registry = session_data
        .get("container_registry")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let provider_app_id = session_data
        .get("provider_app_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut user_apps: Vec<Value> = Vec::new();
    for app in apps.values() {
        let can_access = app.users.iter().any(|u| u == "all" || u == session_username);
        if !can_access {
            continue;
        }
        let mut logo_src = app.logo.clone();
        if logo_src.starts_with("/api/app_icon/") {
            let icon_path = std::path::Path::new(&state.settings.app_icons_path)
                .join(format!("{}.png", app.id));
            if let Ok(bytes) = std::fs::read(&icon_path) {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                logo_src = format!("data:image/png;base64,{b64}");
            }
        }
        user_apps.push(serde_json::json!({
            "id": app.id,
            "name": app.name,
            "running": registry.contains_key(&app.id),
            "active": app.id == provider_app_id,
            "logo": logo_src,
        }));
    }
    user_apps.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|n| n.as_str()).unwrap_or(""))
    });
    let _ = sender
        .send(Message::Text(
            serde_json::json!({ "type": "app_list", "apps": user_apps }).to_string().into(),
        ))
        .await;
}

/// Clean up a room connection on disconnect. Mirrors the `finally` block in
/// `room_websocket`.
async fn cleanup_disconnect(
    state: &AppState,
    session_id: &str,
    token: &str,
    username: &str,
    is_controller: bool,
    public_id: &str,
) {
    let now_ms = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)) as i64;

    let (was_live, has_joined) = {
        let mut rooms = state.room_connections.write().await;
        let room = rooms.get_mut(session_id);
        let (was_live, has_joined) = if let Some(room) = room {
            if is_controller {
                let live = room.controller.as_ref().map(|c| c.public_id == public_id).unwrap_or(false);
                if live {
                    room.controller = None;
                }
                (live, room.controller.is_some())
            } else {
                let live = room
                    .viewers
                    .get(token)
                    .map(|c| c.public_id == public_id)
                    .unwrap_or(false);
                let hj = room.viewers.get(token).map(|c| c.has_joined).unwrap_or(false);
                if live {
                    room.viewers.remove(token);
                }
                (live, hj)
            }
        } else {
            (false, false)
        };
        // Remove the room if now empty.
        if let Some(room) = rooms.get(session_id) {
            if room.controller.is_none() && room.viewers.is_empty() {
                rooms.remove(session_id);
                info!("[{session_id}] Collab room is now empty and has been cleaned up.");
            }
        }
        (was_live, has_joined)
    };

    if is_controller {
        info!("[{session_id}] Controller disconnected from collab room.");
    } else {
        // A viewer token is valid room access until revoked, so log a short
        // prefix instead of the raw token: on hosts where logs are shared or
        // world-readable the full value would be a live credential (ANT-344,
        // item 1).
        info!(
            "[{session_id}] Viewer {} disconnected from collab room.",
            short_token(token)
        );
        // Release slots / MK / designated speaker for the disconnected viewer.
        // The write guard is dropped before the broadcasts, save_sessions,
        // the re-read, and the token push: tokio locks are not reentrant, so
        // holding it across those awaits would deadlock the session map
        // (ANT-887, audit finding 16 family).
        //
        // Per-session push lock acquired before the write lock and held
        // through the push below (ANT-927, same discipline as the slot/MK
        // handlers): this session's pushes keep commit order.
        let push_lock = session_push_lock(state, session_id);
        let _push = push_lock.lock().await;
        let (state_changed, input_released, broadcasts) = {
            let mut sessions = state.sessions.write().await;
            let mut state_changed = false;
            let mut input_released = false;
            let mut broadcasts: Vec<Value> = Vec::new();
            if let Some(s) = sessions.get_mut(session_id) {
                if s.get("designated_speaker")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| tokens_equal(t, token))
                {
                    s["designated_speaker"] = Value::Null;
                    state_changed = true;
                }
                // Read the MK ownership before mutably borrowing the viewers list.
                let mk_is_owner = s
                    .get("mk_owner_token")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| tokens_equal(t, token));
                let mut slot_to_release: Option<i64> = None;
                let mut viewer_uname = "A user".to_string();
                if let Some(viewers) = s.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                    if let Some(v) = viewers.iter_mut().find(|v| {
                        v.get("token")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| tokens_equal(t, token))
                    }) {
                        viewer_uname = v
                            .get("username")
                            .and_then(|u| u.as_str())
                            .unwrap_or("A user")
                            .to_string();
                        if let Some(slot) = v.get("slot").and_then(|sl| sl.as_i64()) {
                            v["slot"] = Value::Null;
                            state_changed = true;
                            input_released = true;
                            slot_to_release = Some(slot);
                        }
                    }
                }
                if mk_is_owner {
                    s["mk_owner_token"] = Value::Null;
                    state_changed = true;
                    input_released = true;
                }
                // Defer the broadcasts until the write guard is dropped.
                if let Some(slot) = slot_to_release {
                    broadcasts.push(serde_json::json!({
                        "type": "gamepad_change",
                        "message": format!("{viewer_uname} disconnected and was unassigned from Gamepad {slot}."),
                        "timestamp": now_ms,
                    }));
                }
                if mk_is_owner {
                    broadcasts.push(serde_json::json!({
                        "type": "mk_change",
                        "message": format!("{viewer_uname} disconnected. MK control reverted to Controller."),
                        "timestamp": now_ms,
                    }));
                }
            }
            (state_changed, input_released, broadcasts)
        };
        for msg in broadcasts {
            broadcast_to_room(state, session_id, &msg).await;
        }
        if state_changed {
            // WS flow: in-memory state is authoritative and the next session
            // event retries the full replace (audit finding 1).
            if let Err(e) = vreckan_core::state::save_sessions(state).await {
                error!("[{session_id}] Failed to persist session after viewer disconnect: {e}");
            }
        }
        if input_released {
            let sd = state.sessions.read().await.get(session_id).cloned();
            if let Some(sd) = sd {
                push_token_table(state, session_id, &sd).await;
            }
        }
        drop(_push);
    }

    if was_live && has_joined {
        broadcast_to_room(
            state,
            session_id,
            &serde_json::json!({ "type": "user_left", "username": username, "timestamp": now_ms }),
        )
        .await;
    }
    broadcast_state(state, session_id).await;
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// The collaboration-room router: the room page, its static assets, and the
/// room WebSocket.
pub fn room_router() -> Router<AppState> {
    Router::new()
        .route("/room/room.css", get(get_room_css))
        .route("/room/room.js", get(get_room_js))
        .route("/room/translation.js", get(get_translation_js))
        .route("/room/{session_id}", get(collaborative_room))
        .route("/ws/room/{session_id}", get(room_websocket))
}

// ---------------------------------------------------------------------------
// Tests (ANT-887, audit finding 16): the slot/MK/viewer mutations must
// commit under the sessions write lock so concurrent control actions cannot
// clobber each other across the token-table push await.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use vreckan_core::settings::Settings;

    const SESSION_ID: &str = "ant887-room";
    /// The fake container's token endpoint holds each request this long
    /// before answering — a compressed version of the production ~1–2 s
    /// push, long enough that concurrently running handlers genuinely
    /// interleave across the await.
    const PUSH_DELAY_MS: u64 = 300;

    /// A loopback stand-in for the app container's selkies token endpoint:
    /// it records every accepted table and delays its 200 by
    /// `PUSH_DELAY_MS`. Connections are handled one at a time, so the
    /// recorded order is the order the pushes went out.
    async fn spawn_delayed_token_endpoint() -> (String, u16, Arc<Mutex<Vec<Value>>>) {
        let tables: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = tables.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (socket, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => break,
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut stream = tokio::net::TcpStream::from_std(socket.into_std().unwrap()).unwrap();
                    let mut buf = [0u8; 65536];
                    let mut n = 0usize;
                    let mut want = 0usize;
                    loop {
                        if n >= want && want > 0 {
                            break;
                        }
                        match stream.read(&mut buf[n..]).await {
                            Ok(0) => break,
                            Ok(k) => {
                                n += k;
                                if want == 0 {
                                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                                    if let Some(head_end) = raw.find("\r\n\r\n") {
                                        let head = &raw[..head_end];
                                        let len = head
                                            .lines()
                                            .find_map(|l| {
                                                l.to_ascii_lowercase()
                                                    .strip_prefix("content-length:")
                                                    .and_then(|v| v.trim().parse::<usize>().ok())
                                            })
                                            .unwrap_or(0);
                                        want = head_end + 4 + len;
                                        if want <= n {
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                    let head_end = raw.find("\r\n\r\n").unwrap_or(0);
                    let body = &raw[head_end + 4..];
                    tokio::time::sleep(std::time::Duration::from_millis(PUSH_DELAY_MS)).await;
                    if let Ok(table) = serde_json::from_slice::<Value>(body.as_bytes()) {
                        recorded.lock().unwrap().push(table);
                    }
                    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    let _ = stream.write_all(resp).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (addr.ip().to_string(), addr.port(), tables)
    }

    /// A throwaway SQLite-backed `AppState` (removed on drop).
    struct TestState {
        state: AppState,
        dir: std::path::PathBuf,
    }

    impl TestState {
        async fn new() -> Self {
            // Unit tests run with the CWD at the crate directory, so point
            // the static dir at the checkout root's web client (the same
            // override the per-crate integration harnesses use, ANT-810);
            // the room page renders `collaboration/room.html` from there.
            static STATIC_DIR_SET: std::sync::OnceLock<()> = std::sync::OnceLock::new();
            STATIC_DIR_SET.get_or_init(|| {
                if std::env::var_os("VRECKAN_STATIC_DIR").is_none() {
                    std::env::set_var(
                        "VRECKAN_STATIC_DIR",
                        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                            .join("..")
                            .join("..")
                            .join("server/static"),
                    );
                }
            });
            let dir = std::env::temp_dir().join(format!(
                "vreckan-collab-test-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = vreckan_core::db::connect(&format!(
                "sqlite://{}?mode=rwc",
                dir.join("test.db").display()
            ))
            .await
            .unwrap();
            db.init().await.unwrap();
            Self {
                state: AppState::new(Settings::from_env(), db),
                dir,
            }
        }
    }

    impl Drop for TestState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A collab room session pointed at the fake container endpoint, with
    /// two viewers and no slots/MK assigned yet.
    fn collab_session(ip: &str, port: u16) -> Value {
        serde_json::json!({
            "instance_id": "ctr-ant887",
            "ip": ip,
            "port": port,
            "created_at": 1.0,
            "access_token": "access-ant887",
            "is_collaboration": true,
            "username": "admin",
            "master_token": "master-ant887",
            "controller_token": "ctrl-ant887",
            "participant_invite_token": "invite-ant887",
            "readonly_invite_token": "readonly-ant887",
            "controller_slot": Value::Null,
            "mk_owner_token": Value::Null,
            "viewers": [
                { "token": "alice", "slot": Value::Null, "mk_control": false,
                  "username": "alice", "permission": "participant" },
                { "token": "bob", "slot": Value::Null, "mk_control": false,
                  "username": "bob", "permission": "participant" },
            ],
            "container_registry": {},
        })
    }

    fn viewer_slot(session: &Value, token: &str) -> Value {
        session
            .get("viewers")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token))
            })
            .and_then(|v| v.get("slot"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// Finding 16's core scenario: the controller assigns gamepad 2 to alice
    /// and gamepad 3 to bob back-to-back. Under the old read-clone / push /
    /// write-back shape the second write clobbers the first; with the
    /// commit under the sessions write lock both land.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_slot_assignments_do_not_clobber_each_other() {
        let (ip, port, tables) = spawn_delayed_token_endpoint().await;
        let ts = TestState::new().await;
        let state = ts.state.clone();
        state
            .sessions
            .write()
            .await
            .insert(SESSION_ID.to_string(), collab_session(&ip, port));

        let s1 = state.clone();
        let a = tokio::spawn(async move {
            handle_assign_slot(&s1, SESSION_ID, Some("alice"), Some(2)).await;
        });
        let s2 = state.clone();
        let b = tokio::spawn(async move {
            handle_assign_slot(&s2, SESSION_ID, Some("bob"), Some(3)).await;
        });
        // A timeout here means the handler deadlocked on the sessions lock.
        let (ra, rb) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(10), a),
            tokio::time::timeout(std::time::Duration::from_secs(10), b)
        );
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();

        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            viewer_slot(&session, "alice"),
            serde_json::json!(2),
            "alice's assignment must not be clobbered by the concurrent bob assignment: {session:?}"
        );
        assert_eq!(viewer_slot(&session, "bob"), serde_json::json!(3), "bob's assignment: {session:?}");

        // The container keeps the last table it was pushed; the lock
        // serializes the commits, so the later push (bob's) was built from
        // a read that already includes alice's assignment.
        let captured = tables.lock().unwrap();
        assert_eq!(captured.len(), 2, "each assignment pushed a table");
        let last = captured.last().unwrap();
        assert_eq!(
            last.get("alice").and_then(|v| v.get("slot")),
            Some(&serde_json::json!(2)),
            "last pushed table keeps alice's slot: {last:?}"
        );
        assert_eq!(
            last.get("bob").and_then(|v| v.get("slot")),
            Some(&serde_json::json!(3)),
            "last pushed table keeps bob's slot: {last:?}"
        );
    }

    /// A slot assignment and an MK transfer racing each other: under the old
    /// shape whichever write-back landed last rewrote the whole session and
    /// dropped the other's change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_slot_and_mk_assignments_do_not_clobber_each_other() {
        let (ip, port, _tables) = spawn_delayed_token_endpoint().await;
        let ts = TestState::new().await;
        let state = ts.state.clone();
        state
            .sessions
            .write()
            .await
            .insert(SESSION_ID.to_string(), collab_session(&ip, port));

        let s1 = state.clone();
        let a = tokio::spawn(async move {
            handle_assign_slot(&s1, SESSION_ID, Some("alice"), Some(2)).await;
        });
        let s2 = state.clone();
        let b = tokio::spawn(async move {
            handle_assign_mk(&s2, SESSION_ID, Some("bob")).await;
        });
        // A timeout here means the handler deadlocked on the sessions lock.
        let (ra, rb) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(10), a),
            tokio::time::timeout(std::time::Duration::from_secs(10), b)
        );
        ra.unwrap().unwrap();
        rb.unwrap().unwrap();

        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            viewer_slot(&session, "alice"),
            serde_json::json!(2),
            "the slot assignment must survive the concurrent MK transfer: {session:?}"
        );
        assert_eq!(
            session.get("mk_owner_token").and_then(|v| v.as_str()),
            Some("bob"),
            "the MK transfer must survive the concurrent slot assignment: {session:?}"
        );
    }

    /// Finding 16's second case: two visitors redeeming the same participant
    /// link at once must both get a viewer entry (the old write-back dropped
    /// one, 401-ing that visitor's token on reload).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_new_viewer_redemptions_both_land() {
        let (ip, port, _tables) = spawn_delayed_token_endpoint().await;
        let ts = TestState::new().await;
        let state = ts.state.clone();
        state
            .sessions
            .write()
            .await
            .insert(SESSION_ID.to_string(), collab_session(&ip, port));

        let router = room_router().with_state(state.clone());
        let make_req = |router: Router| {
            let req = Request::builder()
                .uri(format!("/room/{SESSION_ID}?token=invite-ant887"))
                .body(Body::empty())
                .unwrap();
            router.clone().oneshot(req)
        };
        let (r1, r2) = tokio::join!(
            tokio::time::timeout(std::time::Duration::from_secs(10), make_req(router.clone())),
            tokio::time::timeout(std::time::Duration::from_secs(10), make_req(router))
        );
        let resp1 = r1.unwrap().unwrap();
        let resp2 = r2.unwrap().unwrap();
        assert_eq!(resp1.status(), StatusCode::SEE_OTHER, "visitor 1: {:?}", resp1);
        assert_eq!(resp2.status(), StatusCode::SEE_OTHER, "visitor 2: {:?}", resp2);

        let token_from = |resp: &axum::response::Response| {
            let loc = resp
                .headers()
                .get(axum::http::header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap();
            loc.split("token=")
                .nth(1)
                .unwrap()
                .trim()
                .to_string()
        };
        let t1 = token_from(&resp1);
        let t2 = token_from(&resp2);
        assert!(!t1.is_empty() && !t2.is_empty(), "both redirects carry a token");
        assert_ne!(t1, t2, "each redemption mints its own viewer token");

        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        let viewers = session.get("viewers").and_then(|v| v.as_array()).unwrap();
        // The two seeded viewers plus exactly one entry per redemption.
        assert_eq!(viewers.len(), 4, "both viewer entries landed: {session:?}");
        for t in [&t1, &t2] {
            let entry = viewers
                .iter()
                .find(|v| v.get("token").and_then(|x| x.as_str()) == Some(t))
                .unwrap_or_else(|| panic!("viewer entry for {t} missing: {session:?}"));
            assert_eq!(entry["permission"], "participant");
        }
        // Neither redemption clobbered the other's (or the seeded) entries.
        assert!(
            viewers
                .iter()
                .any(|v| v.get("token").and_then(|x| x.as_str()) == Some("alice"))
                && viewers
                    .iter()
                    .any(|v| v.get("token").and_then(|x| x.as_str()) == Some("bob")),
            "seeded viewers survive: {session:?}"
        );
    }

    /// The adjacent same-family defect (ANT-887): `cleanup_disconnect`
    /// released a viewer's slot/MK and then called `save_sessions` and
    /// re-read the session map while still holding the sessions WRITE guard —
    /// tokio locks are not reentrant, so a viewer holding a slot or MK that
    /// disconnects deadlocked the session map. The guard must be dropped
    /// before any await that re-locks it.
    #[tokio::test]
    async fn viewer_disconnect_releasing_slot_does_not_deadlock() {
        let (ip, port, _tables) = spawn_delayed_token_endpoint().await;
        let ts = TestState::new().await;
        let state = ts.state.clone();
        let mut session = collab_session(&ip, port);
        // alice holds gamepad 2 and the MK.
        if let Some(viewers) = session.get_mut("viewers").and_then(|v| v.as_array_mut()) {
            if let Some(v) = viewers.iter_mut().find(|v| v["token"] == "alice") {
                v["slot"] = serde_json::json!(2);
            }
        }
        session["mk_owner_token"] = Value::String("alice".to_string());
        state.sessions.write().await.insert(SESSION_ID.to_string(), session);

        let s1 = state.clone();
        let handle = tokio::spawn(async move {
            cleanup_disconnect(&s1, SESSION_ID, "alice", "alice", false, "pub-ant887").await;
        });
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), handle).await;
        outcome
            .unwrap()
            .unwrap();

        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            viewer_slot(&session, "alice"),
            Value::Null,
            "the disconnected viewer's slot is released: {session:?}"
        );
        assert!(
            session
                .get("mk_owner_token")
                .map(|v| v.is_null())
                .unwrap_or(true),
            "MK reverts to the controller: {session:?}"
        );
    }

    // ------------------------------------------------------------------
    // ANT-882, audit finding 11: broadcast frames carry the non-secret
    // 8-char identity key — never a replayable full token.
    // ------------------------------------------------------------------

    // Tokens long enough that the 8-char broadcast form is a strict prefix.
    const CTRL882: &str = "ctrltok-ant882-000000";
    const ALICE882: &str = "viewer-alice-ant882-000";
    const BOB882: &str = "viewer-bob-ant882-0000";

    fn ant882_session() -> Value {
        serde_json::json!({
            "instance_id": "ctr-ant882",
            "ip": "127.0.0.1",
            "port": 1,
            "created_at": 1.0,
            "access_token": "access-ant882",
            "is_collaboration": true,
            "username": "admin",
            "master_token": "master-ant882",
            "controller_token": CTRL882,
            "participant_invite_token": "invite-ant882",
            "readonly_invite_token": "readonly-ant882",
            "controller_slot": Value::Null,
            "mk_owner_token": Value::Null,
            "viewers": [
                { "token": ALICE882, "slot": Value::Null, "mk_control": false,
                  "username": "alice", "permission": "participant" },
                { "token": BOB882, "slot": Value::Null, "mk_control": false,
                  "username": "bob", "permission": "participant" },
            ],
            "container_registry": {},
        })
    }

    #[test]
    fn resolve_room_token_maps_full_and_short_forms_to_the_stored_token() {
        let s = ant882_session();
        assert_eq!(resolve_room_token(&s, CTRL882), Some(CTRL882.to_string()));
        assert_eq!(
            resolve_room_token(&s, short_token(CTRL882)),
            Some(CTRL882.to_string())
        );
        assert_eq!(resolve_room_token(&s, ALICE882), Some(ALICE882.to_string()));
        assert_eq!(
            resolve_room_token(&s, short_token(ALICE882)),
            Some(ALICE882.to_string())
        );
        assert_eq!(
            resolve_room_token(&s, short_token(BOB882)),
            Some(BOB882.to_string())
        );
        // One user's key must not resolve to another user, and an unknown
        // key (or the empty string) resolves to nothing.
        assert_ne!(
            resolve_room_token(&s, short_token(ALICE882)).as_deref(),
            Some(BOB882)
        );
        assert_eq!(resolve_room_token(&s, "nobody-here"), None);
        assert_eq!(resolve_room_token(&s, ""), None);
    }

    #[test]
    fn short_identity_key_is_not_replayable_for_admission() {
        // Room-page admission and the WS upgrade compare the submitted token
        // against the stored full token with `tokens_equal` — a
        // length-checked full compare — so the 8-char key admits no one.
        for stored in [CTRL882, ALICE882, BOB882] {
            let key = short_token(stored);
            assert!(
                key.len() < stored.len(),
                "{stored} must be longer than its 8-char key"
            );
            assert!(tokens_equal(stored, stored));
            assert!(
                !tokens_equal(key, stored),
                "the 8-char key of {stored} must not be admitted as that token"
            );
        }
    }

    /// A live controller + alice connection so `broadcast_state` and
    /// `route_message` have somewhere to send. `ts` keeps the temp dir alive.
    async fn ant882_state_with_live_room() -> (
        TestState,
        AppState,
        tokio::sync::mpsc::Receiver<Message>,
        tokio::sync::mpsc::Receiver<Message>,
    ) {
        let ts = TestState::new().await;
        let state = ts.state.clone();
        let mut session = ant882_session();
        // The designated speaker is stored as the full token.
        session["designated_speaker"] = Value::String(ALICE882.to_string());
        state
            .sessions
            .write()
            .await
            .insert(SESSION_ID.to_string(), session);
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<Message>(16);
        let (alice_tx, alice_rx) = tokio::sync::mpsc::channel::<Message>(16);
        state.room_connections.write().await.insert(
            SESSION_ID.to_string(),
            RoomConnections {
                controller: Some(ConnectionInfo {
                    username: "Controller".into(),
                    token: CTRL882.to_string(),
                    public_id: "pub-ctrl".into(),
                    has_joined: true,
                    sender: ctrl_tx,
                }),
                viewers: HashMap::from([(
                    ALICE882.to_string(),
                    ConnectionInfo {
                        username: "alice".into(),
                        token: ALICE882.to_string(),
                        public_id: "pub-alice".into(),
                        has_joined: true,
                        sender: alice_tx,
                    },
                )]),
            },
        );
        (ts, state, ctrl_rx, alice_rx)
    }

    async fn next_text(mut rx: tokio::sync::mpsc::Receiver<Message>) -> Value {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("broadcast timed out")
            .expect("the broadcast channel closed");
        match msg {
            Message::Text(t) => serde_json::from_str(&t).expect("frame is JSON"),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    /// The finding-11 invariant: no client ever sees a *foreign* full token
    /// in the state frame. The recipient's own entry comes back in the
    /// full-token form (mixed-version self-match for un-upgraded clients);
    /// every other entry — and the designated speaker — stays in the
    /// non-secret 8-char identity-key form.
    #[tokio::test]
    async fn state_update_frame_carries_only_identity_keys() {
        let (_ts, state, ctrl_rx, alice_rx) = ant882_state_with_live_room().await;
        broadcast_state(&state, SESSION_ID).await;
        let ctrl_frame = next_text(ctrl_rx).await;
        let alice_frame = next_text(alice_rx).await;

        // A viewer's frame must not carry the controller's or another
        // viewer's full token — that is the replay vector finding 11
        // closes.
        for full in [CTRL882, BOB882] {
            assert!(
                !alice_frame.to_string().contains(full),
                "the viewer's state_update must not carry the full token {full}: {alice_frame:?}"
            );
        }
        assert!(
            !ctrl_frame.to_string().contains(BOB882),
            "the controller's state_update must not carry bob's full token: {ctrl_frame:?}"
        );

        // Each recipient gets its own entry back in the full-token form.
        let own_entry = |frame: &Value, name: &str| -> Value {
            frame["viewers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["username"] == name)
                .cloned()
                .unwrap_or(Value::Null)
        };
        assert_eq!(
            own_entry(&ctrl_frame, "Controller")["token"],
            Value::String(CTRL882.to_string())
        );
        assert_eq!(
            own_entry(&alice_frame, "alice")["token"],
            Value::String(ALICE882.to_string())
        );
        // In the controller's frame alice is a foreign user: short form.
        assert_eq!(
            own_entry(&ctrl_frame, "alice")["token"],
            Value::String(short_token(ALICE882).to_string())
        );

        // The stored full speaker token comes out in its 8-char form in
        // every recipient's frame, and the offline viewer is still listed,
        // keyed by his short form.
        for frame in [&ctrl_frame, &alice_frame] {
            assert_eq!(
                frame["designated_speaker"],
                Value::String(short_token(ALICE882).to_string())
            );
            let bob = own_entry(frame, "bob");
            assert_eq!(bob["token"], Value::String(short_token(BOB882).to_string()));
            assert_eq!(bob["online"], false);
        }
    }

    #[tokio::test]
    async fn control_rebroadcast_carries_short_sender_token() {
        let (_ts, state, _ctrl_rx, alice_rx) = ant882_state_with_live_room().await;
        // The WS loop injects the sender's identity key before routing; the
        // `control` re-broadcast embeds that field in every frame.
        let mut data = serde_json::json!({ "action": "video_state", "state": true });
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "sender_token".into(),
                Value::String(short_token(CTRL882).to_string()),
            );
        }
        route_message(
            &state,
            SESSION_ID,
            CTRL882,
            "Controller",
            true,
            "controller",
            &data,
        )
        .await;
        let frame = next_text(alice_rx).await;
        assert_eq!(frame["type"], "control");
        assert_eq!(
            frame["payload"]["sender_token"],
            Value::String(short_token(CTRL882).to_string())
        );
        assert!(
            !frame.to_string().contains(CTRL882),
            "the control re-broadcast must not carry the full sender token: {frame:?}"
        );
    }

    #[tokio::test]
    async fn set_designated_speaker_resolves_the_short_form_to_the_stored_token() {
        let (_ts, state, _ctrl_rx, _alice_rx) = ant882_state_with_live_room().await;
        let data = serde_json::json!({ "action": "set_designated_speaker", "token": short_token(BOB882) });
        route_message(
            &state,
            SESSION_ID,
            CTRL882,
            "Controller",
            true,
            "controller",
            &data,
        )
        .await;
        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            session.get("designated_speaker").and_then(|v| v.as_str()),
            Some(BOB882),
            "the stored speaker must be the full token: {session:?}"
        );
    }

    #[tokio::test]
    async fn assign_slot_accepts_the_short_identity_key() {
        let (_ts, state, _ctrl_rx, _alice_rx) = ant882_state_with_live_room().await;
        let data = serde_json::json!({ "action": "assign_slot", "viewer_token": short_token(ALICE882), "slot": 2 });
        route_message(
            &state,
            SESSION_ID,
            CTRL882,
            "Controller",
            true,
            "controller",
            &data,
        )
        .await;
        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            viewer_slot(&session, ALICE882),
            serde_json::json!(2),
            "the 8-char key must address the viewer entry: {session:?}"
        );
    }

    #[tokio::test]
    async fn assign_mk_accepts_the_short_identity_key() {
        let (_ts, state, _ctrl_rx, _alice_rx) = ant882_state_with_live_room().await;
        let data = serde_json::json!({ "action": "assign_mk", "token": short_token(BOB882) });
        route_message(
            &state,
            SESSION_ID,
            CTRL882,
            "Controller",
            true,
            "controller",
            &data,
        )
        .await;
        let session = state.sessions.read().await.get(SESSION_ID).cloned().unwrap();
        assert_eq!(
            session.get("mk_owner_token").and_then(|v| v.as_str()),
            Some(BOB882),
            "the stored mk owner must be the full token: {session:?}"
        );
    }
}
