import os
import logging

# The OIDC/SSO settings are the ones editable from the admin UI's SSO page.
# They are stored in the ``app_settings`` table (key = the VRECKAN_ env name)
# and override the environment when present. See ``get_setting`` / the
# ``/api/admin/sso`` endpoints.
OIDC_SETTING_NAMES = [
    "oidc_enabled",
    "oidc_issuer",
    "oidc_client_id",
    "oidc_client_secret",
    "oidc_redirect_uri",
    "oidc_scopes",
    "oidc_allow_signup",
    "oidc_state_ttl_seconds",
    "oidc_admin_groups",
    "oidc_user_groups",
]

# The Let's Encrypt / certbot settings are editable from the admin UI's
# Certificates page. Like the OIDC settings, they are stored in the
# ``app_settings`` table (key = the VRECKAN_ env name) and override the
# environment when present. See ``get_setting`` / the ``/api/admin/certificates``
# endpoints.
LE_SETTING_NAMES = [
    "le_enabled",
    "le_domains",
    "le_auth",
    "le_cloudflare_email",
    "le_cloudflare_token",
    "le_staging",
    "le_auto_renew",
    "le_renew_check_hours",
    "le_renew_before_days",
]

SETTING_DEFINITIONS = [
    {
        "name": "log_level",
        "type": "str",
        "default": "INFO",
        "help": "Logging level (e.g., DEBUG, INFO, WARNING).",
    },
    {
        "name": "api_port",
        "type": "int",
        "default": 8000,
        "help": "Port for the main API server.",
    },
    {
        "name": "session_port",
        "type": "int",
        "default": 8443,
        "help": "Port for the session proxy server.",
    },
    {
        "name": "database_url",
        "type": "str",
        "default": "sqlite+aiosqlite:////data/vreckan.db",
        "help": "SQLAlchemy URL of the configuration database (VRECKAN_DATABASE_URL).",
    },
    {
        "name": "app_resource_path",
        "type": "str",
        "default": "https://raw.githubusercontent.com/linuxserver/sealskin-apps/refs/heads/master/apps.yml",
        "help": "URL for the YAML file defining default available applications.",
    },
    {
        "name": "default_app_templates_path",
        "type": "str",
        "default": "server/default_templates",
        "help": "Path to the directory for default application templates.",
    },
    {
        "name": "upload_dir",
        "type": "str",
        "default": "/data/storage/vreckan_uploads",
        "help": "Directory for temporary file uploads.",
    },
    {
        "name": "session_cookie_name",
        "type": "str",
        "default": "vreckan_session_token",
        "help": "Name of the session cookie.",
    },
    {
        "name": "auth_cookie_name",
        "type": "str",
        "default": "vreckan_auth",
        "help": "Name of the web-login authentication cookie.",
    },
    {
        "name": "auth_token_ttl_seconds",
        "type": "int",
        "default": 28800,
        "help": "Lifetime (in seconds) of a server-issued web authentication token/cookie.",
    },
    {
        "name": "autostart_cache_path",
        "type": "str",
        "default": "/data/autostart_cache",
        "help": "Path to cache autostart scripts.",
    },
    {
        "name": "app_store_cache_path",
        "type": "str",
        "default": "/data/app_stores_cache",
        "help": "Path to cache app store YAML files.",
    },
    {
        "name": "auto_update_apps",
        "type": "bool",
        "default": True,
        "help": "Enable automatic pulling of the latest app images in the background.",
    },
    {
        "name": "auto_update_interval_seconds",
        "type": "int",
        "default": 3600,
        "help": "How often to check for app image updates (in seconds).",
    },
    {
        "name": "puid",
        "type": "int",
        "default": 1000,
        "help": "Default User ID to run containers as.",
    },
    {
        "name": "pgid",
        "type": "int",
        "default": 1000,
        "help": "Default Group ID to run containers as.",
    },
    {
        "name": "data_root",
        "type": "str",
        "default": "/data",
        "help": "Root of the persistent data tree (database, storage, caches, backups).",
    },
    {
        "name": "backups_path",
        "type": "str",
        "default": "/data/backups",
        "help": "Directory for backup archives.",
    },
    {
        "name": "backup_retention",
        "type": "int",
        "default": 0,
        "help": "Number of backup archives to keep; 0 keeps all.",
    },
    {
        "name": "storage_path",
        "type": "str",
        "default": "/data/storage",
        "help": "Base directory for user home directories.",
    },
    {
        "name": "app_icons_path",
        "type": "str",
        "default": "/data/storage/vreckan_app_icons",
        "help": "Directory for storing custom-uploaded application icons.",
    },
    {
        "name": "home_templates_path",
        "type": "str",
        "default": "/data/storage/vreckan_home_templates",
        "help": "Base directory for meta-app home directory templates.",
    },
    {
        "name": "container_config_path",
        "type": "str",
        "default": "/config",
        "help": "Mount point for home directories inside the container.",
    },
    {
        "name": "server_private_key_path",
        "type": "str",
        "default": "/config/ssl/server_key.pem",
        "help": "Path to the server private key PEM file.",
    },
    {
        "name": "public_storage_path",
        "type": "str",
        "default": "/data/storage/vreckan_public",
        "help": "Directory for storing publicly shared files.",
    },
    {
        "name": "share_cleanup_interval_seconds",
        "type": "int",
        "default": 600,
        "help": "How often to run the cleanup job for expired shares (in seconds).",
    },
    # --- OIDC / SSO settings ---
    {
        "name": "oidc_enabled",
        "type": "bool",
        "default": False,
        "help": "Enable OIDC/SSO login (requires an issuer URL and client_id).",
    },
    {
        "name": "oidc_issuer",
        "type": "str",
        "default": "",
        "help": "OIDC issuer base URL (e.g. https://auth.example.com/application/o/vreckan/). Discovery is fetched from {issuer}/.well-known/openid-configuration.",
    },
    {
        "name": "oidc_client_id",
        "type": "str",
        "default": "",
        "help": "OIDC client_id for the authorization-code flow.",
    },
    {
        "name": "oidc_client_secret",
        "type": "str",
        "default": "",
        "help": "OIDC client_secret (optional for public clients).",
    },
    {
        "name": "oidc_redirect_uri",
        "type": "str",
        "default": "",
        "help": "Registered redirect_uri. Defaults to {base}/api/auth/oidc/callback if empty.",
    },
    {
        "name": "oidc_scopes",
        "type": "str",
        "default": "openid email profile",
        "help": "Space-separated OIDC scopes requested from the provider.",
    },
    {
        "name": "oidc_allow_signup",
        "type": "bool",
        "default": True,
        "help": "Auto-provision a Vreckan user when an OIDC identity does not exist yet.",
    },
    {
        "name": "oidc_state_ttl_seconds",
        "type": "int",
        "default": 600,
        "help": "Lifetime of a single OIDC authorize state token (seconds).",
    },
    {
        "name": "oidc_admin_groups",
        "type": "str",
        "default": "",
        "help": "Comma/space-separated group names; OIDC users in any of these groups are auto-promoted to admin.",
    },
    {
        "name": "oidc_user_groups",
        "type": "str",
        "default": "",
        "help": "Comma/space-separated group names that map to regular (non-admin) Vreckan groups; an SSO user not in an admin group is assigned to their first group in this list.",
    },
    {
        "name": "oidc_require_signature",
        "type": "bool",
        "default": True,
        "help": "Require a verified id_token signature for OIDC login. When true (default), login fails closed if the provider's JWKS is unreachable; when false, a claims-only (unsigned) fallback is used for providers that omit a JWKS.",
    },
    # --- Bootstrap admin -----------------------------------------------------
    {
        "name": "bootstrap_admin_username",
        "type": "str",
        "default": "admin",
        "help": "Username of the admin account created on first run (when no admin exists yet).",
    },
    {
        "name": "bootstrap_admin_password",
        "type": "str",
        "default": "admin1234",
        "help": "Initial password for the bootstrap admin account; change it after first login.",
    },
    # --- Let's Encrypt (certbot) ---------------------------------------------
    {
        "name": "le_enabled",
        "type": "bool",
        "default": False,
        "help": "Issue Let's Encrypt certificates via certbot. When off, the server uses a self-signed certificate.",
    },
    {
        "name": "le_domains",
        "type": "str",
        "default": "",
        "help": "Comma-separated domain(s) to issue certificates for (e.g. example.com,www.example.com).",
    },
    {
        "name": "le_auth",
        "type": "str",
        "default": "dns-cloudflare",
        "help": "ACME challenge method: 'dns-cloudflare' (Cloudflare DNS-01) or 'http-01'.",
    },
    {
        "name": "le_cloudflare_email",
        "type": "str",
        "default": "",
        "help": "Account email for Let's Encrypt (used with Cloudflare DNS auth).",
    },
    {
        "name": "le_cloudflare_token",
        "type": "str",
        "default": "",
        "help": "Cloudflare API token with Zone.DNS edit permission for the domain's zone.",
    },
    {
        "name": "le_staging",
        "type": "bool",
        "default": False,
        "help": "Use the Let's Encrypt staging server (test certificates, no production rate limits).",
    },
    {
        "name": "le_auto_renew",
        "type": "bool",
        "default": True,
        "help": "Automatically renew Let's Encrypt certificates in the background.",
    },
    {
        "name": "le_renew_check_hours",
        "type": "int",
        "default": 6,
        "help": "How often to check for certificate renewal (in hours).",
    },
    {
        "name": "le_renew_before_days",
        "type": "int",
        "default": 30,
        "help": "Renew the certificate when it expires within this many days.",
    },
]


class AppSettings:
    """
    Parses and stores application settings from environment variables,
    with fallback to default values.
    """

    def __init__(self):
        self._process_and_set_attributes()

    def _process_and_set_attributes(self):
        """Process definitions and set them as class attributes."""
        for setting in SETTING_DEFINITIONS:
            name = setting["name"]
            stype = setting["type"]
            env_var_name = f"VRECKAN_{name.upper()}"

            default_val = setting.get("default")
            raw_value = os.environ.get(env_var_name)

            if raw_value is None:
                processed_value = default_val
            else:
                try:
                    if stype == "bool":
                        processed_value = str(raw_value).lower() in ["true", "1", "yes"]
                    elif stype == "int":
                        processed_value = int(raw_value)
                    else:
                        processed_value = str(raw_value)
                except (ValueError, TypeError) as e:
                    logging.error(
                        f"Could not parse setting '{name}' with value '{raw_value}'. Using default. Error: {e}"
                    )
                    processed_value = default_val

            setattr(self, name, processed_value)

    async def get_setting(self, name: str):
        """Return the *effective* value of a setting.

        Precedence: a value stored in the ``app_settings`` table (edited on
        the admin SSO page) wins over the ``VRECKAN_*`` environment variable,
        which wins over the code default. This is what lets an admin configure
        SSO from the UI without touching the container environment.

        The instance attribute (populated from the environment at import) is
        consulted before the raw environment so tests can monkeypatch it to
        force a value; in production the attribute always holds the env value
        (or the default), so a stored DB value still takes precedence. The
        value is parsed according to the setting's declared type (bool/int/str).
        """
        from server import db

        definition = next((s for s in SETTING_DEFINITIONS if s["name"] == name), None)
        stype = definition["type"] if definition else "str"
        default_val = definition["default"] if definition else None
        env_var_name = f"VRECKAN_{name.upper()}"

        db_value = await db.get_app_setting(env_var_name)
        if db_value is not None:
            raw = db_value
        else:
            raw = getattr(self, name, None)
            if raw is None:
                return default_val

        try:
            if stype == "bool":
                return str(raw).lower() in ["true", "1", "yes"]
            if stype == "int":
                return int(raw)
            return str(raw)
        except (ValueError, TypeError):
            return default_val


settings = AppSettings()


async def setting_source(name: str) -> str:
    """Report where a setting's current value comes from: ``"db"`` when a
    value is stored in the ``app_settings`` table (edited in the UI),
    ``"env"`` when it comes from the environment, or ``"default"`` when neither
    is set. Used by the SSO page to show a per-field source badge."""
    from server import db

    env_var_name = f"VRECKAN_{name.upper()}"
    if await db.get_app_setting(env_var_name) is not None:
        return "db"
    if getattr(settings, name, None) is not None:
        return "env"
    return "default"
