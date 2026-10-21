# Vreckan (Rust) — API & Endpoint Reference

Endpoint reference for the Rust port. Extracted from the actual route
registrations in `src/` (this file is the live record — if a route is not here,
it is not served), and cross-checked against the **ANT-5 functionality parity
audit** (2026-10-12), which verified every route against the Python original
(Gitea baseline `e7b2de6`, v0.5.0). The audit's full per-route matrix is the
source of truth for Python↔Rust handler mapping; the [port status
document](./port-status.md) tracks the residual gaps.

Scale: **112 route registrations covering 106 unique paths** (143
method+path routes; several paths serve multiple methods) — all
equivalent paths of the Python original, plus representation-level
differences and additive endpoints (the Rust-only liveness probe and the
share-session links routes).

## How requests are protected

Two orthogonal layers:

1. **Transport:** every response carries the standard security headers
   (CSP, HSTS, `X-Content-Type-Options`, `X-Frame-Options`, Permissions-Policy,
   COEP, COOP, CORP) via a shared middleware. The collaboration-room and
   public-password pages use a per-response nonce CSP; proxied session traffic
   gets the looser session CSP.
2. **Authentication & E2EE:**
    - **Auth routes** (`/api/auth/*`, handshake, public shares, the
      session-share public entry, room, session proxy, static) use
      **plaintext** bodies. The web-login credential is the
      `vreckan_auth` cookie (opaque server-issued token, 8 h TTL by
      default).
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
| ANY | `/api/apps/session/{session_id}/` | `initial_session_auth` | first hit: `?access_token=` (the owner's token) is validated constant-time and exchanged for the `vreckan_session_token_{id}` cookie via a 303 to the same URL with the token stripped (the token is not consumed — replaying it re-sets the cookie); a valid collab or share-visit `?token=` in the same URL additionally sets the `collab_token_{id}` cookie so the page's sub-requests authenticate by cookie |
| ANY | `/api/apps/session/{session_id}/{*path}` | `session_proxy_subpath` | any admitted credential (below) |

Admitted credentials (HTTP and WebSocket paths, both routes): the
owner's session cookie; for collaboration sessions — and for plain
sessions promoted to selkies secure mode by a session share — the
collaboration token (the controller token or one of the viewers'
tokens) presented via the `collab_token_{id}` cookie or a first-hit
`?token=`; and the per-visit tokens minted by session shares (the
vreckan-side token map is the source of truth for tokens the container
cannot know about). Anything else is 403 `Forbidden: Invalid session or
token.` When the container runs in secure mode the proxy presents the
request's own valid token to the upstream (falling back to the owner's
container token for the owner), so each participant is individually
authenticated by the container.

One `any()` registration per route handles both HTTP and WebSocket: an
`Upgrade` header dispatches to the WebSocket bridge (client leg
`axum::ws`, upstream leg `tokio-tungstenite`), everything else to the
streaming HTTP proxy. Raw content-encoded bodies pass through undecoded;
hop-by-hop headers are dropped; `SameSite=None` is set when the session page
is opened embedded; the session CSP is applied to proxied responses.
(Python registered the same surface as three routes — all-methods HTTP root,
all-methods HTTP sub-path, and a separate WebSocket route — the
consolidation is a representation difference, not a behavior change.)

### Session shares — `session_shares.rs`

Owner-managed share links that let a session owner hand one of their
running sessions — plain or room — to a recipient who has no Vreckan
account, without sharing the owner's own credentials. Each share has a
**role** — `full` (input plus every file capability) or `view_only`
(no input; browse and download on by default) — freely combinable file
capabilities (`file_browse`, `file_download`, `file_upload`), an optional
password (argon2id, the file-share verifier), an optional expiry, and
per-link revocation. The owner routes are E2EE-encrypted and owner-only;
the public entry is plaintext and account-less.

Owner management (E2EE + the caller must own the session):

| Method | Path | Handler | Auth |
|---|---|---|---|
| POST | `/api/sessions/{session_id}/shares` | `create_session_share` | E2EE (owner) |
| GET | `/api/sessions/{session_id}/shares` | `list_session_shares` | E2EE (owner) |
| PATCH | `/api/sessions/{session_id}/shares/{share_id}` | `update_session_share` | E2EE (owner) |
| DELETE | `/api/sessions/{session_id}/shares/{share_id}` | `revoke_session_share` | E2EE (owner) |
| GET | `/api/sessions/{session_id}/participants` | `list_session_participants` | E2EE (owner) |
| DELETE | `/api/sessions/{session_id}/participants/{username}` | `revoke_session_participant` | E2EE (owner) |

Account-less public entry (plaintext):

| Method | Path | Handler | Auth |
|---|---|---|---|
| GET | `/share/{share_id}` | `access_session_share_get` | — (303 entry redirect, or the password page) |
| POST | `/share/{share_id}` | `access_session_share_post` | — (password submission) |
| GET | `/join/{session_id}/{username}` | `access_session_join_hop` | — (self-serve join hop, ANT-466 F1: 303 `private, no-store` to the session URL with the token stripped; the caller's participant token is handed over via the `collab_token_{session_id}` cookie, which admits the redirect target) |

The share's wire shape (every owner route takes and returns it,
E2EE-encrypted):

| Field | Type | Notes |
|---|---|---|
| `share_id` | string | UUID; the public link is `/share/{share_id}` |
| `session_id` | string | the shared session |
| `role` | string | `full` \| `view_only` |
| `caps` | object | `{ input, file_browse, file_download, file_upload }`, all bool |
| `created_at` | number | unix seconds |
| `expiry_timestamp` | number \| null | unix seconds; `null` = no expiry |
| `has_password` | bool | indicator only — the hash never leaves the server |
| `revoked` | bool | revoked links stay listed; the public entry answers 410 |
| `url` | string | `/share/{share_id}` |

**`POST /api/sessions/{session_id}/shares`** — create. Request body:

| Field | Type | Required | Notes |
|---|---|---|---|
| `role` | string | yes | `full` \| `view_only`; anything else → 422 |
| `caps` | object | no | partial override `{input?, file_browse?, file_download?, file_upload?}`; absent fields take the role's defaults |
| `password` | string | no | non-empty sets the password (argon2id); absent/blank = none |
| `expiry_hours` | number | no | must be `> 0`; sets `expiry_timestamp = now + hours·3600` |

Role invariants: `full` ⇒ every cap on; `view_only` ⇒ `input` off, file
caps freely combinable. Response 200: the new share. Errors: 404 unknown
session; 403 not the owner's session; 422 invalid body or role.

**`GET /api/sessions/{session_id}/shares`** — response 200: a JSON array
of the session's shares (newest first), same shape as above.

**`PATCH /api/sessions/{session_id}/shares/{share_id}`** — edit. Every
body field is optional, and **an absent field leaves the share
unchanged while an explicit `null` clears** (deserialized as a nested
`Option<Option<…>>` so the two are told apart; `""` also clears the
password):

| Field | Type | Semantics |
|---|---|---|
| `role` | string \| null | `full` \| `view_only`. Switching to `full` turns every cap on; switching to `view_only` removes only `input` — the file caps stay as configured |
| `caps` | object | partial override, as at create |
| `password` | string \| null | non-empty sets it; `""` or `null` clears it |
| `expiry_hours` | number \| null | positive sets a new expiry from now; `null` clears it |

Response 200: the updated share. Errors: 404 unknown share (or a share
belonging to a different session); 422 invalid role.

**`DELETE /api/sessions/{session_id}/shares/{share_id}`** — revoke.
Response 204, empty body. Revocation marks the link dead (the public
entry answers 410) and strips **every per-visit token the link has ever
minted** — from the session's `viewers` list, the container's selkies
token table, and the vreckan-side token map — without stopping the
session. Since v0.8.5 the strip proceeds when the durable share row is
missing, which is how the synthetic join grant (`join:{username}`, never
a row in `session_shares`) reaches the same cleanup path.

**Participants (v0.8.5, ANT-467).** Every admitted visitor is recorded in
the session's `viewers` list with how it got in; the two participant
routes expose that to the owner.

**`GET /api/sessions/{session_id}/participants`** — response 200: a JSON
array, one entry per recorded visit (self-serve joins and share-link
visits alike, in viewers order):

| Field | Type | Notes |
|---|---|---|
| `username` | string \| null | the joiner's real username for joins; the random per-visit `User-NNN` for share visits |
| `permission` | string \| null | `participant` \| `readonly` |
| `source` | string | `join` (the Join action) \| `share` (a share-link visit) |
| `share_id` | string \| null | the minting grant's id — `join:{username}` or the share's UUID; `null` for entries persisted before v0.8.5 (classification then falls back to the username shape) |
| `joined_at` | number \| null | unix seconds; absent before v0.8.5 |
| `expiry` | number \| null | unix seconds; joins carry the 24-hour join TTL, share visits inherit the link's expiry, `null` = no expiry |

Errors: 401 unauthenticated (plaintext, as with the other owner routes);
404 unknown session; 403 not the owner's session.

**`DELETE /api/sessions/{session_id}/participants/{username}`** — revoke a
single joiner: strips the joiner's token from the session's `viewers`
list, the container's token table, and the vreckan-side token map, then
persists the session. The joiner drops on the next proxy request; the
session and everyone else's grants are untouched, and a later Join mints
a fresh grant. Response 204 on success; 404 when that user holds no live
join grant (never joined, already revoked, or the grant's TTL ran out).

Join grants carry a 24-hour TTL (`JOIN_TTL_HOURS`, fixed in v0.8.5) from
the mint and from each (re-)join after a lapse — an expired entry is
refreshed in place (same token, new expiry) rather than re-minted, and
entries persisted before v0.8.5 pick the TTL up on their first
post-upgrade (re-)join. The background sweeper
(`cleanup_expired_session_shares`) strips expired join entries and prunes
the in-memory token map of every expired record. The session list
(`GET /api/sessions`) carries `participants` — the joiners' usernames —
for the caller's own sessions only, so the UI can badge the count
without a round trip.

**Public entry.** `GET /share/{share_id}`: 404 `Share not found.`; 410
when revoked, expired, or the session has ended (plain-page message in
each case); the public password page (200) when a password is set;
otherwise a 303 redirect. `POST /share/{share_id}` (form field
`password`): 303 on a correct password; the password page re-rendered
with 401 on a wrong one; 400 when the share has no password. The 303
target:

| Session kind | Redirect |
|---|---|
| Room | `/room/{session_id}?token={visit_token}` |
| Plain, promoted to secure mode | `/api/apps/session/{session_id}/?token={visit_token}` |
| Plain, legacy container (no selkies token control plane), input on | same URL — the legacy default role is controller |
| Plain, legacy container, input off | same URL plus `&role=viewer` |

Each visit mints a fresh per-visit token (128-bit random) and records
the visitor in the session's `viewers` list; the session proxy admits
the token on every subsequent request and (WP3) gates the in-container
file API by the share's caps. A plain session is **lazily promoted** to
selkies secure mode on its first share visit: a `master_token` is minted
and the full token table is pushed to the container's
`/{session_id}/api/tokens`; when that endpoint answers 404/401 or is
unreachable (older selkies images without the control plane) the session
stays legacy and the role rides the `?role=` query parameter instead.
A background sweeper (started in `main.rs`,
`background_share_cleanup_job` in `files.rs`) removes expired shares on
the `share_cleanup_interval_seconds` schedule and strips their tokens
(port-status gap G2, share-sweeper portion; the gap stays open for the
crypto-session sweeper).

**v0.8.1 hardening (F5/F6).** The 303 answers from both public-entry
routes carry `Cache-Control: private, no-store`, so a caching proxy
cannot serve one visitor's per-visit token to another (F5,
`src/session_shares.rs`). And the no-slash `api/files` root — which the
container 301s to the slash-terminated listing — is on the browse side
of the file-cap gate: a share visitor without the file-browse cap gets
the browse 403 there instead of the previously un-gated 301 (F6,
`src/session_proxy.rs`).

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
> the `/api/homedirs` / `/api/pinned` nest prefixes. Axum 0.8 collapses a
> nested `"/"` route onto the prefix, so the live paths are the **no-slash**
> forms — `/api/homedirs` and `/api/pinned` — the same forms the Python
> original served. The trailing-slash forms 404. (The in-tree client and the
> test suite both use the no-slash forms.) See gap **G7** in
> [port status](./port-status.md).

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

48 registrations, 63 method+path routes (the parity-audit's 61, plus the
two auto-update endpoints added in v0.7.0).

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
| PUT | `/api/admin/apps/auto_update` | `save_auto_update_settings` |
| GET | `/api/admin/apps/auto_update_status` | `get_auto_update_status` |
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
| `values.acme_ca` | string | `letsencrypt` (default) \| `zerossl` \| `google` \| full custom ACME directory URL |
| `values.acme_allow_http_ca` | bool | allow a plaintext `http://` custom CA URL (ANT-275) |
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
| `values.acme_dns_provider_config` | string | always `""` (masked — the raw JSON carries the provider's `api_token`/`client_secret`, ANT-275) |
| `values.dns_provider_config_set` | bool | whether a provider config is stored |
| `values.dns_provider_config_length` | int | length of the stored provider config |
| `sources.*` | string | per-field origin: `db` \| `env` \| `default` |
| `status.present` | bool | a certificate exists |
| `status.source` | string \| null | `acme_db` (DB is the source of truth) \| `host` \| `app` (disk fallback) |
| `status.cert` | string \| null | PEM chain (DB) or path (disk) |
| `status.key` | string \| null | elided to a marker when `source` is `acme_db` (the PEM stays out of the admin's browser, ANT-275); path for disk fallbacks |
| `status.key_length` | int \| null | length of the stored private-key PEM, when elided |
| `status.expiry` | string \| null | RFC 3339 expiry, when parseable |
| `status.days_remaining` | int \| null | days until expiry |
| `status.valid` | bool | expiry is in the future |
| `dns_providers` | object[] | `[{ id, label, default }]` for all 11 providers |

**`PUT /api/admin/certificates`** — save the ACME configuration to the
`app_settings` table. Request body carries the same `acme_*` fields as the
GET response's `values` (minus the masked indicators). A blank
`acme_cloudflare_token` or `acme_dns_provider_config` means "leave the
existing value unchanged" (both are masked to blank in the GET response).
Response: `{ "ok": true }`.

**`POST /api/admin/certificates/renew`** — issue or renew the certificate
now. The "Issue / Renew Now" button always (re)issues, even when a valid
certificate already exists. Response:

| Field | Type | Notes |
|---|---|---|
| `present` | bool | a certificate exists after the call |
| `source` | string \| null | `acme_db` \| `host` \| `app` |
| `cert` | string \| null | PEM chain or path |
| `key` | string \| null | elided to a marker when DB-sourced (ANT-275); path for disk fallbacks |
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
  `.route()` calls at audit time; 112 in the current code) while the
  method+path surface is identical.
- **WebSocket handling differs by route.** The session-proxy routes are
  `any()` and dispatch on the `Upgrade` header in-handler; the room WS route
  (`/ws/room/{session_id}`) is a plain `GET` upgrade.
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
