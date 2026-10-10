from pydantic import BaseModel, Field
from typing import List, Optional
import uuid


class GPUInfo(BaseModel):
    device: str
    driver: str

class Application(BaseModel):
    id: str
    name: str
    logo: str
    home_directories: bool
    nvidia_support: bool
    dri3_support: bool
    url_support: bool
    extensions: List[str]
    is_meta_app: bool = False

class LaunchRequestSimple(BaseModel):
    application_id: str
    home_name: Optional[str] = None
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    launch_in_room_mode: bool = False
    wayland_mode: bool = True
    session_name: Optional[str] = None

class LaunchRequestURL(BaseModel):
    url: str
    application_id: str
    home_name: Optional[str] = None
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    launch_in_room_mode: bool = False
    wayland_mode: bool = True
    session_name: Optional[str] = None

class LaunchRequestFile(BaseModel):
    application_id: str
    filename: str
    upload_id: str
    total_chunks: int
    open_file_on_launch: bool = True
    home_name: Optional[str] = None
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    launch_in_room_mode: bool = False
    wayland_mode: bool = True
    session_name: Optional[str] = None

class LaunchResponse(BaseModel):
    session_url: str
    session_id: str

class SessionStatusResponse(BaseModel):
    """Readiness probe result for a running session's upstream app."""
    ready: bool
    detail: Optional[str] = None

class HandshakeInitiateResponse(BaseModel):
    nonce: str
    signature: str

class HandshakeExchangeRequest(BaseModel):
    encrypted_session_key: str

class HandshakeExchangeResponse(BaseModel):
    session_id: str

class EncryptedPayload(BaseModel):
    iv: str
    ciphertext: str

class AppStore(BaseModel):
    name: str
    url: str

class AvailableAppProviderConfig(BaseModel):
    image: str
    port: int
    nvidia_support: bool
    dri3_support: bool
    type: str
    url_support: bool
    open_support: bool
    extensions: List[str]
    autostart: Optional[bool] = False
    custom_autostart_script_b64: Optional[str] = None
    custom_autostart_wayland_script_b64: Optional[str] = None
    docker_overrides: Optional[dict] = None

class AvailableApp(BaseModel):
    id: str
    name: str
    logo: str
    url: str
    provider: str
    provider_config: AvailableAppProviderConfig

class EnvVar(BaseModel):
    name: str
    value: str

class InstalledAppProviderConfig(AvailableAppProviderConfig):
    env: Optional[List[EnvVar]] = []
    # Optional name of the environment variable the app container reads the
    # launch URL from (e.g. "CHROME_CLI" for linuxserver/chrome,
    # "FIREFOX_CLI" for linuxserver/firefox). When set, the URL entered at
    # launch is also passed to the container under this name. Upstream
    # linuxserver images read <APP>_CLI (not VRECKAN_URL) for the initial
    # URL, so this is how an admin points the app at the requested URL.
    url_env_var: Optional[str] = None

class InstalledApp(BaseModel):
    id: str = Field(default_factory=lambda: str(uuid.uuid4()))
    name: str
    logo: str
    url: str
    source: str
    source_app_id: str
    provider: str
    home_directories: bool
    users: List[str]
    groups: List[str]
    provider_config: InstalledAppProviderConfig
    auto_update: bool = True
    app_template: str
    is_meta_app: bool = False
    base_app_id: Optional[str] = None
    home_template_name: Optional[str] = None

class InstalledAppWithStatus(InstalledApp):
    image_sha: Optional[str] = None
    last_checked_at: Optional[float] = None
    pull_status: Optional[str] = None
    # Aggregate download progress while the image pull is in flight:
    # {"status": str, "percentage": float|None, "current": int, "total": int}
    pull_progress: Optional[dict] = None

class ImageUpdateCheckResponse(BaseModel):
    current_sha: Optional[str]
    update_available: bool

class AppUpdateCheckResult(BaseModel):
    app_id: str
    name: str
    image: str
    current_sha: Optional[str] = None
    update_available: bool

class CheckAllUpdatesResponse(BaseModel):
    results: List[AppUpdateCheckResult]
    updates_available: int

class AppImagePullResult(BaseModel):
    app_id: str
    name: str
    image: str
    status: str
    new_sha: Optional[str] = None

class PullAllImagesResponse(BaseModel):
    results: List[AppImagePullResult]
    started: int

class ImagePullResponse(BaseModel):
    status: str
    new_sha: Optional[str]

class AppTemplate(BaseModel):
    name: str
    settings: dict

class UserSettings(BaseModel):
    active: bool = True
    group: str = "none"
    persistent_storage: bool = True
    public_sharing: bool = False
    harden_container: bool = False
    harden_openbox: bool = False
    gpu: bool = True
    storage_limit: int = -1
    session_limit: int = -1
    # The user's preferred UI language (an i18n locale code, e.g. "de").
    # Personal preference, not an admin-managed capability; set via the
    # sidebar language selector and reported back on /api/auth/me.
    ui_language: Optional[str] = None


class UiPreferencesRequest(BaseModel):
    """Body for PUT /api/auth/preferences — the caller's personal UI prefs."""
    ui_language: Optional[str] = None


class AdminStatusResponse(BaseModel):
    is_admin: bool
    username: str
    settings: UserSettings
    gpus: List[GPUInfo] = []
    global_default_gpu: Optional[str] = None
    cpu_model: Optional[str] = None
    disk_total: Optional[int] = None
    disk_used: Optional[int] = None
    # The user's effective (expanded) permission set, plus their direct
    # roles and group memberships. The frontend uses these to decide which
    # admin-panel sections (and which sidebar entries) to show.
    permissions: List[str] = []
    roles: List[str] = []
    groups: List[str] = []

class User(BaseModel):
    username: str
    # Derived: True when the user's effective permissions include the
    # "admin" super-permission (directly, via a role, or via a group).
    is_admin: bool
    settings: Optional[UserSettings] = None
    is_sso: bool = False
    has_password: bool = False
    # The provider (IdP) groups the user belongs to, as reported on their most
    # recent SSO login. Shown on the account card so admins can see which SSO
    # groups a user is in (and which of them grant admin). Empty for local
    # (non-SSO) accounts.
    sso_groups: List[str] = []
    # Roles assigned directly to this account.
    roles: List[str] = []
    # Permissions granted directly to this account (in addition to roles).
    permissions: List[str] = []
    # The groups this user belongs to (multi-membership).
    groups: List[str] = []

class Group(BaseModel):
    name: str
    settings: UserSettings
    # Roles granted to every member of this group.
    roles: List[str] = []
    # Permissions granted to every member of this group.
    permissions: List[str] = []

class Role(BaseModel):
    """A named bundle of permissions, assignable to users and groups."""

    name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")
    description: str = ""
    permissions: List[str] = []
    is_builtin: bool = False

class CreateRoleRequest(BaseModel):
    name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")
    description: str = ""
    permissions: List[str] = []

class UpdateRoleRequest(BaseModel):
    description: Optional[str] = None
    permissions: Optional[List[str]] = None

class ManagementDataResponse(BaseModel):
    admins: List[User]
    users: List[User]
    groups: List[Group]
    roles: List[Role] = []
    volume_mounts: List["VolumeMount"] = []
    api_port: int
    session_port: int
    gpus: List[GPUInfo] = []

class CreatePersonRequest(BaseModel):
    """Unified create request for the /api/admin/people roster endpoint.

    ``is_admin`` decides whether the new account is an admin or a regular
    user. ``settings`` are optional; the server fills in any missing keys
    with defaults and forces ``active`` to True on creation.
    """
    username: str
    settings: Optional[UserSettings] = None
    is_admin: bool = False

class CreateUserResponse(BaseModel):
    user: User

class UpdateUserRequest(BaseModel):
    settings: UserSettings

class SetAdminStatusRequest(BaseModel):
    """Change an existing account's admin status (promote/demote).

    The roster is unified, so an admin can flip any account between user and
    admin without deleting and recreating it. ``is_admin`` is the target
    state; the server is idempotent (a no-op if already in that state) and
    protects the bootstrap 'admin' account from demotion.

    Under the permission model, promoting adds the ``admin`` role to the
    account's direct roles (demoting removes it), so the derived ``is_admin``
    flag follows the permission system.
    """
    is_admin: bool

class SetUserAccessRequest(BaseModel):
    """Set an account's access: its direct roles, direct permissions, and the
    groups it belongs to. All three are optional; an absent key leaves that
    aspect unchanged. This is the fine-grained control surface for the
    Accounts page (the coarse promote/demote is ``SetAdminStatusRequest``).
    """
    roles: Optional[List[str]] = None
    permissions: Optional[List[str]] = None
    groups: Optional[List[str]] = None

class CreateGroupRequest(BaseModel):
    name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")
    settings: UserSettings
    roles: List[str] = []
    permissions: List[str] = []

class UpdateGroupRequest(BaseModel):
    settings: Optional[UserSettings] = None
    roles: Optional[List[str]] = None
    permissions: Optional[List[str]] = None


class VolumeMount(BaseModel):
    """An admin-defined external volume mount, bound into app containers for a
    user (scope=user) or every member of a group (scope=group)."""

    name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")
    host_path: str
    container_path: str
    read_only: bool = False
    scope: str = "none"  # "none" | "user" | "group"
    target: str = ""  # username (scope=user) or group name (scope=group)


class CreateVolumeMountRequest(BaseModel):
    name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")
    host_path: str
    container_path: str
    read_only: bool = False
    scope: str = "none"
    target: str = ""


class UpdateVolumeMountRequest(BaseModel):
    host_path: str
    container_path: str
    read_only: bool = False
    scope: str = "none"
    target: str = ""


# --- Backup / restore ------------------------------------------------------
class CreateBackupRequest(BaseModel):
    include_user_data: bool = False


class BackupInfo(BaseModel):
    name: str
    size: int
    created_at: str
    include_user_data: bool


class RestoreResult(BaseModel):
    source: str
    restored: bool


class BackupRestoreInitiateResponse(BaseModel):
    upload_id: str


class BackupRestoreChunkRequest(BaseModel):
    upload_id: str
    chunk_index: int
    chunk_data_b64: str


class BackupRestoreFinalizeRequest(BaseModel):
    upload_id: str
    total_chunks: int


class CreateMetaAppRequest(BaseModel):
    name: str
    base_app_id: str
    logo: str
    custom_autostart_script_b64: Optional[str] = None
    custom_autostart_wayland_script_b64: Optional[str] = None
    users: List[str]
    groups: List[str]

class LaunchMetaCustomizeRequest(BaseModel):
    application_id: str
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    wayland_mode: bool = True

class RevokeOidcSessionRequest(BaseModel):
    username: str

class RevokeOidcSessionResponse(BaseModel):
    username: str
    revoked: int

class RevokeOwnSessionsRequest(BaseModel):
    # Optional. If supplied it MUST match the authenticated caller; otherwise
    # the request is rejected. The target is always derived from the caller's
    # own credential, never from untrusted input.
    username: Optional[str] = None

class HomeDirectoryList(BaseModel):
    home_dirs: List[str]

class HomeDirectoryCreate(BaseModel):
    home_name: str = Field(..., pattern=r"^[a-zA-Z0-9_-]+$")

class PinnedBehavior(BaseModel):
    """A saved set of launch options ("pinned behaviour") the user can re-apply.

    Mirrors the browser extension's "Save these launch options": it remembers
    which app and which launch options to use, optionally tied to a trigger
    (all URLs, or a file extension). In the web app the trigger is informational
    (there is no browser context menu to auto-fire on); the preset is applied
    manually from the launcher.
    """
    id: str
    name: str
    application_id: str
    home_name: Optional[str] = None
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    launch_in_room_mode: bool = False
    wayland_mode: bool = True
    trigger_type: str = "manual"  # "manual" | "all_urls" | "file_extension"
    trigger_value: str = ""
    created_at: float = 0.0
    is_default: bool = False


class CreatePinnedBehaviorRequest(BaseModel):
    name: str = Field(..., min_length=1, max_length=128)
    application_id: str
    home_name: Optional[str] = None
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    launch_in_room_mode: bool = False
    wayland_mode: bool = True
    trigger_type: str = "manual"
    trigger_value: str = ""
    is_default: bool = False


class ActiveSessionInfo(BaseModel):
    session_id: str
    app_id: str
    app_name: str
    app_logo: str
    created_at: float
    session_url: str
    launch_context: Optional[dict] = None
    is_collaboration: bool = False
    name: Optional[str] = None
    # True when the session's container was launched from an older image than
    # the one currently pulled locally (a newer image is available); the user
    # can "Recreate" the session to pick it up.
    out_of_date: bool = False

class SessionRecreateResponse(BaseModel):
    session_id: str
    session_url: str

class SendFileToSessionRequest(BaseModel):
    filename: str
    upload_id: str
    total_chunks: int

class UserSessionList(BaseModel):
    username: str
    sessions: List[ActiveSessionInfo]

class UploadInitiateRequest(BaseModel):
    filename: str
    total_size: int


class UploadInitiateResponse(BaseModel):
    upload_id: str

class UploadChunkRequest(BaseModel):
    upload_id: str
    chunk_index: int
    chunk_data_b64: str

class UploadToStorageRequest(BaseModel):
    filename: str
    upload_id: str
    total_chunks: int
    home_name: str

class FileListItem(BaseModel):
    name: str
    path: str
    is_dir: bool
    size: int
    mtime: float

class FileListResponse(BaseModel):
    items: List[FileListItem]
    path: str
    page: int
    per_page: int
    total: int

class CreateFolderRequest(BaseModel):
    path: str
    folder_name: str = Field(..., pattern=r"^[^/\\]+$")

class DeleteItemsRequest(BaseModel):
    paths: List[str]

class DeleteTaskResponse(BaseModel):
    message: str
    task_id: str

class DeleteStatusResponse(BaseModel):
    status: str
    message: Optional[str] = None

class FinalizeUploadToDirRequest(BaseModel):
    path: str
    filename: str
    upload_id: str
    total_chunks: int

class FileChunkResponse(BaseModel):
    chunk_data_b64: str
    is_last_chunk: bool

class GenericSuccessMessage(BaseModel):
    message: str

class ShareFileRequest(BaseModel):
    home_dir: str
    path: str
    password: Optional[str] = None
    expiry_hours: Optional[int] = None

class PublicShareInfo(BaseModel):
    share_id: str
    original_filename: str
    size_bytes: int
    created_at: float
    expiry_timestamp: Optional[float] = None
    has_password: bool
    url: str

class PublicShareMetadata(BaseModel):
    owner_username: str
    original_filename: str
    created_at: float
    size_bytes: int
    password_hash: Optional[str] = None
    expiry_timestamp: Optional[float] = None

class LoginRequest(BaseModel):
    username: str = Field(..., min_length=1, max_length=128)
    password: str = Field(..., min_length=1)

class ChangePasswordRequest(BaseModel):
    old_password: str
    new_password: str = Field(..., min_length=8)

class SetPasswordRequest(BaseModel):
    password: str = Field(..., min_length=8)

class MeResponse(BaseModel):
    username: str
    is_admin: bool
    active: bool = True
    has_password: bool = False
    is_sso: bool = False
    settings: Optional[UserSettings] = None
    # The user's effective (expanded) permission set, so the client can gate
    # UI (e.g. which admin sections to show) without guessing.
    permissions: List[str] = []
    roles: List[str] = []
    groups: List[str] = []

class LaunchRequestFilePath(BaseModel):
    application_id: str
    home_name: Optional[str] = None
    filename: str
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    wayland_mode: bool = True
    session_name: Optional[str] = None

class LaunchFromStorageRequest(BaseModel):
    """Open a file that already lives in one of the user's home directories in
    an application. The file is copied into the session's shared-files area
    (mounted at Desktop/files in the container) and, optionally, opened."""
    home_dir: str
    path: str
    application_id: str
    open_file_on_launch: bool = True
    language: Optional[str] = None
    selected_gpu: Optional[str] = None
    wayland_mode: bool = True
