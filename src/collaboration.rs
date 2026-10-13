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
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use crate::catalog;
use crate::security;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Room connection state (mirrors `collaboration.ROOM_CONNECTIONS`)
// ---------------------------------------------------------------------------

/// A single live WebSocket connection in a room.
#[derive(Clone)]
pub struct ConnectionInfo {
    pub username: String,
    pub token: String,
    pub public_id: String,
    pub has_joined: bool,
    /// Channel used to push broadcast messages to this client's WebSocket.
    pub sender: tokio::sync::mpsc::Sender<Message>,
}

/// The live connections for one collaboration room.
#[derive(Default, Clone)]
pub struct RoomConnections {
    pub controller: Option<ConnectionInfo>,
    pub viewers: HashMap<String, ConnectionInfo>,
}

// ---------------------------------------------------------------------------
// Static room assets
// ---------------------------------------------------------------------------

fn serve_room_asset(filename: &str, mime: &str) -> Response {
    let path = crate::static_files::static_dir().join("collaboration").join(filename);
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
        session_data["readonly_invite_token"] = Value::String(catalog::random_token(16));
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
    let is_controller_by_session = main_access_token
        .as_deref()
        .map(|t| t == session_data.get("access_token").and_then(|v| v.as_str()).unwrap_or(""))
        .unwrap_or(false);
    let is_controller_by_collab = collab_token.as_deref() == Some(controller_token);
    let is_viewer_by_collab = session_data
        .get("viewers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|v| v.get("token").and_then(|t| t.as_str()) == collab_token.as_deref())
        })
        .unwrap_or(false);
    let is_new_participant_by_collab = collab_token.as_deref()
        == session_data
            .get("participant_invite_token")
            .and_then(|v| v.as_str());
    let is_new_readonly_by_collab = collab_token.as_deref()
        == session_data
            .get("readonly_invite_token")
            .and_then(|v| v.as_str());

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

    let template_path = crate::static_files::static_dir()
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
                v.get("token").and_then(|t| t.as_str()) == collab_token.as_deref()
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
        let new_viewer_token = catalog::random_token(16);
        let new_username = format!("User-{}", rand::rng().random_range(100..999));
        if let Some(viewers) = session_data.get_mut("viewers").and_then(|v| v.as_array_mut()) {
            viewers.push(serde_json::json!({
                "token": new_viewer_token,
                "slot": Value::Null,
                "mk_control": false,
                "username": new_username,
                "permission": permission,
            }));
        }
        broadcast_token_state(&state, &session_id, &session_data).await;
        *state.sessions.write().await.entry(session_id.clone()).or_insert(Value::Object(
            serde_json::Map::new(),
        )) = session_data.clone();
        catalog::save_sessions(&state).await;
        // Redirect with the new viewer token.
        let redirect = format!("/room/{session_id}?token={new_viewer_token}");
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, redirect)
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
        use rand::RngCore;
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
        .header(header::CONTENT_SECURITY_POLICY, security::strict_csp(&nonce))
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
        if is_controller && c.token == sender_token {
            // skip the controller sender (it's the originator)
        } else {
            senders.push(c.sender.clone());
        }
    }
    for v in conns.viewers.values() {
        if v.token != sender_token {
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
    let controller_info = serde_json::json!({
        "token": controller_token,
        "username": "Controller",
        "slot": session_data.get("controller_slot").cloned().unwrap_or(Value::Null),
        "online": controller_online,
        "has_mk": mk_owner == Some(controller_token) || mk_owner.is_none(),
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
                obj.insert("has_mk".into(), serde_json::json!(mk_owner == Some(vtoken)));
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
    let state_payload = serde_json::json!({
        "type": "state_update",
        "viewers": users_with_status,
        "designated_speaker": session_data.get("designated_speaker").cloned().unwrap_or(Value::Null),
    });
    broadcast_to_room(state, session_id, &state_payload).await;
}

/// Push the per-token state (slots + MK control) to the app container's token
/// endpoint. Mirrors `broadcast_token_state`.
async fn broadcast_token_state(_state: &AppState, session_id: &str, session_data: &Value) {
    let mk_owner = session_data.get("mk_owner_token").and_then(|v| v.as_str());
    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut all_tokens = serde_json::Map::new();
    all_tokens.insert(
        controller_token.to_string(),
        serde_json::json!({
            "role": "controller",
            "slot": session_data.get("controller_slot").cloned().unwrap_or(Value::Null),
            "mk_control": if mk_owner.is_some() { mk_owner == Some(controller_token) } else { true },
        }),
    );
    if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
        for v in viewers {
            let vtoken = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            all_tokens.insert(
                vtoken.to_string(),
                serde_json::json!({
                    "role": "viewer",
                    "slot": v.get("slot").cloned().unwrap_or(Value::Null),
                    "mk_control": mk_owner == Some(vtoken),
                }),
            );
        }
    }
    let all_tokens = Value::Object(all_tokens);

    // Build the target host:port set (session ip + container registry).
    let mut targets: Vec<(String, Option<u16>)> = Vec::new();
    if let Some(ip) = session_data.get("ip").and_then(|v| v.as_str()) {
        let port = session_data.get("port").and_then(|v| v.as_u64()).map(|p| p as u16);
        targets.push((ip.to_string(), port));
    }
    if let Some(registry) = session_data.get("container_registry").and_then(|v| v.as_object()) {
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
        return;
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
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{pass}"));
        stacked.insert("Authorization".to_string(), format!("Basic {b64}"));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .unwrap_or_default();
    for (ip, port) in targets {
        let mut urls: Vec<String> = Vec::new();
        if let Some(p) = port {
            urls.push(format!("http://{ip}:{p}/{session_id}/api/tokens"));
        }
        urls.push(format!("http://{ip}:8083/tokens"));
        for url in urls {
            let is_api = url.ends_with("/api/tokens");
            let mut req = client.post(&url).json(&all_tokens);
            if is_api {
                for (k, v) in &stacked {
                    req = req.header(k.as_str(), v.as_str());
                }
            } else {
                req = req.header("Authorization", bearer.clone());
            }
            if req.send().await.is_ok() {
                break;
            }
        }
    }
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
    let Some(mut session_data) = state.sessions.read().await.get(session_id).cloned() else {
        return;
    };
    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let viewer_token = viewer_token.unwrap_or("");

    let mut target_username = "Unknown".to_string();
    let mut old_slot_for_target: Option<i64> = None;

    let mut notifications: Vec<String> = Vec::new();

    if viewer_token == controller_token {
        target_username = "Controller".to_string();
        old_slot_for_target = session_data
            .get("controller_slot")
            .and_then(|v| v.as_i64());
    } else if let Some(viewers) = session_data.get_mut("viewers").and_then(|v| v.as_array_mut()) {
        if let Some(v) = viewers.iter_mut().find(|v| {
            v.get("token").and_then(|t| t.as_str()) == Some(viewer_token)
        }) {
            target_username = v.get("username").and_then(|u| u.as_str()).unwrap_or("Unnamed").to_string();
            old_slot_for_target = v.get("slot").and_then(|s| s.as_i64());
        }
    }

    // Clear the slot from any previous owner.
    if let Some(slot) = slot {
        let mut previous_owner_cleared = false;
        if session_data.get("controller_slot").and_then(|v| v.as_i64()) == Some(slot)
            && controller_token != viewer_token
        {
            session_data["controller_slot"] = Value::Null;
            notifications.push(format!("Controller was unassigned from Gamepad {slot}."));
            previous_owner_cleared = true;
        }
        if !previous_owner_cleared {
            if let Some(viewers) = session_data.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                if let Some(v) = viewers.iter_mut().find(|v| {
                    v.get("slot").and_then(|s| s.as_i64()) == Some(slot)
                        && v.get("token").and_then(|t| t.as_str()) != Some(viewer_token)
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
    if viewer_token == controller_token {
        session_data["controller_slot"] = slot.map(Value::from).unwrap_or(Value::Null);
    } else if let Some(viewers) = session_data.get_mut("viewers").and_then(|v| v.as_array_mut()) {
        if let Some(v) = viewers.iter_mut().find(|v| {
            v.get("token").and_then(|t| t.as_str()) == Some(viewer_token)
        }) {
            v["slot"] = slot.map(Value::from).unwrap_or(Value::Null);
        }
    }

    if let Some(slot) = slot {
        if old_slot_for_target != Some(slot) {
            notifications.push(format!("Gamepad {slot} was assigned to {target_username}."));
        }
    } else if let Some(old) = old_slot_for_target {
        notifications.push(format!("{target_username} was unassigned from Gamepad {old}."));
    }

    broadcast_token_state(state, session_id, &session_data).await;
    state.sessions.write().await.insert(session_id.to_string(), session_data);
    catalog::save_sessions(state).await;

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
    let Some(mut session_data) = state.sessions.read().await.get(session_id).cloned() else {
        return;
    };
    let controller_token = session_data
        .get("controller_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut target_token = target_token.map(|s| s.to_string());
    if target_token.as_deref() == Some(controller_token) {
        target_token = None;
    }
    let current_owner = session_data.get("mk_owner_token").and_then(|v| v.as_str());
    if current_owner == target_token.as_deref() {
        return;
    }
    session_data["mk_owner_token"] = target_token.clone().map(Value::from).unwrap_or(Value::Null);

    let mut username = "Controller".to_string();
    if let Some(tt) = &target_token {
        if let Some(viewers) = session_data.get("viewers").and_then(|v| v.as_array()) {
            if let Some(v) = viewers.iter().find(|v| {
                v.get("token").and_then(|t| t.as_str()) == Some(tt.as_str())
            }) {
                username = v.get("username").and_then(|u| u.as_str()).unwrap_or("User").to_string();
            }
        }
    }
    broadcast_token_state(state, session_id, &session_data).await;
    state.sessions.write().await.insert(session_id.to_string(), session_data);
    catalog::save_sessions(state).await;
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
    let is_controller = token == controller_token;
    let viewer_data_ref: Option<Value> = session_data
        .get("viewers")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.iter().find(|v| v.get("token").and_then(|t| t.as_str()) == Some(token.as_str())))
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
        use rand::RngCore;
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
                            obj.insert("sender_token".into(), serde_json::json!(token_clone));
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
                            if is_audio_packet && ds != token_clone {
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
            let speaker = data.get("token").and_then(|t| t.as_str());
            let mut sessions = state.sessions.write().await;
            if let Some(s) = sessions.get_mut(session_id) {
                s["designated_speaker"] = speaker.map(Value::from).unwrap_or(Value::Null);
            }
            drop(sessions);
            catalog::save_sessions(state).await;
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
                        v.get("token").and_then(|t| t.as_str()) == Some(token)
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
            catalog::save_sessions(state).await;
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
                use rand::RngCore;
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
                        "token": token,
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
        info!("[{session_id}] Viewer {token} disconnected from collab room.");
        // Release slots / MK / designated speaker for the disconnected viewer.
        let mut sessions = state.sessions.write().await;
        let mut state_changed = false;
        let mut input_released = false;
        if let Some(s) = sessions.get_mut(session_id) {
            if s.get("designated_speaker").and_then(|v| v.as_str()) == Some(token) {
                s["designated_speaker"] = Value::Null;
                state_changed = true;
            }
            // Read the MK ownership before mutably borrowing the viewers list.
            let mk_is_owner = s.get("mk_owner_token").and_then(|v| v.as_str()) == Some(token);
            let mut slot_to_release: Option<i64> = None;
            let mut viewer_uname = "A user".to_string();
            if let Some(viewers) = s.get_mut("viewers").and_then(|v| v.as_array_mut()) {
                if let Some(v) = viewers.iter_mut().find(|v| {
                    v.get("token").and_then(|t| t.as_str()) == Some(token)
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
            // Broadcast after the mutable borrow of `s` has ended.
            if let Some(slot) = slot_to_release {
                broadcast_to_room(
                    state,
                    session_id,
                    &serde_json::json!({
                        "type": "gamepad_change",
                        "message": format!("{viewer_uname} disconnected and was unassigned from Gamepad {slot}."),
                        "timestamp": now_ms,
                    }),
                )
                .await;
            }
            if mk_is_owner {
                broadcast_to_room(
                    state,
                    session_id,
                    &serde_json::json!({
                        "type": "mk_change",
                        "message": format!("{viewer_uname} disconnected. MK control reverted to Controller."),
                        "timestamp": now_ms,
                    }),
                )
                .await;
            }
        }
        if state_changed {
            catalog::save_sessions(state).await;
        }
        if input_released {
            let sd = state.sessions.read().await.get(session_id).cloned();
            if let Some(sd) = sd {
                broadcast_token_state(state, session_id, &sd).await;
            }
        }
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
