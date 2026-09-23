"""Encrypted (E2EE) endpoints: the application catalog and the launch flows.

Extracted from ``app.api`` during the router split. This router uses
``api.EncryptedRoute`` so every request body is end-to-end encrypted; the
plaintext auth/OIDC endpoints live on the separate ``auth_router``.

The heavy lifting (``api._launch_common``, ``api._stop_session``,
``api._reassemble_file``, ``api._user_can_access_app``, ...) and all shared
module state (``api.INSTALLED_APPS``, ``api.AVAILABLE_GPUS``, ...) remain in
``app.api``; this module only holds the thin HTTP endpoints and reaches into
the ``api`` module at call time, so test fixtures that mutate ``app.api`` in
place keep working. Only the auth dependencies and ``api.EncryptedRoute`` are
resolved at import time.
"""

import base64
import os
import re
from typing import List

from fastapi import APIRouter, Depends, HTTPException
from pydantic import ValidationError
from sqlalchemy import select

from server import api
from server import db
from server.models import (
    AdminStatusResponse,
    Application,
    GPUInfo,
    LaunchRequestFile,
    LaunchRequestFilePath,
    LaunchRequestSimple,
    LaunchRequestURL,
    LaunchResponse,
)
from server.settings import settings


encrypted_router = APIRouter(route_class=api.EncryptedRoute)

@encrypted_router.post("/api/applications", response_model=List[Application])
async def get_applications(user: dict = Depends(api.verify_token)):
    user_apps = []
    username = user["username"]
    user_group = user.get("group", "none")

    for app in api.INSTALLED_APPS.values():
        # Only apps the user is granted are listed (see api._user_can_access_app);
        # there is no admin bypass.
        if api._user_can_access_app(app, username, user_group):
            user_apps.append(
                Application(
                    id=app.id,
                    name=app.name,
                    logo=app.logo,
                    home_directories=app.home_directories,
                    is_meta_app=app.is_meta_app,
                    nvidia_support=app.provider_config.nvidia_support,
                    dri3_support=app.provider_config.dri3_support,
                    url_support=app.provider_config.url_support,
                    extensions=app.provider_config.extensions,
                )
            )

    return sorted(user_apps, key=lambda x: x.name.lower())


@encrypted_router.post("/api/launch/simple", response_model=LaunchResponse)
async def launch_simple(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    auth_user: dict = Depends(api.verify_token),
):
    try:
        req = LaunchRequestSimple(**decrypted_body)
        return await api._launch_common(
            req.application_id,
            auth_user["username"],
            auth_user["effective_settings"],
            req.home_name,
            {},
            req.language,
            req.selected_gpu,
            launch_in_room_mode=req.launch_in_room_mode,
            wayland_mode=req.wayland_mode,
            session_name=req.session_name,
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@encrypted_router.post("/api/launch/url", response_model=LaunchResponse)
async def launch_url(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    auth_user: dict = Depends(api.verify_token),
):
    try:
        req = LaunchRequestURL(**decrypted_body)
        return await api._launch_common(
            req.application_id,
            auth_user["username"],
            auth_user["effective_settings"],
            req.home_name,
            {"VRECKAN_URL": req.url},
            req.language,
            req.selected_gpu,
            launch_in_room_mode=req.launch_in_room_mode,
            wayland_mode=req.wayland_mode,
            session_name=req.session_name,
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@encrypted_router.post("/api/launch/file", response_model=LaunchResponse)
async def launch_file(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    auth_user: dict = Depends(api.verify_token),
):
    try:
        req = LaunchRequestFile(**decrypted_body)
        reassembled_file_path = await api._reassemble_file(
            req.upload_id, req.total_chunks, req.filename
        )

        file_bytes = None
        try:
            with open(reassembled_file_path, "rb") as f:
                file_bytes = f.read()
        finally:
            if os.path.exists(reassembled_file_path):
                os.remove(reassembled_file_path)

        return await api._launch_common(
            req.application_id,
            auth_user["username"],
            auth_user["effective_settings"],
            req.home_name,
            {},
            req.language,
            req.selected_gpu,
            file_bytes,
            os.path.basename(req.filename),
            req.open_file_on_launch,
            launch_in_room_mode=req.launch_in_room_mode,
            wayland_mode=req.wayland_mode,
            session_name=req.session_name,
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@encrypted_router.post("/api/launch/file_path", response_model=LaunchResponse)
async def launch_file_path(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    auth_user: dict = Depends(api.verify_persistent_storage_enabled),
):
    try:
        req = LaunchRequestFilePath(**decrypted_body)

        if req.home_name and req.home_name.lower() == "cleanroom":
            raise HTTPException(
                status_code=400,
                detail="Cannot open a server-side file in 'Cleanroom' mode.",
            )

        username = auth_user["username"]
        shared_files_path = os.path.abspath(
            os.path.join(settings.storage_path, username, "_vreckan_shared_files")
        )

        safe_filename = os.path.basename(req.filename)
        file_location = os.path.join(shared_files_path, safe_filename)

        if not os.path.exists(file_location):
            raise HTTPException(
                status_code=404, detail=f"File '{safe_filename}' not found."
            )

        container_file_path = os.path.join(
            settings.container_config_path, "Desktop", "files", safe_filename
        )

        env_vars = {"VRECKAN_FILE": container_file_path}

        return await api._launch_common(
            req.application_id, username, auth_user["effective_settings"], req.home_name, env_vars, req.language, req.selected_gpu, wayland_mode=req.wayland_mode, session_name=req.session_name,
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@encrypted_router.post("/api/admin/status", response_model=AdminStatusResponse)
async def admin_status(user: dict = Depends(api.verify_token)):
    stats = api._get_system_stats()
    response = {
        "is_admin": user.get("is_admin", False),
        "username": user.get("username"),
        "settings": user.get("effective_settings"),
        "gpus": [],
        "permissions": user.get("permissions") or [],
        "roles": user.get("roles") or [],
        "groups": user.get("groups") or [],
        **stats,
    }
    if user.get("effective_settings", {}).get("gpu", False):
        response["gpus"] = [
            GPUInfo(device=gpu["device"], driver=gpu["driver"])
            for gpu in api.AVAILABLE_GPUS
        ]
    # Global default GPU: an admin-managed fallback applied to users who have
    # no personal default preset. Stored in app_settings under a non-VRECKAN_
    # key, so the environment sync never touches or deletes it.
    async with db.async_session_factory() as session:
        row = (
            await session.execute(
                select(db.AppSetting).where(db.AppSetting.key == "global_default_gpu")
            )
        ).scalar_one_or_none()
    if row is not None:
        response["global_default_gpu"] = row.value
    return response


@encrypted_router.get("/api/app_icon/{app_id}")
async def get_app_icon(app_id: str, user: dict = Depends(api.verify_token)):
    """Serves a custom-uploaded app icon, base64-encoded within a JSON object."""
    if not re.match(r"^[a-zA-Z0-9_-]+$", app_id):
        raise HTTPException(status_code=400, detail="Invalid application ID.")

    icon_path = os.path.abspath(os.path.join(settings.app_icons_path, f"{app_id}.png"))

    if not icon_path.startswith(os.path.abspath(settings.app_icons_path)):
        raise HTTPException(status_code=403, detail="Access denied.")

    if not os.path.exists(icon_path):
        raise HTTPException(status_code=404, detail="Icon not found.")

    try:
        with open(icon_path, "rb") as f:
            icon_data = f.read()
        icon_data_b64 = base64.b64encode(icon_data).decode("utf-8")
        return {"icon_data_b64": icon_data_b64}
    except Exception as e:
        api.logger.error(f"Failed to read or encode icon for app {app_id}: {e}")
        raise HTTPException(status_code=500, detail="Error retrieving icon.")
