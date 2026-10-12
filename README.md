# Vreckan

> **Vreckan is a hard fork of [SealSkin](https://github.com/linuxserver/docker-sealskin).** It continues the project under a new name with a leaner codebase and its own roadmap. The server has been rewritten in Rust (see [REWRITE_EVALUATION.md](./REWRITE_EVALUATION.md) and [docs/port-status.md](./docs/port-status.md)). Project homepage: [vreckanproject.com](https://vreckanproject.com) · Server source: [github.com/vreckan-project/vreckan-rust](https://github.com/vreckan-project/vreckan-rust) · Original (Python): [github.com/vreckan-project/vreckan](https://github.com/vreckan-project/vreckan)

Vreckan is a self-hosted, client-server platform that enables users to run powerful, containerized desktop applications streamed directly to a web browser. A built-in web client provides the interface for launching applications, managing files, and, for administrators, managing the platform. Everything runs in the browser, so there is nothing to install.

Get the server [HERE](https://github.com/vreckan-project/vreckan-rust)

## Primary Functions

1.  **Remote Application Access:**
    Stream demanding desktop applications (e.g., video editors, IDEs, 3D modeling software, office suites) to any device with a web browser. This allows any file or link on the web to be opened directly in a full-featured application. It leverages the server's hardware (CPU, GPU, RAM) and provides access to powerful tools without local installation.

2.  **Secure Browser & File Isolation:**
    Isolate the local machine from the public internet. All web content, downloads, and application processes are executed within a sandboxed container on the remote server. This prevents malware, exploits, and trackers from ever reaching the client device, ensuring a clean and secure local environment.

3.  **Centralized Data Management:**
    Manage files through an integrated file manager that stores them in persistent storage on the server. This keeps your data centralized and makes files immediately available to your suite of remote applications for editing and processing, linked through multiple configurable home directories.

## Core Architecture

The system is composed of two primary components that communicate over a secure, end-to-end encrypted channel.

### Vreckan Server

The server is a single Rust binary (axum + tokio; SeaORM on SQLite, Postgres, or MySQL) that handles all management, orchestration, and traffic proxying. It serves two kinds of traffic on the same HTTPS endpoint:

*   **API (control plane):** Handles user authentication, application management, and the orchestration of new application sessions. All management communication is protected by end-to-end encryption.
*   **Session proxy (data plane):** A set of reverse-proxy routes (`/api/apps/session/{id}/...`) that forward live application traffic (HTTP and WebSockets) to the internal application containers, so the containers are never directly exposed to the internet.

The server terminates TLS itself and listens on a single endpoint: `0.0.0.0:$VRECKAN_API_PORT` (default **443**). In the Rust port both planes are served on that one listener; the `api_port` / `session_port` settings are carried over from the Python port as published-port metadata.

All durable configuration (users, groups, applications, sessions, tokens, shares, templates) lives in a database — SQLite by default, PostgreSQL or MySQL for production — rather than config files.

### Vreckan Web Client

The web client is the user's entry point into the Vreckan environment. It is a single-page web application served directly by the server, so there is nothing to install. It provides the UI for launching and managing remote sessions, browsing and sharing files, and, for administrators, managing users, groups, and applications. It is responsible for all client-side cryptographic operations, including the E2EE handshake and authentication.

## How It Works: A Typical Workflow

The interaction between the web client and the server follows a secure and orchestrated process:

1.  **Sign-In:** A user opens the Vreckan web client in their browser and signs in, using a password or single sign-on.
2.  **App Selection:** The **Web Client** presents the user with a list of available remote applications.
3.  **Secure Communication:** The web client establishes an End-to-End Encrypted (E2EE) channel with the server's API and authenticates the user with a secure, server-issued session token.
4.  **Server Orchestration:** The **Server** receives the encrypted launch request. After authenticating the user and verifying permissions, it instructs its backend provider (e.g., Docker) to launch a new, isolated application container.
5.  **Proxy Connection:** Once the container is running, the server returns a unique, single-use URL. The web client opens this URL in a new tab; the server exchanges the one-time token for a session cookie and then proxies all subsequent traffic for that session (HTTP and WebSockets) to the running application container.

## Key Features

*   **End-to-End Encrypted API:** All management communication between the client and server is encrypted, protecting sensitive data like launch parameters and user information.
*   **Flexible Authentication:** Users sign in with a password or via single sign-on (OIDC — authentik, Keycloak, Entra ID, ...), with sessions maintained by secure, server-issued tokens.
*   **Session Proxy:** Live application traffic is reverse-proxied by the server, so application containers are never directly exposed to the internet.
*   **Containerized Application Backend:** Uses Docker as the provider to run applications in isolated, sandboxed environments.
*   **Role-Based Access Control (RBAC):** A clear distinction between Admins (full system control) and Users (can only launch and manage their own sessions).
*   **Database-backed configuration:** Users, groups, apps, sessions, tokens, and shares live in a database (SQLite by default, PostgreSQL supported), so there are no config files to manage by hand.
*   **Let's Encrypt integration:** Optional automatic issuance and renewal of real TLS certificates via certbot (Cloudflare DNS-01 or HTTP-01), configurable from the admin UI.
*   **No Installation Required:** The full client experience, including launching apps, managing files, and administration, runs entirely in the browser.
*   **Full Admin UI:** A dedicated admin area provides a complete management dashboard for administrators, allowing for user, group, application, template, SSO, and certificate management directly from the browser.

## Getting Started

### Prerequisites
*   A server with **Docker** and **Docker Compose** installed, with access to the Docker daemon (the server spawns and manages app containers through it).
*   An SSL certificate for your domain (optional — a self-signed certificate is generated automatically on first run, and Let's Encrypt can be enabled to issue real certificates).

### 1. Run the server
The recommended deployment is the pre-built container image:

```bash
git clone --depth 1 https://github.com/vreckan-project/vreckan-rust.git
cd vreckan-rust
cp .env.example .env   # edit to taste (OIDC, Let's Encrypt, ...)
docker compose up -d
```

This runs the server with TLS on port 443, a persistent `vreckan-data`
bind mount, Docker-socket access for the app containers, and DRI/GPU device
pass-through (see [docs/ops.md](./docs/ops.md)). The compose file's `image:`
tag follows the Rust-cutover decision (ANT-1 D3); `build: .` always builds
the Rust image from this repo when the tag is not present, so a local build
runs the Rust server either way. Versioned image tags (e.g. `:v0.4.2`) are
published alongside `latest` for every release — see the
[release process](./docs/release.md).

To run from source instead (e.g. for development): install the Rust toolchain
(the image builds with `rust:1.99-slim`; any recent stable release works) and
Docker on the host, then:

```bash
cargo run --release
```

The binary reads the same `VRECKAN_*` environment variables; point
`VRECKAN_DATA_ROOT` at a scratch directory for a throwaway dev instance.

### 2. Web Client Access
1.  Open the server's web address in any modern browser (for example, `https://<your-server>/`).
2.  Sign in. On first run the server creates a default `admin` account (bootstrap password `admin1234`, configurable via `VRECKAN_BOOTSTRAP_ADMIN_*`) and prints the credentials to the server logs. Change the password after your first login. If OIDC/SSO is configured, you can sign in via the provider instead.
3.  Use the web client to launch applications, manage files, and, if you are an administrator, manage users, groups, and applications.

## Configuration

The Vreckan server is configured entirely through `VRECKAN_*` environment variables (see `.env.example`). Settings edited on the admin UI (SSO, certificates) are stored in the database and override the environment. The table below lists all available settings.

### Core

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_LOG_LEVEL` | Logging level (e.g., DEBUG, INFO, WARNING). | `INFO` |
| `VRECKAN_API_PORT` | Port the server binds with TLS. The entrypoint reads this directly and defaults to `443` when it is unset; the `api_port` setting itself defaults to `8000`. | `443` (bind) / `8000` (setting) |
| `VRECKAN_SESSION_PORT` | Published-port metadata for session-proxy traffic (carried over from the Python port; in the Rust port the session proxy is served on the same listener). | `8443` |
| `VRECKAN_DATABASE_URL` | Full override of the configuration-database URL (sqlx format). When empty, the URL is built from the `VRECKAN_DB_*` settings below. | *(empty)* |
| `VRECKAN_APP_RESOURCE_PATH` | URL for the YAML file defining default available applications. | `https://raw.githubusercontent.com/linuxserver/sealskin-apps/refs/heads/master/apps.yml` |
| `VRECKAN_DEFAULT_APP_TEMPLATES_PATH` | Path to the directory for default application templates. | `server/default_templates` |
| `VRECKAN_TEMPLATE_SCHEMA_PATH` | Path to the bundled template-schema YAML (the image baseline for the template editor). | `server/template_schema.yml` |
| `VRECKAN_AUTO_UPDATE_APPS` | Enable automatic pulling of the latest app images in the background. | `true` |
| `VRECKAN_AUTO_UPDATE_INTERVAL_SECONDS` | How often to check for app image updates (in seconds). | `3600` |

### Database

Backends are selected with `VRECKAN_DB_BACKEND` (SQLite is the default; the
same 13-table schema is created and migrated idempotently on every start).

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_DB_BACKEND` | Database backend: `sqlite`, `postgres`, or `mysql`. | `sqlite` |
| `VRECKAN_DB_HOST` | Database host (postgres/mysql). | `localhost` |
| `VRECKAN_DB_PORT` | Database port; `0` means the backend default (5432 postgres, 3306 mysql). | `0` |
| `VRECKAN_DB_USER` | Database username (postgres/mysql). | *(empty)* |
| `VRECKAN_DB_PASS` | Database password (postgres/mysql). | *(empty)* |
| `VRECKAN_DB_NAME` | Database name (postgres/mysql). | `vreckan` |
| `VRECKAN_DB_PATH` | SQLite database file path. | `{data_root}/vreckan.db` |

The default compose setup includes a `postgres:16` service; to use it set
`VRECKAN_DB_BACKEND=postgres` and `VRECKAN_DB_HOST=postgres` in `.env`
(see [docs/ops.md](./docs/ops.md)).

### Storage & paths

All persistent data lives under the data root (`VRECKAN_DATA_ROOT`, default `/data` — the `vreckan-data` volume in the default compose setup).

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_DATA_ROOT` | Root of the persistent data tree (database, storage, caches, backups). | `/data` |
| `VRECKAN_STORAGE_PATH` | Base directory for user home directories. | `/data/storage` |
| `VRECKAN_UPLOAD_DIR` | Directory for temporary file uploads. | `/data/storage/vreckan_uploads` |
| `VRECKAN_APP_ICONS_PATH` | Directory for storing custom-uploaded application icons. | `/data/storage/vreckan_app_icons` |
| `VRECKAN_HOME_TEMPLATES_PATH` | Base directory for meta-app home directory templates. | `/data/storage/vreckan_home_templates` |
| `VRECKAN_PUBLIC_STORAGE_PATH` | Directory for storing publicly shared files. | `/data/storage/vreckan_public` |
| `VRECKAN_CONTAINER_CONFIG_PATH` | Mount point for home directories inside the container. | `/config` |
| `VRECKAN_AUTOSTART_CACHE_PATH` | Path to cache autostart scripts. | `/data/autostart_cache` |
| `VRECKAN_APP_STORE_CACHE_PATH` | Path to cache app store YAML files. | `/data/app_stores_cache` |
| `VRECKAN_BACKUPS_PATH` | Directory for backup archives. | `/data/backups` |
| `VRECKAN_BACKUP_RETENTION` | Number of backup archives to keep; 0 keeps all. | `0` |
| `VRECKAN_PUID` | Default User ID to run containers as. | `1000` |
| `VRECKAN_PGID` | Default Group ID to run containers as. | `1000` |

### Authentication & sessions

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_SESSION_COOKIE_NAME` | Name of the session cookie. | `vreckan_session_token` |
| `VRECKAN_AUTH_COOKIE_NAME` | Name of the web-login authentication cookie. | `vreckan_auth` |
| `VRECKAN_AUTH_TOKEN_TTL_SECONDS` | Lifetime (in seconds) of a server-issued web authentication token/cookie. | `28800` |
| `VRECKAN_SHARE_CLEANUP_INTERVAL_SECONDS` | How often to run the cleanup job for expired shares (in seconds). | `600` |
| `VRECKAN_SERVER_PRIVATE_KEY_PATH` | Path to the server private key PEM file (used for the E2EE handshake). | `/config/ssl/server_key.pem` |
| `VRECKAN_BOOTSTRAP_ADMIN_USERNAME` | Username of the admin account created on first run (when no admin exists yet). | `admin` |
| `VRECKAN_BOOTSTRAP_ADMIN_PASSWORD` | Initial password for the bootstrap admin account; change it after first login. | `admin1234` |

### OIDC / SSO

These settings can also be edited from the admin UI's SSO page (a value set there overrides the environment).

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_OIDC_ENABLED` | Enable OIDC/SSO login (requires an issuer URL and client_id). | `False` |
| `VRECKAN_OIDC_ISSUER` | OIDC issuer base URL (e.g. `https://auth.example.com/application/o/vreckan/`). Discovery is fetched from `{issuer}/.well-known/openid-configuration`. | *(empty)* |
| `VRECKAN_OIDC_CLIENT_ID` | OIDC client_id for the authorization-code flow. | *(empty)* |
| `VRECKAN_OIDC_CLIENT_SECRET` | OIDC client_secret (optional for public clients). | *(empty)* |
| `VRECKAN_OIDC_REDIRECT_URI` | Registered redirect_uri. Defaults to `{base}/api/auth/oidc/callback` if empty. | *(empty)* |
| `VRECKAN_OIDC_SCOPES` | Space-separated OIDC scopes requested from the provider. | `openid email profile` |
| `VRECKAN_OIDC_ALLOW_SIGNUP` | Auto-provision a Vreckan user when an OIDC identity does not exist yet. | `True` |
| `VRECKAN_OIDC_STATE_TTL_SECONDS` | Lifetime of a single OIDC authorize state token (seconds). | `600` |
| `VRECKAN_OIDC_ADMIN_GROUPS` | Comma/space-separated group names; OIDC users in any of these groups are auto-promoted to admin. | *(empty)* |
| `VRECKAN_OIDC_USER_GROUPS` | Comma/space-separated group names that map to regular (non-admin) Vreckan groups; an SSO user not in an admin group is assigned to their first group in this list. | *(empty)* |
| `VRECKAN_OIDC_REQUIRE_SIGNATURE` | Require a verified `id_token` signature; fail closed when the JWKS is unreachable (set `false` for a claims-only fallback). | `true` |

### Let's Encrypt (certbot)

These settings can also be edited from the admin UI's Certificates page (a value set there overrides the environment).

| Environment Variable | Description | Default Value |
| --- | --- | --- |
| `VRECKAN_LE_ENABLED` | Issue Let's Encrypt certificates via certbot. When off, the server uses a self-signed certificate. | `False` |
| `VRECKAN_LE_DOMAINS` | Comma-separated domain(s) to issue certificates for (e.g. `example.com,www.example.com`). | *(empty)* |
| `VRECKAN_LE_AUTH` | ACME challenge method: `dns-cloudflare` (Cloudflare DNS-01) or `http-01`. | `dns-cloudflare` |
| `VRECKAN_LE_CLOUDFLARE_EMAIL` | Account email for Let's Encrypt (used with Cloudflare DNS auth). | *(empty)* |
| `VRECKAN_LE_CLOUDFLARE_TOKEN` | Cloudflare API token with Zone.DNS edit permission for the domain's zone. | *(empty)* |
| `VRECKAN_LE_STAGING` | Use the Let's Encrypt staging server (test certificates, no production rate limits). | `False` |
| `VRECKAN_LE_AUTO_RENEW` | Automatically renew Let's Encrypt certificates in the background. | `True` |
| `VRECKAN_LE_RENEW_CHECK_HOURS` | How often to check for certificate renewal (in hours). | `6` |
| `VRECKAN_LE_RENEW_BEFORE_DAYS` | Renew the certificate when it expires within this many days. | `30` |

## Documentation

- **[docs/api.md](./docs/api.md)** — full REST/WS endpoint reference for the Rust port, with per-route auth and E2EE requirements.
- **[docs/ops.md](./docs/ops.md)** — operations & deployment: the container and its host privileges (docker-sock, DRI, group add), image build and registries, the Gitea vLLM-pattern CI/CD flow, database backends, TLS / Let's Encrypt, and the data layout.
- **[docs/release.md](./docs/release.md)** — the release & changelog process (`bump-and-tag`, `.base-image-digest`, changelog conventions).
- **[docs/port-status.md](./docs/port-status.md)** — living record of known gaps vs the Python original, sourced from the ANT-5 parity audit.
- **[REWRITE_EVALUATION.md](./REWRITE_EVALUATION.md)** — the feasibility assessment that kicked off the Rust rewrite.
- **[HISTORY.md](./HISTORY.md)** — the full record of the original SealSkin hard fork (pre-Rust).
- **[CHANGELOG.md](./CHANGELOG.md)** — development log, newest first (post-fork).

## Details

This README covers the Vreckan server. The web client is served by the server and requires no separate installation or documentation.
