"""Pinned launch-preset endpoints.

Extracted from ``app.api`` during the router split. See ``session.py`` for
notes on the ``api`` module indirection.
"""
import time
import uuid
from typing import List

from fastapi import APIRouter, Depends, HTTPException, Response
from pydantic import ValidationError
from sqlalchemy import select

from server import api, db
from server.models import CreatePinnedBehaviorRequest, PinnedBehavior

pinned_router = APIRouter(
    prefix="/api/pinned",
    dependencies=[Depends(api.verify_token)],
    route_class=api.EncryptedRoute,
)


@pinned_router.get("", response_model=List[PinnedBehavior])
async def list_pinned_behaviors(user: dict = Depends(api.verify_token)):
    async with db.async_session_factory() as session:
        rows = (
            await session.execute(
                select(db.PinnedBehavior)
                .where(db.PinnedBehavior.username == user["username"])
                .order_by(db.PinnedBehavior.created_at.desc())
            )
        ).scalars().all()
    out = []
    for r in rows:
        d = dict(r.data or {})
        d["id"] = r.id
        d["created_at"] = r.created_at
        out.append(PinnedBehavior(**d))
    return out


@pinned_router.post("", response_model=PinnedBehavior, status_code=201)
async def create_pinned_behavior(
    decrypted_body: dict = Depends(api.get_decrypted_request_body),
    user: dict = Depends(api.verify_token),
):
    try:
        req = CreatePinnedBehaviorRequest(**decrypted_body)
    except ValidationError as e:
        raise HTTPException(status_code=422, detail=f"Invalid request body: {e}")
    new_id = str(uuid.uuid4())
    now = time.time()
    data = {
        "name": req.name,
        "application_id": req.application_id,
        "home_name": req.home_name,
        "language": req.language,
        "selected_gpu": req.selected_gpu,
        "launch_in_room_mode": req.launch_in_room_mode,
        "wayland_mode": req.wayland_mode,
        "trigger_type": req.trigger_type,
        "trigger_value": req.trigger_value,
        "is_default": bool(req.is_default),
    }
    async with db.async_session_factory() as session:
        if req.is_default:
            # Only one default preset per user: clear the flag on any others so
            # the new preset becomes the single per-user default (all apps).
            rows = (
                await session.execute(
                    select(db.PinnedBehavior).where(db.PinnedBehavior.username == user["username"])
                )
            ).scalars().all()
            for r in rows:
                if (r.data or {}).get("is_default"):
                    r.data = {**(r.data or {}), "is_default": False}
        session.add(
            db.PinnedBehavior(id=new_id, username=user["username"], created_at=now, data=data)
        )
        await session.commit()
    return PinnedBehavior(id=new_id, created_at=now, **data)


@pinned_router.post("/{pinned_id}/set-default", status_code=200)
async def set_default_pinned_behavior(pinned_id: str, user: dict = Depends(api.verify_token)):
    """Make this preset the user's default (applies to all apps) and clear the
    flag on every other preset. Idempotent. No request body (path-param only)."""
    async with db.async_session_factory() as session:
        rows = (
            await session.execute(
                select(db.PinnedBehavior).where(db.PinnedBehavior.username == user["username"])
            )
        ).scalars().all()
        target = None
        for r in rows:
            d = dict(r.data or {})
            if r.id == pinned_id:
                d["is_default"] = True
                target = r
            else:
                d["is_default"] = False
            r.data = d
        if target is None:
            raise HTTPException(status_code=404, detail="Pinned behaviour not found.")
        await session.commit()
    return {"ok": True}


@pinned_router.delete("/{pinned_id}", status_code=204)
async def delete_pinned_behavior(pinned_id: str, user: dict = Depends(api.verify_token)):
    async with db.async_session_factory() as session:
        row = (
            await session.execute(
                select(db.PinnedBehavior).where(db.PinnedBehavior.id == pinned_id)
            )
        ).scalar_one_or_none()
        if not row or row.username != user["username"]:
            raise HTTPException(status_code=404, detail="Pinned behaviour not found.")
        await session.delete(row)
        await session.commit()
    return Response(status_code=204)
