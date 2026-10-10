"""Async configuration database layer for Vreckan.

All durable application configuration — users, groups, volume mounts, app
stores, installed apps, app templates, public shares, plus the ephemeral
session and auth-token stores — is persisted here instead of the legacy YAML
files. The in-memory structures owned by the manager modules remain the
runtime source of truth; this module is their persistence backend.

The backend is selected by ``settings.database_url`` (the
``VRECKAN_DATABASE_URL`` environment variable). It defaults to a SQLite file
in the data root, but any SQLAlchemy async URL works — e.g.
``postgresql+asyncpg://user:pass@host:5432/vreckan`` for PostgreSQL. The
schema and queries are identical across backends.

A ``NullPool`` is used so the engine is safe to drive from more than one
event loop (the server loop plus, e.g., an out-of-process test harness).
Configuration writes are infrequent, so the negligible cost of not reusing
connections is an acceptable trade for that safety.
"""
import json
import logging
import os
from typing import Optional

from sqlalchemy import JSON, Boolean, Float, Integer, String, Text, inspect, select, text
from sqlalchemy.ext.asyncio import (
    async_sessionmaker,
    create_async_engine,
)
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column
from sqlalchemy.pool import NullPool

from .settings import settings

logger = logging.getLogger(__name__)


class Base(DeclarativeBase):
    """Declarative base for all configuration tables."""


class User(Base):
    """A user or admin account.

    Consolidates the legacy file types: the per-user settings block
    (``settings``, NULL for admins) and the web-login password hash
    (``password_hash``, NULL when no password is set). ``is_admin`` is kept
    for compatibility but is *derived* from the permission system: a user is
    an admin exactly when their effective permissions include the ``admin``
    super-permission (directly, via a role, or via a group).

    ``roles`` are the role names assigned directly to this account and
    ``permissions`` are permissions granted directly (bypassing roles).
    ``groups`` is the list of group names the user belongs to (multi-group
    membership; replaces the single ``settings.group`` for permission and
    settings inheritance).
    """

    __tablename__ = "users"
    username: Mapped[str] = mapped_column(String(255), primary_key=True)
    is_admin: Mapped[bool] = mapped_column(Boolean, default=False)
    is_sso: Mapped[bool] = mapped_column(Boolean, default=False)
    settings: Mapped[Optional[dict]] = mapped_column(JSON, nullable=True)
    password_hash: Mapped[Optional[str]] = mapped_column(Text, nullable=True)
    # The full list of provider (e.g. authentik) groups the user belongs to,
    # captured from the OIDC id_token on each SSO login. Unlike `settings.group`
    # (a single derived primary group), this preserves every group so the admin
    # UI can show which SSO groups a user is in. NULL for non-SSO accounts.
    sso_groups: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)
    # Role names assigned directly to this account (see the roles table).
    roles: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)
    # Permissions granted directly to this account (in addition to roles).
    permissions: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)
    # The group names this user belongs to (multi-membership).
    groups: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)


class Group(Base):
    """A group: a named set of members that share permissions, roles, and
    default settings. Groups can grant permissions and roles to all of their
    members (inherited both ways: users can also carry their own)."""

    __tablename__ = "groups"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    settings: Mapped[dict] = mapped_column(JSON, default=dict)
    # Role names granted to every member of this group.
    roles: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)
    # Permissions granted to every member of this group.
    permissions: Mapped[Optional[list]] = mapped_column(JSON, nullable=True)


class Role(Base):
    """A named bundle of permissions that can be assigned to users and groups.

    ``permissions`` is the list of permission names (from the catalog in
    ``app.permissions``) this role grants. ``is_builtin`` marks the roles
    seeded on first run (admin / operator / user); they are fully editable
    but flagged so the UI can show they ship with the application.
    """

    __tablename__ = "roles"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    description: Mapped[str] = mapped_column(Text, default="")
    permissions: Mapped[list] = mapped_column(JSON, default=list)
    is_builtin: Mapped[bool] = mapped_column(Boolean, default=False)


class VolumeMount(Base):
    __tablename__ = "volume_mounts"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    host_path: Mapped[str] = mapped_column(Text, default="")
    container_path: Mapped[str] = mapped_column(Text, default="")
    read_only: Mapped[bool] = mapped_column(Boolean, default=False)
    scope: Mapped[str] = mapped_column(String(50), default="none")
    target: Mapped[str] = mapped_column(String(255), default="")


class AppStore(Base):
    """A configured app store (name + catalog URL).

    Only the *list* of stores is stored here. The actual catalog data (the
    list of available apps fetched from each store's URL) remains external and
    is cached under ``app_store_cache_path``.
    """

    __tablename__ = "app_stores"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    url: Mapped[str] = mapped_column(Text)


class InstalledApp(Base):
    __tablename__ = "installed_apps"
    id: Mapped[str] = mapped_column(String(255), primary_key=True)
    data: Mapped[dict] = mapped_column(JSON)


class SessionRecord(Base):
    """A persisted (re)startable session. The full session dict is stored as JSON."""

    __tablename__ = "sessions"
    session_id: Mapped[str] = mapped_column(String(255), primary_key=True)
    data: Mapped[dict] = mapped_column(JSON)


class AuthToken(Base):
    """A server-issued web-login token (opaque string -> username + expiry)."""

    __tablename__ = "auth_tokens"
    token: Mapped[str] = mapped_column(String(255), primary_key=True)
    username: Mapped[str] = mapped_column(String(255))
    expires_at: Mapped[float] = mapped_column(Float)


class PublicShare(Base):
    """Metadata for a publicly shared file. The file itself stays on disk."""

    __tablename__ = "public_shares"
    share_id: Mapped[str] = mapped_column(String(255), primary_key=True)
    data: Mapped[dict] = mapped_column(JSON)


class AppTemplate(Base):
    """A user-defined application template.

    Built-in (default) templates ship as files inside the application image and
    are *not* stored here; only user-created/edited templates live in this
    table (and may override a default by name).
    """

    __tablename__ = "app_templates"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    settings: Mapped[dict] = mapped_column(JSON, default=dict)


class TemplateSchemaSetting(Base):
    """One row of the application-template schema (the "base layer").

    The schema (the env vars / docker run-options the template editor
    exposes) is persisted here so admins can edit it at runtime. On first
    run the table is seeded from the bundled template_schema.yml; afterwards
    the database is the source of truth and the YAML is the image baseline.

    Each setting has two values:

    * ``default`` — the value the container image ships with (the baseline).
      For built-in settings this always tracks the bundled YAML; the startup
      reconciliation resets it if it drifts. It is reference-only in the UI.
    * ``current`` — the admin's base-layer override. ``NULL`` means "no
      override: let the image default apply". At launch a setting is pushed
      through to the container only when ``current`` is set and differs from
      ``default``; saving normalises ``current == default`` back to ``NULL``.
    """

    __tablename__ = "template_schema_settings"
    name: Mapped[str] = mapped_column(String(255), primary_key=True)
    sort_order: Mapped[int] = mapped_column(Integer, default=0)
    label: Mapped[str] = mapped_column(Text, default="")
    description: Mapped[str] = mapped_column(Text, default="")
    category: Mapped[str] = mapped_column(String(100), default="general")
    type: Mapped[str] = mapped_column(String(50), default="text")
    default: Mapped[str] = mapped_column(Text, default="")
    current: Mapped[Optional[str]] = mapped_column(Text, nullable=True)
    docker: Mapped[bool] = mapped_column(Boolean, default=False)
    options: Mapped[list] = mapped_column(JSON, default=list)


class PinnedBehavior(Base):
    """A user's saved launch-options preset ("pinned behaviour").

    The full preset payload is stored as JSON in ``data``; ``username`` and
    ``created_at`` are real columns so rows can be filtered and ordered without
    touching the JSON.
    """

    __tablename__ = "pinned_behaviors"
    id: Mapped[str] = mapped_column(String(255), primary_key=True)
    username: Mapped[str] = mapped_column(String(255))
    created_at: Mapped[float] = mapped_column(Float, default=0.0)
    data: Mapped[dict] = mapped_column(JSON, default=dict)


class AppSetting(Base):
    """Generic key/value store for application settings.

    On startup the server compares the live ``VRECKAN_*`` environment against
    this table and updates any rows that have drifted, so the database is
    always a faithful shadow of the environment the container was started
    with. That makes it a recovery source: if the ``.env`` file is ever lost,
    the configured values (including the OIDC client secret) can be read back
    from here.
    """

    __tablename__ = "app_settings"
    key: Mapped[str] = mapped_column(String(255), primary_key=True)
    value: Mapped[str] = mapped_column(Text, default="")


# Tables in a sensible restore order. No foreign-key constraints are declared
# (cross-references are handled in application code, as before), so the order
# is only for readability of the logical backup.
TABLE_ORDER = [
    "users",
    "groups",
    "roles",
    "volume_mounts",
    "app_stores",
    "installed_apps",
    "app_templates",
    "template_schema_settings",
    "pinned_behaviors",
    "app_settings",
    "public_shares",
    "sessions",
    "auth_tokens",
]
TABLE_MODELS = {
    "users": User,
    "groups": Group,
    "roles": Role,
    "volume_mounts": VolumeMount,
    "app_stores": AppStore,
    "installed_apps": InstalledApp,
    "app_templates": AppTemplate,
    "template_schema_settings": TemplateSchemaSetting,
    "pinned_behaviors": PinnedBehavior,
    "app_settings": AppSetting,
    "public_shares": PublicShare,
    "sessions": SessionRecord,
    "auth_tokens": AuthToken,
}


def _database_url() -> str:
    url = os.environ.get("VRECKAN_DATABASE_URL")
    if url:
        return url
    return settings.database_url


engine = create_async_engine(_database_url(), echo=False, poolclass=NullPool)
async_session_factory = async_sessionmaker(engine, expire_on_commit=False)


async def init_db() -> None:
    """Create any missing tables (a no-op for tables that already exist)."""
    async with engine.begin() as conn:
        await conn.run_sync(Base.metadata.create_all)
    logger.info("Configuration database ready (%s)", _database_url())


async def migrate_db() -> None:
    """Apply lightweight, idempotent column migrations to existing databases.

    ``Base.metadata.create_all`` (see :func:`init_db`) only creates tables that
    are missing; it never adds columns to tables that already exist. This
    function closes that gap for columns added after the initial schema
    shipped. Each step is guarded by a column-existence check, so it is safe to
    run on every startup and is a no-op once the column is present.
    """
    async with engine.begin() as conn:
        def _migrate(sync_conn):
            inspector = inspect(sync_conn)
            if not inspector.has_table("users"):
                return
            cols = {c["name"] for c in inspector.get_columns("users")}
            if "is_sso" not in cols:
                if engine.dialect.name == "postgresql":
                    alter = "ALTER TABLE users ADD COLUMN is_sso BOOLEAN NOT NULL DEFAULT FALSE"
                else:
                    alter = "ALTER TABLE users ADD COLUMN is_sso BOOLEAN NOT NULL DEFAULT 0"
                sync_conn.execute(text(alter))
                logger.info("Migration: added users.is_sso column")
            if "sso_groups" not in cols:
                # JSON column (NULL for non-SSO accounts). SQLite has no JSON
                # type, so it is stored as TEXT; PostgreSQL uses JSONB.
                if engine.dialect.name == "postgresql":
                    alter = "ALTER TABLE users ADD COLUMN sso_groups JSONB"
                else:
                    alter = "ALTER TABLE users ADD COLUMN sso_groups TEXT"
                sync_conn.execute(text(alter))
                logger.info("Migration: added users.sso_groups column")
            # RBAC: roles/permissions assigned directly to the account, and
            # the (multi-member) group list. JSON columns, NULL-able.
            for col in ("roles", "permissions", "groups"):
                if col not in cols:
                    if engine.dialect.name == "postgresql":
                        alter = f"ALTER TABLE users ADD COLUMN {col} JSONB"
                    else:
                        alter = f"ALTER TABLE users ADD COLUMN {col} TEXT"
                    sync_conn.execute(text(alter))
                    logger.info(f"Migration: added users.{col} column")
            if inspector.has_table("groups"):
                gcols = {c["name"] for c in inspector.get_columns("groups")}
                for col in ("roles", "permissions"):
                    if col not in gcols:
                        if engine.dialect.name == "postgresql":
                            alter = f"ALTER TABLE groups ADD COLUMN {col} JSONB"
                        else:
                            alter = f"ALTER TABLE groups ADD COLUMN {col} TEXT"
                        sync_conn.execute(text(alter))
                        logger.info(f"Migration: added groups.{col} column")
            if inspector.has_table("template_schema_settings"):
                tss_cols = {
                    c["name"]
                    for c in inspector.get_columns("template_schema_settings")
                }
                if "current" not in tss_cols:
                    # The base-layer override column added with the
                    # default/current split. NULL = "use the image default".
                    sync_conn.execute(
                        text(
                            "ALTER TABLE template_schema_settings "
                            "ADD COLUMN current TEXT"
                        )
                    )
                    logger.info(
                        "Migration: added template_schema_settings.current column"
                    )
        await conn.run_sync(_migrate)


async def db_dump(dest_path: str) -> None:
    """Write a logical, backend-agnostic snapshot of every table to a JSON file.

    All rows are read inside a single transaction so the snapshot is internally
    consistent. The format is::

        {"version": 1, "tables": {"users": [ {...}, ... ], ...}}

    which lets the same backup be restored onto SQLite or PostgreSQL.
    """
    data = {"version": 1, "tables": {}}
    async with async_session_factory() as session:
        async with session.begin():
            for table in TABLE_ORDER:
                result = await session.execute(text(f"SELECT * FROM {table}"))
                data["tables"][table] = [dict(row) for row in result.mappings()]
    with open(dest_path, "w", encoding="utf-8") as f:
        json.dump(data, f)
    logger.info("Wrote configuration database snapshot to %s", dest_path)


async def db_restore(source_path: str) -> None:
    """Replace the contents of every table with the rows in a :func:`db_dump` file.

    Runs inside a single transaction so concurrent readers observe either the
    old or the new state, never a partially-restored one. Unknown columns in the
    dump (e.g. from a different schema version) are ignored.
    """
    with open(source_path, "r", encoding="utf-8") as f:
        data = json.load(f)
    tables = data.get("tables", {})
    async with async_session_factory() as session:
        async with session.begin():
            for table in TABLE_ORDER:
                model = TABLE_MODELS[table]
                cols = {c.name: c for c in model.__table__.columns}
                await session.execute(text(f"DELETE FROM {table}"))
                for row in tables.get(table, []):
                    kwargs = {}
                    for k, v in row.items():
                        if k not in cols:
                            continue
                        if isinstance(cols[k].type, JSON) and isinstance(v, str):
                            # The dump stores the raw serialized form of JSON
                            # columns; decode it so the JSON type re-serializes
                            # it identically instead of double-encoding the
                            # already-serialized string.
                            try:
                                v = json.loads(v)
                            except ValueError:
                                pass
                        kwargs[k] = v
                    session.add(model(**kwargs))
    logger.info("Restored configuration database from %s", source_path)


async def get_app_setting(key: str) -> Optional[str]:
    """Read a single value from the app_settings key/value store (None if the
    key is absent). Used for settings that are edited at runtime (e.g. the OIDC
    admin-groups list) rather than sourced from the environment."""
    async with async_session_factory() as session:
        row = (
            await session.execute(select(AppSetting).where(AppSetting.key == key))
        ).scalar_one_or_none()
        return row.value if row else None


async def set_app_setting(key: str, value: str) -> None:
    """Insert or update a single app_settings row."""
    async with async_session_factory() as session:
        async with session.begin():
            row = (
                await session.execute(select(AppSetting).where(AppSetting.key == key))
            ).scalar_one_or_none()
            if row is None:
                session.add(AppSetting(key=key, value=value))
            else:
                row.value = value
