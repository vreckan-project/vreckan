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
    <li>An SSL certificate for your domain — <em>optional</em>. A self-signed certificate is generated automatically on first run, and Let's Encrypt can be enabled to issue real certificates (see below).</li>
  </ul>

  <h2>1. Get the image</h2>
  <p>Vreckan ships as a container image. Pull the latest release:</p>
  <pre><code>docker pull ghcr.io/vreckan-project/vreckan:latest</code></pre>
  <p>Versioned tags (for example <code>:v0.4.7</code>) are published alongside <code>latest</code> for every release.</p>

  <h2>2. Configure</h2>
  <p>Vreckan is configured entirely through <code>VRECKAN_*</code> environment variables. The repository ships a <code>.env.example</code> you can copy and edit:</p>
  <pre><code>git clone --depth 1 https://github.com/vreckan-project/vreckan.git
cd vreckan
cp .env.example .env</code></pre>
  <p>The most important settings:</p>
  <table>
    <thead>
      <tr><th>Variable</th><th>Description</th><th>Default</th></tr>
    </thead>
    <tbody>
      <tr><td><code>VRECKAN_API_PORT</code></td><td>Port the server listens on (TLS). The <code>run_https.py</code> entrypoint defaults to <code>443</code> when unset.</td><td><code>443</code></td></tr>
      <tr><td><code>VRECKAN_SESSION_PORT</code></td><td>Internal port for session-proxy traffic; the server discovers its published host mapping from Docker at startup.</td><td><code>8443</code></td></tr>
      <tr><td><code>VRECKAN_DATABASE_URL</code></td><td>Configuration database. SQLite by default; point at PostgreSQL for production.</td><td><code>sqlite+aiosqlite:////data/vreckan.db</code></td></tr>
      <tr><td><code>VRECKAN_STORAGE_PATH</code></td><td>Base directory for user home directories.</td><td><code>/data/storage</code></td></tr>
      <tr><td><code>VRECKAN_LOG_LEVEL</code></td><td>Logging level (DEBUG, INFO, WARNING, ERROR).</td><td><code>INFO</code></td></tr>
    </tbody>
  </table>
  <p>The full list of settings is in the <a href="https://github.com/vreckan-project/vreckan#configuration">README</a>.</p>

  <h2>3. Run it</h2>
  <p>The repository's <code>docker-compose.yml</code> runs the server with TLS on port 443, mounts a persistent <code>vreckan-data</code> volume, and connects to the Docker daemon. It uses the pre-built image by default:</p>
  <pre><code>docker compose up -d</code></pre>
  <p>On first run the server creates a default <code>admin</code> account (bootstrap password <code>admin1234</code>, configurable via <code>VRECKAN_BOOTSTRAP_ADMIN_*</code>) and prints the credentials to the logs (<code>docker compose logs</code>). Change the password after your first login.</p>

  <div class="callout tip">
    <strong>Prefer to build from source?</strong> Swap the <code>image:</code> line in <code>docker-compose.yml</code> for a <code>build: .</code> directive (the bundled <code>Dockerfile</code>), or build manually: <code>docker build -t vreckan .</code>
  </div>

  <h2>4. Open the web client</h2>
  <p>Point a browser at <code>https://your-server/</code>, sign in, and you're in. Launch an app, manage files, and — as an admin — manage users, groups, and applications.</p>

  <h2>Let's Encrypt (optional)</h2>
  <p>By default Vreckan serves a self-signed certificate. To issue and auto-renew real certificates with certbot, enable Let's Encrypt — either via environment variables or the admin <strong>Certificates</strong> page:</p>
  <pre><code>VRECKAN_LE_ENABLED=true
VRECKAN_LE_DOMAINS=example.com,www.example.com
VRECKAN_LE_AUTH=dns-cloudflare
VRECKAN_LE_CLOUDFLARE_EMAIL=you@example.com
VRECKAN_LE_CLOUDFLARE_TOKEN=your-cloudflare-api-token</code></pre>
  <p>Two auth methods are supported: <code>dns-cloudflare</code> (Cloudflare DNS-01 — works behind a reverse proxy) and <code>http-01</code> (the ACME server fetches from your domain over HTTP). Renewal is automatic: the server checks every <code>VRECKAN_LE_RENEW_CHECK_HOURS</code> hours and renews within <code>VRECKAN_LE_RENEW_BEFORE_DAYS</code> days of expiry.</p>

  <h2>PostgreSQL (optional)</h2>
  <p>The default is a SQLite file in the <code>vreckan-data</code> volume, which is perfect for most setups. For a production deployment with multiple users, the bundled <code>docker-compose.yml</code> already includes a <code>postgres</code> service — just set <code>VRECKAN_DATABASE_URL</code> in your <code>.env</code> to <code>postgresql+asyncpg://vreckan:vreckan@postgres:5432/vreckan</code> and restart.</p>
</div>
