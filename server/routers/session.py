"""Session endpoints: list my sessions, readiness probe, stop, send file.

Extracted from ``app.api`` during the router split. Shared module state and
helpers are accessed through the ``api`` module (``api.SESSIONS_DB``,
``api._stop_session``, ...) rather than imported by name, so test fixtures
that mutate ``app.api`` in place (e.g. ``SESSIONS_DB.clear()``) keep working
unchanged. Only the auth dependencies and ``EncryptedRoute`` are resolved at
import time (inside ``Depends(...)`` / the router constructor); everything
else is looked up on ``api`` at call time.
"""
import asyncio
import os
import shutil
from typing import List

import httpx2
from fastapi import APIRouter, Depends, HTTPException, Response
from pydantic import ValidationError

from server import api
from server.models import (
    ActiveSessionInfo,
    SendFileToSessionRequest,
    SessionStatusResponse,
)
from server.settings import settings

session_router = APIRouter(
    prefix="/api/sessions",
    dependencies=[Depends(api.verify_token)],
    route_class=api.EncryptedRoute,
)


@session_router.get("", response_model=List[ActiveSessionInfo])
async def get_my_sessions(user: dict = Depends(api.verify_token)):
    user_sessions = []
    for sid, s_data in api.SESSIONS_DB.items():
        if s_data.get("username") == user["username"]:
            user_sessions.append(
                ActiveSessionInfo(
                    session_id=sid,
                    app_id=s_data["provider_app_id"],
                    app_name=s_data["app_name"],
                    app_logo=s_data["app_logo"],
                    created_at=s_data["created_at"],
                    session_url=(
                        f"/room/{sid}?token={s_data['controller_token']}"
                        if s_data.get("is_collaboration")
                        else f"/api/apps/session/{sid}/?access_token={s_data['access_token']}"
                    ),
                    launch_context=s_data.get("launch_context"),
                    is_collaboration=s_data.get("is_collaboration", False),
                    name=s_data.get("name"),
                )
            )
    return sorted(user_sessions, key=lambda s: s.created_at, reverse=True)


@session_router.get("/{session_id}/status", response_model=SessionStatusResponse)
async def get_session_status(session_id: str, user: dict = Depends(api.verify_token)):
    """Readiness probe for a session: the server polls the app container's
    upstream directly (it shares the Docker network), so the client can wait
    until the app inside the container actually answers before opening it.
    """
    session_data = api.SESSIONS_DB.get(session_id)
    if not session_data or session_data.get("username") != user["username"]:
        raise HTTPException(
            status_code=404, detail="Session not found or permission denied."
        )
    ip = session_data.get("ip")
    port = session_data.get("port")
    if not ip or not port:
        return SessionStatusResponse(ready=False, detail="No upstream address recorded.")
    try:
        async with httpx2.AsyncClient(timeout=3.0) as client:
            resp = await client.get(f"http://{ip}:{port}/")
        if resp.status_code < 500:
            return SessionStatusResponse(ready=True, detail=f"HTTP {resp.status_code}")
        return SessionStatusResponse(ready=False, detail=f"HTTP {resp.status_code}")
    except Exception as e:
        return SessionStatusResponse(ready=False, detail=f"unreachable ({type(e).__name__})")


@session_router.delete("/{session_id}", status_code=204)
async def stop_my_session(session_id: str, user: dict = Depends(api.verify_token)):
    session_data = api.SESSIONS_DB.get(session_id)
    if not session_data or session_data.get("username") != user["username"]:
        raise HTTPException(
            status_code=404, detail="Session not found or permission denied."
        )
    await api._stop_session(session_id)
    return Response(status_code=204)


@session_router.post("/{session_id}/send_file")
async def send_file_to_session(
    session_id: str,
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_token),
):
    try:
        req = SendFileToSessionRequest(**decrypted_body)
        session_data = api.SESSIONS_DB.get(session_id)
        if not session_data or session_data.get("username") != user["username"]:
            raise HTTPException(
                status_code=404, detail="Session not found or permission denied."
            )

        host_mount_path = session_data.get("host_mount_path")
        if not host_mount_path:
            raise HTTPException(
                status_code=400,
                detail="Cannot send files to this session as it has no mounted storage.",
            )

        reassembled_file_path = await api._reassemble_file(
            req.upload_id, req.total_chunks, req.filename
        )
        safe_filename = os.path.basename(req.filename)

        is_persistent = host_mount_path and not host_mount_path.startswith(
            os.path.join(settings.storage_path, "vreckan_ephemeral")
        )

        if is_persistent:
            file_dest_dir = os.path.abspath(
                os.path.join(
                    settings.storage_path, user["username"], "_vreckan_shared_files"
                )
            )
        else:
            file_dest_dir = os.path.join(host_mount_path, "Desktop", "files")

        os.makedirs(file_dest_dir, exist_ok=True, mode=0o755)

        actual_filename = await asyncio.to_thread(
            api._get_unique_filename, file_dest_dir, safe_filename
        )
        file_location = os.path.join(file_dest_dir, actual_filename)

        try:
            await asyncio.to_thread(shutil.move, reassembled_file_path, file_location)
            await asyncio.to_thread(os.chmod, file_location, 0o644)
        except Exception as e:
            if os.path.exists(reassembled_file_path):
                os.remove(reassembled_file_path)
            api.logger.error(f"Failed to move reassembled file to session storage: {e}")
            raise HTTPException(
                status_code=500, detail="Could not place file in session storage."
            )

        api.logger.info(
            f"[{session_id}] User '{user['username']}' wrote file '{actual_filename}' (as '{safe_filename}') to session."
        )
        return {
            "status": "success",
            "message": f"File '{safe_filename}' sent to session.",
        }
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
