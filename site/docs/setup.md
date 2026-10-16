---
layout: default
title: Setup
nav: setup
description: Prerequisites, server installation, configuration, and getting your first Vreckan session running.
---

<div class="doc">
  <h1>Setup</h1>
  <p class="doc-sub">Run Vreckan from a Docker image — no Python required on the host.</p>

  <h2>Prerequisites</h2>
  <ul>
    <li>A server with <strong>Docker</strong> and <strong>Docker Compose</strong> installed.</li>
    <li>Access to the Docker daemon (the server spawns and manages app containers through it).</li>
    <li>An SSL certificate for your domain — <em>optional</em>. A self-signed certificate is generated automatically on first run, and ACME can be enabled to issue real certificates (see below).</li>
  </ul>

  <h2>1. Get the image</h2>
  <p>Vreckan ships as a container image. Pull the latest release:</p>
  <pre><code>docker pull ghcr.io/vreckan-project/vreckan:latest</code></pre>
  <p>Versioned tags (for example <code>:v0.6.5</code>) are published alongside <code>latest</code> for every release. The current release is **v0.6.5** — see the <a href="https://github.com/vreckan-project/vreckan/blob/main/CHANGELOG.md">changelog</a> for what changed in each version.</p>

  <h2>2. Configure</h2>
  <p>Vreckan is configured entirely through <code>VRECKAN_*</code> environment variables. The repository ships a <code>.env.example</code> you can copy and edit:</p>
  <pre><code>git clone --depth 1 https://github.com/vreckan-project/vreckan.git
cd vreckan
cp .env.example .env</code></pre>
  <p>The most important settings:</p>
  <table>
    <thead>
      <tr><th scope="col">Variable</th><th scope="col">Description</th><th scope="col">Default</th></tr>
    </thead>
    <tbody>
      <tr><td><code>VRECKAN_API_PORT</code></td><td>Port the server listens on (TLS). The Rust server entrypoint reads this directly and defaults to <code>443</code> when unset.</td><td><code>443</code></td></tr>
      <tr><td><code>VRECKAN_SESSION_PORT</code></td><td>Carried-over metadata from the Python port; in the Rust port the session proxy is served on the same listener as the API (the bind port above).</td><td><code>8443</code></td></tr>
      <tr><td><code>VRECKAN_DATABASE_URL</code></td><td>Configuration database. A bare-binary run defaults to a SQLite file; the bundled compose deployment defaults to the bundled PostgreSQL service (see below).</td><td>*(empty)* — SQLite file at <code>/data/vreckan.db</code></td></tr>
      <tr><td><code>VRECKAN_STORAGE_PATH</code></td><td>Base directory for user home directories.</td><td><code>/data/storage</code></td></tr>
      <tr><td><code>VRECKAN_LOG_LEVEL</code></td><td>Logging level (DEBUG, INFO, WARNING, ERROR).</td><td><code>INFO</code></td></tr>
    </tbody>
  </table>
  <p>The full list of settings is in the <a href="https://github.com/vreckan-project/vreckan#configuration">README</a>.</p>

  <h2>3. Run it</h2>
  <p>The repository's <code>docker-compose.yml</code> runs the server with TLS on port 443, mounts a persistent <code>vreckan-data</code> volume, and connects to the Docker daemon. It uses the pre-built image by default:</p>
  <pre><code>docker compose up -d</code></pre>
  <p>On first run the server creates a default <code>admin</code> account (bootstrap password <code>admin1234</code>, configurable via <code>VRECKAN_BOOTSTRAP_ADMIN_*</code>). The account is flagged must-change-password — change the password from the web client (or set up SSO) as soon as possible; while the flag is set, <code>VRECKAN_DEFAULT_PASSWORD_POLICY</code> controls how a login with the literal default <code>admin1234</code> is handled. The startup log (<code>docker compose logs</code>) names the username but only prints the well-known default password, not a configured one.</p>
  <p>Since v0.6.1 the server process runs <strong>non-root</strong> (no <code>privileged</code> container): it reaches the Docker daemon through the host's <code>docker</code> group, and a one-shot step in the compose file takes ownership of <code>vreckan-data</code> (UID/GID 1000) before first boot, so no manual <code>chown</code> is needed on fresh installs or when upgrading from v0.5.x.</p>

  <div class="callout tip">
    <strong>Prefer to build from source?</strong> The bundled <code>docker-compose.yml</code> already carries a <code>build: .</code> fallback (the bundled <code>Dockerfile</code>) — <code>docker compose up --build</code> builds the Rust image from this repo when the image tag is not present, or build manually: <code>docker build -t vreckan .</code>
  </div>

  <h2>4. Open the web client</h2>
  <p>Point a browser at <code>https://your-server/</code>, sign in, and you're in. Launch an app, manage files, and — as an admin — manage users, groups, and applications.</p>

  <h2>ACME certificates (optional)</h2>
  <p>By default Vreckan serves a self-signed certificate. To issue and auto-renew real certificates, enable ACME — either via environment variables or the admin <strong>Certificates</strong> page:</p>
  <pre><code>VRECKAN_ACME_ENABLED=true
VRECKAN_ACME_DOMAINS=example.com,www.example.com
VRECKAN_ACME_AUTH=dns-cloudflare
VRECKAN_ACME_CLOUDFLARE_EMAIL=you@example.com
VRECKAN_ACME_CLOUDFLARE_TOKEN=your-cloudflare-api-token</code></pre>
  <p>Issuance is handled by the native <code>acmex</code> Rust client — no external container or Docker socket is needed for certificate issuance. Three CAs are supported: <strong>Let's Encrypt</strong> (default), <strong>ZeroSSL</strong>, and <strong>Google Public Certificates</strong>. Two challenge types are available: <code>dns-cloudflare</code> (DNS-01, works behind a reverse proxy) and <code>http-01</code> (the ACME server fetches from your domain over HTTP). Eleven DNS-01 providers are supported (Cloudflare, Route 53, DigitalOcean, Linode, Azure, Google Cloud DNS, Alibaba Cloud, GoDaddy, Tencent Cloud, Huawei Cloud, CloudNS); select one with <code>VRECKAN_ACME_DNS_PROVIDER</code>. The issued certificate is stored in the configuration database; a best-effort file mirror is written to <code>/data/acme/</code>. Renewal is automatic: the server checks every <code>VRECKAN_ACME_RENEW_CHECK_HOURS</code> hours and renews within <code>VRECKAN_ACME_RENEW_BEFORE_DAYS</code> days of expiry, with the new certificate hot-reloaded without a restart.</p>

  <h2>PostgreSQL</h2>
  <p>The default <code>docker-compose.yml</code> deployment uses the bundled <code>postgres:16</code> service out of the box (Postgres has been the compose default since v0.6.0). To use SQLite instead, set <code>VRECKAN_DATABASE_URL</code> in your <code>.env</code> to a <code>sqlite:////data/vreckan.db</code> URL (a bare-binary run also defaults to a SQLite file at <code>/data/vreckan.db</code>).</p>
  <p>Since v0.6.1 that service's host port is published on the host <strong>loopback interface only</strong> (<code>127.0.0.1:5432</code>) by default — the server reaches it over the internal compose network either way. Set <code>VRECKAN_POSTGRES_PORT=5432</code> to publish it on every interface again, or comment out the <code>ports</code> entry in <code>docker-compose.yml</code> to disable host publication entirely. Starting against the default <code>vreckan</code>/<code>vreckan</code> credentials logs a warning by default; change them with <code>VRECKAN_DB_USER</code>/<code>VRECKAN_DB_PASS</code>, silence the warning with <code>VRECKAN_ALLOW_DEFAULT_DB_CREDS=1</code>, or make startup fail with <code>VRECKAN_ALLOW_DEFAULT_DB_CREDS=refuse</code>.</p>

  <h2>Security &amp; data (v0.6.1)</h2>
  <p>v0.6.1 addresses the outstanding findings from the v0.6.0 security review. Operator-relevant details:</p>
  <ul>
    <li><strong>Non-root server.</strong> The server process runs as a non-root user (UID/GID 1000) with no <code>privileged</code> container; it reaches the Docker daemon through the host <code>docker</code> group. A one-shot compose step takes ownership of <code>vreckan-data</code> before first boot (no manual <code>chown</code> needed).</li>
    <li><strong>First-boot admin.</strong> The seeded <code>admin</code> account is flagged must-change-password (change it from the web client). <code>VRECKAN_DEFAULT_PASSWORD_POLICY</code> controls how a login with the literal default <code>admin1234</code> is handled while that flag is set.</li>
    <li><strong>E2EE server key.</strong> The server's E2EE RSA key defaults to <code>/data/ssl/server_key.pem</code> (written <code>0600</code>) and now survives container recreation; override the location with <code>VRECKAN_SERVER_PRIVATE_KEY_PATH</code>.</li>
    <li><strong>ACME certificate storage.</strong> The issued ACME certificate (PEM chain + private key + expiry) is persisted in the configuration database; a best-effort file mirror is written to <code>/data/acme/</code>.</li>
    <li><strong>Login rate limiting.</strong> <code>/api/auth/login</code> rate-limits per (client IP, username): <code>VRECKAN_LOGIN_MAX_ATTEMPTS</code> (default 5) failures within <code>VRECKAN_LOGIN_LOCKOUT_SECONDS</code> (default 900 s) lock the pair out with HTTP 429; a successful login clears it.</li>
    <li><strong>Backups.</strong> Config-DB backup archives live in <code>/data/backups</code> (written <code>0600</code>); the in-archive dump excludes live session tokens, and the server keeps the newest <code>VRECKAN_BACKUP_RETENTION</code> (default 7). <code>scripts/backup-postgres.sh</code> provides dated Postgres dumps.</li>
  </ul>
</div>
