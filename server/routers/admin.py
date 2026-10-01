"""Admin management endpoints: roster data, global settings, app stores,
meta apps, app installation/updates, templates, session oversight, OIDC
revocation, people/groups, volume mounts, and backup/restore.

Extracted from ``app.api`` during the router split. Shared module state and
helpers are accessed through the ``api`` module (``api.INSTALLED_APPS``,
``api.PULL_STATUS``, ``api._do_pull_and_cache_image``, ``api.logger``, ...)
rather than imported by name, so test fixtures that mutate ``app.api`` in
place keep working unchanged. Only the auth dependencies and
``EncryptedRoute`` are resolved at import time (inside ``Depends(...)`` /
the router constructor); everything else is looked up on ``api`` at call
time. See ``session.py`` for the full rationale.

Note: ``save_global_settings`` assigns ``api.GLOBAL_DEFAULT_GPU`` directly
(the original used a ``global`` statement) so the reassignment lands on the
``app.api`` module attribute that the rest of the server reads.
"""
import asyncio
import base64
import os
import re
import shutil
import uuid
from collections import defaultdict
from typing import List

import httpx2
import yaml
from fastapi import APIRouter, Depends, HTTPException, Request, Response
from pydantic import BaseModel, ValidationError
from sqlalchemy import delete, select

from server import api, user_manager
from server import backup_manager, db, volume_mount_manager
from server.routers.auth import _oidc_config, _oidc_discovery, _oidc_redirect_uri
from server.models import (
    ActiveSessionInfo,
    AppStore,
    AppTemplate,
    AvailableApp,
    BackupInfo,
    BackupRestoreChunkRequest,
    BackupRestoreFinalizeRequest,
    BackupRestoreInitiateResponse,
    CreateBackupRequest,
    CreateGroupRequest,
    CreateMetaAppRequest,
    CreatePersonRequest,
    CreateUserResponse,
    CreateRoleRequest,
    CreateVolumeMountRequest,
    AppImagePullResult,
    AppUpdateCheckResult,
    CheckAllUpdatesResponse,
    GPUInfo,
    Group,
    HomeDirectoryCreate,
    HomeDirectoryList,
    ImagePullResponse,
    ImageUpdateCheckResponse,
    PullAllImagesResponse,
    InstalledApp,
    InstalledAppWithStatus,
    LaunchMetaCustomizeRequest,
    LaunchResponse,
    ManagementDataResponse,
    RevokeOidcSessionRequest,
    RevokeOidcSessionResponse,
    RestoreResult,
    Role,
    SessionRecreateResponse,
    SetAdminStatusRequest,
    SetUserAccessRequest,
    UpdateGroupRequest,
    UpdateRoleRequest,
    UpdateUserRequest,
    UpdateVolumeMountRequest,
    User,
    UserSessionList,
    UserSettings,
    VolumeMount,
)
from server.providers.docker_provider import DockerProvider
from server.settings import settings, setting_source

# The admin router no longer applies a single router-level ``verify_admin``
# gate. Instead each endpoint declares the specific permission it requires
# (via ``api.require_permission``), so a user who holds, say, only
# ``admin.apps`` can manage apps but not accounts. The frontend mirrors this
# by only showing the admin sections the caller has permission for.
admin_router = APIRouter(
    prefix="/api/admin",
    route_class=api.EncryptedRoute,
)

# Human-readable descriptions for the permission catalog, surfaced by
# GET /api/admin/permissions so the role editor can label its checkboxes.
_PERMISSION_DESCRIPTIONS = {
    "admin": "Super-permission: grants every other permission.",
    "admin.accounts": "Manage accounts (create, edit, delete, passwords, access).",
    "admin.groups": "Manage groups and SSO group configuration.",
    "admin.roles": "Manage roles and the permission catalog.",
    "admin.apps": "Manage installed apps (install, update, delete, launch).",
    "admin.stores": "Manage app stores and browse available apps.",
    "admin.templates": "Manage app templates and the template schema.",
    "admin.mounts": "Manage external volume mounts.",
    "admin.backup": "Manage backups (create, restore, schedule).",
    "admin.sessions": "View and stop all sessions; revoke SSO sessions.",
    "admin.laboratory": "Use the laboratory (live session console).",
    "admin.global_settings": "Edit the global server settings.",
    "admin.sso": "Configure SSO / OIDC (provider connection and group mapping).",
    "admin.certificates": "Manage TLS certificates (Let's Encrypt issuance and renewal).",
    "user.storage": "Use persistent storage (home directories, uploads).",
    "user.sharing": "Create public file shares.",
    "user.gpu": "Use GPUs when launching apps.",
    "user.harden_container": "Harden app containers.",
    "user.harden_openbox": "Harden the Openbox desktop.",
}


def _has_any_admin_permission(user: dict) -> bool:
    """True if the user holds at least one ``admin.*`` permission.

    Used to gate the aggregate ``/data`` endpoint, which returns the whole
    management payload (roster, groups, roles, mounts, ...). Any user who can
    see at least one admin section needs this data to render it, so the gate
    is "any admin permission" rather than a specific one. (The ``admin``
    super-permission expands to every ``admin.*``, so full admins always pass.)
    """
    perms = user.get("permissions") or []
    return any(p.startswith("admin.") or p == "admin" for p in perms)


@admin_router.post(
    "/data",
    response_model=ManagementDataResponse,
    dependencies=[Depends(api.verify_token)],
)
async def get_management_data(user: dict = Depends(api.verify_token)):
    if not _has_any_admin_permission(user):
        raise HTTPException(status_code=403, detail="Admin privileges required.")

    def _roster_entry(u: dict) -> User:
        # The raw USER_DATA dict carries the argon2 password_hash; expose only
        # a has_password flag (never the hash) plus the SSO marker. Fill any
        # missing settings keys with the defaults so the roster form always
        # receives a complete settings object to display and edit (accounts
        # created before the unified settings model may have NULL/partial
        # settings).
        raw_settings = u.get("settings")
        filled = dict(user_manager.DEFAULT_USER_SETTINGS)
        if raw_settings:
            filled.update(raw_settings)
        return User(
            username=u["username"],
            is_admin=u["is_admin"],
            settings=UserSettings(**filled),
            is_sso=bool(u.get("is_sso")),
            has_password=bool(u.get("password_hash")),
            sso_groups=u.get("sso_groups") or [],
            roles=u.get("roles") or [],
            permissions=u.get("permissions") or [],
            groups=u.get("groups") or [],
        )

    return {
        "admins": [_roster_entry(u) for u in user_manager.get_all_admins()],
        "users": [_roster_entry(u) for u in user_manager.get_all_users()],
        "groups": user_manager.get_all_groups(),
        "roles": user_manager.get_all_roles(),
        "volume_mounts": volume_mount_manager.get_all_volume_mounts(),
        "api_port": api.DISCOVERED_API_PORT,
        "session_port": api.DISCOVERED_SESSION_PORT,
        "gpus": [
            GPUInfo(device=gpu["device"], driver=gpu["driver"])
            for gpu in api.AVAILABLE_GPUS
        ],
    }


# Sentinel: "global_default_gpu" absent from the request body means "leave
# the GPU setting unchanged" (an empty string, by contrast, clears it).
_GLOBAL_SETTINGS_UNCHANGED = object()


@admin_router.get(
    "/global-settings",
    dependencies=[Depends(api.require_permission("admin.global_settings"))],
)
async def get_global_settings():
    """The truly global (non-template) settings: the global default GPU and
    the list of available GPUs. The template schema (the base-layer default
    for every setting) is managed separately on the Templates page via
    ``GET/POST /api/admin/apps/templates/schema``. The SSO admin/user group
    lists are read-only (sourced from the environment) and served by
    ``GET /api/admin/sso-groups``.
    """
    return {
        "global_default_gpu": api.GLOBAL_DEFAULT_GPU,
        "gpus": api.AVAILABLE_GPUS,
    }


@admin_router.post(
    "/global-settings",
    dependencies=[Depends(api.require_permission("admin.global_settings"))],
)
async def save_global_settings(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    """Save the global (non-template) settings.

    Body: {"global_default_gpu": "<device>" | null}

    The key is optional; an absent key leaves the setting unchanged.
    "global_default_gpu" (empty string clears it) sets the global default GPU.
    The SSO admin/user group lists are read-only (sourced from the
    environment) and cannot be changed here. The template schema (the
    base-layer defaults) are saved separately via
    ``POST /api/admin/apps/templates/schema``.
    """
    body = decrypted_body or {}

    gpu_value = body.get("global_default_gpu", _GLOBAL_SETTINGS_UNCHANGED)
    if gpu_value is not _GLOBAL_SETTINGS_UNCHANGED:
        device = str(gpu_value or "").strip()
        if device and device not in {gpu["device"] for gpu in api.AVAILABLE_GPUS}:
            raise HTTPException(status_code=400, detail=f"Unknown GPU device: '{device}'.")
        async with db.async_session_factory() as session:
            async with session.begin():
                row = (
                    await session.execute(
                        select(db.AppSetting).where(db.AppSetting.key == "global_default_gpu")
                    )
                ).scalar_one_or_none()
                if device:
                    if row is None:
                        session.add(db.AppSetting(key="global_default_gpu", value=device))
                    else:
                        row.value = device
                elif row is not None:
                    await session.delete(row)
        api.GLOBAL_DEFAULT_GPU = device or None
        api.logger.info(f"Global default GPU {'set to ' + device if device else 'cleared'}.")
    return {"ok": True}


@admin_router.get(
    "/sso-groups", dependencies=[Depends(api.require_permission("admin.groups"))]
)
async def get_sso_groups():
    """The SSO group configuration (read-only) plus the groups SSO users are
    actually in. The admin/user group lists come from the environment
    (VRECKAN_OIDC_ADMIN_GROUPS / VRECKAN_OIDC_USER_GROUPS); the observed list
    is derived from the sso_groups recorded on each SSO account.
    """
    cfg = await _oidc_config()
    return {
        "admin_groups": [t for t in str(cfg["admin_groups"]).replace(",", " ").split() if t],
        "user_groups": [t for t in str(cfg["user_groups"]).replace(",", " ").split() if t],
        "observed": user_manager.observed_sso_groups(),
    }


# --- SSO / OIDC configuration (editable from the UI) -----------------------
# The SSO page edits the same VRECKAN_OIDC_* values the environment provides,
# but stores them in the app_settings table where they override the env. The
# per-field ``source`` ("db"/"env"/"default") tells the UI where each value
# currently comes from.
SSO_SETTING_FIELDS = [
    "oidc_enabled",
    "oidc_issuer",
    "oidc_client_id",
    "oidc_client_secret",
    "oidc_redirect_uri",
    "oidc_scopes",
    "oidc_allow_signup",
    "oidc_state_ttl_seconds",
    "oidc_admin_groups",
    "oidc_user_groups",
]


class SsoConfigRequest(BaseModel):
    """The SSO page's save payload. Group lists are sent as arrays (the UI
    uses tag inputs); they are stored comma-joined, matching the env format."""

    oidc_enabled: bool
    oidc_issuer: str = ""
    oidc_client_id: str = ""
    oidc_client_secret: str = ""
    oidc_redirect_uri: str = ""
    oidc_scopes: str = "openid email profile"
    oidc_allow_signup: bool = True
    oidc_state_ttl_seconds: int = 600
    oidc_admin_groups: List[str] = []
    oidc_user_groups: List[str] = []


@admin_router.get(
    "/sso", dependencies=[Depends(api.require_permission("admin.sso"))]
)
async def get_sso_config():
    """The effective SSO/OIDC configuration plus the source of each field.

    ``values`` carries the current effective value of every SSO setting (DB
    override → env → default); ``sources`` reports where each one comes from
    ("db" when edited in the UI, "env", or "default"). The client secret is
    masked (``secret_set`` + ``secret_length``) so it is never echoed in full.
    """
    cfg = await _oidc_config()
    secret = cfg["client_secret"] or ""
    values = {
        "oidc_enabled": bool(cfg["enabled"]),
        "oidc_issuer": cfg["issuer"] or "",
        "oidc_client_id": cfg["client_id"] or "",
        # Masked: the raw secret is never echoed back to the client.
        "oidc_client_secret": "",
        "secret_set": bool(secret),
        "secret_length": len(secret),
        "oidc_redirect_uri": cfg["redirect_uri"] or "",
        "oidc_scopes": cfg["scopes"] or "",
        "oidc_allow_signup": bool(cfg["allow_signup"]),
        "oidc_state_ttl_seconds": int(cfg["state_ttl"] or 600),
        "oidc_admin_groups": [t for t in str(cfg["admin_groups"]).replace(",", " ").split() if t],
        "oidc_user_groups": [t for t in str(cfg["user_groups"]).replace(",", " ").split() if t],
    }
    sources = {name: await setting_source(name) for name in SSO_SETTING_FIELDS}
    issuer = (cfg["issuer"] or "").strip().rstrip("/")
    enabled = bool(cfg["enabled"] and issuer and cfg["client_id"])
    return {"values": values, "sources": sources, "enabled": enabled}


@admin_router.put(
    "/sso", dependencies=[Depends(api.require_permission("admin.sso"))]
)
async def save_sso_config(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    """Save the SSO configuration to the app_settings table (overriding the
    environment). Group lists are stored comma-joined. The client secret is
    only overwritten when a non-empty value is posted, so the UI can leave it
    blank to keep the existing secret."""
    try:
        req = SsoConfigRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    payload = {
        "oidc_enabled": "true" if req.oidc_enabled else "false",
        "oidc_issuer": req.oidc_issuer.strip(),
        "oidc_client_id": req.oidc_client_id.strip(),
        "oidc_client_secret": req.oidc_client_secret,
        "oidc_redirect_uri": req.oidc_redirect_uri.strip(),
        "oidc_scopes": req.oidc_scopes.strip(),
        "oidc_allow_signup": "true" if req.oidc_allow_signup else "false",
        "oidc_state_ttl_seconds": str(int(req.oidc_state_ttl_seconds)),
        "oidc_admin_groups": ",".join(req.oidc_admin_groups),
        "oidc_user_groups": ",".join(req.oidc_user_groups),
    }
    for name in SSO_SETTING_FIELDS:
        value = payload[name]
        if name == "oidc_client_secret" and value == "":
            # Blank secret = "leave the existing one"; skip the write.
            continue
        await db.set_app_setting(f"VRECKAN_{name.upper()}", value)
    return {"ok": True}


@admin_router.post(
    "/sso/test", dependencies=[Depends(api.require_permission("admin.sso"))]
)
async def test_sso_connection(request: Request):
    """Validate the configured issuer by fetching its OIDC discovery document
    (server-side, so it works even when the browser's CORS would block it).
    Returns the resolved endpoints so a misconfigured issuer is caught before
    saving."""
    cfg = await _oidc_config()
    issuer = (cfg["issuer"] or "").strip().rstrip("/")
    if not issuer:
        raise HTTPException(status_code=400, detail="No issuer URL configured.")
    try:
        doc = await _oidc_discovery(issuer)
    except HTTPException as e:
        raise HTTPException(status_code=400, detail=e.detail)
    return {
        "ok": True,
        "issuer": doc.get("issuer"),
        "authorization_endpoint": doc.get("authorization_endpoint"),
        "token_endpoint": doc.get("token_endpoint"),
        "userinfo_endpoint": doc.get("userinfo_endpoint"),
        "jwks_uri": doc.get("jwks_uri"),
        "redirect_uri": await _oidc_redirect_uri(request),
    }


# --- Certificates (Let's Encrypt / certbot) ----------------------------------
# The LE settings are editable from this page (stored in app_settings,
# overriding the environment), mirroring the SSO page. See server/le_certbot.py
# for the certbot orchestration.
LE_SETTING_FIELDS = [
    "le_enabled",
    "le_domains",
    "le_auth",
    "le_cloudflare_email",
    "le_cloudflare_token",
    "le_staging",
    "le_auto_renew",
    "le_renew_check_hours",
    "le_renew_before_days",
]


class CertificateConfigRequest(BaseModel):
    """The Certificates page's save payload. ``le_domains`` is sent as an
    array (the UI uses a tag input); it is stored comma-joined, matching the
    env format. The Cloudflare token is only overwritten when a non-empty
    value is posted, so the UI can leave it blank to keep the existing token."""

    le_enabled: bool
    le_domains: List[str] = []
    le_auth: str = "dns-cloudflare"
    le_cloudflare_email: str = ""
    le_cloudflare_token: str = ""
    le_staging: bool = False
    le_auto_renew: bool = True
    le_renew_check_hours: int = 6
    le_renew_before_days: int = 30


@admin_router.get(
    "/certificates", dependencies=[Depends(api.require_permission("admin.certificates"))]
)
async def get_certificates():
    """The effective Let's Encrypt configuration, the source of each field,
    and the current certificate status (present/source/expiry/days remaining).
    The Cloudflare token is masked (``token_set`` + ``token_length``)."""
    from server import le_certbot
    from server.settings import setting_source

    cfg = await le_certbot.effective_le_config()
    domains = [d.strip() for d in str(cfg.le_domains).split(",") if d.strip()]
    token = cfg.le_cloudflare_token or ""
    values = {
        "le_enabled": bool(cfg.le_enabled),
        "le_domains": domains,
        "le_auth": cfg.le_auth,
        "le_cloudflare_email": cfg.le_cloudflare_email or "",
        "le_cloudflare_token": "",
        "token_set": bool(token),
        "token_length": len(token),
        "le_staging": bool(cfg.le_staging),
        "le_auto_renew": bool(cfg.le_auto_renew),
        "le_renew_check_hours": int(cfg.le_renew_check_hours),
        "le_renew_before_days": int(cfg.le_renew_before_days),
    }
    sources = {name: await setting_source(name) for name in LE_SETTING_FIELDS}
    status = le_certbot.cert_status(domains)
    return {"values": values, "sources": sources, "status": status}


@admin_router.put(
    "/certificates", dependencies=[Depends(api.require_permission("admin.certificates"))]
)
async def save_certificates(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    """Save the Let's Encrypt configuration to the app_settings table
    (overriding the environment). Domains are stored comma-joined. The
    Cloudflare token is only overwritten when a non-empty value is posted."""
    try:
        req = CertificateConfigRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    payload = {
        "le_enabled": "true" if req.le_enabled else "false",
        "le_domains": ",".join(d.strip() for d in req.le_domains if d.strip()),
        "le_auth": req.le_auth.strip() or "dns-cloudflare",
        "le_cloudflare_email": req.le_cloudflare_email.strip(),
        "le_cloudflare_token": req.le_cloudflare_token,
        "le_staging": "true" if req.le_staging else "false",
        "le_auto_renew": "true" if req.le_auto_renew else "false",
        "le_renew_check_hours": str(int(req.le_renew_check_hours)),
        "le_renew_before_days": str(int(req.le_renew_before_days)),
    }
    for name in LE_SETTING_FIELDS:
        value = payload[name]
        if name == "le_cloudflare_token" and value == "":
            # Blank token = "leave the existing one"; skip the write.
            continue
        await db.set_app_setting(f"VRECKAN_{name.upper()}", value)
    return {"ok": True}


@admin_router.post(
    "/certificates/renew", dependencies=[Depends(api.require_permission("admin.certificates"))]
)
async def renew_certificates():
    """Issue (if no valid cert exists) or renew the Let's Encrypt certificate
    now, using the currently effective configuration. Returns the resulting
    certificate status plus ``issued``/``renewed`` flags."""
    from server import le_certbot

    cfg = await le_certbot.effective_le_config()
    if not cfg.le_enabled:
        raise HTTPException(status_code=400, detail="Let's Encrypt is not enabled.")
    domains = [d.strip() for d in str(cfg.le_domains).split(",") if d.strip()]
    if not domains:
        raise HTTPException(status_code=400, detail="No domains configured.")
    status = le_certbot.cert_status(domains)
    if status["valid"]:
        # Already valid: a "renew now" is a no-op (certbot renew would skip it).
        return {**status, "issued": False, "renewed": False}
    result = await le_certbot.ensure_le_cert(cfg)
    return {**result, "renewed": result.get("issued", False)}


@admin_router.get(
    "/apps/stores",
    response_model=List[AppStore],
    dependencies=[Depends(api.require_permission("admin.stores"))],
)
async def get_app_stores():
    return api.APP_STORES


@admin_router.post(
    "/apps/stores",
    response_model=AppStore,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.stores"))],
)
async def add_app_store(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        store = AppStore(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if any(s.name == store.name for s in api.APP_STORES):
        raise HTTPException(
            status_code=409,
            detail=f"App store with name '{store.name}' already exists.",
        )
    api.APP_STORES.append(store)
    await api.save_app_stores()
    return store


@admin_router.delete(
    "/apps/stores/{store_name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.stores"))],
)
async def delete_app_store(store_name: str):
    store_found = next((s for s in api.APP_STORES if s.name == store_name), None)
    if not store_found:
        raise HTTPException(status_code=404, detail="App store not found.")
    api.APP_STORES.remove(store_found)
    await api.save_app_stores()
    return Response(status_code=204)


@admin_router.post(
    "/apps/meta",
    response_model=InstalledApp,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def create_meta_app(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = CreateMetaAppRequest(**decrypted_body)
        base_app = api.INSTALLED_APPS.get(req.base_app_id)
        if not base_app:
            raise HTTPException(
                status_code=404,
                detail=f"Base application with ID '{req.base_app_id}' not found.",
            )

        new_app_id = str(uuid.uuid4())
        home_template_name = f"meta_{new_app_id}"

        logo_url = base_app.logo

        if req.logo:
            try:
                icon_data = base64.b64decode(req.logo)
                icon_path = os.path.join(settings.app_icons_path, f"{new_app_id}.png")
                with open(icon_path, "wb") as f:
                    f.write(icon_data)
                logo_url = f"/api/app_icon/{new_app_id}"
                api.logger.info(f"Saved custom icon for new meta app {new_app_id}")
            except (ValueError, TypeError, base64.binascii.Error):
                logo_url = req.logo

        template_dir = os.path.join(settings.home_templates_path, home_template_name)
        os.makedirs(template_dir, exist_ok=True, mode=0o700)

        os.makedirs(
            os.path.join(template_dir, "Desktop", "files"), exist_ok=True, mode=0o755
        )

        meta_app = base_app.model_copy()
        meta_app.id = new_app_id
        meta_app.name = req.name
        meta_app.logo = logo_url
        meta_app.provider_config.custom_autostart_script_b64 = (
            req.custom_autostart_script_b64
        )
        meta_app.provider_config.custom_autostart_wayland_script_b64 = (
            req.custom_autostart_wayland_script_b64
        )
        meta_app.is_meta_app = True
        meta_app.base_app_id = req.base_app_id
        meta_app.home_template_name = home_template_name
        meta_app.users = req.users
        meta_app.groups = req.groups
        meta_app.auto_update = False
        meta_app.source = base_app.name
        meta_app.source_app_id = base_app.source_app_id

        api.INSTALLED_APPS[new_app_id] = meta_app
        await api.save_installed_apps()

        return meta_app

    except ValidationError as e:
        raise HTTPException(status_code=422, detail=str(e))
    except HTTPException:
        raise
    except Exception as e:
        api.logger.error(f"Error creating meta app: {e}")
        raise HTTPException(
            status_code=500, detail="Internal server error while creating meta app."
        )


@admin_router.post(
    "/launch/meta_customize",
    response_model=LaunchResponse,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def launch_meta_for_customization(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    auth_user: dict = Depends(api.verify_token),
):
    try:
        req = LaunchMetaCustomizeRequest(**decrypted_body)
        app_config = api.INSTALLED_APPS.get(req.application_id)
        if not app_config or not app_config.is_meta_app:
            raise HTTPException(
                status_code=400,
                detail="This endpoint is only for customizing meta applications.",
            )

        template_path = os.path.join(
            settings.home_templates_path, app_config.home_template_name
        )
        return await api._launch_common(
            application_id=req.application_id,
            username=auth_user["username"],
            effective_settings=auth_user["effective_settings"],
            home_name=None,
            env_vars={},
            language=req.language,
            selected_gpu=req.selected_gpu,
            forced_rw_mount=template_path,
            wayland_mode=req.wayland_mode,
            check_access=False,  # admin-only customization flow
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@admin_router.get(
    "/apps/available",
    response_model=List[AvailableApp],
    dependencies=[Depends(api.require_permission("admin.stores"))],
)
async def get_available_apps(url: str, store_name: str, refresh: bool = False):
    def process_content(content: str, store_name_for_cache: str):
        try:
            def extract_apps_from_data(data):
                if isinstance(data, dict) and "apps" in data:
                    return data["apps"]
                elif isinstance(data, list):
                    return data
                else:
                    raise HTTPException(
                        status_code=500, detail="App store YAML has an invalid format."
                    )

            loaded_data = yaml.safe_load(content)
            apps_list = extract_apps_from_data(loaded_data)
            for app in apps_list:
                provider_config = app.get("provider_config", {})

                if provider_config.get("autostart"):
                    app_id = app.get("id")
                    if app_id:
                        cache_dir = os.path.join(
                            settings.autostart_cache_path, store_name_for_cache
                        )

                        cache_file_path = os.path.join(cache_dir, app_id)
                        script_content_b64 = None
                        if (
                            os.path.exists(cache_file_path)
                            and os.path.getsize(cache_file_path) > 0
                        ):
                            try:
                                with open(cache_file_path, "rb") as f:
                                    script_content = f.read()
                                script_content_b64 = base64.b64encode(
                                    script_content
                                ).decode("utf-8")
                            except Exception as e:
                                api.logger.error(
                                    f"Failed to read/encode autostart cache for {app_id}: {e}"
                                )
                        provider_config[
                            "custom_autostart_script_b64"
                        ] = script_content_b64

                        cache_file_path_wayland = os.path.join(
                            cache_dir, f"{app_id}-wayland"
                        )
                        wayland_script_content_b64 = None
                        if (
                            os.path.exists(cache_file_path_wayland)
                            and os.path.getsize(cache_file_path_wayland) > 0
                        ):
                            try:
                                with open(cache_file_path_wayland, "rb") as f:
                                    script_content = f.read()
                                wayland_script_content_b64 = base64.b64encode(
                                    script_content
                                ).decode("utf-8")
                            except Exception as e:
                                api.logger.error(
                                    f"Failed to read/encode wayland autostart cache for {app_id}: {e}"
                                )
                        provider_config[
                            "custom_autostart_wayland_script_b64"
                        ] = wayland_script_content_b64

                if "extensions" in provider_config and provider_config["extensions"]:
                    original_extensions = provider_config["extensions"]
                    flattened_extensions = []
                    for item in original_extensions:
                        if isinstance(item, list):
                            flattened_extensions.extend(item)
                        else:
                            flattened_extensions.append(item)
                    provider_config["extensions"] = flattened_extensions

            return apps_list

        except (yaml.YAMLError, ValidationError) as e:
            raise HTTPException(
                status_code=500, detail=f"Failed to parse app store YAML: {e}"
            )
        except HTTPException:
            # Let specific errors (e.g. "invalid format") propagate with
            # their own message instead of being re-wrapped by the generic
            # handler below.
            raise
        except Exception as e:
            raise HTTPException(
                status_code=500,
                detail=f"An error occurred while processing the app store: {e}",
            )

    cache_file = os.path.join(settings.app_store_cache_path, f"{store_name}.yml")

    if not refresh and os.path.exists(cache_file):
        try:
            with open(cache_file, "r") as f:
                return process_content(f.read(), store_name)
        except Exception as e:
            api.logger.warning(f"Cache read failed for {store_name}: {e}")

    try:
        async with httpx2.AsyncClient() as client:
            response = await client.get(url, follow_redirects=True, timeout=15)
            response.raise_for_status()

            os.makedirs(settings.app_store_cache_path, exist_ok=True)
            with open(cache_file, "w") as f:
                f.write(response.text)

            return process_content(response.text, store_name)
    except httpx2.RequestError as e:
        raise HTTPException(
            status_code=400, detail=f"Failed to fetch app store from URL '{url}': {e}"
        )


@admin_router.get(
    "/apps/installed",
    response_model=List[InstalledAppWithStatus],
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def list_installed_apps():
    response_apps = []
    for app in api.INSTALLED_APPS.values():
        app_dict = app.model_dump()
        image_name = app.provider_config.image
        metadata = api.IMAGE_METADATA.get(image_name, {})
        app_dict["image_sha"] = metadata.get("sha")
        app_dict["last_checked_at"] = metadata.get("last_checked_at")
        app_dict["pull_status"] = api.PULL_STATUS.get(image_name)
        if image_name in api.PULL_PROGRESS:
            # Strip internal per-layer bookkeeping before sending to clients.
            app_dict["pull_progress"] = {
                k: v for k, v in api.PULL_PROGRESS[image_name].items() if not k.startswith("_")
            }
        response_apps.append(InstalledAppWithStatus(**app_dict))
    return sorted(response_apps, key=lambda x: x.name.lower())


@admin_router.post(
    "/apps/installed",
    response_model=InstalledApp,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def install_app(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        app = InstalledApp(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if app.id in api.INSTALLED_APPS:
        raise HTTPException(status_code=409, detail="App with this ID already exists.")
    api.INSTALLED_APPS[app.id] = app
    await api.save_installed_apps()
    # Claim the image synchronously so the very first client poll already
    # sees a "queued"/"pulling" state (no gap where the app looks idle),
    # then let the download run in the background.
    image = app.provider_config.image
    if api.PULL_STATUS.get(image) not in ("queued", "pulling"):
        api.PULL_STATUS[image] = "queued"
        asyncio.create_task(api._do_pull_and_cache_image(image, app))
    return app


@admin_router.put(
    "/apps/installed/{app_id}",
    response_model=InstalledApp,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def update_installed_app(
    app_id: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    try:
        app_update = InstalledApp(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if app_id not in api.INSTALLED_APPS:
        raise HTTPException(status_code=404, detail="Installed app not found.")
    if app_id != app_update.id:
        raise HTTPException(
            status_code=400, detail="App ID in path does not match body."
        )

    if app_update.logo and not app_update.logo.startswith('/api/app_icon/'):
        try:
            icon_data = base64.b64decode(app_update.logo)
            icon_path = os.path.join(settings.app_icons_path, f"{app_id}.png")
            with open(icon_path, "wb") as f:
                f.write(icon_data)
            app_update.logo = f"/api/app_icon/{app_id}"
            api.logger.info(f"Updated and saved custom icon for app {app_id}")
        except (ValueError, TypeError, base64.binascii.Error):
            api.logger.warning(f"Could not decode logo for app {app_id}. Assuming it's a URL and leaving as is.")

    if app_update.provider_config.custom_autostart_script_b64 == "":
        app_update.provider_config.custom_autostart_script_b64 = None
    if app_update.provider_config.custom_autostart_wayland_script_b64 == "":
        app_update.provider_config.custom_autostart_wayland_script_b64 = None

    old_image_name = api.INSTALLED_APPS[app_id].provider_config.image

    api.INSTALLED_APPS[app_id] = app_update
    await api.save_installed_apps()

    if old_image_name != app_update.provider_config.image:
        new_image = app_update.provider_config.image
        if api.PULL_STATUS.get(new_image) not in ("queued", "pulling"):
            api.PULL_STATUS[new_image] = "queued"
            asyncio.create_task(api._do_pull_and_cache_image(new_image, app_update))

    return app_update


@admin_router.delete(
    "/apps/installed/{app_id}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def delete_installed_app(app_id: str):
    if app_id not in api.INSTALLED_APPS:
        raise HTTPException(status_code=404, detail="Installed app not found.")

    app_to_delete = api.INSTALLED_APPS[app_id]

    if app_to_delete.is_meta_app:
        icon_path = os.path.join(settings.app_icons_path, f"{app_id}.png")
        if os.path.exists(icon_path):
            try:
                os.remove(icon_path)
                api.logger.info(f"Deleted custom icon for meta-app {app_id}")
            except OSError as e:
                api.logger.error(f"Failed to delete icon for meta-app {app_id}: {e}")

        if app_to_delete.is_meta_app and app_to_delete.home_template_name:
            template_dir_path = os.path.join(
                settings.home_templates_path, app_to_delete.home_template_name
            )
            if os.path.isdir(template_dir_path):
                try:
                    api._safe_rmtree(template_dir_path)
                    api.logger.info(
                        f"Purged home template directory for meta-app {app_id}: {template_dir_path}"
                    )
                except (OSError, HTTPException) as e:
                    api.logger.error(
                        f"Failed to purge home template for meta-app {app_id}: {e}"
                    )

    del api.INSTALLED_APPS[app_id]
    await api.save_installed_apps()
    return Response(status_code=204)


@admin_router.post(
    "/apps/installed/{app_id}/check_update",
    response_model=ImageUpdateCheckResponse,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def check_app_update(app_id: str):
    if app_id not in api.INSTALLED_APPS:
        raise HTTPException(status_code=404, detail="Installed app not found.")
    app = api.INSTALLED_APPS[app_id]
    image_name = app.provider_config.image

    provider = DockerProvider(app.model_dump())
    local_info = await provider.get_local_image_info(image_name)
    remote_digest = await provider.get_remote_image_digest(image_name)

    if not remote_digest:
        raise HTTPException(
            status_code=502,
            detail=f"Could not retrieve update information for {image_name} from its registry.",
        )

    local_digests = local_info.get("digests", []) if local_info else []
    update_available = not any(
        remote_digest in local_digest for local_digest in local_digests
    )

    return ImageUpdateCheckResponse(
        current_sha=local_info["short_id"] if local_info else None,
        update_available=update_available,
    )


@admin_router.post(
    "/apps/installed/check_all_updates",
    response_model=CheckAllUpdatesResponse,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def check_all_app_updates():
    """Check every installed app's image against its registry and report which
    ones have a newer version available. Runs the same digest comparison as
    the per-app check, in parallel, and never pulls anything."""
    apps = list(api.INSTALLED_APPS.values())

    async def check_one(app) -> AppUpdateCheckResult:
        image_name = app.provider_config.image
        provider = DockerProvider(app.model_dump())
        local_info = await provider.get_local_image_info(image_name)
        remote_digest = await provider.get_remote_image_digest(image_name)
        local_digests = local_info.get("digests", []) if local_info else []
        update_available = bool(
            remote_digest
            and not any(remote_digest in d for d in local_digests)
        )
        return AppUpdateCheckResult(
            app_id=app.id,
            name=app.name,
            image=image_name,
            current_sha=local_info["short_id"] if local_info else None,
            update_available=update_available,
        )

    results = await asyncio.gather(*(check_one(app) for app in apps))
    return CheckAllUpdatesResponse(
        results=list(results),
        updates_available=sum(1 for r in results if r.update_available),
    )


@admin_router.post(
    "/apps/installed/pull_all_latest",
    response_model=PullAllImagesResponse,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def pull_all_app_images():
    """Kick off a background pull of the latest image for every installed app
    and return immediately. Images already being pulled are reported as such
    rather than queued twice; the client follows progress via the
    installed-apps list (pull_status + pull_progress)."""
    results: List[AppImagePullResult] = []
    for app in api.INSTALLED_APPS.values():
        image_name = app.provider_config.image
        if api.PULL_STATUS.get(image_name) in ("queued", "pulling"):
            results.append(
                AppImagePullResult(
                    app_id=app.id, name=app.name, image=image_name, status="pulling"
                )
            )
            continue
        api.PULL_STATUS[image_name] = "queued"
        asyncio.create_task(api._do_pull_and_cache_image(image_name, app))
        results.append(
            AppImagePullResult(
                app_id=app.id, name=app.name, image=image_name, status="pulling"
            )
        )
    return PullAllImagesResponse(results=results, started=len(results))


@admin_router.post(
    "/apps/installed/{app_id}/pull_latest",
    response_model=ImagePullResponse,
    dependencies=[Depends(api.require_permission("admin.apps"))],
)
async def pull_latest_app_image(app_id: str):
    """Kicks off a background image pull and returns immediately.

    The client then polls the installed-apps list (which carries
    pull_status + pull_progress) to follow the download. If a pull for this
    image is already under way we just report that instead of starting a
    second one.
    """
    if app_id not in api.INSTALLED_APPS:
        raise HTTPException(status_code=404, detail="Installed app not found.")
    app = api.INSTALLED_APPS[app_id]
    image_name = app.provider_config.image

    if api.PULL_STATUS.get(image_name) in ("queued", "pulling"):
        return ImagePullResponse(status="pulling", new_sha=None)

    api.PULL_STATUS[image_name] = "queued"
    asyncio.create_task(api._do_pull_and_cache_image(image_name, app))
    return ImagePullResponse(status="pulling", new_sha=None)


@admin_router.get(
    "/apps/templates",
    response_model=List[AppTemplate],
    dependencies=[Depends(api.require_permission("admin.templates"))],
)
async def get_app_templates():
    return sorted(list(api.APP_TEMPLATES.values()), key=lambda x: x["name"])


@admin_router.get(
    "/apps/templates/schema",
    dependencies=[Depends(api.require_permission("admin.templates"))],
)
async def get_template_schema():
    """Serve the categorised application-template schema that drives the rich
    template editor in the admin UI."""
    return api.TEMPLATE_SCHEMA


@admin_router.post(
    "/apps/templates/schema",
    dependencies=[Depends(api.require_permission("admin.templates"))],
)
async def save_template_schema(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    """Save the template schema — the base layer.

    Replaces the whole schema: upsert by name, remove missing, re-order.
    Each setting carries ``default`` (the image baseline — read-only for
    built-in settings, always reset to the bundled YAML value here) and
    ``current`` (the admin's base-layer override). An empty ``current``
    means "use the image default" and is stored as NULL, so a setting is
    only pushed through to launches when ``current`` is set and differs
    from ``default``.

    Body: {"settings": [ {name, label, description, category, type, default,
    current?, docker, options?}, ... ]}
    """
    body = decrypted_body or {}
    settings_list = body.get("settings")
    if settings_list is None:
        raise HTTPException(status_code=400, detail="'settings' is required.")
    if not isinstance(settings_list, list):
        raise HTTPException(status_code=400, detail="'settings' must be a list.")
    yaml_by_name = {s["name"]: s for s in api.TEMPLATE_SCHEMA_YAML if s.get("name")}
    seen = set()
    clean = []
    for i, s in enumerate(settings_list):
        if not isinstance(s, dict):
            raise HTTPException(status_code=400, detail="Each setting must be an object.")
        name = str(s.get("name") or "").strip()
        if not re.match(r"^[A-Za-z][A-Za-z0-9_]*$", name):
            raise HTTPException(status_code=400, detail=f"Invalid setting name: '{name}'.")
        if name in seen:
            raise HTTPException(status_code=400, detail=f"Duplicate setting name: '{name}'.")
        seen.add(name)
        stype = str(s.get("type") or "text")
        if stype not in ("text", "boolean", "select"):
            raise HTTPException(
                status_code=400, detail=f"Invalid type '{stype}' for setting '{name}'."
            )
        # The baseline always tracks the bundled YAML for built-in settings
        # (the UI presents it as read-only); custom settings keep whatever
        # baseline the admin defines.
        if name in yaml_by_name:
            default_val = str(yaml_by_name[name].get("default") or "")
        else:
            default_val = str(s.get("default") or "")
        raw_current = s.get("current")
        # Empty means "use the image default" -> NULL (not pushed through).
        current_val = None if raw_current is None or raw_current == "" else str(raw_current)
        entry = {
            "name": name,
            "label": str(s.get("label") or name),
            "description": str(s.get("description") or ""),
            "category": str(s.get("category") or "general"),
            "type": stype,
            "default": default_val,
            "current": current_val,
            "docker": bool(s.get("docker", False)),
        }
        if stype == "select":
            entry["options"] = [
                o
                for o in (s.get("options") or [])
                if isinstance(o, dict) and o.get("value") is not None
            ]
        clean.append((i, entry))
    async with db.async_session_factory() as session:
        async with session.begin():
            existing = {
                r.name: r
                for r in (await session.execute(select(db.TemplateSchemaSetting))).scalars()
            }
            for name, row in existing.items():
                if name not in seen:
                    await session.delete(row)
            for i, entry in clean:
                row = existing.get(entry["name"])
                if row is None:
                    row = db.TemplateSchemaSetting(name=entry["name"], sort_order=i)
                    session.add(row)
                else:
                    row.sort_order = i
                for key in ("label", "description", "category", "type", "default", "current", "docker", "options"):
                    if key in entry:
                        setattr(row, key, entry[key])
    # Rebuild the in-memory schema from what was just persisted so the launch
    # path and the template editor see the change immediately.
    await api._load_template_schema()
    api.logger.info(f"Template schema updated ({len(clean)} settings).")
    return {"ok": True}


@admin_router.post(
    "/apps/templates",
    response_model=AppTemplate,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.templates"))],
)
async def save_app_template(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        template = AppTemplate(**decrypted_body)
        if not re.match(r"^[a-zA-Z0-9_ -]+$", template.name):
            raise HTTPException(status_code=400, detail="Invalid template name.")

        # User templates live in the configuration database. A user template
        # may override a built-in (default) template by sharing its name.
        api.APP_TEMPLATES[template.name] = template.model_dump()
        async with db.async_session_factory() as session:
            existing = (
                await session.execute(
                    select(db.AppTemplate).where(db.AppTemplate.name == template.name)
                )
            ).scalar_one_or_none()
            if existing is None:
                session.add(db.AppTemplate(name=template.name, settings=template.settings))
            else:
                existing.settings = template.settings
            await session.commit()
        return template
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=str(e))
    except HTTPException:
        # Let explicit errors (e.g. invalid template name) propagate with
        # their own status instead of being masked by the generic handler
        # below.
        raise
    except Exception as e:
        api.logger.error(f"Error saving app template: {e}")
        raise HTTPException(
            status_code=500, detail="Internal server error while saving template."
        )


@admin_router.delete(
    "/apps/templates/{template_name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.templates"))],
)
async def delete_app_template(template_name: str):
    default_filename = template_name.lower().replace(" ", "_") + ".yml"
    default_path = os.path.join(settings.default_app_templates_path, default_filename)
    is_default = os.path.exists(default_path)

    async with db.async_session_factory() as session:
        user_row = (
            await session.execute(
                select(db.AppTemplate).where(db.AppTemplate.name == template_name)
            )
        ).scalar_one_or_none()

    if user_row is None and is_default:
        # A pure built-in template (no user override) cannot be deleted.
        raise HTTPException(
            status_code=403,
            detail=f"Cannot delete the default template '{template_name}'. You can override it by creating a new template with the same name.",
        )
    if user_row is None and template_name not in api.APP_TEMPLATES:
        raise HTTPException(status_code=404, detail=f"Template '{template_name}' not found.")

    # Remove the user template. If it was overriding a built-in template, the
    # default (from the bundled files) takes effect again.
    api.APP_TEMPLATES.pop(template_name, None)
    if is_default:
        try:
            with open(default_path, "r") as f:
                default_data = yaml.safe_load(f)
            if default_data and default_data.get("name"):
                api.APP_TEMPLATES[template_name] = default_data
        except Exception as e:
            api.logger.error(f"Could not restore default template '{template_name}': {e}")

    async with db.async_session_factory() as session:
        await session.execute(
            delete(db.AppTemplate).where(db.AppTemplate.name == template_name)
        )
        await session.commit()
    return Response(status_code=204)


@admin_router.get(
    "/sessions",
    response_model=List[UserSessionList],
    dependencies=[Depends(api.require_permission("admin.sessions"))],
)
async def get_all_sessions():
    sessions_by_user = defaultdict(list)
    for sid, s_data in api.SESSIONS_DB.items():
        username = s_data.get("username", "unknown")
        try:
            out_of_date = await api._session_out_of_date(s_data)
        except Exception:  # noqa: BLE001 - a probe failure shouldn't hide the list
            out_of_date = False
        sessions_by_user[username].append(
            ActiveSessionInfo(
                session_id=sid,
                app_id=s_data["provider_app_id"],
                app_name=s_data["app_name"],
                app_logo=s_data["app_logo"],
                created_at=s_data["created_at"],
                session_url=f"/api/apps/session/{sid}/?access_token={s_data['access_token']}",
                launch_context=s_data.get("launch_context"),
                is_collaboration=s_data.get("is_collaboration", False),
                name=s_data.get("name"),
                out_of_date=out_of_date,
            )
        )

    response = [
        UserSessionList(
            username=uname,
            sessions=sorted(slist, key=lambda s: s.created_at, reverse=True),
        )
        for uname, slist in sessions_by_user.items()
    ]
    return sorted(response, key=lambda u: u.username)


@admin_router.delete(
    "/sessions/{session_id}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.sessions"))],
)
async def stop_any_session(session_id: str):
    if session_id not in api.SESSIONS_DB:
        raise HTTPException(status_code=404, detail="Session not found.")
    await api._stop_session(session_id)
    return Response(status_code=204)


@admin_router.post(
    "/sessions/{session_id}/recreate",
    response_model=SessionRecreateResponse,
    dependencies=[Depends(api.require_permission("admin.sessions"))],
)
async def recreate_any_session(session_id: str):
    """Recreate any user's session from the current image (see the user-facing
    ``/api/sessions/{id}/recreate``)."""
    if session_id not in api.SESSIONS_DB:
        raise HTTPException(status_code=404, detail="Session not found.")
    session_data = await api.recreate_session(session_id)
    return SessionRecreateResponse(
        session_id=session_id,
        session_url=(
            f"/room/{session_id}?token={session_data['controller_token']}"
            if session_data.get("is_collaboration")
            else f"/api/apps/session/{session_id}/?access_token={session_data['access_token']}"
        ),
    )


@admin_router.post(
    "/oidc/revoke",
    response_model=RevokeOidcSessionResponse,
    dependencies=[Depends(api.require_permission("admin.sessions"))],
)
async def revoke_oidc_sessions(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
):
    """Force-invalidate ALL web (OIDC) sessions for a named user.

    Admin-only. Revokes every active web-login token belonging to the
    given user, so their ``vreckan_auth`` cookie stops working
    immediately — even if it was leaked. The user account itself is not
    touched; they can simply sign in again. Revoking your own sessions
    logs *you* out as well.
    """
    try:
        req = RevokeOidcSessionRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if not user_manager.get_user(req.username):
        raise HTTPException(
            status_code=404, detail=f"User '{req.username}' not found."
        )
    revoked = await api.revoke_all_tokens_for_user(req.username)
    await api.save_auth_tokens_to_disk()
    api.logger.info(
        f"Admin force-logout: revoked {revoked} web token(s) for '{req.username}'."
    )
    return RevokeOidcSessionResponse(username=req.username, revoked=revoked)


@admin_router.post(
    "/people",
    response_model=CreateUserResponse,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def create_person(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    """Create a user or admin. The roster is unified: the request's
    ``is_admin`` flag decides which kind of account is created; both are
    stored and managed identically otherwise."""
    # Separate try: Pydantic's ValidationError subclasses ValueError, so a
    # shared block would let the 409 handler swallow validation errors.
    try:
        req = CreatePersonRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        req_settings = req.settings.model_dump() if req.settings else None
        if req.is_admin:
            user = await user_manager.create_admin(req.username, req_settings)
        else:
            user = await user_manager.create_user(req.username, req_settings)
        return {"user": user}
    except ValueError as e:
        raise HTTPException(status_code=409, detail=str(e))


@admin_router.put(
    "/people/{username}",
    response_model=User,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def update_person(
    username: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    # Separate try: Pydantic's ValidationError subclasses ValueError, so a
    # shared block would let the 404 handler swallow validation errors.
    try:
        req = UpdateUserRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        await user_manager.update_user_settings(username, req.settings.model_dump())
        return user_manager.get_user(username)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))


@admin_router.post(
    "/people/{username}/admin-status",
    response_model=User,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def set_admin_status(
    username: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    """Promote a user to admin or demote an admin to user.

    The roster is unified, so the admin UI can flip an account's ``is_admin``
    flag in place instead of deleting and recreating it. Idempotent: a no-op
    if the account is already in the requested state. The bootstrap 'admin'
    account cannot be demoted (403) — doing so would lock everyone out of the
    admin UI.
    """
    try:
        req = SetAdminStatusRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        if req.is_admin:
            await user_manager.promote_user_to_admin(username)
        else:
            await user_manager.demote_admin_to_user(username)
        return user_manager.get_user(username)
    except ValueError as e:
        if "cannot be demoted" in str(e).lower():
            raise HTTPException(status_code=403, detail=str(e))
        raise HTTPException(status_code=404, detail=str(e))


@admin_router.delete(
    "/people/{username}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def delete_person(username: str):
    try:
        await user_manager.delete_person(username)
        return Response(status_code=204)
    except ValueError as e:
        if "cannot be deleted" in str(e).lower():
            raise HTTPException(status_code=403, detail=str(e))
        raise HTTPException(status_code=404, detail=str(e))


@admin_router.get(
    "/people/{username}/homedirs",
    response_model=HomeDirectoryList,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def list_person_home_dirs(username: str):
    if not user_manager.get_user(username):
        raise HTTPException(status_code=404, detail=f"User or admin '{username}' not found.")
    if not user_manager.get_effective_settings(username).get(
        "persistent_storage", False
    ):
        raise HTTPException(
            status_code=403, detail="Persistent storage is disabled for this account."
        )
    return {"home_dirs": user_manager.get_home_dirs(username)}


@admin_router.post(
    "/people/{username}/homedirs",
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def create_person_home_dir(
    username: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    if not user_manager.get_user(username):
        raise HTTPException(status_code=404, detail=f"User or admin '{username}' not found.")
    if not user_manager.get_effective_settings(username).get(
        "persistent_storage", False
    ):
        raise HTTPException(
            status_code=403, detail="Persistent storage is disabled for this account."
        )
    # Separate try: Pydantic's ValidationError subclasses ValueError, so a
    # shared block would let the 409 handler swallow validation errors.
    try:
        req = HomeDirectoryCreate(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        user_manager.create_home_dir(username, req.home_name)
        return {"status": "success"}
    except ValueError as e:
        raise HTTPException(status_code=409, detail=str(e))


@admin_router.delete(
    "/people/{username}/homedirs/{home_name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def delete_person_home_dir(username: str, home_name: str):
    if not user_manager.get_user(username):
        raise HTTPException(status_code=404, detail=f"User or admin '{username}' not found.")
    if not user_manager.get_effective_settings(username).get(
        "persistent_storage", False
    ):
        raise HTTPException(
            status_code=403, detail="Persistent storage is disabled for this account."
        )
    try:
        user_manager.delete_home_dir(username, home_name)
        return Response(status_code=204)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))


@admin_router.post(
    "/groups",
    response_model=Group,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.groups"))],
)
async def create_group(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = CreateGroupRequest(**decrypted_body)
        if req.name in user_manager.GROUP_DATA:
            raise HTTPException(
                status_code=409, detail=f"Group '{req.name}' already exists."
            )
        await user_manager.write_group_file(
            req.name,
            req.settings.model_dump(),
            roles=req.roles,
            permissions=req.permissions,
        )
        return user_manager.GROUP_DATA.get(req.name)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@admin_router.put(
    "/groups/{group_name}",
    response_model=Group,
    dependencies=[Depends(api.require_permission("admin.groups"))],
)
async def update_group(
    group_name: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    try:
        req = UpdateGroupRequest(**decrypted_body)
        if group_name not in user_manager.GROUP_DATA:
            raise HTTPException(
                status_code=404, detail=f"Group '{group_name}' not found."
            )
        group = user_manager.GROUP_DATA[group_name]
        new_settings = (
            req.settings.model_dump() if req.settings is not None else group.get("settings") or {}
        )
        new_roles = req.roles if req.roles is not None else (group.get("roles") or [])
        new_perms = (
            req.permissions if req.permissions is not None else (group.get("permissions") or [])
        )
        await user_manager.write_group_file(
            group_name, new_settings, roles=new_roles, permissions=new_perms
        )
        # Changing a group's roles/permissions changes the effective
        # permissions of every member, so re-derive their is_admin flags.
        for username, u in user_manager.USER_DATA.items():
            if group_name in (u.get("groups") or []):
                await user_manager.recompute_is_admin(username)
        return user_manager.GROUP_DATA.get(group_name)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@admin_router.delete(
    "/groups/{group_name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.groups"))],
)
async def delete_group(group_name: str):
    try:
        await user_manager.delete_group(group_name)
        return Response(status_code=204)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))


# --- Volume mounts ---------------------------------------------------------
@admin_router.get(
    "/volume_mounts",
    response_model=List[VolumeMount],
    dependencies=[Depends(api.require_permission("admin.mounts"))],
)
async def list_volume_mounts():
    return volume_mount_manager.get_all_volume_mounts()


@admin_router.post(
    "/volume_mounts",
    response_model=VolumeMount,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.mounts"))],
)
async def create_volume_mount(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = CreateVolumeMountRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if req.name in volume_mount_manager.VOLUME_MOUNTS:
        raise HTTPException(status_code=409, detail=f"Volume mount '{req.name}' already exists.")
    if req.scope not in volume_mount_manager.VALID_SCOPES:
        raise HTTPException(status_code=422, detail="scope must be 'none', 'user', or 'group'.")
    if req.scope == "user" and not user_manager.get_user(req.target):
        raise HTTPException(status_code=404, detail=f"User '{req.target}' not found.")
    if req.scope == "group" and req.target not in user_manager.GROUP_DATA:
        raise HTTPException(status_code=404, detail=f"Group '{req.target}' not found.")
    await volume_mount_manager.write_volume_mount(
        req.name, req.model_dump(exclude={"name"})
    )
    return volume_mount_manager.get_volume_mount(req.name)


@admin_router.put(
    "/volume_mounts/{name}",
    response_model=VolumeMount,
    dependencies=[Depends(api.require_permission("admin.mounts"))],
)
async def update_volume_mount(name: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = UpdateVolumeMountRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    if name not in volume_mount_manager.VOLUME_MOUNTS:
        raise HTTPException(status_code=404, detail=f"Volume mount '{name}' not found.")
    if req.scope not in volume_mount_manager.VALID_SCOPES:
        raise HTTPException(status_code=422, detail="scope must be 'none', 'user', or 'group'.")
    if req.scope == "user" and not user_manager.get_user(req.target):
        raise HTTPException(status_code=404, detail=f"User '{req.target}' not found.")
    if req.scope == "group" and req.target not in user_manager.GROUP_DATA:
        raise HTTPException(status_code=404, detail=f"Group '{req.target}' not found.")
    await volume_mount_manager.write_volume_mount(name, req.model_dump())
    return volume_mount_manager.get_volume_mount(name)


@admin_router.delete(
    "/volume_mounts/{name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.mounts"))],
)
async def delete_volume_mount(name: str):
    if name not in volume_mount_manager.VOLUME_MOUNTS:
        raise HTTPException(status_code=404, detail=f"Volume mount '{name}' not found.")
    await volume_mount_manager.delete_volume_mount(name)
    return Response(status_code=204)


# --- Backup / restore ------------------------------------------------------
@admin_router.post(
    "/backup/create",
    response_model=BackupInfo,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def create_backup_endpoint(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = CreateBackupRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        return await backup_manager.create_backup(req.include_user_data)
    except backup_manager.BackupError as e:
        raise HTTPException(status_code=400, detail=str(e))


@admin_router.get(
    "/backup",
    response_model=List[BackupInfo],
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def list_backups_endpoint():
    return backup_manager.list_backups()


@admin_router.get(
    "/backup/{name}/download",
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def download_backup(name: str, chunk_index: int = 0):
    try:
        data, is_last = backup_manager.get_backup_chunk(name, chunk_index)
    except backup_manager.BackupError as e:
        raise HTTPException(status_code=404, detail=str(e))
    return {
        "chunk_data_b64": base64.b64encode(data).decode(),
        "is_last_chunk": is_last,
        "filename": name,
    }


@admin_router.post(
    "/backup/restore",
    response_model=RestoreResult,
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def restore_backup(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    name = decrypted_body.get("name")
    if not name:
        raise HTTPException(status_code=422, detail="Missing 'name' to restore.")
    try:
        return await backup_manager.restore_from_stored(name)
    except backup_manager.BackupError as e:
        raise HTTPException(status_code=400, detail=str(e))


@admin_router.post(
    "/backup/restore/initiate",
    response_model=BackupRestoreInitiateResponse,
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def backup_restore_initiate():
    upload_id = str(uuid.uuid4())
    upload_path = os.path.join(backup_manager.get_backup_dir(), f"restore_{upload_id}")
    os.makedirs(upload_path, exist_ok=True, mode=0o700)
    return {"upload_id": upload_id}


@admin_router.post(
    "/backup/restore/chunk",
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def backup_restore_chunk(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = BackupRestoreChunkRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    upload_path = os.path.join(backup_manager.get_backup_dir(), f"restore_{req.upload_id}")
    if not os.path.isdir(upload_path):
        raise HTTPException(status_code=404, detail="Restore session not found.")
    try:
        with open(os.path.join(upload_path, f"chunk_{req.chunk_index}"), "wb") as f:
            f.write(base64.b64decode(req.chunk_data_b64))
    except (ValueError, TypeError) as e:
        raise HTTPException(status_code=400, detail=f"Invalid Base64 chunk data: {e}")
    return {"status": "ok", "chunk_index": req.chunk_index}


@admin_router.post(
    "/backup/restore/finalize",
    response_model=RestoreResult,
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def backup_restore_finalize(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = BackupRestoreFinalizeRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    upload_path = os.path.join(backup_manager.get_backup_dir(), f"restore_{req.upload_id}")
    if not os.path.isdir(upload_path):
        raise HTTPException(status_code=404, detail="Restore session not found.")
    try:
        assembled = b""
        for i in range(req.total_chunks):
            chunk_path = os.path.join(upload_path, f"chunk_{i}")
            if not os.path.isfile(chunk_path):
                raise HTTPException(status_code=400, detail=f"Missing chunk {i}.")
            with open(chunk_path, "rb") as f:
                assembled += f.read()
        return await backup_manager.restore_from_bytes(assembled)
    except backup_manager.BackupError as e:
        raise HTTPException(status_code=400, detail=str(e))
    finally:
        shutil.rmtree(upload_path, ignore_errors=True)


@admin_router.delete(
    "/backup/{name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.backup"))],
)
async def delete_backup_endpoint(name: str):
    try:
        backup_manager.delete_backup(name)
    except backup_manager.BackupError as e:
        raise HTTPException(status_code=404, detail=str(e))
    return Response(status_code=204)


# --- Roles ------------------------------------------------------------------
# Roles are named bundles of permissions that can be assigned to users and to
# groups. The built-in roles (admin / operator / user) are seeded on first
# run but are fully editable; custom roles can be created and deleted. These
# endpoints are gated on ``admin.roles``.
@admin_router.get(
    "/permissions",
    dependencies=[Depends(api.require_permission("admin.roles"))],
)
async def list_permissions():
    """The full permission catalog, for the role editor's checkbox list."""
    from server.permissions import ADMIN_PERMISSIONS, USER_PERMISSIONS

    return {
        "admin": [
            {"name": p, "description": _PERMISSION_DESCRIPTIONS.get(p, "")}
            for p in ADMIN_PERMISSIONS
        ],
        "user": [
            {"name": p, "description": _PERMISSION_DESCRIPTIONS.get(p, "")}
            for p in USER_PERMISSIONS
        ],
    }


@admin_router.get(
    "/roles",
    response_model=List[Role],
    dependencies=[Depends(api.require_permission("admin.roles"))],
)
async def list_roles():
    return user_manager.get_all_roles()


@admin_router.post(
    "/roles",
    response_model=Role,
    status_code=201,
    dependencies=[Depends(api.require_permission("admin.roles"))],
)
async def create_role(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = CreateRoleRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        return await user_manager.create_role(req.name, req.description, req.permissions)
    except ValueError as e:
        raise HTTPException(status_code=409, detail=str(e))


@admin_router.put(
    "/roles/{role_name}",
    response_model=Role,
    dependencies=[Depends(api.require_permission("admin.roles"))],
)
async def update_role(
    role_name: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    try:
        req = UpdateRoleRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        return await user_manager.update_role(
            role_name, description=req.description, permissions=req.permissions
        )
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))


@admin_router.delete(
    "/roles/{role_name}",
    status_code=204,
    dependencies=[Depends(api.require_permission("admin.roles"))],
)
async def delete_role(role_name: str):
    try:
        await user_manager.delete_role(role_name)
        return Response(status_code=204)
    except ValueError as e:
        msg = str(e)
        if "still assigned" in msg or "built-in" in msg:
            raise HTTPException(status_code=409, detail=msg)
        raise HTTPException(status_code=404, detail=msg)


# --- Per-account access (roles / permissions / groups) ----------------------
@admin_router.post(
    "/people/{username}/access",
    response_model=User,
    dependencies=[Depends(api.require_permission("admin.accounts"))],
)
async def set_user_access(
    username: str, decrypted_body: dict = Depends(api.get_decrypted_request_body)
):
    """Set an account's direct roles, permissions, and group memberships.

    All three fields are optional; an absent field leaves that aspect
    unchanged. This is the fine-grained control surface for the Accounts
    page. After the update the account's ``is_admin`` flag is re-derived
    from its effective permissions.
    """
    try:
        req = SetUserAccessRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        await user_manager.set_user_access(
            username,
            roles=req.roles,
            permissions=req.permissions,
            groups=req.groups,
        )
        return user_manager.get_user(username)
    except ValueError as e:
        msg = str(e)
        if "must keep the 'admin' role" in msg:
            raise HTTPException(status_code=403, detail=msg)
        raise HTTPException(status_code=404, detail=msg)
