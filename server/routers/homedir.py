"""Home-directory endpoints (persistent storage).

Extracted from ``app.api`` during the router split. See ``session.py`` for
notes on the ``api`` module indirection.
"""
from fastapi import APIRouter, Depends, HTTPException, Response
from pydantic import ValidationError

from server import api, user_manager
from server.models import HomeDirectoryCreate, HomeDirectoryList

homedir_router = APIRouter(
    prefix="/api/homedirs",
    dependencies=[Depends(api.verify_persistent_storage_enabled)],
    route_class=api.EncryptedRoute,
)


@homedir_router.get("", response_model=HomeDirectoryList)
async def list_my_home_dirs(user: dict = Depends(api.verify_persistent_storage_enabled)):
    return {"home_dirs": user_manager.get_home_dirs(user["username"])}


@homedir_router.post("", status_code=201)
async def create_my_home_dir(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_persistent_storage_enabled),
):
    # Separate try: Pydantic's ValidationError subclasses ValueError, so a
    # shared block would let the 409 handler swallow validation errors.
    try:
        req = HomeDirectoryCreate(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    try:
        user_manager.create_home_dir(user["username"], req.home_name)
        return {"status": "success", "home_name": req.home_name}
    except ValueError as e:
        raise HTTPException(status_code=409, detail=str(e))


@homedir_router.delete("/{home_name}", status_code=204)
async def delete_my_home_dir(
    home_name: str, user: dict = Depends(api.verify_persistent_storage_enabled)
):
    try:
        user_manager.delete_home_dir(user["username"], home_name)
        return Response(status_code=204)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))
