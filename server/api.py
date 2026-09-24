import base64
import json
import os
import shutil
import uuid
import hashlib
import time
import secrets
import yaml
import logging
import pathlib
import subprocess
import tempfile
import re
import mimetypes
from typing import Callable, Dict, List, Optional
import asyncio
from contextlib import asynccontextmanager
import httpx2
import websockets
from collections import defaultdict
import docker
from docker.errors import DockerException, NotFound

from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
from fastapi import (
    FastAPI,
    Depends,
    HTTPException,
    Request,
    Response,
    APIRouter,
    Query,
    Form,
)
from fastapi.responses import (
    JSONResponse,
    StreamingResponse,
    FileResponse,
    HTMLResponse,
    RedirectResponse,
)
from fastapi.routing import APIRoute
from starlette.websockets import WebSocket, WebSocketState
from jose import JWTError, jwt
from pydantic import ValidationError
from sqlalchemy import delete, func, select, update

from .settings import settings
from .models import *
from .providers.docker_provider import DockerProvider
from . import user_manager
from . import volume_mount_manager
from . import backup_manager
from . import collaboration
from . import db

logger = logging.getLogger(__name__)

SESSIONS_LOCK = asyncio.Lock()
SESSIONS_DB: Dict[str, Dict] = {}
CRYPTO_SESSIONS: Dict[str, bytes] = {}
INSTALLED_APPS: Dict[str, InstalledApp] = {}
APP_STORES: List[AppStore] = []
APP_TEMPLATES: Dict[str, Dict] = {}
# The application-template editor schema (categorised env-var definitions) ships
# inside the image and is served to the admin UI at /api/admin/apps/templates/schema.
TEMPLATE_SCHEMA_PATH = os.path.join(os.path.dirname(__file__), "template_schema.yml")
# The bundled YAML is the image baseline: the startup reconciliation uses it
# to add newly-shipped settings and to reset each built-in setting's
# ``default`` column if it drifted. Kept separately from TEMPLATE_SCHEMA,
# which is replaced by the database contents at startup.
TEMPLATE_SCHEMA_YAML: List[Dict] = []
try:
    with open(TEMPLATE_SCHEMA_PATH, "r") as _f:
        TEMPLATE_SCHEMA = yaml.safe_load(_f) or {}
    TEMPLATE_SCHEMA_YAML = list(TEMPLATE_SCHEMA.get("settings") or [])
except Exception as _e:  # pragma: no cover - schema is bundled with the image
    logger.warning(f"Could not load template schema ({_e}); editor will be limited.")
    TEMPLATE_SCHEMA = {"settings": []}
PROVIDER_CACHE: Dict[str, DockerProvider] = {}
AVAILABLE_GPUS: List[Dict] = []
# Global default GPU (a device path from AVAILABLE_GPUS): the fallback
# gpu_config applied to launches that have no personal GPU pick. Persisted
# in app_settings under a non-VRECKAN_ key; edited on the Global Settings page.
GLOBAL_DEFAULT_GPU: Optional[str] = None
PUBLIC_SHARES_METADATA: Dict[str, PublicShareMetadata] = {}
IMAGE_METADATA: Dict[str, Dict] = {}
DELETION_TASKS: Dict[str, Dict] = {}
PULL_STATUS: Dict[str, str] = {}
# Live download progress per image while a pull is in flight. Values are
# plain dicts ({"status", "percentage", "current", "total", "_layers"}) that
# are mutated from a worker thread and read by the admin API. "_layers" is
# internal bookkeeping and is stripped before being sent to clients.
PULL_PROGRESS: Dict[str, Dict] = {}
SYSTEM_STATS_CACHE: Dict[str, any] = {"data": None, "timestamp": 0}
CPU_MODEL: str = "Unknown"
PATH_PREFIX_MAP: Dict[str, str] = {}
DISCOVERED_API_PORT: int = settings.api_port
DISCOVERED_SESSION_PORT: int = settings.session_port
DISCOVERED_NETWORK: Optional[str] = None
METADATA_LOCK = asyncio.Lock()
DOWNLOAD_TOKENS: dict = {}

# Web-login (server-issued) auth tokens, keyed by the opaque token string.
# Each value: {"username": str, "expires_at": float}. These are the sole
# credential accepted by verify_token().
AUTH_TOKENS_LOCK = asyncio.Lock()
AUTH_TOKENS: Dict[str, Dict] = {}

# OIDC / SSO: in-memory authorization states, keyed by the opaque `state`
# string returned to the provider. Each value: {"expires_at": float}.
# Consumed exactly once at the callback.
OIDC_STATES: Dict[str, Dict] = {}
OIDC_STATES_LOCK = asyncio.Lock()

# Cache of OIDC discovery documents, keyed by issuer URL, to avoid re-fetching
# on every authorize click. Value is the parsed discovery JSON (dict).
OIDC_DISCOVERY_CACHE: Dict[str, Dict] = {}

try:
    with open(settings.server_private_key_path, "rb") as f:
        SERVER_PRIVATE_KEY = serialization.load_pem_private_key(f.read(), password=None)
    SERVER_PUBLIC_KEY_PEM = (
        SERVER_PRIVATE_KEY.public_key()
        .public_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PublicFormat.SubjectPublicKeyInfo,
        )
        .decode("utf-8")
    )
    user_manager.set_server_public_key(SERVER_PUBLIC_KEY_PEM)
except FileNotFoundError as e:
    logger.error(f"Key file not found: {e.filename}. Exiting.")
    exit(1)


async def save_sessions_to_disk():
    async with SESSIONS_LOCK:
        try:
            async with db.async_session_factory() as session:
                async with session.begin():
                    await session.execute(delete(db.SessionRecord))
                    for session_id, data in SESSIONS_DB.items():
                        session.add(db.SessionRecord(session_id=session_id, data=data))
            logger.info(f"Successfully saved {len(SESSIONS_DB)} session(s) to the database.")
        except Exception as e:
            logger.error(f"Failed to save sessions to the database: {e}")


async def load_sessions_from_disk():
    global SESSIONS_DB
    async with SESSIONS_LOCK:
        try:
            async with db.async_session_factory() as session:
                rows = (await session.execute(select(db.SessionRecord))).scalars().all()
            SESSIONS_DB.clear()
            for row in rows:
                SESSIONS_DB[row.session_id] = row.data
            logger.info(f"Loaded {len(SESSIONS_DB)} session(s) from the database.")
        except Exception as e:
            logger.error(f"Failed to load sessions from the database: {e}")


async def save_auth_tokens_to_disk():
    """Atomically persists the web-login token store to the database."""
    async with AUTH_TOKENS_LOCK:
        snapshot = dict(AUTH_TOKENS)
        now = time.time()
        # Drop expired tokens while we hold the lock.
        expired = [t for t, v in snapshot.items() if v.get("expires_at", 0) < now]
        for t in expired:
            snapshot.pop(t, None)
        try:
            async with db.async_session_factory() as session:
                async with session.begin():
                    await session.execute(delete(db.AuthToken))
                    for token, v in snapshot.items():
                        session.add(
                            db.AuthToken(
                                token=token,
                                username=v.get("username", ""),
                                expires_at=v.get("expires_at", 0),
                            )
                        )
            logger.info(f"Saved {len(snapshot)} active web-login token(s) to the database.")
        except Exception as e:
            logger.error(f"Failed to save auth tokens to the database: {e}")


async def load_auth_tokens_from_disk():
    """Loads the web-login token store from the database, pruning expired entries."""
    global AUTH_TOKENS
    try:
        async with db.async_session_factory() as session:
            rows = (await session.execute(select(db.AuthToken))).scalars().all()
        now = time.time()
        valid = {
            r.token: {"username": r.username, "expires_at": r.expires_at}
            for r in rows
            if r.expires_at > now
        }
        async with AUTH_TOKENS_LOCK:
            AUTH_TOKENS.clear()
            AUTH_TOKENS.update(valid)
        logger.info(f"Loaded {len(valid)} active web-login token(s) from the database.")
    except Exception as e:
        logger.error(f"Failed to load auth tokens from the database: {e}")


async def issue_auth_token(username: str) -> str:
    """Creates a new opaque web-login token for a user and stores it in memory.

    Returns the token string. Caller is responsible for persisting (via
    save_auth_tokens_to_disk) and setting the cookie.
    """
    token = secrets.token_urlsafe(32)
    expires_at = time.time() + settings.auth_token_ttl_seconds
    async with AUTH_TOKENS_LOCK:
        AUTH_TOKENS[token] = {"username": username, "expires_at": expires_at}
    return token


async def revoke_auth_token(token: Optional[str]) -> None:
    """Removes a token from the in-memory store (no-op if absent)."""
    if not token:
        return
    async with AUTH_TOKENS_LOCK:
        AUTH_TOKENS.pop(token, None)


async def revoke_all_tokens_for_user(username: str) -> int:
    """Revokes ALL active web-login tokens belonging to a user.

    Returns the number of tokens revoked. Used by the admin
    force-logout endpoint to invalidate a user's web sessions
    server-side (their cookie stops working even if it leaked).
    """
    async with AUTH_TOKENS_LOCK:
        to_revoke = [
            t for t, v in AUTH_TOKENS.items() if v.get("username") == username
        ]
        for t in to_revoke:
            del AUTH_TOKENS[t]
    return len(to_revoke)


def auth_cookie_kwargs(token: Optional[str] = None, request: Optional[Request] = None) -> dict:
    """Builds the Set-Cookie attributes for the web-login cookie.

    When `token` is None the cookie is cleared (Max-Age=0).

    The ``Secure`` flag mirrors the request scheme: enabled on HTTPS, and
    disabled on plain HTTP so the cookie is actually usable in local / test
    environments (TestClient uses an ``http://`` origin, which would drop a
    ``Secure`` cookie).
    """
    secure = True
    if request is not None:
        secure = (request.url.scheme == "https")
    max_age = settings.auth_token_ttl_seconds if token else 0
    return {
        "key": settings.auth_cookie_name,
        "value": token or "",
        "max_age": max_age,
        "httponly": True,
        "secure": secure,
        "samesite": "lax",
        "path": "/",
    }


async def _fetch_and_cache_single_script(
    store_name: str, base_url: str, app_id: str, suffix: str = ""
):
    cache_dir = os.path.join(settings.autostart_cache_path, store_name)
    os.makedirs(cache_dir, exist_ok=True)
    cache_file_path = os.path.join(cache_dir, f"{app_id}{suffix}")
    meta_file_path = cache_file_path + ".meta"
    headers = {}

    if os.path.exists(meta_file_path):
        try:
            with open(meta_file_path, "r") as f:
                meta = json.load(f)
                if "etag" in meta:
                    headers["If-None-Match"] = meta["etag"]
        except (json.JSONDecodeError, IOError):
            logger.warning(f"Could not read meta file for {app_id}{suffix}")

    autostart_url = f"{base_url}/autostart/{app_id}{suffix}"

    try:
        async with httpx2.AsyncClient(follow_redirects=True) as client:
            response = await client.get(autostart_url, timeout=10, headers=headers)

            if response.status_code == 304:
                logger.debug(
                    f"Autostart script for '{app_id}{suffix}' in store '{store_name}' is up to date."
                )
                return

            if response.status_code == 404:
                with open(cache_file_path, "w") as f:
                    f.write("")
                if os.path.exists(meta_file_path):
                    os.remove(meta_file_path)
                return

            response.raise_for_status()
            script_content = response.text

            def write_cache_and_meta():
                with open(cache_file_path, "w") as f:
                    f.write(script_content)
                if "etag" in response.headers:
                    with open(meta_file_path, "w") as f:
                        json.dump({"etag": response.headers["etag"]}, f)

            await asyncio.to_thread(write_cache_and_meta)
            logger.info(
                f"Successfully cached autostart script for '{app_id}{suffix}' from store '{store_name}'."
            )

    except httpx2.RequestError as e:
        logger.warning(f"Failed to fetch autostart script for '{app_id}{suffix}': {e}")
    except Exception as e:
        logger.error(
            f"Unexpected error updating autostart script for '{app_id}{suffix}': {e}"
        )


async def _fetch_and_cache_app_store(store: AppStore):
    cache_file_path = os.path.join(settings.app_store_cache_path, f"{store.name}.yml")
    meta_file_path = cache_file_path + ".meta"
    headers = {}

    if os.path.exists(meta_file_path):
        try:
            with open(meta_file_path, "r") as f:
                meta = json.load(f)
                if "etag" in meta:
                    headers["If-None-Match"] = meta["etag"]
        except (json.JSONDecodeError, IOError):
            pass

    try:
        async with httpx2.AsyncClient(follow_redirects=True) as client:
            response = await client.get(store.url, timeout=15, headers=headers)

            if response.status_code == 304:
                logger.debug(f"App store '{store.name}' is up to date.")
                return

            response.raise_for_status()

            try:
                yaml.safe_load(response.text)
            except yaml.YAMLError:
                logger.error(
                    f"Invalid YAML content for store '{store.name}'. Skipping cache."
                )
                return

            def write_cache_and_meta():
                with open(cache_file_path, "w") as f:
                    f.write(response.text)
                if "etag" in response.headers:
                    with open(meta_file_path, "w") as f:
                        json.dump({"etag": response.headers["etag"]}, f)

            await asyncio.to_thread(write_cache_and_meta)
            logger.info(f"Successfully cached app store '{store.name}'.")

    except Exception as e:
        logger.error(f"Failed to update cache for app store '{store.name}': {e}")


async def _update_all_app_store_caches():
    logger.info("Starting app store cache refresh...")
    os.makedirs(settings.app_store_cache_path, exist_ok=True)
    tasks = [_fetch_and_cache_app_store(store) for store in APP_STORES]
    if tasks:
        await asyncio.gather(*tasks)
    logger.info("App store cache refresh complete.")


async def _update_all_autostart_caches():
    logger.info("Starting autostart script cache refresh for all app stores...")
    tasks = []
    async with httpx2.AsyncClient(follow_redirects=True) as client:
        for store in APP_STORES:
            apps_list = []
            base_url = store.url.rsplit("/", 1)[0]

            cache_file = os.path.join(
                settings.app_store_cache_path, f"{store.name}.yml"
            )
            if os.path.exists(cache_file):
                try:
                    with open(cache_file, "r") as f:
                        data = yaml.safe_load(f)
                    apps_list = (
                        data["apps"]
                        if isinstance(data, dict) and "apps" in data
                        else data
                    )
                except Exception as e:
                    logger.error(f"Error reading cached store '{store.name}': {e}")

            if not apps_list:
                try:
                    response = await client.get(store.url, timeout=15)
                    response.raise_for_status()
                    data = yaml.safe_load(response.text)
                    apps_list = (
                        data["apps"]
                        if isinstance(data, dict) and "apps" in data
                        else data
                    )
                except Exception as e:
                    logger.error(
                        f"Failed to fetch app store '{store.name}' for autostart: {e}"
                    )
                    continue

            if not isinstance(apps_list, list):
                logger.error(f"Could not find a list of apps in store '{store.name}'")
                continue

            for app in apps_list:
                if app.get("provider_config", {}).get("autostart"):
                    tasks.append(
                        _fetch_and_cache_single_script(
                            store.name, base_url, app["id"], suffix=""
                        )
                    )
                    tasks.append(
                        _fetch_and_cache_single_script(
                            store.name, base_url, app["id"], suffix="-wayland"
                        )
                    )

    if tasks:
        await asyncio.gather(*tasks)
    logger.info("Autostart script cache refresh complete.")


def _get_app_config_with_overrides(app: InstalledApp) -> dict:
    config_dict = app.model_dump()
    if not app.source or not app.source_app_id:
        return config_dict

    store_cache_file = os.path.join(settings.app_store_cache_path, f"{app.source}.yml")
    if not os.path.exists(store_cache_file):
        return config_dict

    try:
        with open(store_cache_file, "r") as f:
            store_data = yaml.safe_load(f)

        apps_list = (
            store_data.get("apps", [])
            if isinstance(store_data, dict) and "apps" in store_data
            else store_data
        )
        if isinstance(apps_list, list):
            source_app = next(
                (a for a in apps_list if a.get("id") == app.source_app_id), None
            )
            if source_app:
                overrides = (
                    source_app.get("provider_config", {}).get("docker_overrides")
                )
                if overrides:
                    if "provider_config" not in config_dict:
                        config_dict["provider_config"] = {}
                    config_dict["provider_config"]["docker_overrides"] = overrides
    except Exception as e:
        logger.warning(f"Failed to apply overrides for app {app.name}: {e}")

    return config_dict


async def _inspect_self_container():
    global PATH_PREFIX_MAP, DISCOVERED_API_PORT, DISCOVERED_SESSION_PORT, DISCOVERED_NETWORK
    if not os.path.exists("/var/run/docker.sock"):
        logger.info("Docker socket not found. Assuming running on host.")
        return

    try:
        client = await asyncio.to_thread(docker.from_env)
        containers = await asyncio.to_thread(
            client.containers.list, filters={"name": "vreckan"}
        )

        if not containers:
            hostname = os.uname()[1]
            logger.info(
                f"No container named 'vreckan' found. Trying with hostname '{hostname}'."
            )
            try:
                container = await asyncio.to_thread(client.containers.get, hostname)
                containers = [container]
            except docker.errors.NotFound:
                logger.warning(
                    "Could not find self-container by name 'vreckan' or hostname. Path remapping will be disabled."
                )
                return

        container = containers[0]
        logger.info(f"Found self-container '{container.name}'. Inspecting mounts.")

        mounts = container.attrs.get("Mounts", [])
        if not mounts:
            logger.info(f"Container '{container.name}' has no volume mounts to map.")
            return

        for mount in mounts:
            host_path = mount.get("Source")
            container_path = mount.get("Destination")
            if host_path and container_path:
                PATH_PREFIX_MAP[container_path] = host_path

        if PATH_PREFIX_MAP:
            logger.info(f"Detected container mount prefixes: {PATH_PREFIX_MAP}")
        else:
            logger.warning(
                "Could not find any usable mount points on the current container."
            )

    except DockerException as e:
        logger.warning(
            f"Could not inspect self in Docker. Path mapping disabled. Error: {e}"
        )
    except Exception as e:
        logger.error(f"An unexpected error occurred during Docker self-inspection: {e}")

    networks = container.attrs.get("NetworkSettings", {}).get("Networks", {})
    if networks:
        DISCOVERED_NETWORK = list(networks.keys())[0]
        logger.info(f"Discovered self-container network: {DISCOVERED_NETWORK}")

    ports = container.attrs.get("NetworkSettings", {}).get("Ports", {})
    if not ports:
        logger.info(f"Container '{container.name}' has no port mappings to inspect.")
    else:
        api_internal = f"{settings.api_port}/tcp"
        session_internal = f"{settings.session_port}/tcp"

        if api_internal in ports and ports[api_internal]:
            if host_port := ports[api_internal][0].get("HostPort"):
                DISCOVERED_API_PORT = int(host_port)
                logger.info(
                    f"Discovered external API port mapping: {settings.api_port} -> {DISCOVERED_API_PORT}"
                )

        if session_internal in ports and ports[session_internal]:
            if host_port := ports[session_internal][0].get("HostPort"):
                DISCOVERED_SESSION_PORT = int(host_port)
                logger.info(
                    f"Discovered external Session port mapping: {settings.session_port} -> {DISCOVERED_SESSION_PORT}"
                )


def _schema_default_env() -> dict:
    """The base-layer env for app launches, from the template schema.

    A non-docker setting is pushed through only when its ``current`` value
    (the admin's base-layer override) is set and differs from ``default``
    (the image baseline); otherwise the image's built-in default applies.
    This is the lowest-priority env layer: the base runtime vars, per-app
    templates, and per-launch overrides all apply on top and win any
    collision. The values live in the database (seeded from the bundled
    YAML on first run) and are edited on the admin Templates page, so e.g.
    SELKIES_ENCODER is explicitly applied to every launch only when the
    admin has actually changed it from what the image ships.
    """
    env = {}
    for s in TEMPLATE_SCHEMA.get("settings", []):
        if s.get("docker"):
            continue
        name = s.get("name")
        current = s.get("current")
        if name and current is not None and current != s.get("default"):
            env[name] = str(current)
    return env


def _extract_docker_overrides(settings_dict: dict) -> dict:
    overrides = {}
    
    def parse_list(val):
        if not val: return []
        return [x.strip() for x in str(val).split(",") if x.strip()]

    def parse_kv_list(val, separator="="):
        if not val: return {}
        res = {}
        for item in str(val).split(","):
            if separator in item:
                k, v = item.split(separator, 1)
                res[k.strip()] = v.strip()
        return res

    for k, v in settings_dict.items():
        if not k.startswith("DOCKER_"):
            continue
        
        val_str = str(v).strip()
        if not val_str:
            continue

        if k == "DOCKER_PRIVILEGED":
            overrides["privileged"] = val_str.lower() == "true"
        elif k == "DOCKER_CAP_ADD":
            overrides["cap_add"] = parse_list(val_str)
        elif k == "DOCKER_CAP_DROP":
            overrides["cap_drop"] = parse_list(val_str)
        elif k == "DOCKER_SECURITY_OPT":
            overrides["security_opt"] = parse_list(val_str)
        elif k == "DOCKER_DEVICES":
            overrides["devices"] = parse_list(val_str)
        elif k == "DOCKER_DNS":
            overrides["dns"] = parse_list(val_str)
        elif k == "DOCKER_SHM_SIZE":
            overrides["shm_size"] = val_str
        elif k == "DOCKER_MEM_LIMIT":
            overrides["mem_limit"] = val_str
        elif k == "DOCKER_CPU_SHARES":
            try:
                overrides["cpu_shares"] = int(val_str)
            except ValueError:
                pass
        elif k == "DOCKER_NANO_CPUS":
            try:
                overrides["nano_cpus"] = int(val_str)
            except ValueError:
                pass
        elif k == "DOCKER_NETWORK_MODE":
            overrides["network_mode"] = val_str
        elif k == "DOCKER_IPC_MODE":
            overrides["ipc_mode"] = val_str
        elif k == "DOCKER_PID_MODE":
            overrides["pid_mode"] = val_str
        elif k == "DOCKER_GROUP_ADD":
            overrides["group_add"] = parse_list(val_str)
        elif k == "DOCKER_EXTRA_HOSTS":
            overrides["extra_hosts"] = parse_kv_list(val_str, ":")
        elif k == "DOCKER_SYSCTLS":
            overrides["sysctls"] = parse_kv_list(val_str, "=")
        elif k == "DOCKER_ULIMITS":
            ulimits = []
            for item in val_str.split(","):
                if "=" in item:
                    n, limit = item.split("=", 1)
                    if ":" in limit:
                        soft, hard = limit.split(":", 1)
                        ulimits.append(docker.types.Ulimit(name=n.strip(), soft=int(soft), hard=int(hard)))
                    else:
                        ulimits.append(docker.types.Ulimit(name=n.strip(), soft=int(limit), hard=int(limit)))
            if ulimits:
                overrides["ulimits"] = ulimits
        elif k == "DOCKER_TMPFS":
            overrides["tmpfs"] = parse_kv_list(val_str, ":")
        elif k == "DOCKER_BIND_MOUNTS":
            overrides["volumes"] = parse_list(val_str)
        elif k == "DOCKER_ENV":
            overrides["environment"] = parse_kv_list(val_str, "=")
            
    return overrides


def _schema_docker_overrides() -> dict:
    """The base-layer docker run-options, from the template schema.

    Mirrors :func:`_schema_default_env` for the docker-flagged settings: a
    DOCKER_* setting is applied only when its ``current`` value is set and
    differs from the image baseline. Applied as the lowest-priority docker
    override layer (below the app config's own overrides and the app
    template's).
    """
    raw = {}
    for s in TEMPLATE_SCHEMA.get("settings", []):
        if not s.get("docker"):
            continue
        name = s.get("name")
        current = s.get("current")
        if name and current is not None and current != s.get("default"):
            raw[name] = current
    return _extract_docker_overrides(raw)


def _apply_base_docker_overrides(app_config_dict: dict) -> None:
    """Merge the base-layer docker overrides into an app config as the
    lowest-priority layer.

    Values already present in the app config's own docker_overrides win;
    list-valued keys (devices, volumes) are unioned with the base entries
    added only if not already present, and environment dicts are merged
    key-by-key without clobbering.
    """
    base = _schema_docker_overrides()
    if not base:
        return
    pc = app_config_dict.get("provider_config")
    if pc is None:
        pc = {}
        app_config_dict["provider_config"] = pc
    overrides = pc.get("docker_overrides")
    if overrides is None:
        overrides = {}
        pc["docker_overrides"] = overrides
    for k, v in base.items():
        if overrides.get(k) is None:
            overrides[k] = v
        elif k in ("devices", "volumes") and isinstance(overrides.get(k), list) and isinstance(v, list):
            for item in v:
                if item not in overrides[k]:
                    overrides[k].append(item)
        elif k == "environment" and isinstance(overrides.get(k), dict) and isinstance(v, dict):
            for ek, ev in v.items():
                overrides[k].setdefault(ek, ev)


def _translate_path_to_host(internal_path: str) -> str:
    if not PATH_PREFIX_MAP or not internal_path:
        return internal_path

    sorted_prefixes = sorted(PATH_PREFIX_MAP.keys(), key=len, reverse=True)

    for container_prefix in sorted_prefixes:
        if internal_path == container_prefix or internal_path.startswith(
            container_prefix + "/"
        ):
            host_prefix = PATH_PREFIX_MAP[container_prefix]

            relative_path = os.path.relpath(internal_path, container_prefix)

            if relative_path == ".":
                return host_prefix

            translated_path = os.path.join(host_prefix, relative_path)
            logger.debug(f"Translated path '{internal_path}' -> '{translated_path}'")
            return translated_path

    return internal_path


def _safe_copytree(src: str, dst: str, symlinks: bool = True):
    try:
        shutil.copytree(src, dst, symlinks=symlinks, ignore_dangling_symlinks=True)
    except shutil.Error as e:
        logger.warning(f"Ignored errors during directory copy from {src} to {dst}: {e}")


def _safe_rmtree(path: str):
    try:
        shutil.rmtree(path)
    except OSError as e:
        if e.errno == 13:
            raise HTTPException(
                status_code=403,
                detail="Directory cannot be deleted because of perms: there are non PUID and PGID user owned files in those directories and we are not able to remove them",
            )
        raise e

def _get_cpu_model():
    global CPU_MODEL
    try:
        with open("/proc/cpuinfo", "r") as f:
            for line in f:
                if "model name" in line:
                    CPU_MODEL = line.split(":", 1)[1].strip()
                    logger.info(f"Detected CPU Model: {CPU_MODEL}")
                    break
    except Exception as e:
        logger.warning(f"Could not read CPU model from /proc/cpuinfo: {e}")


def _get_system_stats() -> Dict:
    now = time.time()
    if SYSTEM_STATS_CACHE["data"] and (now - SYSTEM_STATS_CACHE["timestamp"] < 60):
        return SYSTEM_STATS_CACHE["data"]

    try:
        usage = shutil.disk_usage(settings.storage_path)
        stats = {
            "cpu_model": CPU_MODEL,
            "disk_total": usage.total,
            "disk_used": usage.used,
        }
        SYSTEM_STATS_CACHE["data"] = stats
        SYSTEM_STATS_CACHE["timestamp"] = now
        return stats
    except Exception as e:
        logger.error(f"Failed to get system stats: {e}")
        return {"cpu_model": CPU_MODEL, "disk_total": None, "disk_used": None}


def detect_gpus():
    global AVAILABLE_GPUS
    AVAILABLE_GPUS.clear()
    cmd = "ls -la /sys/class/drm/renderD*/device/driver 2>/dev/null | awk '{print $11}' | awk -F/ '{print $NF}'"
    try:
        result = subprocess.run(
            cmd, shell=True, check=True, capture_output=True, text=True
        )
        drivers = result.stdout.strip().split("\n")

        render_devices = sorted(
            [f for f in os.listdir("/sys/class/drm") if f.startswith("renderD")],
            key=lambda x: int(x.replace("renderD", "")),
        )

        if len(drivers) != len(render_devices):
            logger.warning(
                f"Mismatch between detected drivers ({len(drivers)}) and render devices ({len(render_devices)}). GPU detection might be inaccurate."
            )
            return

        nvidia_index = 0
        for i, driver in enumerate(drivers):
            if not driver:
                continue
            device_name = render_devices[i]
            device_path = f"/dev/dri/{device_name}"
            gpu_info = {"device": device_path, "driver": driver}
            if driver == "nvidia":
                gpu_info["type"] = "nvidia"
                gpu_info["index"] = nvidia_index
                nvidia_index += 1
            else:
                gpu_info["type"] = "dri3"

            AVAILABLE_GPUS.append(gpu_info)

        logger.info(f"Detected {len(AVAILABLE_GPUS)} GPU(s): {AVAILABLE_GPUS}")

    except (subprocess.CalledProcessError, FileNotFoundError) as e:
        logger.info(
            f"GPU detection command failed or could not be run: {e}. No GPUs will be available."
        )
    except Exception as e:
        logger.error(f"An unexpected error occurred during GPU detection: {e}")


async def load_app_templates():
    global APP_TEMPLATES
    APP_TEMPLATES.clear()

    # Built-in (default) templates ship as files inside the application image
    # and are loaded from disk. They are not stored in the database.
    if os.path.isdir(settings.default_app_templates_path):
        for filename in os.listdir(settings.default_app_templates_path):
            if filename.endswith((".yml", ".yaml")):
                try:
                    with open(
                        os.path.join(settings.default_app_templates_path, filename), "r"
                    ) as f:
                        template_data = yaml.safe_load(f)
                        template_name = template_data.get("name")
                        if template_name:
                            APP_TEMPLATES[template_name] = template_data
                except Exception as e:
                    logger.error(f"Error loading default template {filename}: {e}")

    # User-defined templates live in the configuration database and override
    # built-in templates by name.
    async with db.async_session_factory() as session:
        for t in (await session.execute(select(db.AppTemplate))).scalars():
            APP_TEMPLATES[t.name] = {"name": t.name, "settings": t.settings}

    if not APP_TEMPLATES:
        logger.warning("No app templates found. Creating a blank 'Default' template.")
        default_template = {"name": "Default", "settings": {}}
        APP_TEMPLATES["Default"] = default_template
        async with db.async_session_factory() as session:
            session.add(db.AppTemplate(name="Default", settings={}))
            await session.commit()

    logger.info(f"Loaded {len(APP_TEMPLATES)} application template(s).")


async def load_app_configs():
    global INSTALLED_APPS, APP_STORES

    # Installed applications (from the configuration database).
    try:
        async with db.async_session_factory() as session:
            rows = (await session.execute(select(db.InstalledApp))).scalars().all()
        INSTALLED_APPS = {row.id: InstalledApp(**row.data) for row in rows}
        logger.info(f"Loaded {len(INSTALLED_APPS)} installed application(s) from the database.")
    except Exception as e:
        logger.error(f"Error loading installed apps from the database: {e}")
        INSTALLED_APPS = {}

    # App stores — the *list* of stores (name + catalog URL) is stored in the
    # database. The actual catalog data (the available apps) stays external and
    # is cached under app_store_cache_path. On first run the default Vreckan
    # Apps store is seeded.
    try:
        async with db.async_session_factory() as session:
            rows = (await session.execute(select(db.AppStore))).scalars().all()
        if rows:
            APP_STORES = [AppStore(name=r.name, url=r.url) for r in rows]
        else:
            APP_STORES = [
                AppStore(name="Vreckan Apps", url=settings.app_resource_path)
            ]
            await save_app_stores()
        logger.info(f"Loaded {len(APP_STORES)} app store(s) from the database.")
    except Exception as e:
        logger.error(f"Error loading app stores from the database: {e}")
        APP_STORES = []


async def save_installed_apps():
    """Sync the in-memory installed-app set to the database (full replace)."""
    try:
        async with db.async_session_factory() as session:
            async with session.begin():
                await session.execute(delete(db.InstalledApp))
                for app in INSTALLED_APPS.values():
                    session.add(db.InstalledApp(id=app.id, data=app.model_dump()))
        logger.info(f"Saved {len(INSTALLED_APPS)} installed application(s) to the database.")
    except Exception as e:
        logger.error(f"Failed to save installed apps to the database: {e}")


async def save_app_stores():
    """Sync the in-memory app-store list to the database (full replace)."""
    try:
        async with db.async_session_factory() as session:
            async with session.begin():
                await session.execute(delete(db.AppStore))
                for store in APP_STORES:
                    session.add(db.AppStore(name=store.name, url=store.url))
        logger.info(f"Saved {len(APP_STORES)} app store(s) to the database.")
    except Exception as e:
        logger.error(f"Failed to save app stores to the database: {e}")


async def _get_and_cache_image_metadata(image_name: str, force_refresh: bool = False):
    if (
        not force_refresh
        and image_name in IMAGE_METADATA
        and "sha" in IMAGE_METADATA[image_name]
    ):
        return

    provider = DockerProvider({"provider_config": {"image": image_name}})
    info = await provider.get_local_image_info(image_name)

    if image_name not in IMAGE_METADATA:
        IMAGE_METADATA[image_name] = {}

    if info:
        IMAGE_METADATA[image_name]["sha"] = info["short_id"]
        IMAGE_METADATA[image_name]["digests"] = info["digests"]
    else:
        IMAGE_METADATA[image_name]["sha"] = None
        IMAGE_METADATA[image_name]["digests"] = []


def _get_autostart_cache_path(
    app_config: InstalledApp, suffix: str = ""
) -> Optional[str]:
    store = next((s for s in APP_STORES if s.name == app_config.source), None)
    if not store:
        return None

    cache_dir = os.path.join(settings.autostart_cache_path, store.name)
    return os.path.join(cache_dir, f"{app_config.source_app_id}{suffix}")


async def _update_autostart_cache(app_config: InstalledApp):
    if not app_config.provider_config.autostart:
        return

    store = next((s for s in APP_STORES if s.name == app_config.source), None)
    if not store:
        logger.error(
            f"Could not find app store named '{app_config.source}' for app '{app_config.name}'. Cannot fetch autostart script."
        )
        return

    app_source_url = store.url
    if not (app_source_url.endswith(".yml") or app_source_url.endswith(".yaml")):
        logger.error(
            f"App store URL does not appear to be a YAML file: {app_source_url}. Cannot determine autostart script path."
        )
        return

    base_url = app_source_url.rsplit("/", 1)[0]

    async def fetch_one(suffix: str):
        cache_file_path = _get_autostart_cache_path(app_config, suffix)
        if not cache_file_path:
            return

        os.makedirs(os.path.dirname(cache_file_path), exist_ok=True)
        autostart_url = f"{base_url}/autostart/{app_config.source_app_id}{suffix}"

        logger.info(
            f"Checking for autostart script for '{app_config.source_app_id}{suffix}' from {autostart_url}"
        )

        try:
            async with httpx2.AsyncClient(follow_redirects=True) as client:
                response = await client.get(autostart_url, timeout=10)

                if response.status_code == 404:
                    logger.warning(
                        f"No autostart script found for '{app_config.source_app_id}{suffix}' (404 Not Found). Caching empty response."
                    )
                    with open(cache_file_path, "w") as f:
                        f.write("")
                    return

                response.raise_for_status()
                script_content = response.text

                def write_cache():
                    with open(cache_file_path, "w") as f:
                        f.write(script_content)

                await asyncio.to_thread(write_cache)
                logger.info(
                    f"Successfully cached autostart script for '{app_config.source_app_id}{suffix}'."
                )

        except httpx2.RequestError as e:
            logger.error(
                f"Failed to fetch autostart script for '{app_config.source_app_id}{suffix}': {e}"
            )
        except Exception as e:
            logger.error(
                f"Unexpected error updating autostart script for '{app_config.source_app_id}{suffix}': {e}"
            )

    await fetch_one("")
    await fetch_one("-wayland")


def _update_pull_progress(image_name: str, line: dict) -> None:
    """Folds one Docker pull progress event into the aggregate state.

    Called from a worker thread (docker-py streams one event per line), so
    this only performs plain dict mutations (safe under the GIL) and never
    touches the event loop.
    """
    prog = PULL_PROGRESS.setdefault(
        image_name,
        {"status": "pulling", "percentage": None, "current": 0, "total": 0},
    )
    status = line.get("status")
    if status:
        prog["status"] = status
    partial = line.get("partial")
    if partial:
        layers = prog.setdefault("_layers", {})
        if line.get("total") is not None:
            layers[partial] = {
                "current": line.get("current") or 0,
                "total": line["total"],
            }
        elif line.get("current") is not None and partial in layers:
            layers[partial]["current"] = line["current"]
    layers = prog.get("_layers") or {}
    total = sum(v["total"] for v in layers.values() if v.get("total"))
    current = sum(v["current"] for v in layers.values() if v.get("total"))
    if total:
        prog["total"] = total
        prog["current"] = current
        prog["percentage"] = round(current / total * 100, 1)


async def _do_pull_and_cache_image(image_name: str, app: Optional[InstalledApp] = None):
    """Runs the actual pull + metadata caching. The caller must have claimed
    the image in PULL_STATUS first (i.e. no other pull is queued/running for
    it). On success the status is cleared; on failure it is left as
    "pull_failed" so the UI can surface the error."""
    PULL_STATUS[image_name] = "pulling"
    PULL_PROGRESS[image_name] = {
        "status": "pulling", "percentage": None, "current": 0, "total": 0,
    }
    try:
        logger.info(f"Starting background pull for image '{image_name}'...")
        provider = DockerProvider({"provider_config": {"image": image_name}})
        await provider.pull_image(
            image_name,
            progress_cb=lambda line: _update_pull_progress(image_name, line),
        )
        await _get_and_cache_image_metadata(image_name, force_refresh=True)

        if image_name in IMAGE_METADATA:
            IMAGE_METADATA[image_name]["last_checked_at"] = time.time()

        if app is not None:
            try:
                await _update_autostart_cache(app)
            except Exception as e:
                logger.warning(
                    f"Autostart cache refresh after pull failed for '{image_name}': {e}"
                )

        logger.info(f"Background pull for '{image_name}' completed successfully.")
    except Exception as e:
        logger.error(f"Background pull for image '{image_name}' failed: {e}")
        PULL_STATUS[image_name] = "pull_failed"
        PULL_PROGRESS.setdefault(image_name, {"status": "pulling"})["status"] = f"failed: {e}"
    else:
        PULL_STATUS.pop(image_name, None)
        PULL_PROGRESS.pop(image_name, None)


async def _pull_and_cache_image(image_name: str, app: Optional[InstalledApp] = None):
    """Claims the image for a background pull (unless one is already
    queued/running) and awaits it. Used by the scheduled update job, which
    pulls images sequentially. User-facing endpoints (install / pull button)
    claim the image and schedule _do_pull_and_cache_image as a task instead,
    so the HTTP request returns immediately and the client can poll progress.
    """
    if PULL_STATUS.get(image_name) in ("queued", "pulling"):
        logger.info(f"Pull for image '{image_name}' is already in progress.")
        return
    PULL_STATUS[image_name] = "queued"
    await _do_pull_and_cache_image(image_name, app)


async def background_update_job():
    while True:
        await asyncio.sleep(settings.auto_update_interval_seconds)

        await _update_all_app_store_caches()
        await _update_all_autostart_caches()

        logger.info("Starting scheduled app image update check...")
        apps_to_update = [app for app in INSTALLED_APPS.values() if app.auto_update]
        images_to_pull = {app.provider_config.image for app in apps_to_update}

        for image_name in images_to_pull:
            await _pull_and_cache_image(image_name)
            await asyncio.sleep(2)

        logger.info("Cleaning up dangling images...")
        try:
            client = await asyncio.to_thread(docker.from_env)
            await asyncio.to_thread(client.images.prune, filters={"dangling": True})
            logger.info("Successfully cleaned up dangling images.")
        except Exception as e:
            logger.error(f"Failed to prune dangling images: {e}")


async def _relaunch_stale_session(session_id: str):
    """Re-launch a session whose container(s) are missing (e.g. after a reboot).

    Each container in the session's registry is re-created from the launch
    parameters stored on the session record, so the session is restored
    identically. If the primary container is missing and cannot be restored, the
    session is dropped.
    """
    session_data = SESSIONS_DB.get(session_id)
    if not session_data:
        return
    registry = session_data.get("container_registry", {})
    if not registry and "provider_app_id" in session_data:
        registry = {
            session_data["provider_app_id"]: {
                "instance_id": session_data["instance_id"],
            }
        }
    try:
        docker_client = await asyncio.to_thread(docker.from_env)
    except DockerException as e:
        logger.error(f"[{session_id}] Cannot connect to Docker to re-launch: {e}")
        return
    primary_missing = False
    primary_restored = False
    for app_id, container_info in registry.items():
        instance_id = container_info.get("instance_id")
        if not instance_id:
            continue
        is_primary = app_id == session_data.get("provider_app_id")
        try:
            await asyncio.to_thread(docker_client.containers.get, instance_id)
            continue  # container still present; nothing to do
        except NotFound:
            if is_primary:
                primary_missing = True
        app_config = container_info.get("app_config")
        launch_kwargs = container_info.get("launch_kwargs")
        if not app_config or not launch_kwargs:
            logger.warning(
                f"[{session_id}] No stored launch parameters for app '{app_id}'; "
                "cannot re-launch."
            )
            continue
        try:
            provider = DockerProvider(app_config)
            kwargs = dict(launch_kwargs)
            kwargs["session_id"] = session_id
            details = await provider.launch(**kwargs)
        except Exception as e:
            logger.error(
                f"[{session_id}] Failed to re-launch container for app '{app_id}': {e}"
            )
            continue
        container_info["instance_id"] = details["instance_id"]
        container_info["ip"] = details["ip"]
        container_info["port"] = details["port"]
        if is_primary:
            session_data["instance_id"] = details["instance_id"]
            session_data["ip"] = details["ip"]
            session_data["port"] = details["port"]
            primary_restored = True
        logger.info(
            f"[{session_id}] Re-launched container for app '{app_id}' "
            f"({details['instance_id']})."
        )
    async with SESSIONS_LOCK:
        if session_id in SESSIONS_DB and primary_missing and not primary_restored:
            logger.warning(
                f"[{session_id}] Primary container could not be restored; "
                "removing session."
            )
            SESSIONS_DB.pop(session_id, None)
    await save_sessions_to_disk()


async def sync_app_settings_from_env():
    """Keep the ``app_settings`` table in sync with the live environment.

    The container's ``VRECKAN_*`` environment is the source of truth at boot:
    any stored value that has changed — or a variable that is newly set — is
    written to the database. Rows are only ever added or updated, never
    deleted, so the table is a monotonic record of the environment the server
    has run with (and a recovery source if the .env file is lost).

    Exception: the OIDC/SSO settings are editable from the admin UI's SSO
    page, where a stored value *overrides* the environment. For those keys a
    pre-existing row is left untouched (the UI value wins); the env value is
    only written when no row exists yet, so a first-ever boot still records
    the env baseline.
    """
    from server.settings import OIDC_SETTING_NAMES

    oidc_env_names = {f"VRECKAN_{n.upper()}" for n in OIDC_SETTING_NAMES}
    try:
        changed = []
        async with db.async_session_factory() as session:
            async with session.begin():
                for name in sorted(os.environ):
                    if not name.startswith("VRECKAN_"):
                        continue
                    value = os.environ[name]
                    row = (
                        await session.execute(
                            select(db.AppSetting).where(db.AppSetting.key == name)
                        )
                    ).scalar_one_or_none()
                    if row is None:
                        session.add(db.AppSetting(key=name, value=value))
                        changed.append(f"{name} (added)")
                    elif row.value != value and name not in oidc_env_names:
                        # Non-SSO keys track the environment; SSO keys keep
                        # their UI-edited value (see docstring).
                        row.value = value
                        changed.append(f"{name} (updated)")
        if changed:
            logger.info(f"app_settings synced from environment: {', '.join(changed)}")
        else:
            logger.info("app_settings already in sync with the environment.")
    except Exception as e:
        logger.error(f"Failed to sync app_settings from environment: {e}")


async def _init_template_schema() -> None:
    """Reconcile the template schema table with the bundled YAML baseline,
    then load it from the database (the runtime source of truth).

    The YAML is the image baseline. On every startup this:

    * adds settings the YAML defines that the table lacks (``current`` NULL),
    * resets a built-in row's ``default`` to the YAML baseline when it has
      drifted — moving the drifted value into ``current`` first when no
      override was recorded yet (this migrates rows from before the
      default/current split, and absorbs image upgrades), and
    * clears a ``current`` that equals the baseline (redundant override),
    * re-indexes ``sort_order`` (YAML order first, custom settings after).

    Admin edits to labels/descriptions/categories/types of existing rows are
    preserved; only ``default`` tracks the YAML. Idempotent.
    """
    yaml_settings = TEMPLATE_SCHEMA_YAML
    yaml_names = {s["name"] for s in yaml_settings if s.get("name")}
    changed = []
    async with db.async_session_factory() as session:
        async with session.begin():
            rows = {
                r.name: r
                for r in (
                    await session.execute(select(db.TemplateSchemaSetting))
                ).scalars()
            }
            for i, ys in enumerate(yaml_settings):
                name = ys.get("name")
                if not name:
                    continue
                baseline_default = str(ys.get("default") or "")
                row = rows.get(name)
                if row is None:
                    row = db.TemplateSchemaSetting(
                        name=name,
                        sort_order=i,
                        label=ys.get("label", ""),
                        description=ys.get("description", ""),
                        category=ys.get("category", "general"),
                        type=ys.get("type", "text"),
                        default=baseline_default,
                        current=None,
                        docker=bool(ys.get("docker", False)),
                        options=ys.get("options") or [],
                    )
                    session.add(row)
                    rows[name] = row
                    changed.append(f"{name} (added)")
                    continue
                if row.default != baseline_default:
                    if row.current is None:
                        # Pre-split row: the stored "default" was really the
                        # admin's base-layer value — move it to current.
                        row.current = row.default
                    row.default = baseline_default
                    changed.append(f"{name} (default reset to baseline)")
                if row.current is not None and row.current == row.default:
                    row.current = None
                    changed.append(f"{name} (redundant current cleared)")
                row.sort_order = i
            # Custom (non-YAML) settings keep their admin-defined baseline;
            # just keep them ordered after the built-ins.
            custom_rows = sorted(
                (r for n, r in rows.items() if n not in yaml_names),
                key=lambda r: (r.sort_order is None, r.sort_order or 0, r.name),
            )
            for j, row in enumerate(custom_rows):
                row.sort_order = len(yaml_settings) + j
    if changed:
        logger.info(
            f"Template schema reconciled with bundled YAML: {', '.join(changed)}"
        )
    else:
        logger.info("Template schema already in sync with the bundled YAML.")
    await _load_template_schema()


async def _load_template_schema() -> None:
    """Load the template schema (and the global default GPU) from the
    database into the in-memory structures used by the launch path and the
    admin UI."""
    global TEMPLATE_SCHEMA, GLOBAL_DEFAULT_GPU
    async with db.async_session_factory() as session:
        rows = (
            await session.execute(
                select(db.TemplateSchemaSetting).order_by(
                    db.TemplateSchemaSetting.sort_order, db.TemplateSchemaSetting.name
                )
            )
        ).scalars().all()
        gpu_row = (
            await session.execute(
                select(db.AppSetting).where(db.AppSetting.key == "global_default_gpu")
            )
        ).scalar_one_or_none()
    yaml_names = {s["name"] for s in TEMPLATE_SCHEMA_YAML if s.get("name")}
    settings_list = []
    for r in rows:
        entry = {
            "name": r.name,
            "label": r.label,
            "description": r.description,
            "category": r.category,
            "type": r.type,
            "default": r.default,
            "current": r.current,
            "docker": r.docker,
            # Built-in (YAML-shipped) settings have a read-only baseline;
            # custom settings may define their own.
            "builtin": r.name in yaml_names,
        }
        if r.options:
            entry["options"] = r.options
        settings_list.append(entry)
    TEMPLATE_SCHEMA = {"settings": settings_list}
    GLOBAL_DEFAULT_GPU = gpu_row.value if gpu_row and gpu_row.value else None
    logger.info(
        f"Template schema loaded from database ({len(settings_list)} settings); "
        f"global default GPU: {GLOBAL_DEFAULT_GPU or 'none'}."
    )


@asynccontextmanager
async def lifespan(app: FastAPI):
    logger.info("API server starting up...")
    os.makedirs(settings.upload_dir, exist_ok=True, mode=0o700)
    os.makedirs(settings.app_icons_path, exist_ok=True, mode=0o700)
    os.makedirs(settings.autostart_cache_path, exist_ok=True, mode=0o700)
    os.makedirs(settings.app_store_cache_path, exist_ok=True, mode=0o700)
    os.makedirs(settings.storage_path, exist_ok=True, mode=0o755)
    os.makedirs(settings.home_templates_path, exist_ok=True, mode=0o700)
    os.makedirs(
        os.path.join(settings.storage_path, "vreckan_ephemeral"),
        exist_ok=True,
        mode=0o700,
    )
    os.makedirs(settings.public_storage_path, exist_ok=True, mode=0o700)
    # Initialise the configuration database (creates tables on first run) and
    # load all durable configuration from it.
    await db.init_db()
    # Add any columns that were introduced after the initial schema shipped
    # (create_all above only creates missing tables, never new columns).
    await db.migrate_db()
    # Mirror the live VRECKAN_* environment into the app_settings table so the
    # DB always reflects — and can recover — the env the container was started with.
    await sync_app_settings_from_env()
    # Seed (first run) / load the editable template schema from the database;
    # its per-setting defaults are the global baseline applied at launch.
    await _init_template_schema()
    await load_public_shares_metadata()
    await load_sessions_from_disk()
    await load_auth_tokens_from_disk()

    if SESSIONS_DB:
        logger.info("Checking for stale sessions from persistence...")
        try:
            docker_client = await asyncio.to_thread(docker.from_env)
            stale_sessions = []
            for session_id, session_data in SESSIONS_DB.items():
                registry = session_data.get("container_registry", {})
                if not registry and "provider_app_id" in session_data:
                    registry = {
                        session_data["provider_app_id"]: {
                            "instance_id": session_data["instance_id"],
                        }
                    }
                missing = False
                for app_id, container_info in registry.items():
                    instance_id = container_info.get("instance_id")
                    if not instance_id:
                        continue
                    try:
                        await asyncio.to_thread(
                            docker_client.containers.get, instance_id
                        )
                    except NotFound:
                        missing = True
                        break
                if missing:
                    stale_sessions.append(session_id)

            if stale_sessions:
                logger.info(
                    f"Found {len(stale_sessions)} session(s) with missing "
                    "container(s); re-launching so they survive the reboot."
                )
                for session_id in stale_sessions:
                    await _relaunch_stale_session(session_id)
        except DockerException as e:
            logger.error(f"Could not connect to Docker to check stale sessions: {e}")

    await _inspect_self_container()
    _get_cpu_model()
    user_manager.set_external_ports(DISCOVERED_API_PORT, DISCOVERED_SESSION_PORT)
    await user_manager.load_users_and_groups()
    await volume_mount_manager.load_volume_mounts()
    await load_app_configs()
    await load_app_templates()
    detect_gpus()

    logger.info("Populating app store cache...")
    await _update_all_app_store_caches()

    logger.info("Performing initial population of autostart script cache...")
    await _update_all_autostart_caches()
    logger.info("Initial autostart script cache population complete.")

    logger.info("Populating initial image metadata cache...")
    all_images = {app.provider_config.image for app in INSTALLED_APPS.values()}
    for image_name in all_images:
        await _get_and_cache_image_metadata(image_name)
    logger.info("Image metadata cache populated.")

    update_task = None
    cleanup_task = None
    if settings.auto_update_apps:
        update_task = asyncio.create_task(background_update_job())
    cleanup_task = asyncio.create_task(background_share_cleanup_job())
    yield
    logger.info("API server shutting down...")
    if update_task:
        update_task.cancel()
    if cleanup_task:
        cleanup_task.cancel()
    try:
        tasks_to_await = []
        if update_task:
            tasks_to_await.append(update_task)
        if cleanup_task:
            tasks_to_await.append(cleanup_task)
        if tasks_to_await:
            await asyncio.gather(*tasks_to_await)
    except asyncio.CancelledError:
        logger.info("Background tasks successfully cancelled.")


async def load_public_shares_metadata():
    global PUBLIC_SHARES_METADATA
    try:
        async with db.async_session_factory() as session:
            rows = (await session.execute(select(db.PublicShare))).scalars().all()
        PUBLIC_SHARES_METADATA = {
            row.share_id: PublicShareMetadata(**row.data) for row in rows
        }
        logger.info(
            f"Loaded {len(PUBLIC_SHARES_METADATA)} public share(s) from the database."
        )
    except Exception as e:
        logger.error(f"Error loading public shares metadata from the database: {e}")
        PUBLIC_SHARES_METADATA = {}


async def save_public_shares_metadata():
    async with METADATA_LOCK:
        try:
            async with db.async_session_factory() as session:
                async with session.begin():
                    await session.execute(delete(db.PublicShare))
                    for share_id, metadata in PUBLIC_SHARES_METADATA.items():
                        session.add(
                            db.PublicShare(
                                share_id=share_id,
                                data=metadata.model_dump(exclude_none=True),
                            )
                        )
            logger.info(f"Saved {len(PUBLIC_SHARES_METADATA)} public share(s) to the database.")
        except Exception as e:
            logger.error(f"Failed to save public shares metadata to the database: {e}")


async def background_share_cleanup_job():
    while True:
        await asyncio.sleep(settings.share_cleanup_interval_seconds)
        now = time.time()
        expired_ids = [
            share_id
            for share_id, meta in PUBLIC_SHARES_METADATA.items()
            if meta.expiry_timestamp and meta.expiry_timestamp < now
        ]
        if not expired_ids:
            continue
        logger.info(f"Found {len(expired_ids)} expired share(s) to clean up.")
        for share_id in expired_ids:
            if public_file_path := os.path.join(settings.public_storage_path, share_id):
                if os.path.exists(public_file_path):
                    try:
                        os.remove(public_file_path)
                    except OSError as e:
                        logger.error(
                            f"Error deleting expired share file for {share_id}: {e}"
                        )
            del PUBLIC_SHARES_METADATA[share_id]
        await save_public_shares_metadata()
        logger.info("Expired share cleanup complete.")


api_app = FastAPI(title="Vreckan API", lifespan=lifespan)


async def get_decrypted_request_body(request: Request) -> dict:
    session_id = request.headers.get("X-Session-ID")
    if not session_id or session_id not in CRYPTO_SESSIONS:
        raise HTTPException(status_code=400, detail="Invalid or missing session ID")
    aesgcm = AESGCM(CRYPTO_SESSIONS[session_id])
    try:
        encrypted_body = await request.json()
        payload = EncryptedPayload(**encrypted_body)
        decrypted_bytes = aesgcm.decrypt(
            base64.b64decode(payload.iv), base64.b64decode(payload.ciphertext), None
        )
        return json.loads(decrypted_bytes)
    except Exception as e:
        logger.warning(f"Failed to decrypt request for session {session_id[:8]}: {e}")
        raise HTTPException(status_code=400, detail="Failed to decrypt request")


class EncryptedRoute(APIRoute):
    def get_route_handler(self) -> Callable:
        original_handler = super().get_route_handler()
        async def custom_handler(request: Request) -> Response:
            if request.url.path not in ["/api/handshake/initiate", "/api/handshake/exchange"]:
                sid = request.headers.get("X-Session-ID")
                if not sid or sid not in CRYPTO_SESSIONS:
                    logger.warning(f"Security: Request to {request.url.path} has invalid/missing session key. Header: {sid}")

            response = await original_handler(request)
            is_json = isinstance(response, JSONResponse)
            if not is_json and response.headers.get("content-type") == "application/json":
                is_json = True
            if is_json and response.body:
                session_id = request.headers.get("X-Session-ID")
                if session_id and session_id in CRYPTO_SESSIONS:
                    try:
                        aesgcm = AESGCM(CRYPTO_SESSIONS[session_id])
                        iv = os.urandom(12)
                        ciphertext = aesgcm.encrypt(iv, response.body, None)
                        encrypted_payload = EncryptedPayload(
                            iv=base64.b64encode(iv).decode("utf-8"),
                            ciphertext=base64.b64encode(ciphertext).decode("utf-8"),
                        )
                        return JSONResponse(status_code=response.status_code, content=encrypted_payload.model_dump())
                    except Exception as e:
                        logger.error(f"Encryption Error for {request.url.path}: {str(e)}")
                        return JSONResponse(status_code=500, content={"detail": "Encryption failed server-side."})
                logger.error(f"Security Block: Prevented unencrypted JSON response for {request.url.path}. Session ID: {session_id}")
                return JSONResponse(
                    status_code=400,
                    content={"detail": "Secure session required. Encryption key missing or invalid."}
                )

            return response

        return custom_handler


async def _user_from_server_token(token: Optional[str]) -> Optional[Dict]:
    """Resolves a server-issued opaque web-login token to an active user dict.

    Returns the user dict (with effective_settings/group) or None if the token
    is not a valid, unexpired web-login credential.
    """
    if not token:
        return None
    async with AUTH_TOKENS_LOCK:
        entry = AUTH_TOKENS.get(token)
    if not entry:
        return None
    if entry.get("expires_at", 0) < time.time():
        return None
    username = entry.get("username")
    user = user_manager.get_user(username)
    if not user:
        return None
    effective_settings = user_manager.get_effective_settings(username)
    # No admin bypass: an admin account that has been individually disabled
    # (active=False) is rejected here, exactly like any other inactive user.
    is_active = effective_settings.get("active", False)
    if not is_active:
        raise HTTPException(status_code=403, detail="User account is inactive.")
    user["effective_settings"] = effective_settings
    user["group"] = effective_settings.get("group", "none")
    # Attach the resolved RBAC data so downstream checks (verify_admin,
    # require_permission, /api/admin/status) can gate on permissions without
    # re-resolving on every request.
    user["permissions"] = user_manager.get_effective_permissions(username)
    user["roles"] = user.get("roles") or []
    user["groups"] = user.get("groups") or []
    return user


async def verify_token(req: Request) -> Dict:
    # Web-login path: server-issued opaque token via cookie or Bearer header.
    server_token = req.cookies.get(settings.auth_cookie_name)
    if not server_token:
        auth_header = req.headers.get("Authorization", "")
        if auth_header.startswith("Bearer "):
            bearer = auth_header.split(" ")[1]
            # Only treat as a web token if it is one we issued (opaque tokens
            # never parse as JWTs, but check membership to be safe).
            async with AUTH_TOKENS_LOCK:
                if bearer in AUTH_TOKENS:
                    server_token = bearer
    resolved = await _user_from_server_token(server_token)
    if resolved is not None:
        return resolved
    raise HTTPException(status_code=401, detail="Not authenticated.")


async def verify_admin(user: dict = Depends(verify_token)) -> dict:
    # is_admin is derived from the permission system: it is True iff the
    # user's effective permissions include the ``admin`` super-permission.
    # The stored flag is kept in sync by recompute_is_admin, but we re-check
    # the effective set here so a permission change takes effect immediately.
    if "admin" not in (user.get("permissions") or []):
        raise HTTPException(status_code=403, detail="Admin privileges required.")
    return user


def require_permission(permission: str):
    """Build a FastAPI dependency that requires a specific permission.

    Usage::

        @router.get("/x", dependencies=[Depends(api.require_permission("admin.apps"))])

    The check runs after ``verify_token`` (so the user is authenticated and
    their effective permissions are attached) and raises 403 if the user's
    effective permission set does not include ``permission``. Because the
    ``admin`` super-permission expands to the whole catalog, a full admin
    passes every ``require_permission`` check.
    """
    def _dependency(user: dict = Depends(verify_token)) -> dict:
        if permission not in (user.get("permissions") or []):
            raise HTTPException(
                status_code=403, detail=f"Permission '{permission}' required."
            )
        return user
    return _dependency


async def verify_persistent_storage_enabled(user: dict = Depends(verify_token)) -> dict:
    if not user.get("effective_settings", {}).get("persistent_storage", False):
        raise HTTPException(
            status_code=403, detail="Persistent storage is disabled for this account."
        )
    return user


async def verify_public_sharing_enabled(
    user: dict = Depends(verify_persistent_storage_enabled),
) -> dict:
    # No admin bypass: public sharing is governed by the (effective)
    # public_sharing setting for everyone, including admins.
    if user.get("effective_settings", {}).get("public_sharing", False):
        return user
    raise HTTPException(
        status_code=403, detail="Public file sharing is disabled for this account."
    )


@api_app.post("/api/handshake/initiate", response_model=HandshakeInitiateResponse)
async def handshake_initiate():
    nonce = os.urandom(32)
    signature = SERVER_PRIVATE_KEY.sign(
        nonce,
        padding.PSS(mgf=padding.MGF1(hashes.SHA256()), salt_length=32),
        hashes.SHA256(),
    )
    return {
        "nonce": base64.b64encode(nonce).decode("utf-8"),
        "signature": base64.b64encode(signature).decode("utf-8"),
    }


@api_app.post("/api/handshake/exchange", response_model=HandshakeExchangeResponse)
async def handshake_exchange(request: HandshakeExchangeRequest):
    try:
        aes_key = SERVER_PRIVATE_KEY.decrypt(
            base64.b64decode(request.encrypted_session_key),
            padding.OAEP(
                mgf=padding.MGF1(algorithm=hashes.SHA256()),
                algorithm=hashes.SHA256(),
                label=None,
            ),
        )
        session_id = str(uuid.uuid4())
        CRYPTO_SESSIONS[session_id] = aes_key
        logger.info(
            f"E2EE handshake successful. New crypto session: {session_id[:8]}..."
        )
        return {"session_id": session_id}
    except Exception as e:
        logger.error(f"Failed to decrypt session key during handshake: {e}")
        raise HTTPException(status_code=400, detail="Failed to decrypt session key")


@api_app.get("/api/handshake/public_key", include_in_schema=False)
async def handshake_public_key():
    return {"server_public_key": SERVER_PUBLIC_KEY_PEM}


# ---------------------------------------------------------------------------
# Auth endpoints (password + server-issued tokens + OIDC/SSO) — extracted
# to app/routers/auth.py during the router split. They stay OUTSIDE the
# encrypted router on purpose: the E2EE handshake proves server identity,
# not user identity, so it cannot gate the login flow that establishes the
# credential in the first place. The token issued there is the credential
# accepted by `verify_token`.
# ---------------------------------------------------------------------------
from server.routers.auth import auth_router


from server.routers.encrypted import encrypted_router


def _validated_upload_path(upload_id: str) -> str:
    """Resolve an upload id to its on-disk directory.

    Upload ids are server-issued UUIDs. Anything that resolves outside
    ``settings.upload_dir`` (e.g. ``..``) is treated as an unknown
    session, so a client cannot read or write chunk files elsewhere
    on disk.
    """
    root = os.path.realpath(settings.upload_dir)
    path = os.path.realpath(os.path.join(root, upload_id))
    # Must be a *proper* subdirectory of the upload root: the root itself
    # (ids like "." or "") and anything escaping it are rejected.
    if path == root or not path.startswith(root + os.sep):
        raise HTTPException(status_code=404, detail="Upload session not found.")
    if not os.path.isdir(path):
        raise HTTPException(status_code=404, detail="Upload session not found.")
    return path


async def _reassemble_file(upload_id: str, total_chunks: int, filename: str) -> str:
    upload_dir = _validated_upload_path(upload_id)

    for i in range(total_chunks):
        if not os.path.exists(os.path.join(upload_dir, f"chunk_{i}")):
            raise HTTPException(
                status_code=400, detail=f"Missing chunk {i} for upload."
            )

    fd, temp_path = tempfile.mkstemp(dir=settings.upload_dir, prefix=f"{upload_id}-")

    try:
        with os.fdopen(fd, "wb") as final_file:
            for i in range(total_chunks):
                chunk_path = os.path.join(upload_dir, f"chunk_{i}")
                with open(chunk_path, "rb") as chunk_file:
                    final_file.write(chunk_file.read())

        await asyncio.to_thread(shutil.rmtree, upload_dir, ignore_errors=True)

        return temp_path
    except Exception as e:
        if os.path.exists(temp_path):
            os.remove(temp_path)
        await asyncio.to_thread(shutil.rmtree, upload_dir, ignore_errors=True)
        logger.error(f"Failed to reassemble file for upload {upload_id}: {e}")
        raise HTTPException(status_code=500, detail="Failed to reassemble file.")


def _get_unique_filename(directory: str, filename: str) -> str:
    if not os.path.exists(os.path.join(directory, filename)):
        return filename

    name, ext = os.path.splitext(filename)
    counter = 1
    while True:
        new_filename = f"{name}-{counter}{ext}"
        if not os.path.exists(os.path.join(directory, new_filename)):
            return new_filename
        counter += 1


def _sanitize_for_filename(name: str) -> str:
    if not name:
        return "unnamed"
    s = name.lower().strip()
    s = re.sub(r"[\s_]+", "-", s)
    s = re.sub(r"[^a-z0-9-]", "", s)
    return s[:50]


def _user_can_access_app(app, username: str, user_groups: list) -> bool:
    """Return True if the user may use the app.

    The user must be listed in ``app.users`` or belong to *any* of the groups
    in ``app.groups``. ``"*"`` / ``"all"`` are wildcards meaning "every user" /
    "every group". There is no admin bypass: admins are subject to the same
    per-app access rules as regular users.

    ``user_groups`` is the user's full multi-group membership list (not just
    the primary group), so a user qualifies if *any* of their groups is
    allowed by the app.
    """
    allowed_users = app.users or []
    allowed_groups = app.groups or []
    user_groups = user_groups or []
    return (
        "*" in allowed_users
        or "all" in allowed_users
        or username in allowed_users
        or "*" in allowed_groups
        or "all" in allowed_groups
        or any(g in allowed_groups for g in user_groups)
    )


async def _stop_session(session_id: str):
    logger.info(f"[{session_id}] Stopping session...")
    try:
        await collaboration.notify_session_ended(session_id)
    except Exception as e:
        logger.error(f"[{session_id}] Failed to broadcast session end: {e}")

    if session_data := SESSIONS_DB.pop(session_id, None):
        registry = session_data.get("container_registry", {})
        if not registry and "provider_app_id" in session_data:
            registry = {session_data["provider_app_id"]: {"instance_id": session_data["instance_id"]}}
        for app_id, container_info in registry.items():
            try:
                app_config = INSTALLED_APPS.get(app_id)
                if app_config:
                    provider = DockerProvider(app_config.model_dump())
                    await provider.stop(container_info["instance_id"])
            except Exception as e:
                logger.error(f"[{session_id}] Failed to stop container for app {app_id}: {e}")
        host_mount_path = session_data.get("host_mount_path")
        await save_sessions_to_disk()
        ephemeral_base_path = os.path.join(settings.storage_path, "vreckan_ephemeral")
        if host_mount_path and host_mount_path.startswith(ephemeral_base_path):
            if os.path.exists(host_mount_path):
                await asyncio.to_thread(
                    shutil.rmtree, host_mount_path, ignore_errors=True
                )
        shared_files_path = session_data.get("shared_files_path")
        if shared_files_path and shared_files_path.startswith(ephemeral_base_path) and os.path.exists(shared_files_path):
            await asyncio.to_thread(shutil.rmtree, shared_files_path, ignore_errors=True)
            logger.info(f"[{session_id}] Removed ephemeral shared files directory.")
        logger.info(f"[{session_id}] Session stopped and cleaned up successfully.")
    else:
        logger.warning(
            f"Attempted to stop session {session_id}, but it was not found in the database."
        )

async def ensure_container_for_session(session_id: str, target_app_id: str) -> dict:
    session = SESSIONS_DB.get(session_id)
    if not session:
        raise HTTPException(status_code=404, detail="Session not found.")
    if "container_registry" not in session:
        session["container_registry"] = {}
    if target_app_id in session["container_registry"]:
        return session["container_registry"][target_app_id]
    app_config = INSTALLED_APPS.get(target_app_id)
    if not app_config:
        raise HTTPException(status_code=404, detail=f"App {target_app_id} not found.")
    # Lowest-priority layer: the admin-editable global defaults from the
    # template schema; everything applied below overrides them.
    env_vars = _schema_default_env()
    env_vars.update({
        "SUBFOLDER": f"/api/apps/session/{session_id}/",
        "PUID": str(settings.puid),
        "PGID": str(settings.pgid),
        "CUSTOM_USER": session.get("custom_user", "abc"),
        "PASSWORD": session.get("password", "abc"),
        "TZ": "America/Phoenix",
        "SELKIES_ALLOWED_ORIGINS": "*",
    })
    if session.get("wayland_mode", True):
        env_vars["PIXELFLUX_WAYLAND"] = "true"
    else:
        env_vars["PIXELFLUX_WAYLAND"] = "false"
    if session.get("is_collaboration"):
        env_vars["SELKIES_MASTER_TOKEN"] = session.get("master_token")
    template = APP_TEMPLATES.get(app_config.app_template)
    if template and template.get("settings"):
        # DOCKER_* entries are docker run-options, not container env vars;
        # they are applied via _extract_docker_overrides below.
        env_vars.update(
            {k: str(v) for k, v in template["settings"].items() if not k.startswith("DOCKER_")}
        )
    if app_config.provider_config.env:
        for env_override in app_config.provider_config.env:
            env_vars[env_override.name] = env_override.value
    # Keep the session's custom name in the window title for apps added to
    # an existing (named) session, mirroring _launch_common.
    if session.get("name"):
        env_vars["TITLE"] = f"{app_config.name} - {session['name']}"
    gpu_config = session.get("gpu_config")
    if gpu_config:
        if gpu_config["type"] == "nvidia" and not app_config.provider_config.nvidia_support:
            gpu_config = None
        elif gpu_config["type"] == "dri3":
            if not app_config.provider_config.dri3_support:
                gpu_config = None
            else:
                env_vars["DRI_NODE"] = gpu_config["device"]
                env_vars["DRINODE"] = gpu_config["device"]

        if gpu_config and gpu_config["type"] == "nvidia" and session.get("wayland_mode", True):
            env_vars["DRI_NODE"] = gpu_config["device"]
            env_vars["DRINODE"] = gpu_config["device"]
    volumes = {}
    current_home_path = session.get("host_mount_path")
    ephemeral_base = os.path.join(settings.storage_path, "vreckan_ephemeral")
    is_ephemeral = current_home_path and current_home_path.startswith(ephemeral_base)
    new_home_path = None
    if is_ephemeral:
        new_home_path = os.path.join(ephemeral_base, str(uuid.uuid4()))
        os.makedirs(new_home_path, exist_ok=True, mode=0o700)
    else:
        username = session.get("username")
        if username:
            sanitized_app_name = _sanitize_for_filename(app_config.name)
            new_home_path = os.path.join(settings.storage_path, username, f"auto-{sanitized_app_name}")
            if app_config.is_meta_app and not os.path.exists(new_home_path):
                template_path = os.path.join(settings.home_templates_path, app_config.home_template_name)
                if os.path.isdir(template_path):
                     await asyncio.to_thread(_safe_copytree, template_path, new_home_path, symlinks=True)
            os.makedirs(new_home_path, exist_ok=True, mode=0o700)
    session["host_mount_path"] = new_home_path
    if new_home_path:
        translated_home = _translate_path_to_host(new_home_path)
        volumes[translated_home] = {
            "bind": settings.container_config_path,
            "mode": "rw",
        }
    shared_files_path = session.get("shared_files_path")
    if shared_files_path and os.path.exists(shared_files_path):
        if new_home_path:
            os.makedirs(os.path.join(new_home_path, "Desktop", "files"), exist_ok=True, mode=0o755)
        translated_shared = _translate_path_to_host(shared_files_path)
        volumes[translated_shared] = {
            "bind": os.path.join(settings.container_config_path, "Desktop", "files"),
            "mode": "rw"
        }
    # External volume mounts (app-swap path) — keep parity with _launch_common.
    _apply_external_mounts(volumes, session.get("username"), new_home_path)
    autostart_content = None
    autostart_subpath = None
    wayland_mode = session.get("wayland_mode", True)

    if wayland_mode:
        autostart_subpath = os.path.join(".config", "labwc", "autostart")
        if app_config.provider_config.custom_autostart_wayland_script_b64:
            try:
                autostart_content = base64.b64decode(
                    app_config.provider_config.custom_autostart_wayland_script_b64
                ).decode("utf-8")
            except Exception:
                pass
        elif app_config.provider_config.autostart:
            cache_path = _get_autostart_cache_path(app_config, suffix="-wayland")
            if cache_path and os.path.exists(cache_path):
                try:
                    with open(cache_path, "r") as f:
                        autostart_content = f.read()
                except Exception:
                    pass
    else:
        autostart_subpath = os.path.join(".config", "openbox", "autostart")
        if app_config.provider_config.custom_autostart_script_b64:
            try:
                autostart_content = base64.b64decode(
                    app_config.provider_config.custom_autostart_script_b64
                ).decode("utf-8")
            except Exception:
                pass
        elif app_config.provider_config.autostart:
            cache_path = _get_autostart_cache_path(app_config)
            if cache_path and os.path.exists(cache_path):
                try:
                    with open(cache_path, "r") as f:
                        autostart_content = f.read()
                except Exception:
                    pass

    if autostart_content and new_home_path and autostart_subpath:
        autostart_file = os.path.join(new_home_path, autostart_subpath)
        autostart_dir = os.path.dirname(autostart_file)
        try:
            os.makedirs(autostart_dir, exist_ok=True, mode=0o755)
            with open(autostart_file, "w") as f:
                f.write(autostart_content)
            os.chmod(autostart_file, 0o755)
        except Exception as e:
            logger.error(f"Failed to inject autostart on swap: {e}")

    app_config_dict = _get_app_config_with_overrides(app_config)

    # Lowest-priority docker layer: the base layer (template schema). The
    # app config's own overrides (already in app_config_dict) and the app
    # template's (merged below) take precedence over it.
    _apply_base_docker_overrides(app_config_dict)

    template_overrides = _extract_docker_overrides(template.get("settings", {}) if template else {})
    if template_overrides:
        if "provider_config" not in app_config_dict:
            app_config_dict["provider_config"] = {}
        if "docker_overrides" not in app_config_dict["provider_config"]:
            app_config_dict["provider_config"]["docker_overrides"] = {}
        
        for k, v in template_overrides.items():
            if k == "devices" and "devices" in app_config_dict["provider_config"]["docker_overrides"]:
                 app_config_dict["provider_config"]["docker_overrides"][k].extend(v)
            elif k == "volumes" and "volumes" in app_config_dict["provider_config"]["docker_overrides"]:
                 app_config_dict["provider_config"]["docker_overrides"][k].extend(v)
            elif k == "environment" and "environment" in app_config_dict["provider_config"]["docker_overrides"]:
                 app_config_dict["provider_config"]["docker_overrides"][k].update(v)
            else:
                 app_config_dict["provider_config"]["docker_overrides"][k] = v

    provider = DockerProvider(app_config_dict)

    launch_kwargs = {
        "session_id": session_id,
        "env_vars": env_vars,
        "volumes": volumes,
        "gpu_config": gpu_config,
        "network": DISCOVERED_NETWORK,
    }

    if session.get("is_collaboration"):
        mk_owner = session.get("mk_owner_token")
        controller_token = session.get("controller_token")
        
        initial_tokens = {
            controller_token: {
                "role": "controller", 
                "slot": session.get("controller_slot"),
                "mk_control": (mk_owner == controller_token) if mk_owner else True
            }
        }
        
        for v in session.get("viewers", []):
            initial_tokens[v["token"]] = {
                "role": "viewer", 
                "slot": v.get("slot"),
                "mk_control": v["token"] == mk_owner
            }
        launch_kwargs.update({
            "is_collaboration": True,
            "master_token": session.get("master_token"),
            "initial_tokens": initial_tokens
        })
        for v in session.get("viewers", []):
            launch_kwargs["initial_tokens"][v["token"]] = {"role": "viewer", "slot": v.get("slot")}
    instance_details = await provider.launch(**launch_kwargs)
    container_info = {
        "instance_id": instance_details["instance_id"],
        "ip": instance_details["ip"],
        "port": instance_details["port"],
        "app_id": target_app_id,
        "created_at": time.time(),
        # Stored so a missing container can be re-launched after a reboot
        # (see _relaunch_stale_session).
        "app_config": app_config_dict,
        "launch_kwargs": launch_kwargs,
    }
    session["container_registry"][target_app_id] = container_info
    await save_sessions_to_disk()
    return container_info

async def stop_container_in_session(session_id: str, target_app_id: str):
    session = SESSIONS_DB.get(session_id)
    if not session or "container_registry" not in session:
        return

    if target_app_id in session["container_registry"]:
        container_info = session["container_registry"][target_app_id]
        app_config = INSTALLED_APPS.get(target_app_id)
        if app_config:
            provider = DockerProvider(app_config.model_dump())
            await provider.stop(container_info["instance_id"])
        del session["container_registry"][target_app_id]
        await save_sessions_to_disk()

def _apply_external_mounts(volumes: dict, username: str, host_mount_path: str) -> None:
    """Add admin-assigned external volume mounts to a container's volumes dict.

    Mounts are resolved by user (scope=user) and by the user's effective group
    (scope=group). Container paths are relative to the in-container home-dir
    mount point unless they are absolute. No-op when there is no home mount.
    """
    if not host_mount_path or not username:
        return
    try:
        group = user_manager.get_effective_settings(username).get("group", "none")
    except Exception:  # noqa: BLE001
        group = "none"
    for m in volume_mount_manager.get_mounts_for_user(username, group):
        host = m.get("host_path") or ""
        cpath = m.get("container_path") or ""
        if not host or not cpath:
            continue
        bind = cpath if cpath.startswith("/") else os.path.join(
            settings.container_config_path, cpath
        )
        volumes[host] = {
            "bind": bind,
            "mode": "ro" if m.get("read_only") else "rw",
        }


async def _launch_common(
    application_id: str,
    username: str,
    effective_settings: dict,
    home_name: Optional[str],
    env_vars: dict,
    language: Optional[str],
    selected_gpu: Optional[str],
    file_bytes: Optional[bytes] = None,
    filename: Optional[str] = None,
    open_file_on_launch: bool = True,
    forced_rw_mount: Optional[str] = None,
    launch_in_room_mode: bool = False,
    wayland_mode: bool = True,
    session_name: Optional[str] = None,
    check_access: bool = True,
) -> dict:
    app_config = INSTALLED_APPS.get(application_id)
    if not app_config:
        raise HTTPException(
            status_code=404, detail=f"Application with ID '{application_id}' not found."
        )

    if check_access:
        # Per-app access control: only users listed in app.users, or members
        # of a group in app.groups, may launch the app. There is no admin
        # bypass (see _user_can_access_app). The user's full multi-group
        # membership is checked (not just the primary group).
        user_groups = (user_manager.get_user(username) or {}).get("groups") or []
        if not _user_can_access_app(app_config, username, user_groups):
            logger.info(
                f"[{username}] Launch denied for '{app_config.name}': "
                f"users={app_config.users} groups={app_config.groups} (user groups: {user_groups})"
            )
            raise HTTPException(
                status_code=403,
                detail=f"You do not have access to '{app_config.name}'.",
            )

    session_id = str(uuid.uuid4())
    access_token = secrets.token_urlsafe(32)
    subfolder = f"/api/apps/session/{session_id}/"
    launch_context = None

    master_token = None
    controller_token = None
    viewer_token = None
    if launch_in_room_mode:
        master_token = secrets.token_urlsafe(32)
        controller_token = secrets.token_urlsafe(16)
        viewer_token = secrets.token_urlsafe(16)

    custom_user = str(uuid.uuid4())
    password = str(uuid.uuid4())

    # Lowest-priority layer: the admin-editable global defaults from the
    # template schema; everything applied below overrides them.
    final_env = _schema_default_env()
    final_env.update({
        "SUBFOLDER": subfolder,
        "PUID": str(settings.puid),
        "PGID": str(settings.pgid),
        "CUSTOM_USER": custom_user,
        "PASSWORD": password,
        "TZ": "America/Phoenix",
        "SELKIES_ALLOWED_ORIGINS": "*",
    })

    if wayland_mode:
        final_env["PIXELFLUX_WAYLAND"] = "true"

    if master_token:
        final_env["SELKIES_MASTER_TOKEN"] = master_token

    template_name = app_config.app_template
    template = APP_TEMPLATES.get(template_name)
    docker_overrides_from_template = {}

    if template and template.get("settings"):
        raw_settings = template["settings"]
        template_settings = {}
        for k, v in raw_settings.items():
            if not k.startswith("DOCKER_"):
                template_settings[k] = str(v)
        
        docker_overrides_from_template = _extract_docker_overrides(raw_settings)
        final_env.update(template_settings)
    elif not template:
        logger.warning(
            f"[{session_id}] Template '{template_name}' not found for app '{app_config.name}'. Using container defaults."
        )

    final_env.update(env_vars)
    if language and language.lower() != "en_us.utf-8":
        final_env["LC_ALL"] = language
    if app_config.provider_config.env:
        for env_override in app_config.provider_config.env:
            final_env[env_override.name] = env_override.value

    # Optional per-session name: when set, the window title becomes
    # "<App Name> - <Session Name>". Applied last so it wins over every
    # other TITLE layer (base layer, app template, app config, image default).
    if session_name:
        final_env["TITLE"] = f"{app_config.name} - {session_name}"

    # Record the launch context. If this app's provider config names a CLI
    # env var (e.g. CHROME_CLI / FIREFOX_CLI), also map the requested URL
    # onto it: upstream linuxserver images read <APP>_CLI for the initial
    # URL/arguments rather than VRECKAN_URL, so without this the app would
    # start with no URL.
    if "VRECKAN_URL" in final_env:
        launch_context = {"type": "url", "value": final_env["VRECKAN_URL"]}
        url_env_var = app_config.provider_config.url_env_var
        if url_env_var:
            final_env[url_env_var] = final_env["VRECKAN_URL"]

    app_config_dict = _get_app_config_with_overrides(app_config)

    # Lowest-priority docker layer: the base layer (template schema). The
    # app config's own overrides (already in app_config_dict) and the app
    # template's (merged below) take precedence over it.
    _apply_base_docker_overrides(app_config_dict)

    if docker_overrides_from_template:
        if "provider_config" not in app_config_dict:
            app_config_dict["provider_config"] = {}
        
        if "docker_overrides" not in app_config_dict["provider_config"] or app_config_dict["provider_config"]["docker_overrides"] is None:
            app_config_dict["provider_config"]["docker_overrides"] = {}
        
        existing_overrides = app_config_dict["provider_config"]["docker_overrides"]
        for k, v in docker_overrides_from_template.items():
            if k == "devices" and existing_overrides.get("devices") is not None:
                 existing_overrides[k].extend(v)
            elif k == "volumes" and existing_overrides.get("volumes") is not None:
                 if isinstance(existing_overrides[k], list) and isinstance(v, list):
                     existing_overrides[k].extend(v)
            elif k == "environment" and existing_overrides.get("environment") is not None:
                 existing_overrides[k].update(v)
            else:
                 existing_overrides[k] = v

    provider = DockerProvider(app_config_dict)

    volumes = {}
    host_mount_path = None
    shared_files_path = None
    is_ephemeral_storage = False

    is_persistent_launch = effective_settings.get("persistent_storage", False) and (
        home_name is None or home_name.lower() != "cleanroom"
    )

    if forced_rw_mount:
        host_mount_path = forced_rw_mount
        os.makedirs(host_mount_path, exist_ok=True, mode=0o700)
    elif app_config.is_meta_app:
        template_path = os.path.join(
            settings.home_templates_path, app_config.home_template_name
        )
        if not os.path.isdir(template_path):
            raise HTTPException(
                status_code=500,
                detail=f"Home directory template for meta app '{app_config.name}' not found on server.",
            )

        if is_persistent_launch:
            sanitized_app_name = _sanitize_for_filename(app_config.name)
            user_facing_dir_name = f"auto-{sanitized_app_name}"
            host_mount_path = os.path.join(
                settings.storage_path, username, user_facing_dir_name
            )
            if not os.path.exists(host_mount_path):
                logger.info(
                    f"[{session_id}] First launch for meta-app. Copying template '{app_config.home_template_name}' for user '{username}'."
                )
                await asyncio.to_thread(
                    _safe_copytree, template_path, host_mount_path, symlinks=True
                )
            shared_files_path = os.path.join(settings.storage_path, username, "_vreckan_shared_files")
        else:
            is_ephemeral_storage = True
            host_mount_path = os.path.join(
                settings.storage_path, "vreckan_ephemeral", str(uuid.uuid4())
            )
            logger.info(
                f"[{session_id}] Launching meta-app in cleanroom mode. Copying template '{app_config.home_template_name}' to ephemeral storage."
            )
            await asyncio.to_thread(
                _safe_copytree, template_path, host_mount_path, symlinks=True
            )
            shared_files_path = os.path.join(settings.storage_path, "vreckan_ephemeral", f"{session_id}_shared")
            os.makedirs(shared_files_path, exist_ok=True, mode=0o755)
    else:
        use_persistent_storage = (
            effective_settings.get("persistent_storage", False)
            and app_config.home_directories
        )
        if not use_persistent_storage:
            home_name = "cleanroom"

        if home_name and home_name.lower() != "cleanroom":
            host_mount_path = os.path.abspath(
                os.path.join(settings.storage_path, username, home_name)
            )
            if not os.path.isdir(host_mount_path):
                raise HTTPException(
                    status_code=404, detail=f"Home directory '{home_name}' not found."
                )
            shared_files_path = os.path.abspath(
                os.path.join(settings.storage_path, username, "_vreckan_shared_files")
            )
        else:
            is_ephemeral_storage = True
            host_mount_path = os.path.join(
                settings.storage_path, "vreckan_ephemeral", str(uuid.uuid4())
            )
            shared_files_path = os.path.join(settings.storage_path, "vreckan_ephemeral", f"{session_id}_shared")
            os.makedirs(shared_files_path, exist_ok=True, mode=0o755)

    gpu_config = None
    user_selected_gpu = bool(selected_gpu and effective_settings.get("gpu", False))
    if user_selected_gpu:
        gpu_info = next(
            (gpu for gpu in AVAILABLE_GPUS if gpu["device"] == selected_gpu), None
        )
        if not gpu_info:
            raise HTTPException(
                status_code=400,
                detail=f"Selected GPU '{selected_gpu}' is not available.",
            )
        if (
            gpu_info["type"] == "nvidia"
            and not app_config.provider_config.nvidia_support
        ):
            raise HTTPException(
                status_code=400,
                detail=f"App '{app_config.name}' does not support Nvidia GPUs.",
            )
        if gpu_info["type"] == "dri3" and not app_config.provider_config.dri3_support:
            raise HTTPException(
                status_code=400,
                detail=f"App '{app_config.name}' does not support DRI3 GPUs.",
            )
        gpu_config = gpu_info
    elif GLOBAL_DEFAULT_GPU:
        # Global default GPU (set on the Global Settings page): fallback for
        # launches without a personal GPU pick. Silently skipped when the app
        # doesn't support the GPU type.
        gpu_info = next(
            (gpu for gpu in AVAILABLE_GPUS if gpu["device"] == GLOBAL_DEFAULT_GPU), None
        )
        if gpu_info and (
            (gpu_info["type"] != "nvidia" or app_config.provider_config.nvidia_support)
            and (gpu_info["type"] != "dri3" or app_config.provider_config.dri3_support)
        ):
            gpu_config = gpu_info
            logger.info(
                f"[{session_id}] Using global default GPU '{gpu_info['device']}' for '{app_config.name}'."
            )
    if gpu_config and (
        gpu_config["type"] == "dri3"
        or (gpu_config["type"] == "nvidia" and wayland_mode)
    ):
        final_env["DRI_NODE"] = gpu_config["device"]
        final_env["DRINODE"] = gpu_config["device"]

    autostart_content = None
    autostart_subpath = None

    if wayland_mode:
        autostart_subpath = os.path.join(".config", "labwc", "autostart")
        if app_config.provider_config.custom_autostart_wayland_script_b64:
            try:
                autostart_content = base64.b64decode(
                    app_config.provider_config.custom_autostart_wayland_script_b64
                ).decode("utf-8")
                logger.info(
                    f"[{session_id}] Using custom Wayland autostart script for '{app_config.name}'."
                )
            except Exception as e:
                logger.error(
                    f"[{session_id}] Failed to decode custom Wayland autostart script: {e}"
                )
        elif app_config.provider_config.autostart:
            autostart_cache_path = _get_autostart_cache_path(
                app_config, suffix="-wayland"
            )
            if (
                autostart_cache_path
                and os.path.exists(autostart_cache_path)
                and os.path.getsize(autostart_cache_path) > 0
            ):
                try:
                    with open(autostart_cache_path, "r") as f:
                        autostart_content = f.read()
                    logger.info(
                        f"[{session_id}] Using cached repository Wayland autostart script for '{app_config.name}'."
                    )
                except Exception as e:
                    logger.error(
                        f"[{session_id}] Failed to read cached Wayland autostart script: {e}"
                    )
    else:
        autostart_subpath = os.path.join(".config", "openbox", "autostart")
        if app_config.provider_config.custom_autostart_script_b64:
            try:
                autostart_content = base64.b64decode(
                    app_config.provider_config.custom_autostart_script_b64
                ).decode("utf-8")
                logger.info(
                    f"[{session_id}] Using custom autostart script for '{app_config.name}'."
                )
            except Exception as e:
                logger.error(
                    f"[{session_id}] Failed to decode custom autostart script: {e}"
                )
        elif app_config.provider_config.autostart:
            autostart_cache_path = _get_autostart_cache_path(app_config)
            if (
                autostart_cache_path
                and os.path.exists(autostart_cache_path)
                and os.path.getsize(autostart_cache_path) > 0
            ):
                try:
                    with open(autostart_cache_path, "r") as f:
                        autostart_content = f.read()
                    logger.info(
                        f"[{session_id}] Using cached repository autostart script for '{app_config.name}'."
                    )
                except Exception as e:
                    logger.error(
                        f"[{session_id}] Failed to read cached autostart script: {e}"
                    )

    if autostart_content and autostart_subpath:
        if not host_mount_path:
            host_mount_path = os.path.join(
                settings.storage_path, "vreckan_ephemeral", str(uuid.uuid4())
            )
            logger.info(
                f"[{session_id}] Created ephemeral storage for autostart script."
            )

        autostart_file = os.path.join(host_mount_path, autostart_subpath)
        autostart_dir = os.path.dirname(autostart_file)
        try:
            os.makedirs(autostart_dir, exist_ok=True, mode=0o755)
            with open(autostart_file, "w") as f:
                f.write(autostart_content)
            os.chmod(autostart_file, 0o755)
            logger.info(
                f"[{session_id}] Successfully wrote autostart script to session storage."
            )
        except Exception as e:
            logger.error(f"[{session_id}] Failed to write autostart script: {e}")

    if host_mount_path:
        translated_host_mount_path = _translate_path_to_host(host_mount_path)
        volumes[translated_host_mount_path] = {
            "bind": settings.container_config_path,
            "mode": "rw",
        }

    if shared_files_path:
        os.makedirs(shared_files_path, exist_ok=True, mode=0o755)
        translated_shared_files_path = _translate_path_to_host(shared_files_path)
        volumes[translated_shared_files_path] = {
            "bind": os.path.join(settings.container_config_path, "Desktop", "files"),
            "mode": "rw",
        }
        if file_bytes and filename:
            if shared_files_path:
                file_dest_dir = shared_files_path
            else:
                file_dest_dir = os.path.join(host_mount_path, "Desktop", "files")

            os.makedirs(file_dest_dir, exist_ok=True, mode=0o755)
            actual_filename = _get_unique_filename(file_dest_dir, filename)
            file_location = os.path.join(file_dest_dir, actual_filename)

            with open(file_location, "wb") as f:
                f.write(file_bytes)
            os.chmod(file_location, 0o644)
            if open_file_on_launch:
                container_file_path = os.path.join(
                    settings.container_config_path, "Desktop", "files", actual_filename
                )
                final_env["VRECKAN_FILE"] = container_file_path
                launch_context = {"type": "file", "value": filename}

    # External volume mounts assigned to this user (directly or via group).
    _apply_external_mounts(volumes, username, host_mount_path)

    try:
        provider_launch_kwargs = {
            "session_id": session_id,
            "env_vars": final_env,
            "volumes": volumes,
            "gpu_config": gpu_config,
            "network": DISCOVERED_NETWORK,
        }
        if launch_in_room_mode:
            provider_launch_kwargs.update(
                {
                    "is_collaboration": True,
                    "master_token": master_token,
                    "initial_tokens": {
                        controller_token: {"role": "controller", "slot": None},
                    },
                }
            )

        instance_details = await provider.launch(**provider_launch_kwargs)
        SESSIONS_DB[session_id] = {
            "instance_id": instance_details["instance_id"],
            "ip": instance_details["ip"],
            "port": instance_details["port"],
            "created_at": time.time(),
            "access_token": access_token,
            "provider_app_id": application_id,
            "username": username,
            "app_name": app_config.name,
            "app_logo": app_config.logo,
            "name": session_name,
            "host_mount_path": host_mount_path,
            "shared_files_path": shared_files_path,
            "launch_context": launch_context,
            "custom_user": custom_user,
            "password": password,
            "gpu_config": gpu_config,
            "wayland_mode": wayland_mode,
            "container_registry": {
                application_id: {
                    "instance_id": instance_details["instance_id"],
                    "ip": instance_details["ip"],
                    "port": instance_details["port"],
                    "app_id": application_id,
                    "created_at": time.time(),
                    # Stored so a missing container can be re-launched after a
                    # reboot (see _relaunch_stale_session).
                    "app_config": app_config_dict,
                    "launch_kwargs": provider_launch_kwargs,
                }
            }
        }
        if launch_in_room_mode:
            SESSIONS_DB[session_id].update(
                {
                    "is_collaboration": True,
                    "master_token": master_token,
                    "controller_token": controller_token,
                    "viewer_token": viewer_token,
                    "viewers": [],
                }
            )

        # Persist immediately: the in-memory dict is the runtime source of
        # truth, but without a DB row a server restart would forget this
        # session and orphan its container(s).
        await save_sessions_to_disk()

        logger.info(
            f"[{session_id}] Session ready for {username}. Proxying to {instance_details['ip']}:{instance_details['port']}"
        )
        if launch_in_room_mode:
            session_url = f"/room/{session_id}?access_token={access_token}&token={controller_token}"
        else:
            session_url = f"/api/apps/session/{session_id}/?access_token={access_token}"

        return {"session_url": session_url, "session_id": session_id}
    except Exception as e:
        if host_mount_path and host_mount_path.startswith(
            os.path.join(settings.storage_path, "vreckan_ephemeral")
        ):
            shutil.rmtree(host_mount_path, ignore_errors=True)
        logger.error(
            f"[{session_id}] Unhandled exception during launch for app '{application_id}': {e}",
            exc_info=True,
        )
        if isinstance(e, HTTPException):
            raise
        raise HTTPException(
            status_code=500,
            detail="An internal error occurred during application launch.",
        )


from server.routers.session import session_router


from server.routers.homedir import homedir_router


from server.routers.pinned import pinned_router


from server.routers.admin import admin_router


from server.routers.upload import upload_router


from server.routers.files import files_router


encrypted_router.include_router(admin_router)
encrypted_router.include_router(homedir_router)
encrypted_router.include_router(session_router)
encrypted_router.include_router(upload_router)
encrypted_router.include_router(files_router)
encrypted_router.include_router(pinned_router)
api_app.include_router(encrypted_router)
api_app.include_router(collaboration.router)
# Auth endpoints are exempt from E2EE (like /api/handshake/*) and are mounted
# directly on api_app so that login works before the session key exists.
api_app.include_router(auth_router)

# ---------------------------------------------------------------------------
# Session reverse proxy
#
# The API server terminates TLS directly and reverse-proxies
# /api/apps/session/{session_id}/... to each app container: it validates the
# session cookie and forwards the request (HTTP, or a WebSocket upgrade) to
# the container's upstream.
# ---------------------------------------------------------------------------

_PROXY_DROP_HEADERS = {
    "authorization",
    "connection",
    "host",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "x-upstream-host",
    "x-upstream-auth",
}


def _session_cookie_name(session_id: str) -> str:
    return f"{settings.session_cookie_name}_{session_id}"


def _session_upstream_host(session: dict) -> str:
    return f"{session['ip']}:{session['port']}"


def _upstream_path(path: str, session_id: str) -> str:
    """Return the path to forward to the app container.

    The container's own reverse proxy (e.g. the linuxserver/chrome nginx)
    is generated with the session's URL prefix baked into its location
    blocks (``location /api/apps/session/{session_id}/ ...``), so it
    expects requests to arrive with that prefix INTACT. Stripping it would
    fall through to nginx's default welcome page. Forward as-is.
    """
    return path


def _validate_session_cookie(session: dict, cookie_token: Optional[str]) -> bool:
    return bool(cookie_token) and secrets.compare_digest(
        cookie_token, session.get("access_token", "")
    )


def _upstream_headers(request_headers, session: dict, upstream_host: str) -> dict:
    headers = {}
    for key, value in request_headers.items():
        if key.lower() in _PROXY_DROP_HEADERS:
            continue
        headers[key] = value
    headers["Host"] = upstream_host
    if session.get("custom_user") and session.get("password"):
        auth = f"{session['custom_user']}:{session['password']}"
        headers["Authorization"] = f"Basic {base64.b64encode(auth.encode()).decode()}"
    return headers


async def _proxy_session_request(request: Request, session_id: str) -> Response:
    """Reverse-proxy an HTTP request to the session's app container."""
    session = SESSIONS_DB.get(session_id)
    if not session or "ip" not in session or "port" not in session:
        raise HTTPException(status_code=404, detail="Session not found.")
    cookie_token = request.cookies.get(_session_cookie_name(session_id))
    if not _validate_session_cookie(session, cookie_token):
        raise HTTPException(status_code=403, detail="Forbidden: Invalid session or token.")

    upstream_host = _session_upstream_host(session)
    sub_path = _upstream_path(request.url.path, session_id)
    target_url = f"http://{upstream_host}{sub_path}"
    if request.url.query:
        target_url += f"?{request.url.query}"

    headers = _upstream_headers(request.headers, session, upstream_host)
    body = await request.body()

    client = httpx2.AsyncClient(
        timeout=httpx2.Timeout(connect=10.0, read=300.0, write=30.0, pool=10.0)
    )
    try:
        upstream_request = client.build_request(
            request.method, target_url, headers=headers, content=body
        )
        upstream_response = await client.send(upstream_request, stream=True)
    except httpx2.HTTPError as exc:
        await client.aclose()
        logger.error(f"[{session_id}] Proxy connect to {upstream_host} failed: {exc}")
        raise HTTPException(status_code=502, detail="Could not reach the application container.")

    status_code = upstream_response.status_code
    response_headers = {
        key: value
        for key, value in upstream_response.headers.items()
        if key.lower() not in _PROXY_DROP_HEADERS and key.lower() != "content-length"
    }

    async def body_stream():
        # Stream the RAW (still content-encoded) bytes: the Content-Encoding
        # header is forwarded to the client, which performs the decoding.
        # aiter_bytes() would decompress here, and the client would then try
        # to decode an already-decoded body (ERR_CONTENT_DECODING_FAILED).
        try:
            async for chunk in upstream_response.aiter_raw():
                yield chunk
        finally:
            await upstream_response.aclose()
            await client.aclose()

    logger.debug(
        f"[{session_id}] Proxied {request.method} {sub_path} -> {upstream_host} ({status_code})"
    )
    return StreamingResponse(content=body_stream(), status_code=status_code, headers=response_headers)


async def _proxy_session_websocket(websocket: WebSocket, session_id: str) -> None:
    """Proxy a WebSocket connection to the session's app container."""
    session = SESSIONS_DB.get(session_id)
    if not session or "ip" not in session or "port" not in session:
        await websocket.close(code=1003)
        return
    cookie_token = websocket.cookies.get(_session_cookie_name(session_id))
    if not _validate_session_cookie(session, cookie_token):
        await websocket.close(code=1008)
        return

    upstream_host = _session_upstream_host(session)
    sub_path = _upstream_path(websocket.url.path, session_id)
    target_url = f"ws://{upstream_host}{sub_path}"
    if websocket.url.query:
        target_url += f"?{websocket.url.query}"

    # Drop the client's Sec-WebSocket-* headers: the websockets client
    # generates its OWN handshake headers (Sec-WebSocket-Key/Version/
    # Protocol). Forwarding the browser's copies would duplicate
    # Sec-WebSocket-Key and the upstream rejects the handshake with 400.
    ws_headers = {
        key: value
        for key, value in websocket.headers.items()
        if not key.lower().startswith("sec-websocket-")
        and key.lower() not in ("authorization", "host", "connection", "upgrade", "x-upstream-host", "x-upstream-auth")
    }
    if session.get("custom_user") and session.get("password"):
        auth = f"{session['custom_user']}:{session['password']}"
        ws_headers["Authorization"] = f"Basic {base64.b64encode(auth.encode()).decode()}"

    # Starlette's WebSocket object does not expose a .subprotocols attribute;
    # the client-offered subprotocols live in the ASGI scope.
    client_subprotocols = websocket.scope.get("subprotocols") or []
    try:
        upstream_ws = await websockets.connect(
            target_url,
            additional_headers=ws_headers,
            subprotocols=list(client_subprotocols) if client_subprotocols else None,
        )
    except Exception as exc:
        logger.error(f"[{session_id}] WebSocket connect to {upstream_host} failed: {exc}")
        await websocket.close(code=1003)
        return

    negotiated = getattr(upstream_ws, "protocol", None)
    try:
        # Only echo a subprotocol the client actually offered. The upstream
        # may announce a default subprotocol even when the client offered
        # none; relaying that to the client violates RFC 6455 and the
        # browser rejects the handshake ("Response must not include
        # Sec-WebSocket-Protocol header if not present in request").
        if negotiated and client_subprotocols and negotiated in client_subprotocols:
            await websocket.accept(subprotocol=negotiated)
        else:
            await websocket.accept()
    except Exception:
        await upstream_ws.close()
        return

    async def client_to_upstream():
        while True:
            message = await websocket.receive()
            mtype = message.get("type")
            if mtype == "websocket.receive":
                if "text" in message:
                    await upstream_ws.send(message["text"])
                elif "bytes" in message:
                    await upstream_ws.send(message["bytes"])
            else:  # websocket.disconnect / close
                break

    async def upstream_to_client():
        while True:
            message = await upstream_ws.recv()
            if isinstance(message, str):
                await websocket.send_text(message)
            else:
                await websocket.send_bytes(message)

    c2u = asyncio.create_task(client_to_upstream())
    u2c = asyncio.create_task(upstream_to_client())
    try:
        await asyncio.wait({c2u, u2c}, return_when=asyncio.FIRST_COMPLETED)
    finally:
        for task in (c2u, u2c):
            task.cancel()
        for task in (c2u, u2c):
            try:
                await task
            except BaseException:
                pass
        try:
            await upstream_ws.close()
        except Exception:
            pass
        try:
            if websocket.client_state == WebSocketState.CONNECTED:
                await websocket.close()
        except Exception:
            pass


@api_app.api_route(
    "/api/apps/session/{session_id:uuid}/",
    methods=["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"],
    include_in_schema=False,
)
async def initial_session_auth(session_id: uuid.UUID, request: Request):
    session_id_str = str(session_id)
    token = request.query_params.get("access_token")

    session = SESSIONS_DB.get(session_id_str)

    # No access_token in the query string: the browser presents the session
    # cookie. Reverse-proxy the request to the app container.
    if not token:
        return await _proxy_session_request(request, session_id_str)

    if (
        not session
        or not secrets.compare_digest(token, session.get("access_token", ""))
    ):
        raise HTTPException(
            status_code=403, detail="Forbidden: Invalid session or token."
        )

    # Honor a TLS-terminating reverse proxy (if present) so the redirect uses
    # the public scheme (https) rather than the plain-HTTP upstream connection.
    scheme = request.headers.get("x-forwarded-proto") or request.url.scheme
    redirect_url = request.url.replace(scheme=scheme).remove_query_params("access_token")
    response = RedirectResponse(url=str(redirect_url), status_code=303)

    logger.info(
        f"[{session_id_str}] Initial auth successful. Setting session cookie and redirecting."
    )
    
    is_embedded = request.query_params.get("embedded") == "true"
    samesite_policy = "none" if is_embedded else "lax"
    logger.info(f"[{session_id_str}] Setting session cookie with SameSite='{samesite_policy}'.")

    response.set_cookie(
        key=f"{settings.session_cookie_name}_{session_id_str}",
        value=token,
        httponly=True,
        secure=True,
        samesite=samesite_policy,
        path=f"/api/apps/session/{session_id_str}",
    )

    collab_token = request.query_params.get("token")
    if collab_token and session.get("is_collaboration"):
        response.set_cookie(
            key=f"collab_token_{session_id_str}",
            value=collab_token,
            path=f"/api/apps/session/{session_id_str}",
            httponly=True,
            secure=True,
            samesite="none",
        )

    return response


@api_app.api_route(
    "/api/apps/session/{session_id:uuid}/{path:path}",
    methods=["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD"],
    include_in_schema=False,
)
async def session_proxy_subpath(session_id: uuid.UUID, request: Request, path: str):
    """Proxy cookie-authenticated sub-path requests to the app container."""
    return await _proxy_session_request(request, str(session_id))


@api_app.websocket("/api/apps/session/{session_id:uuid}/{path:path}")
async def session_proxy_websocket(session_id: uuid.UUID, websocket: WebSocket, path: str):
    """Proxy WebSocket upgrades (cookie-authenticated) to the app container."""
    await _proxy_session_websocket(websocket, str(session_id))


async def _load_shares_from_db() -> dict:
    """Read public-share metadata straight from the database.

    The unauthenticated public endpoints use this (rather than the in-memory
    cache) so a share is visible as soon as it is persisted, even if the
    in-memory map has not yet been refreshed.
    """
    try:
        async with db.async_session_factory() as session:
            rows = (await session.execute(select(db.PublicShare))).scalars().all()
        return {row.share_id: PublicShareMetadata(**row.data) for row in rows}
    except Exception as e:
        logger.error(f"[API_LOAD] Error reading shares metadata from the database: {e}")
        return {}


@api_app.get("/public/download/{token}")
async def download_shared_file(token: str):
    token_data = DOWNLOAD_TOKENS.pop(token, None)
    if not token_data or token_data.get("expires_at", 0) < time.time():
        raise HTTPException(
            status_code=403, detail="Invalid or expired download token."
        )

    share_id = token_data.get("share_id")
    all_shares = await _load_shares_from_db()
    metadata = all_shares.get(share_id)
    if not metadata:
        raise HTTPException(status_code=404, detail="Shared file not found.")

    file_path = os.path.join(settings.public_storage_path, share_id)
    if not os.path.exists(file_path):
        raise HTTPException(status_code=404, detail="Shared file not found on disk.")

    return FileResponse(
        path=file_path,
        filename=metadata.original_filename,
        media_type="application/octet-stream",
    )


@api_app.get("/public/{share_id}")
async def access_public_share_get(share_id: str):
    all_shares = await _load_shares_from_db()
    metadata = all_shares.get(share_id)
    if not metadata:
        raise HTTPException(status_code=404, detail="Share not found.")

    if metadata.expiry_timestamp and metadata.expiry_timestamp < time.time():
        return HTMLResponse(content="<h1>This link has expired.</h1>", status_code=410)

    if metadata.password_hash:
        password_page_path = os.path.join(
            os.path.dirname(__file__), "static", "public_password.html"
        )
        if os.path.exists(password_page_path):
            with open(password_page_path, "r") as f:
                html_content = (
                    f.read()
                    .replace("{{SHARE_ID}}", share_id)
                    .replace("{{ERROR_MESSAGE}}", "")
                )
            return HTMLResponse(content=html_content)
        return HTMLResponse(content="<h1>Password protected</h1>", status_code=500)

    file_path = os.path.join(settings.public_storage_path, share_id)
    if not os.path.exists(file_path):
        raise HTTPException(status_code=404, detail="Shared file not found on disk.")

    return FileResponse(
        path=file_path,
        filename=metadata.original_filename,
        media_type="application/octet-stream",
    )


@api_app.post("/public/{share_id}")
async def access_public_share_post(share_id: str, password: str = Form(...)):
    all_shares = await _load_shares_from_db()
    metadata = all_shares.get(share_id)
    if not metadata:
        raise HTTPException(status_code=404, detail="Share not found.")
    if metadata.expiry_timestamp and metadata.expiry_timestamp < time.time():
        return HTMLResponse(content="<h1>This link has expired.</h1>", status_code=410)
    if not metadata.password_hash:
        raise HTTPException(
            status_code=400, detail="This share is not password protected."
        )

    submitted_hash = hashlib.sha256(password.encode()).hexdigest()
    if secrets.compare_digest(submitted_hash, metadata.password_hash):
        token = secrets.token_urlsafe(32)
        DOWNLOAD_TOKENS[token] = {"share_id": share_id, "expires_at": time.time() + 60}
        return RedirectResponse(url=f"/public/download/{token}", status_code=303)
    else:
        password_page_path = os.path.join(
            os.path.dirname(__file__), "static", "public_password.html"
        )
        if os.path.exists(password_page_path):
            with open(password_page_path, "r") as f:
                html_content = (
                    f.read()
                    .replace("{{SHARE_ID}}", share_id)
                    .replace(
                        "{{ERROR_MESSAGE}}", "Incorrect password. Please try again."
                    )
                )
            return HTMLResponse(content=html_content, status_code=401)
        return HTMLResponse(content="<h1>Incorrect Password</h1>", status_code=401)


# Short content hash over every file under static/js, used to cache-bust the
# module script tag in the HTML below. Browsers keep ES-module scripts in a
# cache that is separate from (and not cleared by) the HTTP cache, so the only
# reliable way to make a browser that already holds an old copy load a newly
# deployed one is to change the script's URL. Hashing every client script means
# editing ANY of them bumps the version and forces a fresh fetch.
def _client_js_version() -> str:
    js_dir = os.path.join(os.path.dirname(__file__), "static", "js")
    digest = hashlib.sha256()
    try:
        for name in sorted(os.listdir(js_dir)):
            if name.endswith(".js"):
                with open(os.path.join(js_dir, name), "rb") as f:
                    digest.update(name.encode("utf-8"))
                    digest.update(f.read())
    except OSError:
        pass
    return digest.hexdigest()[:16]


@api_app.get("/", response_class=HTMLResponse, include_in_schema=False)
async def read_root():
    html_file_path = os.path.join(os.path.dirname(__file__), "static", "index.html")
    if os.path.exists(html_file_path):
        with open(html_file_path, "r") as f:
            html = f.read()
        # Cache-bust the module entry point so browsers always load fresh JS.
        # The entry is entry.js (a thin wrapper that imports app.js once); see
        # entry.js for why app.js must not be the <script> target directly.
        html = html.replace(
            'src="/js/entry.js"',
            f'src="/js/entry.js?v={_client_js_version()}"',
        )
        # no-cache: always revalidate the document, so a new deploy (and its
        # new JS version string) is picked up on the user's next load.
        return HTMLResponse(content=html, headers={"Cache-Control": "no-cache"})
    return HTMLResponse(content="<h1>Vreckan Server</h1>", status_code=404)


STATIC_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "static"))


@api_app.get("/{path:path}", include_in_schema=False)
async def serve_static(path: str):
    """Serves web-app static assets (css/js/img) from the static directory.

    Registered last so it never shadows explicit API/session routes. Only files
    that physically exist under the static directory are served; any path
    traversal attempt or missing file results in a 404.
    """
    candidate = os.path.abspath(os.path.join(STATIC_DIR, path))
    if candidate != STATIC_DIR and not candidate.startswith(STATIC_DIR + os.sep):
        raise HTTPException(status_code=404, detail="Not found")
    if os.path.isfile(candidate):
        media_type, _ = mimetypes.guess_type(candidate)
        # JS/CSS are never stored (no-store): browsers keep ES modules in a
        # separate in-memory cache that ordinary cache headers do not clear, so
        # we forbid storing them at all — every page load fetches the current
        # files. Other assets (images, etc.) simply revalidate (no-cache).
        if candidate.lower().endswith((".js", ".css")):
            headers = {"Cache-Control": "no-store"}
        else:
            headers = {"Cache-Control": "no-cache"}
        return FileResponse(
            candidate,
            media_type=media_type or "application/octet-stream",
            headers=headers,
        )
    raise HTTPException(status_code=404, detail="Not found")
