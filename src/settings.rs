//! Copyright (c) 2026 Antitux Networks LLC. SPDX-License-Identifier: MPL-2.0
//! Application settings.
//!
//! Ports `server/settings`. Every setting is sourced from a `VRECKAN_*`
//! environment variable with a code default. The *effective* value of a
//! setting can also be overridden at runtime by a row in the `app_settings`
//! table (edited on the admin SSO / Certificates pages); that DB-over-env-
//! over-default precedence is applied by the DB layer (see `db.rs`) and the
//! admin endpoints, mirroring `AppSettings.get_setting`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A single setting definition, mirroring one entry of `SETTING_DEFINITIONS`.
#[derive(Clone, Debug)]
pub struct SettingDef {
    pub name: &'static str,
    pub type_: &'static str, // "str" | "int" | "bool"
    pub default: &'static str,
    pub help: &'static str,
}

/// The full, ordered list of setting definitions. This is the source of truth
/// for the admin Settings page and for the env/DB precedence logic.
pub const SETTING_DEFINITIONS: &[SettingDef] = &[
    SettingDef { name: "log_level", type_: "str", default: "INFO", help: "Logging level (e.g., DEBUG, INFO, WARNING)." },
    SettingDef { name: "api_port", type_: "int", default: "8000", help: "Port for the main API server." },
    SettingDef { name: "session_port", type_: "int", default: "8443", help: "Port for the session proxy server." },
    SettingDef { name: "database_url", type_: "str", default: "", help: "Full database URL override (sqlx format, e.g. postgres://user:pass@host:5432/db). If empty, the URL is built from the db_* settings below." },
    SettingDef { name: "db_backend", type_: "str", default: "sqlite", help: "Database backend: 'sqlite', 'postgres', or 'mysql'." },
    SettingDef { name: "db_host", type_: "str", default: "localhost", help: "Database host (postgres/mysql)." },
    SettingDef { name: "db_port", type_: "int", default: "0", help: "Database port. 0 = backend default (5432 postgres, 3306 mysql)." },
    SettingDef { name: "db_user", type_: "str", default: "", help: "Database username (postgres/mysql)." },
    SettingDef { name: "db_pass", type_: "str", default: "", help: "Database password (postgres/mysql)." },
    SettingDef { name: "db_name", type_: "str", default: "vreckan", help: "Database name (postgres/mysql)." },
    SettingDef { name: "db_path", type_: "str", default: "", help: "SQLite database file path. Defaults to {data_root}/vreckan.db." },
    SettingDef { name: "app_resource_path", type_: "str", default: "https://raw.githubusercontent.com/linuxserver/sealskin-apps/refs/heads/master/apps.yml", help: "URL for the YAML file defining default available applications." },
    SettingDef { name: "default_app_templates_path", type_: "str", default: "server/default_templates", help: "Path to the directory for default application templates." },
    SettingDef { name: "template_schema_path", type_: "str", default: "server/template_schema.yml", help: "Path to the bundled template schema YAML (the image baseline for the template editor)." },
    SettingDef { name: "upload_dir", type_: "str", default: "/data/storage/vreckan_uploads", help: "Directory for temporary file uploads." },
    SettingDef { name: "session_cookie_name", type_: "str", default: "vreckan_session_token", help: "Name of the session cookie." },
    SettingDef { name: "auth_cookie_name", type_: "str", default: "vreckan_auth", help: "Name of the web-login authentication cookie." },
    SettingDef { name: "auth_token_ttl_seconds", type_: "int", default: "28800", help: "Lifetime (in seconds) of a server-issued web authentication token/cookie." },
    SettingDef { name: "autostart_cache_path", type_: "str", default: "/data/autostart_cache", help: "Path to cache autostart scripts." },
    SettingDef { name: "app_store_cache_path", type_: "str", default: "/data/app_stores_cache", help: "Directory to cache app store YAML files." },
    SettingDef { name: "auto_update_apps", type_: "bool", default: "true", help: "Enable automatic pulling of the latest app images in the background." },
    SettingDef { name: "auto_update_interval_seconds", type_: "int", default: "3600", help: "How often to check for app image updates (in seconds)." },
    SettingDef { name: "puid", type_: "int", default: "1000", help: "Default User ID to run containers as." },
    SettingDef { name: "pgid", type_: "int", default: "1000", help: "Default Group ID to run containers as." },
    SettingDef { name: "data_root", type_: "str", default: "/data", help: "Root of the persistent data tree (database, storage, caches, backups)." },
    SettingDef { name: "backups_path", type_: "str", default: "/data/backups", help: "Directory for backup archives." },
    SettingDef { name: "backup_retention", type_: "int", default: "7", help: "Number of backup archives to keep; 0 keeps all." },
    SettingDef { name: "storage_path", type_: "str", default: "/data/storage", help: "Base directory for user home directories." },
    SettingDef { name: "app_icons_path", type_: "str", default: "/data/storage/vreckan_app_icons", help: "Directory for storing custom-uploaded application icons." },
    SettingDef { name: "home_templates_path", type_: "str", default: "/data/storage/vreckan_home_templates", help: "Base directory for meta-app home directory templates." },
    SettingDef { name: "container_config_path", type_: "str", default: "/config", help: "Mount point for home directories inside the container." },
    SettingDef { name: "server_private_key_path", type_: "str", default: "/data/ssl/server_key.pem", help: "Path to the server private key PEM file (written 0600). Default lives under the persistent data root so the key survives container recreation." },
    SettingDef { name: "public_storage_path", type_: "str", default: "/data/storage/vreckan_public", help: "Directory for storing publicly shared files." },
    SettingDef { name: "share_cleanup_interval_seconds", type_: "int", default: "600", help: "How often to run the cleanup job for expired shares (in seconds)." },
    // --- OIDC / SSO settings ---
    SettingDef { name: "oidc_enabled", type_: "bool", default: "false", help: "Enable OIDC/SSO login (requires an issuer URL and client_id)." },
    SettingDef { name: "oidc_issuer", type_: "str", default: "", help: "OIDC issuer base URL. Discovery is fetched from {issuer}/.well-known/openid-configuration." },
    SettingDef { name: "oidc_client_id", type_: "str", default: "", help: "OIDC client_id for the authorization-code flow." },
    SettingDef { name: "oidc_client_secret", type_: "str", default: "", help: "OIDC client_secret (optional for public clients)." },
    SettingDef { name: "oidc_redirect_uri", type_: "str", default: "", help: "Registered redirect_uri. Defaults to {base}/api/auth/oidc/callback if empty." },
    SettingDef { name: "oidc_scopes", type_: "str", default: "openid email profile", help: "Space-separated OIDC scopes requested from the provider." },
    SettingDef { name: "oidc_allow_signup", type_: "bool", default: "true", help: "Auto-provision a Vreckan user when an OIDC identity does not exist yet." },
    SettingDef { name: "oidc_state_ttl_seconds", type_: "int", default: "600", help: "Lifetime of a single OIDC authorize state token (seconds)." },
    SettingDef { name: "oidc_admin_groups", type_: "str", default: "", help: "Comma/space-separated group names; OIDC users in any of these groups are auto-promoted to admin." },
    SettingDef { name: "oidc_user_groups", type_: "str", default: "", help: "Comma/space-separated group names that map to regular (non-admin) Vreckan groups." },
    SettingDef { name: "oidc_require_signature", type_: "bool", default: "true", help: "Require a verified id_token signature for OIDC login. When true (default), login fails closed if the provider's JWKS is unreachable; when false, a claims-only (unsigned) fallback is used for providers that omit a JWKS." },
    // --- Bootstrap admin ---
    SettingDef { name: "bootstrap_admin_username", type_: "str", default: "admin", help: "Username of the admin account created on first run (when no admin exists yet)." },
    SettingDef { name: "bootstrap_admin_password", type_: "str", default: "admin1234", help: "Initial password for the bootstrap admin account; change it after first login." },
    // --- Let's Encrypt (certbot) ---
    SettingDef { name: "le_enabled", type_: "bool", default: "false", help: "Issue Let's Encrypt certificates via certbot. When off, the server uses a self-signed certificate." },
    SettingDef { name: "le_domains", type_: "str", default: "", help: "Comma-separated domain(s) to issue certificates for." },
    SettingDef { name: "le_auth", type_: "str", default: "dns-cloudflare", help: "ACME challenge method: 'dns-cloudflare' (Cloudflare DNS-01) or 'http-01'." },
    SettingDef { name: "le_cloudflare_email", type_: "str", default: "", help: "Account email for Let's Encrypt (used with Cloudflare DNS auth)." },
    SettingDef { name: "le_cloudflare_token", type_: "str", default: "", help: "Cloudflare API token with Zone.DNS edit permission for the domain's zone." },
    SettingDef { name: "le_staging", type_: "bool", default: "false", help: "Use the Let's Encrypt staging server (test certificates, no production rate limits)." },
    SettingDef { name: "le_auto_renew", type_: "bool", default: "true", help: "Automatically renew Let's Encrypt certificates in the background." },
    SettingDef { name: "le_renew_check_hours", type_: "int", default: "6", help: "How often to check for certificate renewal (in hours)." },
    SettingDef { name: "le_renew_before_days", type_: "int", default: "30", help: "Renew the certificate when it expires within this many days." },
];

/// Parse a raw string into a setting value according to its declared type.
fn parse_value(type_: &str, raw: &str) -> String {
    match type_ {
        "bool" => {
            if raw.eq_ignore_ascii_case("true") || raw == "1" || raw.eq_ignore_ascii_case("yes") {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        "int" => raw.parse::<i64>().map(|v| v.to_string()).unwrap_or_else(|_| "0".to_string()),
        _ => raw.to_string(),
    }
}

/// Application settings, parsed from the environment with code defaults.
///
/// This is the env/default layer of the precedence chain. The DB layer
/// (`app_settings` table) can override any of these at runtime.
#[derive(Clone, Debug)]
pub struct Settings {
    pub log_level: String,
    pub api_port: i64,
    pub session_port: i64,
    pub database_url: String,
    pub db_backend: String,
    pub db_host: String,
    pub db_port: i64,
    pub db_user: String,
    pub db_pass: String,
    pub db_name: String,
    pub db_path: String,
    pub app_resource_path: String,
    pub default_app_templates_path: String,
    pub template_schema_path: String,
    pub upload_dir: String,
    pub session_cookie_name: String,
    pub auth_cookie_name: String,
    pub auth_token_ttl_seconds: i64,
    pub autostart_cache_path: String,
    pub app_store_cache_path: String,
    pub auto_update_apps: bool,
    pub auto_update_interval_seconds: i64,
    pub puid: i64,
    pub pgid: i64,
    pub data_root: String,
    pub backups_path: String,
    pub backup_retention: i64,
    pub storage_path: String,
    pub app_icons_path: String,
    pub home_templates_path: String,
    pub container_config_path: String,
    pub server_private_key_path: String,
    pub public_storage_path: String,
    pub share_cleanup_interval_seconds: i64,
    pub oidc_enabled: bool,
    pub oidc_issuer: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub oidc_redirect_uri: String,
    pub oidc_scopes: String,
    pub oidc_allow_signup: bool,
    pub oidc_state_ttl_seconds: i64,
    pub oidc_admin_groups: String,
    pub oidc_user_groups: String,
    pub oidc_require_signature: bool,
    pub bootstrap_admin_username: String,
    pub bootstrap_admin_password: String,
    pub le_enabled: bool,
    pub le_domains: String,
    pub le_auth: String,
    pub le_cloudflare_email: String,
    pub le_cloudflare_token: String,
    pub le_staging: bool,
    pub le_auto_renew: bool,
    pub le_renew_check_hours: i64,
    pub le_renew_before_days: i64,
}

impl Settings {
    /// Build settings from the `VRECKAN_*` environment, falling back to the
    /// code defaults. Mirrors `AppSettings._process_and_set_attributes`.
    pub fn from_env() -> Self {
        let mut values: HashMap<&str, String> = HashMap::new();
        for def in SETTING_DEFINITIONS {
            let env_var = format!("VRECKAN_{}", def.name.to_uppercase());
            let raw = std::env::var(&env_var).unwrap_or_else(|_| def.default.to_string());
            values.insert(def.name, parse_value(def.type_, &raw));
        }
        let get = |name: &str| values.get(name).cloned().unwrap_or_default();
        let get_bool = |name: &str| get(name) == "true";
        let get_int = |name: &str| get(name).parse::<i64>().unwrap_or(0);

        Self {
            log_level: get("log_level"),
            api_port: get_int("api_port"),
            session_port: get_int("session_port"),
            database_url: get("database_url"),
            db_backend: get("db_backend"),
            db_host: get("db_host"),
            db_port: get_int("db_port"),
            db_user: get("db_user"),
            db_pass: get("db_pass"),
            db_name: get("db_name"),
            db_path: get("db_path"),
            app_resource_path: get("app_resource_path"),
            default_app_templates_path: get("default_app_templates_path"),
            template_schema_path: get("template_schema_path"),
            upload_dir: get("upload_dir"),
            session_cookie_name: get("session_cookie_name"),
            auth_cookie_name: get("auth_cookie_name"),
            auth_token_ttl_seconds: get_int("auth_token_ttl_seconds"),
            autostart_cache_path: get("autostart_cache_path"),
            app_store_cache_path: get("app_store_cache_path"),
            auto_update_apps: get_bool("auto_update_apps"),
            auto_update_interval_seconds: get_int("auto_update_interval_seconds"),
            puid: get_int("puid"),
            pgid: get_int("pgid"),
            data_root: get("data_root"),
            backups_path: get("backups_path"),
            backup_retention: get_int("backup_retention"),
            storage_path: get("storage_path"),
            app_icons_path: get("app_icons_path"),
            home_templates_path: get("home_templates_path"),
            container_config_path: get("container_config_path"),
            server_private_key_path: get("server_private_key_path"),
            public_storage_path: get("public_storage_path"),
            share_cleanup_interval_seconds: get_int("share_cleanup_interval_seconds"),
            oidc_enabled: get_bool("oidc_enabled"),
            oidc_issuer: get("oidc_issuer"),
            oidc_client_id: get("oidc_client_id"),
            oidc_client_secret: get("oidc_client_secret"),
            oidc_redirect_uri: get("oidc_redirect_uri"),
            oidc_scopes: get("oidc_scopes"),
            oidc_allow_signup: get_bool("oidc_allow_signup"),
            oidc_state_ttl_seconds: get_int("oidc_state_ttl_seconds"),
            oidc_admin_groups: get("oidc_admin_groups"),
            oidc_user_groups: get("oidc_user_groups"),
            oidc_require_signature: get_bool("oidc_require_signature"),
            bootstrap_admin_username: get("bootstrap_admin_username"),
            bootstrap_admin_password: get("bootstrap_admin_password"),
            le_enabled: get_bool("le_enabled"),
            le_domains: get("le_domains"),
            le_auth: get("le_auth"),
            le_cloudflare_email: get("le_cloudflare_email"),
            le_cloudflare_token: get("le_cloudflare_token"),
            le_staging: get_bool("le_staging"),
            le_auto_renew: get_bool("le_auto_renew"),
            le_renew_check_hours: get_int("le_renew_check_hours"),
            le_renew_before_days: get_int("le_renew_before_days"),
        }
    }

    /// Look up a setting definition by name.
    pub fn definition(name: &str) -> Option<&'static SettingDef> {
        SETTING_DEFINITIONS.iter().find(|d| d.name == name)
    }

    /// The raw env/default value of a setting (before any DB override).
    pub fn raw_value(&self, name: &str) -> Option<String> {
        Some(match name {
            "log_level" => self.log_level.clone(),
            "api_port" => self.api_port.to_string(),
            "session_port" => self.session_port.to_string(),
            "database_url" => self.database_url.clone(),
            "db_backend" => self.db_backend.clone(),
            "db_host" => self.db_host.clone(),
            "db_port" => self.db_port.to_string(),
            "db_user" => self.db_user.clone(),
            "db_pass" => self.db_pass.clone(),
            "db_name" => self.db_name.clone(),
            "db_path" => self.db_path.clone(),
            "app_resource_path" => self.app_resource_path.clone(),
            "default_app_templates_path" => self.default_app_templates_path.clone(),
            "template_schema_path" => self.template_schema_path.clone(),
            "upload_dir" => self.upload_dir.clone(),
            "session_cookie_name" => self.session_cookie_name.clone(),
            "auth_cookie_name" => self.auth_cookie_name.clone(),
            "auth_token_ttl_seconds" => self.auth_token_ttl_seconds.to_string(),
            "autostart_cache_path" => self.autostart_cache_path.clone(),
            "app_store_cache_path" => self.app_store_cache_path.clone(),
            "auto_update_apps" => self.auto_update_apps.to_string(),
            "auto_update_interval_seconds" => self.auto_update_interval_seconds.to_string(),
            "puid" => self.puid.to_string(),
            "pgid" => self.pgid.to_string(),
            "data_root" => self.data_root.clone(),
            "backups_path" => self.backups_path.clone(),
            "backup_retention" => self.backup_retention.to_string(),
            "storage_path" => self.storage_path.clone(),
            "app_icons_path" => self.app_icons_path.clone(),
            "home_templates_path" => self.home_templates_path.clone(),
            "container_config_path" => self.container_config_path.clone(),
            "server_private_key_path" => self.server_private_key_path.clone(),
            "public_storage_path" => self.public_storage_path.clone(),
            "share_cleanup_interval_seconds" => self.share_cleanup_interval_seconds.to_string(),
            "oidc_enabled" => self.oidc_enabled.to_string(),
            "oidc_issuer" => self.oidc_issuer.clone(),
            "oidc_client_id" => self.oidc_client_id.clone(),
            "oidc_client_secret" => self.oidc_client_secret.clone(),
            "oidc_redirect_uri" => self.oidc_redirect_uri.clone(),
            "oidc_scopes" => self.oidc_scopes.clone(),
            "oidc_allow_signup" => self.oidc_allow_signup.to_string(),
            "oidc_state_ttl_seconds" => self.oidc_state_ttl_seconds.to_string(),
            "oidc_admin_groups" => self.oidc_admin_groups.clone(),
            "oidc_user_groups" => self.oidc_user_groups.clone(),
            "oidc_require_signature" => self.oidc_require_signature.to_string(),
            "bootstrap_admin_username" => self.bootstrap_admin_username.clone(),
            "bootstrap_admin_password" => self.bootstrap_admin_password.clone(),
            "le_enabled" => self.le_enabled.to_string(),
            "le_domains" => self.le_domains.clone(),
            "le_auth" => self.le_auth.clone(),
            "le_cloudflare_email" => self.le_cloudflare_email.clone(),
            "le_cloudflare_token" => self.le_cloudflare_token.clone(),
            "le_staging" => self.le_staging.to_string(),
            "le_auto_renew" => self.le_auto_renew.to_string(),
            "le_renew_check_hours" => self.le_renew_check_hours.to_string(),
            "le_renew_before_days" => self.le_renew_before_days.to_string(),
            _ => return None,
        })
    }

    /// The *effective* value of a setting, applying the DB-over-env-over-
    /// default precedence. A value stored in the `app_settings` table (edited
    /// on the admin SSO page) wins over the `VRECKAN_*` environment variable,
    /// which wins over the code default. Mirrors `AppSettings.get_setting`.
    ///
    /// The value is parsed according to the setting's declared type
    /// (bool/int/str) and returned as a `serde_json::Value`.
    pub async fn get_setting(&self, db: &crate::db::Db, name: &str) -> serde_json::Value {
        let def = Self::definition(name);
        let stype = def.map(|d| d.type_).unwrap_or("str");
        let default = def.map(|d| d.default).unwrap_or("");
        let env_var = format!("VRECKAN_{}", name.to_uppercase());

        // DB value wins if present.
        let raw = match db.get_app_setting(&env_var).await {
            Ok(Some(v)) => v,
            _ => match self.raw_value(name) {
                Some(v) => v,
                None => return serde_json::Value::String(default.to_string()),
            },
        };
        match stype {
            "bool" => serde_json::Value::Bool(parse_value("bool", &raw) == "true"),
            "int" => serde_json::Value::Number(
                parse_value("int", &raw).parse::<i64>().unwrap_or(0).into(),
            ),
            _ => serde_json::Value::String(raw),
        }
    }

    /// Build the sqlx database URL from the discrete `db_*` settings.
    ///
    /// If `database_url` is explicitly set (non-empty), it is used as-is
    /// (after normalising any legacy SQLAlchemy-style scheme). Otherwise the
    /// URL is assembled from `db_backend`, `db_host`, `db_port`, `db_user`,
    /// `db_pass`, `db_name` (postgres/mysql) or `db_path` (sqlite).
    pub fn build_database_url(&self) -> String {
        // Explicit override wins.
        if !self.database_url.is_empty() {
            return Self::normalize_url(&self.database_url);
        }

        match self.db_backend.as_str() {
            "postgres" | "postgresql" => {
                let port = if self.db_port > 0 { self.db_port } else { 5432 };
                let user = if self.db_user.is_empty() { "postgres" } else { &self.db_user };
                let name = if self.db_name.is_empty() { "vreckan" } else { &self.db_name };
                let host = if self.db_host.is_empty() { "localhost" } else { &self.db_host };
                if self.db_pass.is_empty() {
                    format!("postgres://{}@{}:{}/{}", user, host, port, name)
                } else {
                    format!("postgres://{}:{}@{}:{}/{}", user, self.db_pass, host, port, name)
                }
            }
            "mysql" | "mariadb" => {
                let port = if self.db_port > 0 { self.db_port } else { 3306 };
                let user = if self.db_user.is_empty() { "root" } else { &self.db_user };
                let name = if self.db_name.is_empty() { "vreckan" } else { &self.db_name };
                let host = if self.db_host.is_empty() { "localhost" } else { &self.db_host };
                if self.db_pass.is_empty() {
                    format!("mysql://{}@{}:{}/{}", user, host, port, name)
                } else {
                    format!("mysql://{}:{}@{}:{}/{}", user, self.db_pass, host, port, name)
                }
            }
            _ => {
                // sqlite (default)
                let path = if self.db_path.is_empty() {
                    format!("{}/vreckan.db", self.data_root)
                } else {
                    self.db_path.clone()
                };
                format!("sqlite://{}?mode=rwc", path)
            }
        }
    }

    /// Normalise a user-supplied URL, converting legacy SQLAlchemy-style
    /// schemes (`sqlite+aiosqlite`, `postgresql+asyncpg`, `mysql+aiomysql`)
    /// to their sqlx equivalents.
    fn normalize_url(url: &str) -> String {
        if url.starts_with("sqlite+") {
            let after = url.splitn(2, ':').nth(1).unwrap_or("");
            let sep = if after.contains('?') { "&" } else { "?" };
            let with_mode = if after.contains("mode=") {
                after.to_string()
            } else {
                format!("{}{}mode=rwc", after, sep)
            };
            format!("sqlite:{}", with_mode)
        } else if url.starts_with("postgresql+") || url.starts_with("postgres+") {
            let rest = url.splitn(2, "://").nth(1).unwrap_or(url);
            format!("postgres://{}", rest)
        } else if url.starts_with("mysql+") || url.starts_with("mariadb+") {
            let rest = url.splitn(2, "://").nth(1).unwrap_or(url);
            format!("mysql://{}", rest)
        } else {
            url.to_string()
        }
    }

    /// Extract the `user:pass` userinfo from a sqlx database URL, or `None`
    /// when the URL carries no `@`-separated credentials (e.g. the
    /// `postgres://localhost:5432/db` form, which has nothing to compare
    /// against the default).
    fn extract_url_creds(url: &str) -> Option<(String, String)> {
        let after_scheme = url.split("://").nth(1)?;
        let at = after_scheme.rfind('@')?;
        let userinfo = &after_scheme[..at];
        let mut parts = userinfo.splitn(2, ':');
        let user = parts.next().unwrap_or("").to_string();
        let pass = parts.next().unwrap_or("").to_string();
        Some((user, pass))
    }

    /// Whether the resolved database connection uses the bundled Postgres
    /// service's default `vreckan`/`vreckan` credentials (ANT-45, P2-6).
    ///
    /// The compose `VRECKAN_DATABASE_URL` default injects exactly these
    /// credentials when `.env` is left at defaults, leaving the database that
    /// holds every password hash and auth token reachable with trivially
    /// guessable creds. This inspects the *resolved* URL (an explicit
    /// `VRECKAN_DATABASE_URL`, or one assembled from the `db_*` settings), so
    /// it catches every deployment path that ends up on the defaults.
    pub fn uses_default_postgres_creds(&self) -> bool {
        let url = self.build_database_url();
        if !url.starts_with("postgres://") {
            return false;
        }
        match Self::extract_url_creds(&url) {
            Some((user, pass)) => user == "vreckan" && pass == "vreckan",
            None => false,
        }
    }
}

/// A lazily-built, process-wide copy of the env/default settings. Handlers
/// that only need the env/default layer (not the DB override) can use this to
/// avoid threading `&Settings` through. The DB layer reads `Settings` directly
/// from `AppState` when it needs the full precedence chain.
pub fn global_settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(Settings::from_env)
}

/// How the server reacts when it starts against Postgres with the default
/// `vreckan`/`vreckan` credentials (ANT-45, P2-6), selected by the
/// `VRECKAN_ALLOW_DEFAULT_DB_CREDS` escape hatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultDbCredsPolicy {
    /// Log a prominent warning and continue starting (the default).
    Warn,
    /// The operator explicitly opted in; start silently.
    Silent,
    /// Fail startup — for hardened deployments that must not run on defaults.
    Refuse,
}

/// Resolve the `VRECKAN_ALLOW_DEFAULT_DB_CREDS` escape hatch into a
/// [`DefaultDbCredsPolicy`].
///
/// - unset/empty → [`DefaultDbCredsPolicy::Warn`] (a loud warning)
/// - `1` / `true` / `yes` / `on` / `silent` / `allow` → [`DefaultDbCredsPolicy::Silent`]
/// - `refuse` / `strict` / `fail` → [`DefaultDbCredsPolicy::Refuse`] (fail-fast)
pub fn default_db_creds_policy() -> DefaultDbCredsPolicy {
    let raw = std::env::var("VRECKAN_ALLOW_DEFAULT_DB_CREDS")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase());
    match raw.as_deref() {
        Some(v) if matches!(v, "1" | "true" | "yes" | "on" | "silent" | "allow") => {
            DefaultDbCredsPolicy::Silent
        }
        Some(v) if matches!(v, "refuse" | "strict" | "fail") => DefaultDbCredsPolicy::Refuse,
        _ => DefaultDbCredsPolicy::Warn,
    }
}
