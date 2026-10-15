# Vreckan (Rust) — API & Endpoint Reference

Endpoint reference for the Rust port. Extracted from the actual route
registrations in `src/` (this file is the live record — if a route is not here,
it is not served), and cross-checked against the **ANT-5 functionality parity
audit** (2026-10-12), which verified every route against the Python original
(Gitea baseline `e7b2de6`, v0.5.0). The audit's full per-route matrix is the
source of truth for Python↔Rust handler mapping; the [port status
document](./port-status.md) tracks the residual gaps.

Scale: **104 route registrations covering 103 unique paths** — all 102
equivalent paths of the Python original, plus representation-level
differences and one additive endpoint.

## How requests are protected

Two orthogonal layers:

1. **Transport:** every response carries the standard security headers
   (CSP, HSTS, `X-Content-Type-Options`, `X-Frame-Options`, Permissions-Policy,
   COEP, COOP, CORP) via a shared middleware. The collaboration-room and
   public-password pages use a per-response nonce CSP; proxied session traffic
   gets the looser session CSP.
2. **Authentication & E2EE:**
   - **Auth routes** (`/api/auth/*`, handshake, public shares, room, session
     proxy, static) use **plaintext** bodies. The web-login credential is the
     `vreckan_auth` cookie (opaque server-issued token, 8 h TTL by default).
   - **E2EE routes** (catalog, launch, sessions, files, upload, homedirs,
     pinned, and all of `/api/admin/*`) require the same token and
     **AES-256-GCM-encrypted request/response bodies**. The client first runs
     the handshake (`/api/handshake/*`) to establish the per-session crypto
     key (RSA-PSS-signed nonce → RSA-OAEP key wrap → AES-GCM; the AAD binds
     the key to the session id, and `X-Session-ID` carries it).
   - **Admin routes** additionally enforce per-permission RBAC (18-permission
     catalog; the `admin` super-permission implies all). Auth-failure
     responses are deliberately plaintext in both ports (401 bypasses the
     E2EE wrapper).

| Mark | Meaning |
|---|---|
| `E2EE` | Body must be E2EE-encrypted; token required |
| `admin` | E2EE + one or more admin permissions |
| `—` | Plaintext; no auth (public) or cookie/session auth only |

## Route groups

### Root / static / health — `app.rs`

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/` | `index` | — (serves the web client, cache-busted) |
| GET | `/api/health` | `health` | — (Rust-only liveness probe: `ok (N session(s))`) |
| ANY | `/{*path}` | `serve_static` | — (catch-all static handler, registered last; path-traversal guarded; JS/CSS `no-store`, rest `no-cache`) |

### E2EE handshake — `e2ee.rs` (plaintext — it establishes the crypto session)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/api/handshake/public_key` | `handshake_public_key` | — |
| POST | `/api/handshake/initiate` | `handshake_initiate` | — (RSA-PSS-signed nonce) |
| POST | `/api/handshake/exchange` | `handshake_exchange` | — (RSA-OAEP wrap of the AES session key) |

### Auth / SSO — `auth.rs` (plaintext bodies)

| Method | Path | Handler | Auth |
|---|---|---|---|
| POST | `/api/auth/login` | `login` | — (sets `vreckan_auth`) |
| POST | `/api/auth/logout` | `logout` | cookie |
| POST | `/api/auth/revoke-sessions` | `revoke_sessions` | cookie |
| GET | `/api/auth/me` | `me` | cookie |
| POST | `/api/auth/change_password` | `change_password` | cookie |
| POST | `/api/auth/set_password` | `set_password` | cookie |
| PUT | `/api/auth/preferences` | `preferences` | cookie |
| GET | `/api/auth/oidc/status` | `oidc_status` | — |
| GET | `/api/auth/oidc/authorize` | `oidc_authorize` | — (302 to IdP) |
| GET | `/api/auth/oidc/callback` | `oidc_callback` | — (state CSRF check, JWKS id_token verify, auto-provisioning + group re-stamp) |

### Catalog / launch / own sessions — `catalog.rs` (E2EE)

| Method | Path | Handler | Auth |
|---|---|---|---|
| POST | `/api/applications` | `get_applications` | E2EE |
| POST | `/api/launch/simple` | `launch_simple` | E2EE |
| POST | `/api/launch/url` | `launch_url` | E2EE |
| POST | `/api/launch/file` | `launch_file` | E2EE |
| POST | `/api/launch/file_path` | `launch_file_path` | E2EE |
| POST | `/api/admin/status` | `admin_status` | E2EE |
| GET | `/api/app_icon/{app_id}` | `get_app_icon` | E2EE |
| GET | `/api/sessions` | `get_my_sessions` | E2EE |
| GET | `/api/sessions/{session_id}/status` | `get_session_status` | E2EE (own session) |
| DELETE | `/api/sessions/{session_id}` | `stop_my_session` | E2EE (own session) |
| POST | `/api/sessions/{session_id}/recreate` | `recreate_my_session` | E2EE (own session) |
| POST | `/api/sessions/{session_id}/send_file` | `send_file_to_session` | E2EE (own session) |

### Session reverse proxy (data plane) — `session_proxy.rs`

| Method | Path | Handler | Auth |
|---|---|---|---|
| ANY | `/api/apps/session/{session_id}/` | `initial_session_auth` | first hit: exchange the single-use `access_token` for a session cookie; then session cookie |
| ANY | `/api/apps/session/{session_id}/{*path}` | `session_proxy_subpath` | session cookie |

One `any()` registration per route handles both HTTP and WebSocket: an
`Upgrade` header dispatches to the WebSocket bridge (client leg
`axum::ws`, upstream leg `tokio-tungstenite`), everything else to the
streaming HTTP proxy. Raw content-encoded bodies pass through undecoded;
hop-by-hop headers are dropped; `SameSite=None` is set when the session page
is opened embedded; the session CSP is applied to proxied responses.
(Python registered the same surface as three routes — all-methods HTTP root,
all-methods HTTP sub-path, and a separate WebSocket route — the
consolidation is a representation difference, not a behavior change.)

### File management — `files.rs` (E2EE)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/api/files/list/{home_dir}` | `list_files` | E2EE |
| GET | `/api/files/download/chunk/{home_dir}` | `download_file_chunk` | E2EE |
| POST | `/api/files/create_folder/{home_dir}` | `create_folder` | E2EE |
| POST | `/api/files/delete/{home_dir}` | `initiate_deletion` | E2EE |
| GET | `/api/files/delete_status/{task_id}` | `check_deletion_status` | E2EE |
| POST | `/api/files/upload_to_dir/{home_dir}` | `finalize_upload_to_dir` | E2EE |
| POST | `/api/files/launch_from_storage` | `launch_from_storage` | E2EE |
| POST | `/api/files/share` | `create_public_share` | E2EE |
| GET | `/api/files/shares` | `list_public_shares` | E2EE |
| DELETE | `/api/files/share/{share_id}` | `delete_public_share` | E2EE (owner) |

### Uploads — `files.rs` (E2EE)

| Method | Path | Handler | Auth |
|---|---|---|---|
| POST | `/api/upload/initiate` | `upload_initiate` | E2EE |
| POST | `/api/upload/chunk` | `upload_chunk` | E2EE |
| POST | `/api/upload/to_storage` | `upload_to_storage` | E2EE |

### Home directories — `files.rs` (E2EE)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/api/homedirs/` | `list_my_home_dirs` | E2EE |
| POST | `/api/homedirs/` | `create_my_home_dir` | E2EE |
| DELETE | `/api/homedirs/{home_name}` | `delete_my_home_dir` | E2EE |

> Trailing-slash note: these collection routes are registered as `"/"` under
> the `/api/homedirs` / `/api/pinned` nest prefixes, so they serve the prefix
> roots. The Python original served the same endpoints *without* the trailing
> slash — a strict client that calls one form against the other port gets
> 404/405. See gap **G7** in [port status](./port-status.md).

### Pinned behaviors — `files.rs` (E2EE)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/api/pinned/` | `list_pinned_behaviors` | E2EE |
| POST | `/api/pinned/` | `create_pinned_behavior` | E2EE |
| POST | `/api/pinned/{pinned_id}/set-default` | `set_default_pinned_behavior` | E2EE |
| DELETE | `/api/pinned/{pinned_id}` | `delete_pinned_behavior` | E2EE |

### Public shares — `files.rs` (plaintext)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/public/download/{token}` | `download_shared_file` | — (single-use token) |
| GET | `/public/{share_id}` | `access_public_share_get` | — (password page where applicable) |
| POST | `/public/{share_id}` | `access_public_share_post` | — |

### Admin — `admin.rs` (E2EE + per-permission RBAC)

46 registrations, 61 method+path routes (matches the Python admin router's
61 exactly).

| Method | Path | Handler |
|---|---|---|
| POST | `/api/admin/data` | `get_management_data` |
| GET | `/api/admin/global-settings` | `get_global_settings` |
| POST | `/api/admin/global-settings` | `save_global_settings` |
| GET | `/api/admin/sso-groups` | `get_sso_groups` |
| GET | `/api/admin/sso` | `get_sso_config` |
| PUT | `/api/admin/sso` | `save_sso_config` |
| POST | `/api/admin/sso/test` | `test_sso_connection` |
| GET | `/api/admin/certificates` | `get_certificates` |
| PUT | `/api/admin/certificates` | `save_certificates` |
| POST | `/api/admin/certificates/renew` | `renew_certificates` |
| GET | `/api/admin/apps/stores` | `get_app_stores` |
| POST | `/api/admin/apps/stores` | `add_app_store` |
| DELETE | `/api/admin/apps/stores/{store_name}` | `delete_app_store` |
| POST | `/api/admin/apps/meta` | `create_meta_app` |
| POST | `/api/admin/launch/meta_customize` | `launch_meta_for_customization` |
| GET | `/api/admin/apps/available` | `get_available_apps` |
| GET | `/api/admin/apps/installed` | `list_installed_apps` |
| POST | `/api/admin/apps/installed` | `install_app` |
| PUT | `/api/admin/apps/installed/{app_id}` | `update_installed_app` |
| DELETE | `/api/admin/apps/installed/{app_id}` | `delete_installed_app` |
| POST | `/api/admin/apps/installed/{app_id}/check_update` | `check_app_update` |
| POST | `/api/admin/apps/installed/check_all_updates` | `check_all_app_updates` |
| POST | `/api/admin/apps/installed/pull_all_latest` | `pull_all_app_images` |
| POST | `/api/admin/apps/installed/{app_id}/pull_latest` | `pull_latest_app_image` |
| GET | `/api/admin/apps/templates` | `get_app_templates` |
| POST | `/api/admin/apps/templates` | `save_app_template` |
| DELETE | `/api/admin/apps/templates/{template_name}` | `delete_app_template` |
| GET | `/api/admin/apps/templates/schema` | `get_template_schema` |
| POST | `/api/admin/apps/templates/schema` | `save_template_schema` |
| GET | `/api/admin/sessions` | `get_all_sessions` |
| DELETE | `/api/admin/sessions/{session_id}` | `stop_any_session` |
| POST | `/api/admin/sessions/{session_id}/recreate` | `recreate_any_session` |
| POST | `/api/admin/oidc/revoke` | `revoke_oidc_sessions` |
| POST | `/api/admin/people` | `create_person` |
| PUT | `/api/admin/people/{username}` | `update_person` |
| DELETE | `/api/admin/people/{username}` | `delete_person` |
| POST | `/api/admin/people/{username}/admin-status` | `set_admin_status` |
| GET | `/api/admin/people/{username}/homedirs` | `list_person_home_dirs` |
| POST | `/api/admin/people/{username}/homedirs` | `create_person_home_dir` |
| DELETE | `/api/admin/people/{username}/homedirs/{home_name}` | `delete_person_home_dir` |
| POST | `/api/admin/people/{username}/access` | `set_user_access` |
| POST | `/api/admin/groups` | `create_group` |
| PUT | `/api/admin/groups/{group_name}` | `update_group` |
| DELETE | `/api/admin/groups/{group_name}` | `delete_group` |
| GET | `/api/admin/volume_mounts` | `list_volume_mounts` |
| POST | `/api/admin/volume_mounts` | `create_volume_mount` |
| PUT | `/api/admin/volume_mounts/{name}` | `update_volume_mount` |
| DELETE | `/api/admin/volume_mounts/{name}` | `delete_volume_mount` |
| GET | `/api/admin/permissions` | `list_permissions` |
| GET | `/api/admin/roles` | `list_roles` |
| POST | `/api/admin/roles` | `create_role` |
| PUT | `/api/admin/roles/{role_name}` | `update_role` |
| DELETE | `/api/admin/roles/{role_name}` | `delete_role` |
| POST | `/api/admin/backup/create` | `create_backup` |
| GET | `/api/admin/backup` | `list_backups` |
| POST | `/api/admin/backup/restore` | `restore_backup` |
| POST | `/api/admin/backup/restore/initiate` | `backup_restore_initiate` |
| POST | `/api/admin/backup/restore/chunk` | `backup_restore_chunk` |
| POST | `/api/admin/backup/restore/finalize` | `backup_restore_finalize` |
| GET | `/api/admin/backup/{name}/download` | `download_backup` |
| DELETE | `/api/admin/backup/{name}` | `delete_backup` |

### Certificate fields — `admin.rs`

The three certificate endpoints exchange the following fields (all
`acme_*`; the legacy `le_*` names are retired).

**`GET /api/admin/certificates`** — effective config (DB-over-env-over-
default), current certificate status, and the available DNS providers:

| Field | Type | Notes |
|---|---|---|
| `values.acme_enabled` | bool | ACME on/off |
| `values.acme_ca` | string | `letsencrypt` (default) \| `zerossl` \| `google` |
| `values.acme_domains` | string[] | target domains |
| `values.acme_auth` | string | `dns-cloudflare` (default) \| `http-01` |
| `values.acme_cloudflare_email` | string | ACME account contact |
| `values.acme_cloudflare_token` | string | always `""` (masked) |
| `values.token_set` | bool | whether a token is stored |
| `values.token_length` | int | length of the stored token |
| `values.acme_staging` | bool | use the LE staging endpoint |
| `values.acme_auto_renew` | bool | background renewal on/off |
| `values.acme_renew_check_hours` | int | renewal poll interval (h) |
| `values.acme_renew_before_days` | int | renew-within window (days) |
| `values.acme_dns_provider` | string | `cloudflare` (default) \| `route53` \| `digitalocean` \| `linode` \| `azure` \| `google` \| `alibaba` \| `godaddy` \| `tencent` \| `huawei` \| `cloudns` |
| `values.acme_cloudflare_zone_id` | string | optional Cloudflare zone pin |
| `values.acme_dns_provider_config` | string | JSON credentials for non-Cloudflare providers |
| `sources.*` | string | per-field origin: `db` \| `env` \| `default` |
| `status.present` | bool | a certificate exists |
| `status.source` | string \| null | `acme_db` (DB is the source of truth) \| `host` \| `app` (disk fallback) |
| `status.cert` | string \| null | PEM chain (DB) or path (disk) |
| `status.key` | string \| null | private-key PEM (DB) or path (disk) |
| `status.expiry` | string \| null | RFC 3339 expiry, when parseable |
| `status.days_remaining` | int \| null | days until expiry |
| `status.valid` | bool | expiry is in the future |
| `dns_providers` | object[] | `[{ id, label, default }]` for all 11 providers |

**`PUT /api/admin/certificates`** — save the ACME configuration to the
`app_settings` table. Request body carries the same `acme_*` fields as the
GET response's `values` (minus `token_set`/`token_length`). A blank
`acme_cloudflare_token` means "leave the existing token unchanged".
Response: `{ "ok": true }`.

**`POST /api/admin/certificates/renew`** — issue or renew the certificate
now. The "Issue / Renew Now" button always (re)issues, even when a valid
certificate already exists. Response:

| Field | Type | Notes |
|---|---|---|
| `present` | bool | a certificate exists after the call |
| `source` | string \| null | `acme_db` \| `host` \| `app` |
| `cert` | string \| null | PEM chain or path |
| `key` | string \| null | private-key PEM or path |
| `expiry` | string \| null | RFC 3339 expiry |
| `days_remaining` | int \| null | days until expiry |
| `valid` | bool | certificate is currently valid |
| `issued` | bool | a new certificate was issued during this call |
| `renewed` | bool | same as `issued` (alias for the UI) |
| `enabled` | bool | ACME was enabled |
| `logs` | string | human-readable acmex milestone log (when present) |
| `error` | string | human-readable error (when present) |

### Collaboration room — `collaboration.rs` (plaintext)

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/room/{session_id}` | `collaborative_room` | session cookie (nonce CSP page) |
| GET | `/room/room.css` | `get_room_css` | — |
| GET | `/room/room.js` | `get_room_js` | — |
| GET | `/room/translation.js` | `get_translation_js` | — |
| GET | `/ws/room/{session_id}` | `room_websocket` | session cookie (WS) |

## Representation differences vs the Python original

Verified in the ANT-5 audit; none change functionality except where a gap
is linked:

- **Multi-method routes collapse.** axum registers `get(...).post(...)` on one
  path where Python used separate decorators per method — this is why the
  raw registration counts differ (119 Python route objects vs 104 Rust
  `.route()` calls) while the method+path surface is identical.
- **WebSocket routes are `any()`.** The session-proxy and room WS routes
  dispatch on the `Upgrade` header in-handler.
- **Trailing slash** on the `/api/homedirs` / `/api/pinned` collection roots
  (gap **G7**).
- **UUID path params.** Python validated `{session_id:uuid}` at routing
  (malformed → 422); Rust matches any string and the handler fails later
  (gap **G8**).
- **Static catch-all is `any()`** in Rust, `GET` in Python — a superset;
  non-GET to unknown paths reaches the static handler instead of 405.
- **`GET /api/health`** exists only in Rust (additive liveness probe, gap
  **G10**).
- **Session ids** on `/room/{session_id}` and `/ws/room/{session_id}` are
  unvalidated strings in Rust (same as G8).
