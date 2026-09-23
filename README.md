# Vreckan

> **Vreckan is a hard fork of [SealSkin](https://github.com/linuxserver/docker-sealskin).** It continues the project under a new name with a leaner codebase and its own roadmap. Project homepage: [vreckanproject.com](https://vreckanproject.com) · Source code: [github.com/vreckan-project/vreckan](https://github.com/vreckan-project/vreckan)

Vreckan is a self-hosted, client-server platform that enables users to run powerful, containerized desktop applications streamed directly to a web browser. A built-in web client provides the interface for launching applications, managing files, and, for administrators, managing the platform. Everything runs in the browser, so there is nothing to install.

Get the server [HERE](https://github.com/vreckan-project/vreckan)

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

The server is the central hub of the platform. It is responsible for all management, orchestration, and traffic proxying. Its key design feature is a **Dual-Port Architecture** that separates management tasks from live application traffic, enhancing security and stability.

*   **Control Plane (API Server):** Handles user authentication, application management, and the orchestration of new application sessions. All communication is protected by end-to-end encryption.
*   **Data Plane (Session Proxy Server):** Acts as a secure reverse proxy for all live application traffic (HTTP and WebSockets). It ensures that the internal application containers are never directly exposed to the internet.

### Vreckan Web Client

The web client is the user's entry point into the Vreckan environment. It is a single-page web application served directly by the server, so there is nothing to install. It provides the UI for launching and managing remote sessions, browsing and sharing files, and, for administrators, managing users, groups, and applications. It is responsible for all client-side cryptographic operations, including the E2EE handshake and authentication.

## How It Works: A Typical Workflow

The interaction between the web client and the server follows a secure and orchestrated process:

1.  **Sign-In:** A user opens the Vreckan web client in their browser and signs in, using a password or single sign-on.
2.  **App Selection:** The **Web Client** presents the user with a list of available remote applications.
3.  **Secure Communication:** The web client establishes an End-to-End Encrypted (E2EE) channel with the server's API and authenticates the user with a secure, server-issued session token.
4.  **Server Orchestration:** The **Server** receives the encrypted launch request. After authenticating the user and verifying permissions, it instructs its backend provider (e.g., Docker) to launch a new, isolated application container.
5.  **Proxy Connection:** Once the container is running, the server returns a unique, single-use URL. The web client opens this URL in a new tab, connecting the user to the running application through the server's secure **Session Proxy**. All subsequent traffic for that session flows through the proxy.

## Key Features

*   **End-to-End Encrypted API:** All management communication between the client and server is encrypted, protecting sensitive data like launch parameters and user information.
*   **Flexible Authentication:** Users sign in with a password or via single sign-on (OIDC), with sessions maintained by secure, server-issued tokens.
*   **Dual-Port Architecture:** A strict separation between the management control plane and the application data plane enhances security.
*   **Containerized Application Backend:** Uses Docker as the primary provider to run applications in isolated, sandboxed environments.
*   **Role-Based Access Control (RBAC):** A clear distinction between Admins (full system control) and Users (can only launch and manage their own sessions).
*   **No Installation Required:** The full client experience, including launching apps, managing files, and administration, runs entirely in the browser.
*   **Full Admin UI:** A dedicated admin area provides a complete management dashboard for administrators, allowing for user, group, and application management directly from the browser.

## Getting Started

### Prerequisites
*   A server with **Python** and **Docker** installed.
*   An SSL certificate and key for your server's domain (optional — a self-signed certificate is generated automatically on first run if you don't provide one).

### 1. Server Setup
1.  Clone the repository to your server.
2.  Configure the server by setting the required environment variables. See the **Configuration** section below. A self-signed TLS certificate is generated automatically on first run; drop a real certificate + key at the configured SSL path to override it.
3.  Install requirements `pip3 install -r requirements.txt`.
4.  Run the server `python3 run_https.py`.

### 2. Web Client Access
1.  Open the server's web address in any modern browser (for example, `https://vreckan.antitux.net/` or `https://<your-server>:443`).
2.  Sign in with your username and password, or via single sign-on if it is configured. The default admin credentials are provided in the server logs on first run.
3.  Use the web client to launch applications, manage files, and, if you are an administrator, manage users, groups, and applications.

## Configuration

The Vreckan server is configured entirely through environment variables. The table below lists all available settings.

| Environment Variable | CLI Setting | Description | Default Value |
| --- | --- | --- | --- |
| `VRECKAN_LOG_LEVEL` | `--log-level` | Logging level (e.g., DEBUG, INFO, WARNING). | `INFO` |
| `VRECKAN_API_PORT` | `--api-port` | Port for the main API server. | `8000` |
| `VRECKAN_SESSION_PORT` | `--session-port` | Port for the session proxy server. | `8443` |
| `VRECKAN_APP_RESOURCE_PATH` | `--app-resource-path` | URL for the YAML file defining default available applications. | `https://raw.githubusercontent.com/linuxserver/sealskin-apps/refs/heads/master/apps.yml` |
| `VRECKAN_DEFAULT_APP_TEMPLATES_PATH` | `--default-app-templates-path` | Path to the directory for default application templates. | `server/default_templates` |
| `VRECKAN_UPLOAD_DIR` | `--upload-dir` | Directory for temporary file uploads. | `/storage/vreckan_uploads` |
| `VRECKAN_SESSION_COOKIE_NAME` | `--session-cookie-name` | Name of the session cookie. | `vreckan_session_token` |
| `VRECKAN_AUTOSTART_CACHE_PATH` | `--autostart-cache-path` | Path to cache autostart scripts. | `/config/.config/vreckan/autostart_cache` |
| `VRECKAN_APP_STORE_CACHE_PATH` | `--app-store-cache-path` | Path to cache app store YAML files. | `/config/.config/vreckan/app_stores_cache` |
| `VRECKAN_AUTO_UPDATE_APPS` | `--auto-update-apps` | Enable automatic pulling of the latest app images in the background. | `True` |
| `VRECKAN_AUTO_UPDATE_INTERVAL_SECONDS` | `--auto-update-interval-seconds` | How often to check for app image updates (in seconds). | `3600` |
| `VRECKAN_PUID` | `--puid` | Default User ID to run containers as. | `1000` |
| `VRECKAN_PGID` | `--pgid` | Default Group ID to run containers as. | `1000` |
| `VRECKAN_STORAGE_PATH` | `--storage-path` | Base directory for user home directories. | `/storage` |
| `VRECKAN_APP_ICONS_PATH` | `--app-icons-path` | Directory for storing custom-uploaded application icons. | `/storage/vreckan_app_icons` |
| `VRECKAN_HOME_TEMPLATES_PATH` | `--home-templates-path` | Base directory for meta-app home directory templates. | `/storage/vreckan_home_templates` |
| `VRECKAN_CONTAINER_CONFIG_PATH` | `--container-config-path` | Mount point for home directories inside the container. | `/config` |
| `VRECKAN_SERVER_PRIVATE_KEY_PATH` | `--server-private-key-path` | Path to the server private key PEM file. | `/config/ssl/server_key.pem` |
| `VRECKAN_PUBLIC_STORAGE_PATH` | `--public-storage-path` | Directory for storing publicly shared files. | `/storage/vreckan_public` |
| `VRECKAN_SHARE_CLEANUP_INTERVAL_SECONDS` | `--share-cleanup-interval-seconds` | How often to run the cleanup job for expired shares (in seconds). | `600` |

## Details

This README covers the Vreckan server. The web client is served by the server and requires no separate installation or documentation.
