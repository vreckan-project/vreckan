//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! File-management endpoints: chunked download, folder creation, async
//! deletion, file listing, upload finalization, public sharing, and
//! launching a stored file in an application.
//!
//! Ports `server/routers/files`, `server/routers/upload`,
//! `server/routers/homedir`, `server/routers/pinned`, and the
//! `/public/...` routes from `server/api`.

use std::collections::HashMap;
use std::path::Path;

use axum::body::Bytes;
use axum::extract::{Path as PathParam, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::auth;
use crate::catalog;
use crate::e2ee;
use crate::entities;
use crate::security;
use crate::state::AppState;
use crate::user_manager;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// 2 MiB chunk size for file downloads and backup chunks.
const CHUNK_SIZE: usize = 2 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Request / response models (mirror server/models)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateFolderRequest {
    pub path: String,
    pub folder_name: String,
}

#[derive(Deserialize)]
pub struct DeleteItemsRequest {
    pub paths: Vec<String>,
}

#[derive(Serialize)]
pub struct DeleteTaskResponse {
    pub message: String,
    pub task_id: String,
}

#[derive(Deserialize)]
pub struct FinalizeUploadToDirRequest {
    pub path: String,
    pub filename: String,
    pub upload_id: String,
    pub total_chunks: i64,
}

#[derive(Serialize)]
pub struct FileChunkResponse {
    pub chunk_data_b64: String,
    pub is_last_chunk: bool,
}

#[derive(Serialize)]
pub struct FileListItem {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: f64,
}

#[derive(Serialize)]
pub struct FileListResponse {
    pub items: Vec<FileListItem>,
    pub path: String,
    pub page: i64,
    pub per_page: i64,
    pub total: usize,
}

#[derive(Serialize)]
pub struct GenericSuccessMessage {
    pub message: String,
}

#[derive(Deserialize)]
pub struct ShareFileRequest {
    pub home_dir: String,
    pub path: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub expiry_hours: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PublicShareMetadata {
    pub owner_username: String,
    pub original_filename: String,
    pub created_at: f64,
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_timestamp: Option<f64>,
}

#[derive(Serialize)]
pub struct PublicShareInfo {
    pub share_id: String,
    pub original_filename: String,
    pub size_bytes: u64,
    pub created_at: f64,
    pub expiry_timestamp: Option<f64>,
    pub has_password: bool,
    pub url: String,
}

#[derive(Deserialize)]
pub struct LaunchFromStorageRequest {
    pub home_dir: String,
    pub path: String,
    pub application_id: String,
    #[serde(default = "default_true")]
    pub open_file_on_launch: bool,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
}

#[derive(Deserialize)]
pub struct UploadInitiateRequest {
    pub filename: String,
    pub total_size: i64,
}

#[derive(Serialize)]
pub struct UploadInitiateResponse {
    pub upload_id: String,
}

#[derive(Deserialize)]
pub struct UploadChunkRequest {
    pub upload_id: String,
    pub chunk_index: i64,
    pub chunk_data_b64: String,
}

#[derive(Deserialize)]
pub struct UploadToStorageRequest {
    pub filename: String,
    pub upload_id: String,
    pub total_chunks: i64,
    pub home_name: String,
}

#[derive(Deserialize)]
pub struct HomeDirectoryCreate {
    pub home_name: String,
}

#[derive(Serialize)]
pub struct HomeDirectoryList {
    pub home_dirs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PinnedBehavior {
    pub id: String,
    pub name: String,
    pub application_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default = "default_manual")]
    pub trigger_type: String,
    #[serde(default)]
    pub trigger_value: String,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Deserialize)]
pub struct CreatePinnedBehaviorRequest {
    pub name: String,
    pub application_id: String,
    #[serde(default)]
    pub home_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub selected_gpu: Option<String>,
    #[serde(default)]
    pub launch_in_room_mode: bool,
    #[serde(default = "default_true")]
    pub wayland_mode: bool,
    #[serde(default = "default_manual")]
    pub trigger_type: String,
    #[serde(default)]
    pub trigger_value: String,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Deserialize)]
pub struct PublicPasswordForm {
    pub password: String,
}

fn default_true() -> bool {
    true
}

fn default_manual() -> String {
    "manual".to_string()
}

// ---------------------------------------------------------------------------
// Path validation helper (mirrors `_get_validated_path`)
// ---------------------------------------------------------------------------

/// Validate a sub-path within a user's home directory, preventing directory
/// traversal. Returns the resolved full path.
fn get_validated_path(
    settings: &crate::settings::Settings,
    username: &str,
    home_dir: &str,
    sub_path: &str,
    check_existence: bool,
) -> Result<std::path::PathBuf, (StatusCode, String)> {
    // Validate home_dir name.
    let valid: bool = home_dir
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return Err((StatusCode::BAD_REQUEST, "Invalid home directory name.".into()));
    }
    let homes = user_manager::get_home_dirs(settings, username);
    if !homes.contains(&home_dir.to_string()) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("Access to home directory '{home_dir}' denied."),
        ));
    }

    let base_dir = Path::new(&settings.storage_path).join(username).join(home_dir);
    let base_dir = base_dir.canonicalize().map_err(|_| {
        (StatusCode::NOT_FOUND, "Home directory not found.".to_string())
    })?;

    // Normalize the sub-path (strip leading "/", keep ".." so traversal is
    // detectable). Mirrors `os.path.normpath(sub_path).lstrip("/")`.
    let normalized: String = sub_path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("/");
    // Check for ".." in the normalized path.
    if normalized.split('/').any(|seg| seg == "..") {
        return Err((
            StatusCode::FORBIDDEN,
            "Directory traversal attempt detected.".to_string(),
        ));
    }

    let full_path = base_dir.join(&normalized);
    let full_path = full_path.canonicalize().unwrap_or(full_path);

    // Ensure the resolved path is still under the base directory.
    if full_path != base_dir && !full_path.starts_with(&base_dir) {
        return Err((
            StatusCode::FORBIDDEN,
            "Directory traversal attempt detected.".to_string(),
        ));
    }

    if check_existence && !full_path.exists() {
        return Err((StatusCode::NOT_FOUND, "Path not found.".to_string()));
    }

    Ok(full_path)
}

// ---------------------------------------------------------------------------
// Public share DB load/save (mirrors `load_public_shares_metadata` /
// `save_public_shares_metadata` in api)
// ---------------------------------------------------------------------------

/// Load public-share metadata from the database into `state.public_shares`.
pub async fn load_public_shares(state: &AppState) {
    let rows = match entities::public_share::Entity::find().all(&state.db.conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Error loading public shares from the database: {e}");
            return;
        }
    };
    let mut shares = HashMap::new();
    for row in rows {
        match serde_json::from_value::<PublicShareMetadata>(row.data) {
            Ok(meta) => {
                shares.insert(row.share_id, meta);
            }
            Err(e) => {
                warn!("Skipping malformed public share row: {e}");
            }
        }
    }
    let count = shares.len();
    *state.public_shares.write().await = shares;
    info!("Loaded {count} public share(s) from the database.");
}

/// Persist the in-memory public-share map to the database (full replace).
pub async fn save_public_shares(state: &AppState) {
    let shares = state.public_shares.read().await.clone();
    let conn = &state.db.conn;
    // Delete all existing rows.
    let all = match entities::public_share::Entity::find().all(conn).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("Failed to save public shares (delete): {e}");
            return;
        }
    };
    for row in all {
        let am = entities::public_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set(row.share_id),
            data: sea_orm::ActiveValue::NotSet,
        };
        if let Err(e) = entities::public_share::Entity::delete(am).exec(conn).await {
            warn!("Failed to delete public share row: {e}");
        }
    }
    // Re-insert.
    for (share_id, meta) in shares.iter() {
        let data = match serde_json::to_value(meta) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to serialize public share: {e}");
                continue;
            }
        };
        let am = entities::public_share::ActiveModel {
            share_id: sea_orm::ActiveValue::Set(share_id.clone()),
            data: sea_orm::ActiveValue::Set(data),
        };
        if let Err(e) = entities::public_share::Entity::insert(am).exec(conn).await {
            warn!("Failed to insert public share row: {e}");
        }
    }
    info!("Saved {} public share(s) to the database.", shares.len());
}

/// Remove expired shares (file + metadata) and save. Mirrors
/// `background_share_cleanup_job`.
pub async fn cleanup_expired_shares(state: &AppState) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut to_remove = Vec::new();
    {
        let shares = state.public_shares.read().await;
        for (id, meta) in shares.iter() {
            if let Some(exp) = meta.expiry_timestamp {
                if exp < now {
                    to_remove.push(id.clone());
                }
            }
        }
    }
    if to_remove.is_empty() {
        return;
    }
    info!("Found {} expired share(s) to clean up.", to_remove.len());
    for id in &to_remove {
        let file_path = Path::new(&state.settings.public_storage_path).join(id);
        if file_path.exists() {
            let _ = std::fs::remove_file(&file_path);
        }
    }
    {
        let mut shares = state.public_shares.write().await;
        for id in &to_remove {
            shares.remove(id);
        }
    }
    save_public_shares(state).await;
    info!("Expired share cleanup complete.");
}

// ---------------------------------------------------------------------------
// /api/files route handlers (all E2EE-encrypted)
// ---------------------------------------------------------------------------

/// `GET /api/files/download/chunk/{home_dir}?path=&chunk_index=`
async fn download_file_chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_dir): PathParam<String>,
    Query(q): Query<ChunkQuery>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    // Mirrors the Python `chunk_index: int = Query(..., ge=0)` constraint: a
    // negative index fails query validation before the file is even read.
    if q.chunk_index < 0 {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "chunk_index must be >= 0." }),
        )
        .await;
    }
    let validated = match get_validated_path(&state.settings, &user.record.username, &home_dir, &q.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !validated.is_file() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "File not found or is a directory." }),
        )
        .await;
    }
    let result = std::fs::File::open(&validated).and_then(|mut f| {
        use std::io::{Read, Seek, SeekFrom};
        f.seek(SeekFrom::Start((q.chunk_index as u64) * CHUNK_SIZE as u64))?;
        let mut buf = vec![0u8; CHUNK_SIZE];
        let n = f.read(&mut buf)?;
        buf.truncate(n);
        Ok::<Vec<u8>, std::io::Error>(buf)
    });
    match result {
        Ok(chunk_data) => {
            let is_last = chunk_data.len() < CHUNK_SIZE;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&chunk_data);
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::OK,
                serde_json::json!({ "chunk_data_b64": b64, "is_last_chunk": is_last }),
            )
            .await
        }
        Err(e) => {
            warn!("Error reading chunk for file {}: {e}", q.path);
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": "Error reading file chunk." }),
            )
            .await
        }
    }
}

#[derive(Deserialize)]
struct ChunkQuery {
    path: String,
    chunk_index: i64,
}

/// `POST /api/files/create_folder/{home_dir}`
async fn create_folder(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_dir): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: CreateFolderRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    // Mirrors the Python `folder_name: Field(..., pattern=r"^[^/\\]+$")`
    // constraint: a folder name may not contain a path separator.
    if req.folder_name.is_empty() || req.folder_name.contains('/') || req.folder_name.contains('\\') {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: folder_name must not contain a path separator." }),
        )
        .await;
    }
    let validated = match get_validated_path(&state.settings, &user.record.username, &home_dir, &req.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !validated.is_dir() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Path is not a valid directory." }),
        )
        .await;
    }
    let new_folder = validated.join(&req.folder_name);
    if new_folder.exists() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": format!("Folder '{}' already exists.", req.folder_name) }),
        )
        .await;
    }
    match std::fs::create_dir(&new_folder) {
        Ok(()) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::OK,
            serde_json::json!({ "message": format!("Folder '{}' created successfully.", req.folder_name) }),
        )
        .await,
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("Could not create folder: {e}") }),
        )
        .await,
    }
}

/// `POST /api/files/delete/{home_dir}` — initiate an async deletion task.
async fn initiate_deletion(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_dir): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: DeleteItemsRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let task_id = catalog::random_uuid();
    {
        let mut tasks = state.deletion_tasks.write().await;
        tasks.insert(task_id.clone(), serde_json::json!({ "status": "pending" }));
    }
    // Spawn the async deletion.
    let state_clone = state.clone();
    let username = user.record.username.clone();
    let home_dir_clone = home_dir.clone();
    let task_id_clone = task_id.clone();
    tokio::spawn(async move {
        perform_deletion(&state_clone, &task_id_clone, &username, &home_dir_clone, &req.paths).await;
    });
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({ "message": "Deletion task started.", "task_id": task_id }),
    )
    .await
}

/// Perform the actual deletion work (runs in a spawned task). Mirrors
/// `_perform_deletion`.
async fn perform_deletion(
    state: &AppState,
    task_id: &str,
    username: &str,
    home_dir: &str,
    paths_to_delete: &[String],
) {
    {
        let mut tasks = state.deletion_tasks.write().await;
        if let Some(t) = tasks.get_mut(task_id) {
            if let Some(obj) = t.as_object_mut() {
                obj.insert("status".into(), serde_json::json!("processing"));
            }
        }
    }
    let mut deleted_count = 0usize;
    let result: Result<(), (StatusCode, String)> = (|| {
        for p in paths_to_delete {
            let validated = get_validated_path(&state.settings, username, home_dir, p, true)?;
            if validated.is_dir() {
                std::fs::remove_dir_all(&validated).map_err(|e| {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete directory: {e}"))
                })?;
            } else if validated.is_file() {
                std::fs::remove_file(&validated).map_err(|e| {
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to delete file: {e}"))
                })?;
            }
            deleted_count += 1;
        }
        Ok(())
    })();
    let mut tasks = state.deletion_tasks.write().await;
    match result {
        Ok(()) => {
            if let Some(t) = tasks.get_mut(task_id) {
                if let Some(obj) = t.as_object_mut() {
                    obj.insert("status".into(), serde_json::json!("completed"));
                    obj.insert(
                        "message".into(),
                        serde_json::json!(format!("Successfully deleted {deleted_count} items.")),
                    );
                }
            }
        }
        Err((_, msg)) => {
            if let Some(t) = tasks.get_mut(task_id) {
                if let Some(obj) = t.as_object_mut() {
                    obj.insert("status".into(), serde_json::json!("error"));
                    obj.insert("message".into(), serde_json::json!(msg));
                }
            }
        }
    }
}

/// `GET /api/files/delete_status/{task_id}`
async fn check_deletion_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(task_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let _ = user;
    let task = state.deletion_tasks.read().await.get(&task_id).cloned();
    match task {
        Some(t) => e2ee::encrypt_response(&state, &headers, StatusCode::OK, t).await,
        None => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Task not found." }),
        )
        .await,
    }
}

/// `GET /api/files/list/{home_dir}?path=&page=&per_page=`
async fn list_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_dir): PathParam<String>,
    Query(q): Query<ListQuery>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let validated = match get_validated_path(&state.settings, &user.record.username, &home_dir, &q.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !validated.is_dir() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Path is not a valid directory." }),
        )
        .await;
    }
    let home_dir_root = Path::new(&state.settings.storage_path)
        .join(&user.record.username)
        .join(&home_dir);

    let read_result = std::fs::read_dir(&validated);
    let all_items: Vec<std::path::PathBuf> = match read_result {
        Ok(entries) => {
            let mut v: Vec<std::path::PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            // Sort: directories first, then by lowercase name.
            v.sort_by(|a, b| {
                let a_dir = a.is_dir();
                let b_dir = b.is_dir();
                if a_dir != b_dir {
                    b_dir.cmp(&a_dir)
                } else {
                    let a_name = a.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
                    let b_name = b.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
                    a_name.cmp(&b_name)
                }
            });
            v
        }
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("Error reading directory: {e}") }),
            )
            .await
        }
    };
    let total = all_items.len();
    let start = ((q.page - 1) * q.per_page) as usize;
    let end = (start + q.per_page as usize).min(total);
    let paginated = &all_items[start..end];

    let mut response_items = Vec::new();
    for item in paginated {
        let stat = match item.metadata() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rel = item.strip_prefix(&home_dir_root).unwrap_or(item);
        let mut item_path = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
        if item_path == "/." {
            item_path = "/".to_string();
        }
        response_items.push(FileListItem {
            name: item
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: item_path,
            is_dir: item.is_dir(),
            size: stat.len(),
            mtime: stat
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
        });
    }
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({
            "items": response_items,
            "total": total,
            "page": q.page,
            "per_page": q.per_page,
            "path": q.path,
        }),
    )
    .await
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_root_path")]
    path: String,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_per_page")]
    per_page: i64,
}

fn default_root_path() -> String {
    "/".to_string()
}
fn default_page() -> i64 {
    1
}
fn default_per_page() -> i64 {
    50
}

/// `POST /api/files/upload_to_dir/{home_dir}`
async fn finalize_upload_to_dir(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_dir): PathParam<String>,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: FinalizeUploadToDirRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let dest_dir = match get_validated_path(&state.settings, &user.record.username, &home_dir, &req.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !dest_dir.is_dir() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Destination path is not a valid directory." }),
        )
        .await;
    }
    let reassembled = match catalog::reassemble_file(&state.settings, &req.upload_id, req.total_chunks) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let safe_filename = Path::new(&req.filename).file_name().and_then(|f| f.to_str()).unwrap_or(&req.filename);
    let actual_filename = catalog::get_unique_filename(&dest_dir.to_string_lossy(), safe_filename);
    let final_location = dest_dir.join(&actual_filename);
    match std::fs::rename(&reassembled, &final_location) {
        Ok(()) => {
            let _ = std::fs::set_permissions(&final_location, std::os::unix::fs::PermissionsExt::from_mode(0o644));
            info!(
                "User '{}' uploaded '{}' to '{}{}'",
                user.record.username, actual_filename, home_dir, req.path
            );
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::OK,
                serde_json::json!({ "message": "File uploaded successfully." }),
            )
            .await
        }
        Err(e) => {
            let _ = std::fs::remove_file(&reassembled);
            warn!("Failed to move finalized upload for user '{}': {e}", user.record.username);
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": "Could not place file in destination." }),
            )
            .await
        }
    }
}

/// `POST /api/files/share` — create a public share.
async fn create_public_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    // Public sharing requires the effective public_sharing setting.
    if !user
        .effective_settings
        .get("public_sharing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Public file sharing is disabled for this account." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: ShareFileRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let username = &user.record.username;
    let source_path = match get_validated_path(&state.settings, username, &req.home_dir, &req.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !source_path.is_file() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Path does not point to a file." }),
        )
        .await;
    }
    let share_id = catalog::random_uuid();
    let dest_path = Path::new(&state.settings.public_storage_path).join(&share_id);
    if let Err(e) = std::fs::copy(&source_path, &dest_path) {
        warn!("Failed to create share for user '{username}': {e}");
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": "Failed to create share." }),
        )
        .await;
    }
    let stat_info = source_path.metadata().ok();
    let password_hash = req
        .password
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| user_manager::hash_share_password(p));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let expiry_timestamp = req
        .expiry_hours
        .filter(|h| *h > 0)
        .map(|h| now + (h as f64) * 3600.0);
    let metadata = PublicShareMetadata {
        owner_username: username.clone(),
        original_filename: source_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        created_at: now,
        size_bytes: stat_info.map(|s| s.len()).unwrap_or(0),
        password_hash,
        expiry_timestamp,
    };
    {
        let mut shares = state.public_shares.write().await;
        shares.insert(share_id.clone(), metadata.clone());
    }
    save_public_shares(&state).await;
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({
            "share_id": share_id,
            "original_filename": metadata.original_filename,
            "size_bytes": metadata.size_bytes,
            "created_at": metadata.created_at,
            "expiry_timestamp": metadata.expiry_timestamp,
            "has_password": metadata.password_hash.is_some(),
            "url": format!("/public/{share_id}"),
        }),
    )
    .await
}

/// `GET /api/files/shares` — list the caller's public shares.
async fn list_public_shares(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !user
        .effective_settings
        .get("public_sharing")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Public file sharing is disabled for this account." }),
        )
        .await;
    }
    let shares = state.public_shares.read().await;
    let mut user_shares: Vec<PublicShareInfo> = shares
        .iter()
        .filter(|(_, meta)| meta.owner_username == user.record.username)
        .map(|(sid, meta)| PublicShareInfo {
            share_id: sid.clone(),
            original_filename: meta.original_filename.clone(),
            size_bytes: meta.size_bytes,
            created_at: meta.created_at,
            expiry_timestamp: meta.expiry_timestamp,
            has_password: meta.password_hash.is_some(),
            url: format!("/public/{sid}"),
        })
        .collect();
    user_shares.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::to_value(&user_shares).unwrap_or_default(),
    )
    .await
}

/// `DELETE /api/files/share/{share_id}`
async fn delete_public_share(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(share_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let metadata = state
        .public_shares
        .read()
        .await
        .get(&share_id)
        .cloned();
    let Some(metadata) = metadata else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Share not found or permission denied." }),
        )
        .await;
    };
    if metadata.owner_username != user.record.username {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Share not found or permission denied." }),
        )
        .await;
    }
    let public_file_path = Path::new(&state.settings.public_storage_path).join(&share_id);
    if public_file_path.exists() {
        let _ = std::fs::remove_file(&public_file_path);
    }
    {
        let mut shares = state.public_shares.write().await;
        shares.remove(&share_id);
    }
    save_public_shares(&state).await;
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, Value::Null).await
}

/// `POST /api/files/launch_from_storage`
async fn launch_from_storage(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    // Persistent storage is required to open a stored file in an app. Mirrors
    // the Python `verify_persistent_storage_enabled` dependency, which runs
    // before the request body is validated.
    if !user
        .effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    let req: LaunchFromStorageRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let validated = match get_validated_path(&state.settings, &user.record.username, &req.home_dir, &req.path, true) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    if !validated.is_file() {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::BAD_REQUEST,
            serde_json::json!({ "detail": "Only files can be opened in an application." }),
        )
        .await;
    }
    let file_bytes = match std::fs::read(&validated) {
        Ok(b) => b,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("Failed to read file: {e}") }),
            )
            .await
        }
    };
    let filename = validated
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    match catalog::launch_common(
        &state,
        &req.application_id,
        &user.record.username,
        &user.effective_settings,
        Some(&req.home_dir),
        HashMap::new(),
        req.language.as_deref(),
        req.selected_gpu.as_deref(),
        Some(file_bytes),
        Some(&filename),
        req.open_file_on_launch,
        None,
        false,
        req.wayland_mode,
        None,
        true,
    )
    .await
    {
        Ok(resp) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::OK,
            serde_json::to_value(&resp).unwrap_or_default(),
        )
        .await,
        Err((s, d)) => e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await,
    }
}

// ---------------------------------------------------------------------------
// /api/upload route handlers (all E2EE-encrypted)
// ---------------------------------------------------------------------------

/// `POST /api/upload/initiate`
async fn upload_initiate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let _ = user;
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: UploadInitiateRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let upload_id = catalog::random_uuid();
    let upload_path = Path::new(&state.settings.upload_dir).join(&upload_id);
    if let Err(e) = std::fs::create_dir_all(&upload_path) {
        let _ = std::fs::set_permissions(&upload_path, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        warn!("Failed to create upload dir: {e}");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let metadata = serde_json::json!({
        "filename": req.filename,
        "size": req.total_size,
        "started": now,
    });
    let _ = std::fs::write(
        upload_path.join("metadata.json"),
        serde_json::to_string(&metadata).unwrap_or_default(),
    );
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({ "upload_id": upload_id }),
    )
    .await
}

/// `POST /api/upload/chunk`
async fn upload_chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let _ = user;
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: UploadChunkRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let upload_path = match catalog::validated_upload_path(&state.settings, &req.upload_id) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let chunk_bytes = match base64::engine::general_purpose::STANDARD.decode(&req.chunk_data_b64) {
        Ok(b) => b,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::BAD_REQUEST,
                serde_json::json!({ "detail": format!("Invalid Base64 chunk data: {e}") }),
            )
            .await
        }
    };
    let chunk_path = format!("{upload_path}/chunk_{}", req.chunk_index);
    if let Err(e) = std::fs::write(&chunk_path, &chunk_bytes) {
        warn!("Failed to write upload chunk: {e}");
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": "Failed to write chunk." }),
        )
        .await;
    }
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({ "status": "ok", "chunk_index": req.chunk_index }),
    )
    .await
}

/// `POST /api/upload/to_storage`
async fn upload_to_storage(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !user
        .effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: UploadToStorageRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    let username = &user.record.username;
    let available_homes = user_manager::get_home_dirs(&state.settings, username);
    if !available_homes.contains(&req.home_name) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": format!("Home directory '{}' not found for user.", req.home_name) }),
        )
        .await;
    }
    let reassembled = match catalog::reassemble_file(&state.settings, &req.upload_id, req.total_chunks) {
        Ok(p) => p,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let safe_filename = Path::new(&req.filename).file_name().and_then(|f| f.to_str()).unwrap_or(&req.filename);
    let file_dest_dir = Path::new(&state.settings.storage_path)
        .join(username)
        .join("_vreckan_shared_files");
    let _ = std::fs::create_dir_all(&file_dest_dir);
    let _ = std::fs::set_permissions(&file_dest_dir, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    let actual_filename = catalog::get_unique_filename(&file_dest_dir.to_string_lossy(), safe_filename);
    let file_location = file_dest_dir.join(&actual_filename);
    match std::fs::rename(&reassembled, &file_location) {
        Ok(()) => {
            let _ = std::fs::set_permissions(&file_location, std::os::unix::fs::PermissionsExt::from_mode(0o644));
            info!(
                "User '{username}' uploaded file '{actual_filename}' (as '{safe_filename}') to shared storage."
            );
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::OK,
                serde_json::json!({
                    "status": "success",
                    "message": format!("File '{safe_filename}' uploaded successfully."),
                }),
            )
            .await
        }
        Err(e) => {
            let _ = std::fs::remove_file(&reassembled);
            warn!("Failed to move reassembled file to session storage for user '{username}': {e}");
            e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": "Could not place file in session storage." }),
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// /api/homedirs route handlers (all E2EE-encrypted)
// ---------------------------------------------------------------------------

/// `GET /api/homedirs`
async fn list_my_home_dirs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !user
        .effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    let home_dirs = user_manager::get_home_dirs(&state.settings, &user.record.username);
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::json!({ "home_dirs": home_dirs }),
    )
    .await
}

/// `POST /api/homedirs`
async fn create_my_home_dir(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !user
        .effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: HomeDirectoryCreate = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    // Validate the name pattern up front (mirrors the Pydantic
    // `HomeDirectoryCreate` pattern check) so a malformed name is a 422
    // rather than being swallowed by the 409 duplicate handler.
    if !user_manager::is_valid_home_name(&req.home_name) {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: home_name must match ^[a-zA-Z0-9_-]+$" }),
        )
        .await;
    }
    match user_manager::create_home_dir(&state.settings, &user.record.username, &req.home_name) {
        Ok(()) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CREATED,
            serde_json::json!({ "status": "success", "home_name": req.home_name }),
        )
        .await,
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::CONFLICT,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

/// `DELETE /api/homedirs/{home_name}`
async fn delete_my_home_dir(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(home_name): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    if !user
        .effective_settings
        .get("persistent_storage")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::FORBIDDEN,
            serde_json::json!({ "detail": "Persistent storage is disabled for this account." }),
        )
        .await;
    }
    match user_manager::delete_home_dir(&state.settings, &user.record.username, &home_name) {
        Ok(()) => e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, Value::Null).await,
        Err(e) => e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": e.to_string() }),
        )
        .await,
    }
}

// ---------------------------------------------------------------------------
// /api/pinned route handlers (all E2EE-encrypted)
// ---------------------------------------------------------------------------

/// `GET /api/pinned`
async fn list_pinned_behaviors(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let rows = match entities::pinned_behavior::Entity::find()
        .filter(entities::pinned_behavior::Column::Username.eq(&user.record.username))
        .all(&state.db.conn)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("DB error: {e}") }),
            )
            .await
        }
    };
    let mut out: Vec<PinnedBehavior> = Vec::new();
    for r in rows {
        let mut d = r.data;
        if let Some(obj) = d.as_object_mut() {
            obj.insert("id".into(), serde_json::json!(r.id));
            obj.insert("created_at".into(), serde_json::json!(r.created_at));
        }
        if let Ok(pb) = serde_json::from_value::<PinnedBehavior>(d) {
            out.push(pb);
        }
    }
    // Order by created_at desc.
    out.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
    e2ee::encrypt_response(
        &state,
        &headers,
        StatusCode::OK,
        serde_json::to_value(&out).unwrap_or_default(),
    )
    .await
}

/// `POST /api/pinned`
async fn create_pinned_behavior(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let decrypted = match e2ee::decrypt_request_body(&state, &headers, body).await {
        Ok(v) => v,
        Err((s, d)) => {
            return e2ee::encrypt_response(&state, &headers, s, serde_json::json!({ "detail": d })).await
        }
    };
    let req: CreatePinnedBehaviorRequest = match serde_json::from_value(decrypted) {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::UNPROCESSABLE_ENTITY,
                serde_json::json!({ "detail": format!("Invalid request body: {e}") }),
            )
            .await
        }
    };
    // Mirrors the Pydantic `name: Field(..., min_length=1, max_length=128)`
    // validation (a 422 raised before the handler body runs).
    if req.name.is_empty() || req.name.len() > 128 {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({ "detail": "Invalid request body: name must be 1-128 characters." }),
        )
        .await;
    }
    let new_id = catalog::random_uuid();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let data = serde_json::json!({
        "name": req.name,
        "application_id": req.application_id,
        "home_name": req.home_name,
        "language": req.language,
        "selected_gpu": req.selected_gpu,
        "launch_in_room_mode": req.launch_in_room_mode,
        "wayland_mode": req.wayland_mode,
        "trigger_type": req.trigger_type,
        "trigger_value": req.trigger_value,
        "is_default": req.is_default,
    });
    let conn = &state.db.conn;
    if req.is_default {
        // Clear the default flag on any other presets for this user.
        let rows = match entities::pinned_behavior::Entity::find()
            .filter(entities::pinned_behavior::Column::Username.eq(&user.record.username))
            .all(conn)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                return e2ee::encrypt_response(
                    &state,
                    &headers,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "detail": format!("DB error: {e}") }),
                )
                .await
            }
        };
        for r in rows {
            if r.data.get("is_default").and_then(|v| v.as_bool()).unwrap_or(false) {
                let mut d = r.data;
                if let Some(obj) = d.as_object_mut() {
                    obj.insert("is_default".into(), serde_json::json!(false));
                }
                let am = entities::pinned_behavior::ActiveModel {
                    id: sea_orm::ActiveValue::Set(r.id),
                    username: sea_orm::ActiveValue::NotSet,
                    created_at: sea_orm::ActiveValue::NotSet,
                    data: sea_orm::ActiveValue::Set(d),
                };
                let _ = entities::pinned_behavior::Entity::update(am).exec(conn).await;
            }
        }
    }
    let am = entities::pinned_behavior::ActiveModel {
        id: sea_orm::ActiveValue::Set(new_id.clone()),
        username: sea_orm::ActiveValue::Set(user.record.username.clone()),
        created_at: sea_orm::ActiveValue::Set(now),
        data: sea_orm::ActiveValue::Set(data.clone()),
    };
    if let Err(e) = entities::pinned_behavior::Entity::insert(am).exec(conn).await {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("DB error: {e}") }),
        )
        .await;
    }
    let mut resp = data;
    if let Some(obj) = resp.as_object_mut() {
        obj.insert("id".into(), serde_json::json!(new_id));
        obj.insert("created_at".into(), serde_json::json!(now));
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::CREATED, resp).await
}

/// `POST /api/pinned/{pinned_id}/set-default`
async fn set_default_pinned_behavior(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(pinned_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let conn = &state.db.conn;
    let rows = match entities::pinned_behavior::Entity::find()
        .filter(entities::pinned_behavior::Column::Username.eq(&user.record.username))
        .all(conn)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("DB error: {e}") }),
            )
            .await
        }
    };
    let mut found_target = false;
    for r in rows {
        let mut d = r.data;
        if let Some(obj) = d.as_object_mut() {
            obj.insert("is_default".into(), serde_json::json!(r.id == pinned_id));
        }
        if r.id == pinned_id {
            found_target = true;
        }
        let am = entities::pinned_behavior::ActiveModel {
            id: sea_orm::ActiveValue::Set(r.id),
            username: sea_orm::ActiveValue::NotSet,
            created_at: sea_orm::ActiveValue::NotSet,
            data: sea_orm::ActiveValue::Set(d),
        };
        let _ = entities::pinned_behavior::Entity::update(am).exec(conn).await;
    }
    if !found_target {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Pinned behaviour not found." }),
        )
        .await;
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::OK, serde_json::json!({ "ok": true })).await
}

/// `DELETE /api/pinned/{pinned_id}`
async fn delete_pinned_behavior(
    State(state): State<AppState>,
    headers: HeaderMap,
    PathParam(pinned_id): PathParam<String>,
) -> Response {
    let user = match auth::verify_token(&state, &headers).await {
        Ok(u) => u,
        Err(e) => {
            // Auth failures are returned in plaintext (mirroring Python, where
            // the HTTPException raised in the auth dependency bypasses the
            // E2EE response wrapper). Routing them through encrypt_response
            // would turn a 401 into a 400 "Secure session required" when no
            // session exists yet.
            return (
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({ "detail": e.to_string() })),
            )
                .into_response()
        }
    };
    let conn = &state.db.conn;
    let row = match entities::pinned_behavior::Entity::find_by_id(&pinned_id)
        .one(conn)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return e2ee::encrypt_response(
                &state,
                &headers,
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "detail": format!("DB error: {e}") }),
            )
            .await
        }
    };
    let Some(row) = row else {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Pinned behaviour not found." }),
        )
        .await;
    };
    if row.username != user.record.username {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::NOT_FOUND,
            serde_json::json!({ "detail": "Pinned behaviour not found." }),
        )
        .await;
    }
    let am = entities::pinned_behavior::ActiveModel {
        id: sea_orm::ActiveValue::Set(row.id),
        username: sea_orm::ActiveValue::NotSet,
        created_at: sea_orm::ActiveValue::NotSet,
        data: sea_orm::ActiveValue::NotSet,
    };
    if let Err(e) = entities::pinned_behavior::Entity::delete(am).exec(conn).await {
        return e2ee::encrypt_response(
            &state,
            &headers,
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "detail": format!("DB error: {e}") }),
        )
        .await;
    }
    e2ee::encrypt_response(&state, &headers, StatusCode::NO_CONTENT, Value::Null).await
}

// ---------------------------------------------------------------------------
// /public/... routes (plaintext — accessed by anonymous downloaders)
// ---------------------------------------------------------------------------

/// `GET /public/download/{token}` — stream the shared file.
async fn download_shared_file(
    State(state): State<AppState>,
    PathParam(token): PathParam<String>,
) -> Response {
    let token_data = state.download_tokens.write().await.remove(&token);
    let Some(token_data) = token_data else {
        return (
            StatusCode::FORBIDDEN,
            "Invalid or expired download token.",
        )
            .into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if token_data.expires_at < now {
        return (
            StatusCode::FORBIDDEN,
            "Invalid or expired download token.",
        )
            .into_response();
    }
    let metadata = state
        .public_shares
        .read()
        .await
        .get(&token_data.share_id)
        .cloned();
    let Some(metadata) = metadata else {
        return (StatusCode::NOT_FOUND, "Shared file not found.").into_response();
    };
    let file_path = Path::new(&state.settings.public_storage_path).join(&token_data.share_id);
    if !file_path.exists() {
        return (StatusCode::NOT_FOUND, "Shared file not found on disk.").into_response();
    }
    let data = match std::fs::read(&file_path) {
        Ok(d) => d,
        Err(_) => return (StatusCode::NOT_FOUND, "Shared file not found on disk.").into_response(),
    };
    let disposition = format!("attachment; filename=\"{}\"", metadata.original_filename);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(data))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// Render the public password page with template substitution + strict CSP.
/// `status` is the HTTP status to return: 200 when the page is presented on a
/// GET (the share is password-protected), 401 when a POST submitted a wrong
/// password (mirrors the Python `access_public_share_post`).
fn render_password_page(share_id: &str, error_message: &str, status: StatusCode) -> Response {
    let template_path = crate::static_files::static_dir().join("public_password.html");
    let nonce = {
        let mut buf = [0u8; 16];
        use rand::RngCore;
        rand::rng().fill_bytes(&mut buf);
        base64::engine::general_purpose::STANDARD.encode(buf)
    };
    let html = match std::fs::read_to_string(&template_path) {
        Ok(t) => t
            .replace("{{SHARE_ID}}", share_id)
            .replace("{{ERROR_MESSAGE}}", error_message)
            .replace("{{CSP_NONCE}}", &nonce),
        Err(_) => "<h1>Password protected</h1>".to_string(),
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, security::strict_csp(&nonce))
        .body(axum::body::Body::from(html))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// `GET /public/{share_id}` — present the share (file, password page, or 410).
async fn access_public_share_get(
    State(state): State<AppState>,
    PathParam(share_id): PathParam<String>,
) -> Response {
    let metadata = state.public_shares.read().await.get(&share_id).cloned();
    let Some(metadata) = metadata else {
        return (StatusCode::NOT_FOUND, "Share not found.").into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Some(exp) = metadata.expiry_timestamp {
        if exp < now {
            return (
                StatusCode::GONE,
                Html("<h1>This link has expired.</h1>"),
            )
                .into_response();
        }
    }
    if metadata.password_hash.is_some() {
        return render_password_page(&share_id, "", StatusCode::OK);
    }
    let file_path = Path::new(&state.settings.public_storage_path).join(&share_id);
    if !file_path.exists() {
        return (StatusCode::NOT_FOUND, "Shared file not found on disk.").into_response();
    }
    let data = match std::fs::read(&file_path) {
        Ok(d) => d,
        Err(_) => return (StatusCode::NOT_FOUND, "Shared file not found on disk.").into_response(),
    };
    let disposition = format!("attachment; filename=\"{}\"", metadata.original_filename);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, disposition)
        .body(axum::body::Body::from(data))
        .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
}

/// `POST /public/{share_id}` — submit the share password.
async fn access_public_share_post(
    State(state): State<AppState>,
    PathParam(share_id): PathParam<String>,
    form: axum::extract::Form<PublicPasswordForm>,
) -> Response {
    let metadata = state.public_shares.read().await.get(&share_id).cloned();
    let Some(metadata) = metadata else {
        return (StatusCode::NOT_FOUND, "Share not found.").into_response();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Some(exp) = metadata.expiry_timestamp {
        if exp < now {
            return (
                StatusCode::GONE,
                Html("<h1>This link has expired.</h1>"),
            )
                .into_response();
        }
    }
    let Some(hash) = &metadata.password_hash else {
        return (
            StatusCode::BAD_REQUEST,
            "This share is not password protected.",
        )
            .into_response();
    };
    if user_manager::verify_share_password(hash, &form.password) {
        let token = catalog::random_token(32);
        {
            let mut tokens = state.download_tokens.write().await;
            tokens.insert(
                token.clone(),
                crate::state::DownloadTokenRecord {
                    share_id: share_id.clone(),
                    expires_at: now + 60.0,
                },
            );
        }
        let redirect = format!("/public/download/{token}");
        Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, redirect)
            .body(axum::body::Body::empty())
            .unwrap_or_else(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response())
    } else {
        render_password_page(&share_id, "Incorrect password. Please try again.", StatusCode::UNAUTHORIZED)
    }
}

// ---------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------

/// The E2EE-encrypted file-management routers: `/api/files`, `/api/upload`,
/// `/api/homedirs`, `/api/pinned`.
pub fn files_router() -> Router<AppState> {
    let files = Router::new()
        .route("/download/chunk/{home_dir}", get(download_file_chunk))
        .route("/create_folder/{home_dir}", post(create_folder))
        .route("/delete/{home_dir}", post(initiate_deletion))
        .route("/delete_status/{task_id}", get(check_deletion_status))
        .route("/list/{home_dir}", get(list_files))
        .route("/upload_to_dir/{home_dir}", post(finalize_upload_to_dir))
        .route("/share", post(create_public_share))
        .route("/shares", get(list_public_shares))
        .route("/share/{share_id}", delete(delete_public_share))
        .route("/launch_from_storage", post(launch_from_storage));
    let upload = Router::new()
        .route("/initiate", post(upload_initiate))
        .route("/chunk", post(upload_chunk))
        .route("/to_storage", post(upload_to_storage));
    let homedirs = Router::new()
        .route("/", get(list_my_home_dirs))
        .route("/", post(create_my_home_dir))
        .route("/{home_name}", delete(delete_my_home_dir));
    let pinned = Router::new()
        .route("/", get(list_pinned_behaviors))
        .route("/", post(create_pinned_behavior))
        .route("/{pinned_id}/set-default", post(set_default_pinned_behavior))
        .route("/{pinned_id}", delete(delete_pinned_behavior));
    Router::new()
        .nest("/files", files)
        .nest("/upload", upload)
        .nest("/homedirs", homedirs)
        .nest("/pinned", pinned)
}

/// The plaintext public-share routes: `/public/...`.
pub fn public_router() -> Router<AppState> {
    Router::new()
        .route("/public/download/{token}", get(download_shared_file))
        .route("/public/{share_id}", get(access_public_share_get))
        .route("/public/{share_id}", post(access_public_share_post))
}
