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
  <p>Versioned tags (for example <code>:v1.2.3</code>) are published alongside <code>latest</code> for every release.</p>

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
      <tr><td><code>VRECKAN_API_PORT</code></td><td>Port for the main API server.</td><td><code>8000</code></td></tr>
      <tr><td><code>VRECKAN_SESSION_PORT</code></td><td>Port for the session proxy (live app traffic).</td><td><code>8443</code></td></tr>
      <tr><td><code>VRECKAN_DATABASE_URL</code></td><td>Configuration database. SQLite by default; point at PostgreSQL for production.</td><td><code>sqlite+aiosqlite:////data/vreckan.db</code></td></tr>
      <tr><td><code>VRECKAN_STORAGE_PATH</code></td><td>Base directory for user home directories.</td><td><code>/data/storage</code></td></tr>
      <tr><td><code>VRECKAN_LOG_LEVEL</code></td><td>Logging level (DEBUG, INFO, WARNING, ERROR).</td><td><code>INFO</code></td></tr>
    </tbody>
  </table>
  <p>The full list of settings is in the <a href="https://github.com/vreckan-project/vreckan#configuration">README</a>.</p>

  <h2>3. Run it</h2>
  <p>The repository's <code>docker-compose.prod.yml</code> is the production deployment: it runs the API with TLS on port 8443, mounts a persistent <code>vreckan-data</code> volume, and connects to the Docker daemon. Point it at the pre-built image instead of building from source:</p>
  <pre><code># in docker-compose.prod.yml, under services.vreckan-api:
#   image: vreckan-api:local   ← replace with:
  image: ghcr.io/vreckan-project/vreckan:latest</code></pre>
  <pre><code>docker compose -f docker-compose.prod.yml up -d</code></pre>
  <p>On first run the server creates a default <code>admin</code> account and prints its bootstrap password to the logs (<code>docker compose -f docker-compose.prod.yml logs</code>). Change it after your first login.</p>

  <div class="callout tip">
    <strong>Prefer to build from source?</strong> Leave the compose file as-is (it builds the image from the bundled <code>Dockerfile</code> with <code>up -d --build</code>), or build manually: <code>docker build -t vreckan-api .</code>
  </div>

  <h2>4. Open the web client</h2>
  <p>Point a browser at <code>https://your-server:8443</code>, sign in, and you're in. Launch an app, manage files, and — as an admin — manage users, groups, and applications.</p>

  <h2>Let's Encrypt (optional)</h2>
  <p>By default Vreckan serves a self-signed certificate. To issue and auto-renew real certificates with certbot, enable Let's Encrypt — either via environment variables or the admin <strong>Certificates</strong> page:</p>
  <pre><code>VRECKAN_LE_ENABLED=true
VRECKAN_LE_DOMAINS=example.com,www.example.com
VRECKAN_LE_AUTH=dns-cloudflare
VRECKAN_LE_CLOUDFLARE_EMAIL=you@example.com
VRECKAN_LE_CLOUDFLARE_TOKEN=your-cloudflare-api-token</code></pre>
  <p>Two auth methods are supported: <code>dns-cloudflare</code> (Cloudflare DNS-01 — works behind a reverse proxy) and <code>http-01</code> (the ACME server fetches from your domain over HTTP). Renewal is automatic: the server checks every <code>VRECKAN_LE_RENEW_CHECK_HOURS</code> hours and renews within <code>VRECKAN_LE_RENEW_BEFORE_DAYS</code> days of expiry.</p>

  <h2>PostgreSQL (optional)</h2>
  <p>The default is a SQLite file in the <code>vreckan-data</code> volume, which is perfect for most setups. For a production deployment with multiple users, add the bundled <code>docker-compose.postgres.yml</code> service and set <code>VRECKAN_DATABASE_URL</code> to a <code>postgresql+asyncpg://</code> URL.</p>
</div>
