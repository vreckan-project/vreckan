"""File management endpoints: chunked download, folder creation, async
deletion, file listing, upload finalization, public sharing, and launching
a stored file in an application.

Extracted from ``app.api`` during the router split. Shared module state and
helpers are accessed through the ``api`` module (``api.DELETION_TASKS``,
``api.PUBLIC_SHARES_METADATA``, ``api._safe_rmtree``, ``api.logger``, ...)
rather than imported by name, so test fixtures that mutate ``app.api`` in
place keep working unchanged. Only the auth dependencies and
``EncryptedRoute`` are resolved at import time (inside ``Depends(...)`` /
the router constructor); everything else is looked up on ``api`` at call
time. See ``session.py`` for the full rationale.

``_get_validated_path``, ``_perform_deletion`` and ``CHUNK_SIZE`` live here
rather than in ``app.api`` because they are only used by this router.
"""
import asyncio
import base64
import hashlib
import os
import pathlib
import re
import shutil
import time
import uuid
from typing import List

from fastapi import APIRouter, Depends, HTTPException, Query, Response
from pydantic import ValidationError

from server import api, user_manager
from server.models import (
    CreateFolderRequest,
    DeleteItemsRequest,
    DeleteStatusResponse,
    DeleteTaskResponse,
    FileChunkResponse,
    FileListResponse,
    FinalizeUploadToDirRequest,
    GenericSuccessMessage,
    LaunchFromStorageRequest,
    LaunchResponse,
    PublicShareInfo,
    PublicShareMetadata,
    ShareFileRequest,
)
from server.settings import settings

CHUNK_SIZE = 2 * 1024 * 1024


def _get_validated_path(
    username: str, home_dir: str, sub_path: str, check_existence: bool = True
) -> pathlib.Path:
    if not re.match(r"^[a-zA-Z0-9_-]+$", home_dir):
        raise HTTPException(status_code=400, detail="Invalid home directory name.")

    if home_dir not in user_manager.get_home_dirs(username):
        raise HTTPException(
            status_code=403, detail=f"Access to home directory '{home_dir}' denied."
        )

    base_dir = (pathlib.Path(settings.storage_path) / username / home_dir).resolve()

    if not base_dir.is_dir():
        raise HTTPException(status_code=404, detail="Home directory not found.")

    normalized_sub_path = os.path.normpath(sub_path).lstrip("/")
    if ".." in normalized_sub_path.split(os.path.sep):
        raise HTTPException(
            status_code=403, detail="Directory traversal attempt detected."
        )

    full_path = (base_dir / normalized_sub_path).resolve()

    if base_dir not in full_path.parents and full_path != base_dir:
        raise HTTPException(
            status_code=403, detail="Directory traversal attempt detected."
        )

    if check_existence and not full_path.exists():
        raise HTTPException(status_code=404, detail="Path not found.")

    return full_path


async def _perform_deletion(
    task_id: str, username: str, home_dir: str, paths_to_delete: List[str]
):
    api.DELETION_TASKS[task_id]["status"] = "processing"
    deleted_count = 0
    try:
        for p in paths_to_delete:
            validated_path = _get_validated_path(username, home_dir, p)
            if validated_path.is_dir():
                await asyncio.to_thread(api._safe_rmtree, str(validated_path))
            elif validated_path.is_file():
                await asyncio.to_thread(os.remove, validated_path)
            deleted_count += 1
        api.DELETION_TASKS[task_id].update(
            {
                "status": "completed",
                "message": f"Successfully deleted {deleted_count} items.",
            }
        )
    except HTTPException as e:
        api.DELETION_TASKS[task_id].update({"status": "error", "message": e.detail})
    except Exception as e:
        api.logger.error(f"Deletion task {task_id} failed: {e}")
        api.DELETION_TASKS[task_id].update(
            {"status": "error", "message": "An error occurred during deletion."}
        )


files_router = APIRouter(
    prefix="/api/files",
    dependencies=[Depends(api.verify_persistent_storage_enabled)],
    route_class=api.EncryptedRoute,
)


@files_router.get("/download/chunk/{home_dir}", response_model=FileChunkResponse)
async def download_file_chunk(
    home_dir: str,
    path: str = Query(...),
    chunk_index: int = Query(..., ge=0),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    validated_path = _get_validated_path(user["username"], home_dir, path)
    if not validated_path.is_file():
        raise HTTPException(status_code=404, detail="File not found or is a directory.")

    try:
        with open(validated_path, "rb") as f:
            f.seek(chunk_index * CHUNK_SIZE)
            chunk_data = f.read(CHUNK_SIZE)
            is_last_chunk = len(chunk_data) < CHUNK_SIZE

        return {
            "chunk_data_b64": base64.b64encode(chunk_data).decode("utf-8"),
            "is_last_chunk": is_last_chunk,
        }
    except Exception as e:
        api.logger.error(f"Error reading chunk for file {path}: {e}")
        raise HTTPException(status_code=500, detail="Error reading file chunk.")


@files_router.post("/create_folder/{home_dir}", response_model=GenericSuccessMessage)
async def create_folder(
    home_dir: str,
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    try:
        req = CreateFolderRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")

    validated_path = _get_validated_path(user["username"], home_dir, req.path)
    if not validated_path.is_dir():
        raise HTTPException(status_code=400, detail="Path is not a valid directory.")
    new_folder_path = validated_path / req.folder_name
    if new_folder_path.exists():
        raise HTTPException(
            status_code=409, detail=f"Folder '{req.folder_name}' already exists."
        )
    try:
        new_folder_path.mkdir()
        return {"message": f"Folder '{req.folder_name}' created successfully."}
    except Exception as e:
        raise HTTPException(status_code=500, detail=f"Could not create folder: {e}")


@files_router.post("/delete/{home_dir}", response_model=DeleteTaskResponse)
async def initiate_deletion(
    home_dir: str,
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    try:
        req = DeleteItemsRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")

    task_id = str(uuid.uuid4())
    api.DELETION_TASKS[task_id] = {"status": "pending"}
    asyncio.create_task(
        _perform_deletion(task_id, user["username"], home_dir, req.paths)
    )
    return {"message": "Deletion task started.", "task_id": task_id}


@files_router.get("/delete_status/{task_id}", response_model=DeleteStatusResponse)
async def check_deletion_status(task_id: str, user: dict = Depends(api.verify_token)):
    task = api.DELETION_TASKS.get(task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found.")
    return task


@files_router.get("/list/{home_dir}", response_model=FileListResponse)
async def list_files(
    home_dir: str,
    path: str = Query("/"),
    page: int = Query(1, ge=1),
    per_page: int = Query(50, ge=1, le=200),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    validated_path = _get_validated_path(user["username"], home_dir, path)
    if not validated_path.is_dir():
        raise HTTPException(status_code=400, detail="Path is not a valid directory.")

    try:
        all_items = sorted(
            validated_path.iterdir(), key=lambda p: (not p.is_dir(), p.name.lower())
        )
    except OSError as e:
        raise HTTPException(status_code=500, detail=f"Error reading directory: {e}")

    start = (page - 1) * per_page
    end = start + per_page
    paginated_items = all_items[start:end]

    home_dir_root = pathlib.Path(settings.storage_path) / user["username"] / home_dir

    response_items = []
    for item in paginated_items:
        stat = item.stat()
        item_path = f"/{item.relative_to(home_dir_root)}".replace("\\", "/")
        if str(item.relative_to(home_dir_root)) == ".":
            item_path = "/"

        response_items.append(
            {
                "name": item.name,
                "path": item_path,
                "is_dir": item.is_dir(),
                "size": stat.st_size,
                "mtime": stat.st_mtime,
            }
        )

    return {
        "items": response_items,
        "total": len(all_items),
        "page": page,
        "per_page": per_page,
        "path": path,
    }


@files_router.post("/upload_to_dir/{home_dir}", response_model=GenericSuccessMessage)
async def finalize_upload_to_dir(
    home_dir: str,
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    try:
        req = FinalizeUploadToDirRequest(**decrypted_body)

        dest_dir_path = _get_validated_path(
            user["username"], home_dir, req.path, check_existence=True
        )
        if not dest_dir_path.is_dir():
            raise HTTPException(
                status_code=400, detail="Destination path is not a valid directory."
            )

        reassembled_file_path = await api._reassemble_file(
            req.upload_id, req.total_chunks, req.filename
        )

        safe_filename = os.path.basename(req.filename)
        actual_filename = await asyncio.to_thread(
            api._get_unique_filename, str(dest_dir_path), safe_filename
        )
        final_location = dest_dir_path / actual_filename

        try:
            await asyncio.to_thread(
                shutil.move, reassembled_file_path, str(final_location)
            )
            await asyncio.to_thread(os.chmod, str(final_location), 0o644)
            api.logger.info(
                f"User '{user['username']}' uploaded '{actual_filename}' to '{home_dir}{req.path}'"
            )
            return {"message": "File uploaded successfully."}
        except Exception as e:
            if os.path.exists(reassembled_file_path):
                os.remove(reassembled_file_path)
            api.logger.error(
                f"Failed to move finalized upload for user '{user['username']}': {e}"
            )
            raise HTTPException(
                status_code=500, detail="Could not place file in destination."
            )

    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")


@files_router.post("/share", response_model=PublicShareInfo)
async def create_public_share(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_public_sharing_enabled),
):
    try:
        req = ShareFileRequest(**decrypted_body)
        username = user["username"]

        source_path = _get_validated_path(username, req.home_dir, req.path)
        if not source_path.is_file():
            raise HTTPException(
                status_code=400, detail="Path does not point to a file."
            )

        share_id = str(uuid.uuid4())
        dest_path = os.path.join(settings.public_storage_path, share_id)

        await asyncio.to_thread(shutil.copy, source_path, dest_path)

        stat_info = source_path.stat()

        password_hash = None
        if req.password:
            # argon2id for new shares (memory-hard, resists offline brute
            # force); legacy SHA-256 shares are still verified on read.
            password_hash = user_manager.hash_share_password(req.password)

        expiry_timestamp = None
        if req.expiry_hours is not None and req.expiry_hours > 0:
            expiry_timestamp = time.time() + (req.expiry_hours * 3600)

        metadata = PublicShareMetadata(
            owner_username=username,
            original_filename=source_path.name,
            created_at=time.time(),
            size_bytes=stat_info.st_size,
            password_hash=password_hash,
            expiry_timestamp=expiry_timestamp,
        )

        api.PUBLIC_SHARES_METADATA[share_id] = metadata
        await api.save_public_shares_metadata()

        return PublicShareInfo(
            share_id=share_id,
            original_filename=metadata.original_filename,
            size_bytes=metadata.size_bytes,
            created_at=metadata.created_at,
            expiry_timestamp=metadata.expiry_timestamp,
            has_password=bool(metadata.password_hash),
            url=f"/public/{share_id}",
        )

    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    except HTTPException:
        raise
    except Exception as e:
        api.logger.error(f"Failed to create share for user '{user['username']}': {e}")
        raise HTTPException(status_code=500, detail="Failed to create share.")


@files_router.get("/shares", response_model=List[PublicShareInfo])
async def list_public_shares(user: dict = Depends(api.verify_public_sharing_enabled)):
    user_shares = [
        PublicShareInfo(
            share_id=sid,
            url=f"/public/{sid}",
            has_password=bool(meta.password_hash),
            **meta.model_dump(),
        )
        for sid, meta in api.PUBLIC_SHARES_METADATA.items()
        if meta.owner_username == user["username"]
    ]
    return sorted(user_shares, key=lambda s: s.created_at, reverse=True)


@files_router.delete("/share/{share_id}", status_code=204)
async def delete_public_share(
    share_id: str, user: dict = Depends(api.verify_public_sharing_enabled)
):
    if (
        metadata := api.PUBLIC_SHARES_METADATA.get(share_id)
    ) and metadata.owner_username == user["username"]:
        if os.path.exists(
            public_file_path := os.path.join(settings.public_storage_path, share_id)
        ):
            try:
                os.remove(public_file_path)
            except OSError as e:
                api.logger.error(f"Error deleting share file for {share_id}: {e}")
        del api.PUBLIC_SHARES_METADATA[share_id]
        await api.save_public_shares_metadata()
        return Response(status_code=204)
    raise HTTPException(
        status_code=404 if not metadata else 403,
        detail="Share not found or permission denied.",
    )

@files_router.post("/launch_from_storage", response_model=LaunchResponse)
async def launch_from_storage(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    """Open a file that already lives in one of the user's home directories in
    an application. The file is copied into the session's shared-files area
    (mounted at Desktop/files in the container) and, if requested, opened."""
    try:
        req = LaunchFromStorageRequest(**decrypted_body)
        validated = _get_validated_path(user["username"], req.home_dir, req.path)
        if not validated.is_file():
            raise HTTPException(
                status_code=400, detail="Only files can be opened in an application."
            )
        file_bytes = await asyncio.to_thread(validated.read_bytes)
        return await api._launch_common(
            req.application_id,
            user["username"],
            user["effective_settings"],
            req.home_dir,
            {},
            req.language,
            req.selected_gpu,
            file_bytes=file_bytes,
            filename=validated.name,
            open_file_on_launch=req.open_file_on_launch,
            wayland_mode=req.wayland_mode,
        )
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
