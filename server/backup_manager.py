"""Backup / restore of the Vreckan application configuration.

A backup is a ``tar.gz`` of the persistent ``/data`` tree (the ``data_root``).
The durable configuration now lives in the configuration database, so a backup
includes a logical JSON dump of that database (``db_dump.json``) together with
the file-based material that is not stored in the DB (SSL material, the
app-store and autostart caches). User home data under ``storage/`` is included
only when ``include_user_data`` is set.
Transient dirs (uploads, ephemeral, the backups dir itself) are excluded.

Restore safely extracts an archive over ``data_root`` (rejecting absolute
paths, ``..`` traversal and any path that escapes the root), restores the
database from the bundled dump, and reloads the durable in-memory state so no
process restart is required. Active sessions / auth tokens are intentionally
left untouched.
"""
import base64
import logging
import os
import re
import tarfile
import uuid
from datetime import datetime

from . import db
from .settings import settings

logger = logging.getLogger(__name__)

# Must match the client-side chunk size (app/static/js/admin.js).
CHUNK_SIZE = 2 * 1024 * 1024  # 2 MiB

# Backup file names look like: backup_YYYYMMDD_HHMMSS.tar.gz
# or backup_YYYYMMDD_HHMMSS_ud.tar.gz (user data included), with an optional
# _<n> suffix to break same-second collisions.
_BACKUP_NAME_RE = re.compile(r"^backup_\d{8}_\d{6}(_ud)?(_\d+)?\.tar\.gz$")

# Sub-directories of storage/ that are transient and never worth backing up.
_TRANSIENT_STORAGE_SUBDIRS = ("vreckan_uploads", "vreckan_ephemeral")

# Name (relative to the data root) of the logical database dump stored inside
# a backup archive.
_DB_DUMP_ARCNAME = "db_dump.json"


class BackupError(Exception):
    """Raised for any backup/restore failure that should surface to the caller."""


def get_backup_dir() -> str:
    path = os.path.abspath(settings.backups_path)
    os.makedirs(path, exist_ok=True, mode=0o700)
    return path


def _data_root() -> str:
    return os.path.abspath(settings.data_root)


def _is_under(path: str, base: str) -> bool:
    path = os.path.abspath(path)
    base = os.path.abspath(base)
    return path == base or path.startswith(base + os.sep)


def _collect_entries(include_user_data: bool):
    """Return a list of (absolute_path, arcname) pairs to include in the tar.

    ``arcname`` is the path relative to ``data_root`` (POSIX separators). Paths
    that are not under ``data_root`` (e.g. a non-container layout where storage
    lives outside the root) are skipped, as is the backups dir itself.
    """
    root = _data_root()
    backup_dir = os.path.abspath(get_backup_dir())
    ssl_dir = os.path.dirname(settings.server_private_key_path)

    # The durable configuration lives in the database; a logical JSON dump of
    # it (written just before the archive is built) is the primary payload.
    # The remaining entries are the file-based pieces that are not stored in
    # the DB.
    candidates = [
        os.path.join(root, _DB_DUMP_ARCNAME),
        settings.autostart_cache_path,
        settings.app_store_cache_path,
        ssl_dir,
    ]
    if include_user_data:
        candidates.append(settings.storage_path)

    entries = []
    for path in candidates:
        abs_path = os.path.abspath(path)
        if not os.path.exists(abs_path):
            continue
        if abs_path == backup_dir or _is_under(abs_path, backup_dir):
            continue  # never back up the backups dir (recursion guard)
        rel = os.path.relpath(abs_path, root)
        if rel == ".." or rel.startswith(".." + os.sep) or os.path.isabs(rel):
            continue  # not under the data root; skip for non-container layouts
        entries.append((abs_path, rel.replace(os.sep, "/")))
    return entries


def _tar_filter(tarinfo):
    """tarfile filter: drop transient / non-config members from the archive."""
    parts = tarinfo.name.split("/")
    if parts and parts[0] == "backups":
        return None
    if (
        parts
        and parts[0] == "storage"
        and len(parts) > 1
        and parts[1] in _TRANSIENT_STORAGE_SUBDIRS
    ):
        return None
    return tarinfo


def _iso_mtime(path: str) -> str:
    return datetime.fromtimestamp(os.path.getmtime(path)).isoformat()


def _apply_retention() -> None:
    """If retention is enabled (N > 0), delete backups beyond the N newest."""
    keep = settings.backup_retention
    if not keep or keep <= 0:
        return  # 0 / unset => keep all
    backup_dir = get_backup_dir()
    try:
        backups = [
            f
            for f in os.listdir(backup_dir)
            if f.startswith("backup_") and f.endswith(".tar.gz")
        ]
    except OSError:
        return
    backups.sort(key=lambda f: os.path.getmtime(os.path.join(backup_dir, f)), reverse=True)
    for old in backups[keep:]:
        try:
            os.remove(os.path.join(backup_dir, old))
            logger.info("Backup retention: removed old backup %s", old)
        except OSError as e:
            logger.warning("Backup retention: could not remove %s: %s", old, e)


async def create_backup(include_user_data: bool = False) -> dict:
    root = _data_root()
    backup_dir = get_backup_dir()
    ts = datetime.now().strftime("%Y%m%d_%H%M%S")
    suffix = "_ud" if include_user_data else ""
    name = f"backup_{ts}{suffix}.tar.gz"
    counter = 0
    while os.path.exists(os.path.join(backup_dir, name)):
        counter += 1
        name = f"backup_{ts}{suffix}_{counter}.tar.gz"
    path = os.path.join(backup_dir, name)

    # Write a consistent logical dump of the configuration database first so
    # the archive captures a single point-in-time snapshot.
    dump_path = os.path.join(root, _DB_DUMP_ARCNAME)
    await db.db_dump(dump_path)

    try:
        entries = _collect_entries(include_user_data)
        if not entries:
            raise BackupError("No configuration paths found to back up.")

        with tarfile.open(path, "w:gz") as tar:
            for abs_path, arcname in entries:
                tar.add(abs_path, arcname=arcname, filter=_tar_filter)
    finally:
        # Don't leave a stale dump in the data root between backups.
        try:
            if os.path.exists(dump_path):
                os.remove(dump_path)
        except OSError:
            pass

    size = os.path.getsize(path)
    logger.info(
        "Created backup %s (%d bytes, user_data=%s)", name, size, include_user_data
    )
    _apply_retention()
    return {
        "name": name,
        "size": size,
        "created_at": _iso_mtime(path),
        "include_user_data": include_user_data,
    }


def list_backups() -> list:
    backup_dir = get_backup_dir()
    out = []
    for f in os.listdir(backup_dir):
        if not (f.startswith("backup_") and f.endswith(".tar.gz")):
            continue
        path = os.path.join(backup_dir, f)
        out.append(
            {
                "name": f,
                "size": os.path.getsize(path),
                "created_at": _iso_mtime(path),
                "include_user_data": "_ud" in f,
            }
        )
    out.sort(key=lambda b: b["created_at"], reverse=True)
    return out


def _resolve_backup(name: str) -> str:
    if not name or not _BACKUP_NAME_RE.match(name):
        raise BackupError("Invalid backup name.")
    backup_dir = os.path.abspath(get_backup_dir())
    path = os.path.abspath(os.path.join(backup_dir, name))
    if not _is_under(path, backup_dir) or not os.path.isfile(path):
        raise BackupError("Backup not found.")
    return path


def get_backup_chunk(name: str, chunk_index: int):
    """Return (raw_chunk_bytes, is_last) for the given backup and chunk index."""
    path = _resolve_backup(name)
    size = os.path.getsize(path)
    if chunk_index < 0:
        raise BackupError("chunk_index must be >= 0")
    start = chunk_index * CHUNK_SIZE
    if start >= size:
        return b"", True
    with open(path, "rb") as f:
        f.seek(start)
        data = f.read(CHUNK_SIZE)
    is_last = (start + len(data)) >= size
    return data, is_last


def delete_backup(name: str) -> None:
    path = _resolve_backup(name)
    os.remove(path)
    logger.info("Deleted backup %s", name)


async def restore_from_stored(name: str) -> dict:
    path = _resolve_backup(name)
    _safe_extract(path)
    await _reload_state()
    logger.info("Restored configuration from stored backup %s", name)
    return {"source": name, "restored": True}


async def restore_from_bytes(data: bytes) -> dict:
    if not data:
        raise BackupError("Empty archive.")
    backup_dir = get_backup_dir()
    tmp = os.path.join(backup_dir, f".restore_tmp_{uuid.uuid4().hex}.tar.gz")
    try:
        with open(tmp, "wb") as f:
            f.write(data)
        _safe_extract(tmp)
        await _reload_state()
        logger.info("Restored configuration from uploaded archive (%d bytes)", len(data))
        return {"source": "upload", "restored": True}
    finally:
        try:
            if os.path.exists(tmp):
                os.remove(tmp)
        except OSError:
            pass


def _validate_archive(tar) -> None:
    root = _data_root()
    for member in tar.getmembers():
        name = member.name
        if os.path.isabs(name):
            raise BackupError(f"Archive contains an absolute path: {name}")
        parts = name.replace("\\", "/").split("/")
        if ".." in parts:
            raise BackupError(f"Archive contains path traversal: {name}")
        target = os.path.abspath(os.path.join(root, name))
        if not (target == root or target.startswith(root + os.sep)):
            raise BackupError(f"Archive path escapes the data root: {name}")


def _safe_extract(archive_path: str) -> None:
    root = _data_root()
    os.makedirs(root, exist_ok=True)
    with tarfile.open(archive_path, "r:gz") as tar:
        _validate_archive(tar)
        try:
            # Python 3.12+ "data" filter: the safest extraction mode. It
            # rejects absolute paths, .. traversal, and links that escape the
            # destination. _validate_archive above gives clearer error messages.
            tar.extractall(path=root, filter="data")
        except TypeError:
            # Python < 3.12 has no `filter` kwarg; rely on _validate_archive.
            tar.extractall(path=root)
        except tarfile.TarError as e:
            raise BackupError(f"Refusing to extract unsafe archive: {e}")


async def _reload_state() -> None:
    """Restore the database (if the archive carried a dump) and refresh the
    durable in-memory state.

    Imported lazily to avoid a circular import (api.py imports this module).
    Active sessions / auth tokens are deliberately NOT reloaded so a restore
    does not disrupt currently-connected users.
    """
    from . import user_manager, volume_mount_manager, api

    # Restore the configuration database from the bundled logical dump, if the
    # archive contains one (older, pre-database backups do not).
    dump_path = os.path.join(_data_root(), _DB_DUMP_ARCNAME)
    if os.path.exists(dump_path):
        await db.db_restore(dump_path)
        logger.info("Backup restore: restored configuration database.")
        try:
            os.remove(dump_path)
        except OSError:
            pass

    await user_manager.load_users_and_groups()
    await volume_mount_manager.load_volume_mounts()
    await api.load_app_configs()
    await api.load_app_templates()
    await api.load_public_shares_metadata()
    logger.info("Backup restore: reloaded in-memory configuration.")
