"""Upload endpoints: initiate, chunked upload, and finalize to storage.

Extracted from ``app.api`` during the router split. Shared module state and
helpers are accessed through the ``api`` module (``api._reassemble_file``,
``api.logger``, ...) rather than imported by name, so test fixtures that
mutate ``app.api`` in place keep working unchanged. Only the auth
dependencies and ``EncryptedRoute`` are resolved at import time (inside
``Depends(...)`` / the router constructor); everything else is looked up on
``api`` at call time. See ``session.py`` for the full rationale.
"""
import asyncio
import base64
import json
import os
import shutil
import time
import uuid

from fastapi import APIRouter, Depends, HTTPException
from pydantic import ValidationError

from server import api, user_manager
from server.models import (
    UploadChunkRequest,
    UploadInitiateRequest,
    UploadInitiateResponse,
    UploadToStorageRequest,
)
from server.settings import settings

upload_router = APIRouter(
    prefix="/api/upload",
    dependencies=[Depends(api.verify_token)],
    route_class=api.EncryptedRoute,
)


@upload_router.post("/initiate", response_model=UploadInitiateResponse)
async def upload_initiate(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = UploadInitiateRequest(**decrypted_body)
        upload_id = str(uuid.uuid4())
        upload_path = os.path.join(settings.upload_dir, upload_id)
        os.makedirs(upload_path, exist_ok=True, mode=0o700)

        with open(os.path.join(upload_path, "metadata.json"), "w") as f:
            json.dump(
                {
                    "filename": req.filename,
                    "size": req.total_size,
                    "started": time.time(),
                },
                f,
            )

        return {"upload_id": upload_id}
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@upload_router.post("/chunk")
async def upload_chunk(decrypted_body: dict = Depends(api.get_decrypted_request_body)):
    try:
        req = UploadChunkRequest(**decrypted_body)
        upload_path = api._validated_upload_path(req.upload_id)

        chunk_path = os.path.join(upload_path, f"chunk_{req.chunk_index}")
        with open(chunk_path, "wb") as f:
            f.write(base64.b64decode(req.chunk_data_b64))

        return {"status": "ok", "chunk_index": req.chunk_index}
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    except (ValueError, TypeError) as e:
        raise HTTPException(status_code=400, detail=f"Invalid Base64 chunk data: {e}")


@upload_router.post(
    "/to_storage", dependencies=[Depends(api.verify_persistent_storage_enabled)]
)
async def upload_to_storage(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    try:
        req = UploadToStorageRequest(**decrypted_body)
        username = user["username"]

        available_homes = user_manager.get_home_dirs(username)
        if req.home_name not in available_homes:
            raise HTTPException(
                status_code=404,
                detail=f"Home directory '{req.home_name}' not found for user.",
            )

        reassembled_file_path = await api._reassemble_file(
            req.upload_id, req.total_chunks, req.filename
        )

        safe_filename = os.path.basename(req.filename)
        file_dest_dir = os.path.join(
            settings.storage_path, username, "_vreckan_shared_files"
        )

        await asyncio.to_thread(os.makedirs, file_dest_dir, exist_ok=True, mode=0o755)

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
            api.logger.error(
                f"Failed to move reassembled file to session storage for user '{username}': {e}"
            )
            raise HTTPException(
                status_code=500, detail="Could not place file in session storage."
            )

        api.logger.info(
            f"User '{username}' uploaded file '{actual_filename}' (as '{safe_filename}') to shared storage."
        )
        return {
            "status": "success",
            "message": f"File '{safe_filename}' uploaded successfully.",
        }

    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
