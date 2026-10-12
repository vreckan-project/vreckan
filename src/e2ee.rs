//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! E2EE (end-to-end encryption) handshake + encrypted request/response bodies.
//!
//! Ports the E2EE machinery in `server/api`: the server RSA key
//! (`SERVER_PRIVATE_KEY` / `SERVER_PUBLIC_KEY_PEM`), the handshake routes
//! (`/api/handshake/public_key`, `/api/handshake/initiate`,
//! `/api/handshake/exchange`), the per-session AES-GCM key store
//! (`CRYPTO_SESSIONS`, held in `AppState.crypto_sessions`), and the
//! `EncryptedRoute` request/response encryption.
//!
//! Protocol (mirrors `server/static/js/crypto.js`):
//!   1. Client fetches the server's RSA public key (SPKI PEM).
//!   2. Client POSTs `/initiate`; the server returns a random nonce signed
//!      with RSA-PSS (SHA-256, salt 32) — proving server identity.
//!   3. Client generates a random AES-256-GCM key, wraps it with RSA-OAEP
//!      (SHA-256) using the server public key, and POSTs it to `/exchange`.
//!   4. The server unwraps the AES key and returns a new `session_id`.
//!   5. Every subsequent request/response on the encrypted router is
//!      AES-256-GCM encrypted with that key, using the `session_id` as AAD so
//!      a ciphertext cannot be replayed against a different session.
//!
//! The handshake routes live on the *plaintext* router (like the auth routes):
//! the E2EE handshake proves server identity, not user identity, so it cannot
//! be gated by the very session it establishes.

use std::sync::OnceLock;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rand::RngCore;
use rand_core::OsRng;
use rsa::{Oaep, Pss, RsaPrivateKey, sha2::Sha256};
use sha2::Digest;
use serde::{Deserialize, Serialize};
use spki::EncodePublicKey;
use tracing::{info, warn};

use crate::settings::Settings;
use crate::state::AppState;

/// The `X-Session-ID` header name (carries the crypto session id).
const X_SESSION_ID: &str = "x-session-id";

/// The server's RSA private key (loaded once at startup). Mirrors the Python
/// module global `SERVER_PRIVATE_KEY`. Wrapped in `Arc` so it can be shared
/// across the (cloned) request handlers.
static SERVER_KEY: OnceLock<std::sync::Arc<RsaPrivateKey>> = OnceLock::new();
/// The server's RSA public key (SPKI PEM), served to clients. Mirrors
/// `SERVER_PUBLIC_KEY_PEM`.
static SERVER_PUBLIC_KEY_PEM: OnceLock<String> = OnceLock::new();

// ---------------------------------------------------------------------------
// Server key
// ---------------------------------------------------------------------------

/// Load (or generate on first run) the server's RSA key used for the E2EE
/// handshake, and publish it to the module statics. Mirrors
/// `run_https::_ensure_server_key` plus the `api` import-time key load.
///
/// The key is a 2048-bit RSA key stored as unencrypted PKCS#8 PEM at
/// `settings.server_private_key_path`. If the file is missing a fresh key is
/// generated and written there, so a first run "just works".
pub fn init_server_key(settings: &Settings) -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(&settings.server_private_key_path);
    if !path.exists() {
        let key = RsaPrivateKey::new(&mut OsRng, 2048)?;
        let pem = key.to_pkcs8_pem(LineEnding::LF)?.to_string();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &pem)?;
        info!("generated new E2EE server key at {}", path.display());
    }
    let pem = std::fs::read_to_string(&path)?;
    let key = RsaPrivateKey::from_pkcs8_pem(&pem)?;
    let public_pem = key.to_public_key().to_public_key_pem(LineEnding::LF)?;
    let _ = SERVER_KEY.set(std::sync::Arc::new(key));
    let _ = SERVER_PUBLIC_KEY_PEM.set(public_pem);
    info!("loaded E2EE server key from {}", path.display());
    Ok(())
}

/// The loaded server private key (panics if `init_server_key` was not run).
fn server_key() -> std::sync::Arc<RsaPrivateKey> {
    SERVER_KEY
        .get()
        .cloned()
        .expect("E2EE server key not initialised")
}

// ---------------------------------------------------------------------------
// Handshake routes
// ---------------------------------------------------------------------------

/// Response for `POST /api/handshake/initiate`.
#[derive(Serialize)]
pub struct HandshakeInitiateResponse {
    pub nonce: String,
    pub signature: String,
}

/// Body for `POST /api/handshake/exchange`.
#[derive(Deserialize)]
pub struct HandshakeExchangeRequest {
    pub encrypted_session_key: String,
}

/// Response for `POST /api/handshake/exchange`.
#[derive(Serialize)]
pub struct HandshakeExchangeResponse {
    pub session_id: String,
}

/// The encrypted request/response payload envelope (base64 IV + ciphertext).
#[derive(Serialize, Deserialize)]
pub struct EncryptedPayload {
    pub iv: String,
    pub ciphertext: String,
}

/// `GET /api/handshake/public_key` — the server's RSA public key (SPKI PEM).
pub async fn handshake_public_key() -> impl IntoResponse {
    let pem = SERVER_PUBLIC_KEY_PEM.get().cloned().unwrap_or_default();
    Json(serde_json::json!({ "server_public_key": pem }))
}

/// `POST /api/handshake/initiate` — return a random nonce signed with
/// RSA-PSS (SHA-256, salt 32) to prove server identity.
pub async fn handshake_initiate() -> impl IntoResponse {
    let mut nonce = [0u8; 32];
    // Use the OS CSPRNG (consistent with the server-key generation below) rather
    // than the thread-local `rand::rng()`, so the nonce is drawn from the same
    // entropy source the rest of the handshake uses. `OsRng` is from
    // `rand_core 0.6`, so call its `RngCore` method fully-qualified (the
    // `rand::RngCore` import above is the 0.9 trait and doesn't apply here).
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut nonce);
    let key = server_key();
    // PSS always requires an RNG (for the random salt); the plain `sign`
    // helper passes `None` and fails, so use `sign_with_rng`. The 32-byte
    // salt matches the Python `salt_length=32`.
    //
    // IMPORTANT: `RsaPrivateKey::sign_with_rng` treats its input as an
    // *already-hashed* digest (rsa crate docs: "Sign the given digest"). The
    // browser's WebCrypto `crypto.subtle.verify(RSA-PSS, SHA-256)` hashes the
    // message itself before verifying, so we must pass SHA256(nonce) here —
    // not the raw nonce — to match. (Python's `cryptography.sign(nonce, PSS,
    // SHA256)` hashes internally, which is why the Python server worked in the
    // browser while the raw-nonce Rust signature did not.)
    let mut rng = rand_core::OsRng;
    let hashed = Sha256::digest(&nonce);
    let signature = key
        .sign_with_rng(&mut rng, Pss::new_with_salt::<Sha256>(32), &hashed)
        .expect("RSA-PSS sign failed");
    Json(HandshakeInitiateResponse {
        nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
        signature: base64::engine::general_purpose::STANDARD.encode(signature),
    })
}

/// `POST /api/handshake/exchange` — unwrap the client's AES-GCM session key
/// (RSA-OAEP) and register a new crypto session.
pub async fn handshake_exchange(
    State(state): State<AppState>,
    Json(req): Json<HandshakeExchangeRequest>,
) -> impl IntoResponse {
    let key = server_key();
    let encrypted = match base64::engine::general_purpose::STANDARD
        .decode(&req.encrypted_session_key)
    {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "detail": "Failed to decrypt session key" })),
            )
                .into_response()
        }
    };
    let aes_key = match key.decrypt(Oaep::new::<Sha256>(), &encrypted) {
        Ok(k) => k,
        Err(e) => {
            warn!("Failed to decrypt session key during handshake: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "detail": "Failed to decrypt session key" })),
            )
                .into_response()
        }
    };
    let session_id = uuid::Uuid::new_v4().to_string();
    state
        .crypto_sessions
        .write()
        .await
        .insert(session_id.clone(), aes_key);
    info!(
        "E2EE handshake successful. New crypto session: {}...",
        &session_id[..session_id.len().min(8)]
    );
    Json(HandshakeExchangeResponse { session_id }).into_response()
}

/// Build the handshake router (mounted at `/api/handshake`).
pub fn handshake_router() -> Router<AppState> {
    Router::new()
        .route("/public_key", get(handshake_public_key))
        .route("/initiate", post(handshake_initiate))
        .route("/exchange", post(handshake_exchange))
}

// ---------------------------------------------------------------------------
// Encrypted request / response bodies (the `EncryptedRoute` equivalent)
// ---------------------------------------------------------------------------

/// Read the crypto session id from the `X-Session-ID` header (None if absent).
fn session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::HeaderName::from_static(X_SESSION_ID))
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Decrypt an encrypted request body for the session identified by the
/// `X-Session-ID` header. Mirrors `api.get_decrypted_request_body`. The
/// ciphertext is bound to the session via AAD so it cannot be replayed against
/// a different session's key.
pub async fn decrypt_request_body(
    state: &AppState,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let session_id = match session_id_from_headers(headers) {
        Some(s) => s,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                "Invalid or missing session ID".to_string(),
            ))
        }
    };
    let aes_key = {
        let map = state.crypto_sessions.read().await;
        match map.get(&session_id) {
            Some(k) => k.clone(),
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Invalid or missing session ID".to_string(),
                ))
            }
        }
    };
    let payload: EncryptedPayload = serde_json::from_slice(&body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to decrypt request: {e}"),
        )
    })?;
    let iv = base64::engine::general_purpose::STANDARD
        .decode(&payload.iv)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Failed to decrypt request".to_string()))?;
    let ct = base64::engine::general_purpose::STANDARD
        .decode(&payload.ciphertext)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Failed to decrypt request".to_string()))?;
    let cipher = Aes256Gcm::new_from_slice(&aes_key)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Failed to decrypt request".to_string()))?;
    let mut nonce = Nonce::default();
    nonce.copy_from_slice(&iv);
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload { msg: &ct, aad: session_id.as_bytes() },
        )
        .map_err(|_| (StatusCode::BAD_REQUEST, "Failed to decrypt request".to_string()))?;
    serde_json::from_slice(&plaintext).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("Failed to decrypt request: {e}"),
        )
    })
}

/// Encrypt a JSON-serialisable response for the session identified by the
/// `X-Session-ID` header. Mirrors the response half of `api.EncryptedRoute`:
/// if the session is missing/invalid the response is blocked with a 400 so no
/// plaintext ever leaves the server.
pub async fn encrypt_response(
    state: &AppState,
    headers: &HeaderMap,
    status: StatusCode,
    value: serde_json::Value,
) -> Response {
    let session_id = match session_id_from_headers(headers) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "detail": "Secure session required. Encryption key missing or invalid."
                })),
            )
                .into_response()
        }
    };
    let aes_key = {
        let map = state.crypto_sessions.read().await;
        match map.get(&session_id) {
            Some(k) => k.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "detail": "Secure session required. Encryption key missing or invalid."
                    })),
                )
                    .into_response()
            }
        }
    };
    let cipher = match Aes256Gcm::new_from_slice(&aes_key) {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "detail": "Encryption failed server-side." })),
            )
                .into_response()
        }
    };
    let mut iv = [0u8; 12];
    rand::rng().fill_bytes(&mut iv);
    let mut nonce = Nonce::default();
    nonce.copy_from_slice(&iv);
    let plaintext = serde_json::to_vec(&value).unwrap_or_default();
    match cipher.encrypt(
        &nonce,
        Payload { msg: &plaintext, aad: session_id.as_bytes() },
    ) {
        Ok(ct) => {
            let payload = EncryptedPayload {
                iv: base64::engine::general_purpose::STANDARD.encode(iv),
                ciphertext: base64::engine::general_purpose::STANDARD.encode(ct),
            };
            (status, Json(payload)).into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "detail": "Encryption failed server-side." })),
        )
            .into_response(),
    }
}
