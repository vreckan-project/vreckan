"""Manages admin-defined external volume mounts.

A volume mount binds a directory that exists on the Docker HOST into the app
containers that are spawned for a given user (directly) or for every member of
a group. Each mount is stored as a row in the configuration database.

Mount shape (YAML / in-memory dict)::

    name: depot
    host_path: /mnt/NVME/apps/offline-depot/depot
    container_path: depot        # relative to the home-dir mount point
    read_only: true
    scope: group                 # "none" | "user" | "group"
    target: vdi-users            # username (scope=user) or group (scope=group)

Container paths are relative to the in-container home-dir mount point
(``settings.container_config_path``) unless they are absolute.
"""
import logging
from sqlalchemy import delete, select

from . import db

logger = logging.getLogger(__name__)

# name -> mount dict
VOLUME_MOUNTS: dict = {}

VALID_SCOPES = ("none", "user", "group")


async def load_volume_mounts() -> None:
    """Load all volume mounts from the configuration database into memory."""
    global VOLUME_MOUNTS
    VOLUME_MOUNTS.clear()
    async with db.async_session_factory() as session:
        for m in (await session.execute(select(db.VolumeMount))).scalars():
            VOLUME_MOUNTS[m.name] = {
                "name": m.name,
                "host_path": m.host_path,
                "container_path": m.container_path,
                "read_only": m.read_only,
                "scope": m.scope,
                "target": m.target,
            }
    logger.info(f"Loaded {len(VOLUME_MOUNTS)} volume mount(s) from the database.")


def get_all_volume_mounts() -> list:
    return list(VOLUME_MOUNTS.values())


def get_volume_mount(name: str):
    return VOLUME_MOUNTS.get(name)


async def write_volume_mount(name: str, data: dict) -> None:
    """Persist a mount (create or overwrite) in the database and in memory."""
    data = dict(data)
    data["name"] = name
    VOLUME_MOUNTS[name] = data
    async with db.async_session_factory() as session:
        mount = (
            await session.execute(select(db.VolumeMount).where(db.VolumeMount.name == name))
        ).scalar_one_or_none()
        if mount is None:
            mount = db.VolumeMount(name=name)
            session.add(mount)
        mount.host_path = data.get("host_path", "")
        mount.container_path = data.get("container_path", "")
        mount.read_only = data.get("read_only", False)
        mount.scope = data.get("scope", "none")
        mount.target = data.get("target", "")
        await session.commit()
    logger.info(f"Wrote volume mount '{name}'.")


async def delete_volume_mount(name: str) -> None:
    VOLUME_MOUNTS.pop(name, None)
    async with db.async_session_factory() as session:
        await session.execute(delete(db.VolumeMount).where(db.VolumeMount.name == name))
        await session.commit()
    logger.info(f"Deleted volume mount '{name}'.")


def get_mounts_for_user(username: str, group: str) -> list:
    """Return the mounts that apply to ``username``: those assigned to the user
    directly (scope=user) plus those assigned to the user's group (scope=group).
    Mounts with scope=none are inactive and never returned."""
    result = []
    for name, m in VOLUME_MOUNTS.items():
        scope = m.get("scope", "none")
        target = m.get("target", "")
        if scope == "user" and target == username:
            result.append(m)
        elif scope == "group" and target and target == group:
            result.append(m)
    return result
