---
layout: default
title: FAQ
nav: faq
description: Answers to the most common questions about running and using Vreckan.
---

<div class="doc">
  <h1>Frequently Asked Questions</h1>
  <p class="doc-sub">The questions that come up most often.</p>

  <div class="faq-item">
    <h2>Do I need to install anything on my computer?</h2>
    <p>No. The entire client — launching apps, managing files, and the admin UI — runs in a modern web browser. You only need to run the Vreckan server on your own hardware.</p>
  </div>

  <div class="faq-item">
    <h2>What's the difference between Vreckan and SealSkin?</h2>
    <p>Vreckan is a hard fork of <a href="https://github.com/linuxserver/docker-sealskin">SealSkin</a>. It continues the project under a new name with a leaner codebase and its own roadmap — including a database-backed configuration store, a full admin UI, SSO, and built-in ACME. As of v0.6.0 the server is a Rust rewrite (axum + SeaORM); the original Python server is preserved in the <a href="https://github.com/vreckan-project/vreckan-python">vreckan-python</a> archive repo (currently private; the link resolves once it is made public).</p>
  </div>

  <div class="faq-item">
    <h2>How does the end-to-end encryption work?</h2>
    <p>All management traffic between the web client and the server's API is protected by an end-to-end encrypted channel established during a cryptographic handshake. The client performs all client-side crypto, so sensitive data like launch parameters and credentials is encrypted before it leaves your browser.</p>
  </div>

  <div class="faq-item">
    <h2>How does the session proxy work?</h2>
    <p>The server serves both the encrypted API and a reverse proxy for live application traffic on the same HTTPS endpoint. When you launch an app, the server returns a unique, single-use URL; after a one-time token exchange, all subsequent traffic (HTTP and WebSockets) is proxied through the server to the app container. The internal containers are never directly exposed to the internet.</p>
  </div>

  <div class="faq-item">
    <h2>Do I need a real SSL certificate?</h2>
    <p>Not strictly — a self-signed certificate is generated automatically on first run, which is fine for a LAN or a trusted setup. For a public domain, enable <strong>ACME</strong> (DNS-01 or HTTP-01 challenge, with support for Let's Encrypt, ZeroSSL, and Google CAs and eleven DNS providers) to issue and auto-renew real certificates, or drop your own certificate and key at the configured SSL path. The issued certificate is stored in the configuration database.</p>
  </div>

  <div class="faq-item">
    <h2>Can I use GPUs?</h2>
    <p>Yes. Vreckan detects GPUs on the host and can pass the matching device through into app containers. Intel GPUs work via <code>/dev/dri</code> passthrough; NVIDIA GPUs are supported through the NVIDIA Container Toolkit.</p>
  </div>

  <div class="faq-item">
    <h2>SQLite or PostgreSQL?</h2>
    <p>Both are fully supported and tested. The bundled <code>docker-compose.yml</code> ships a <code>postgres:16</code> service and uses it by default — the right choice for production deployments with multiple users. When running the server binary directly, SQLite is the default and is ideal for development and single-user setups. Switch backends with <code>VRECKAN_DATABASE_URL</code> (or the <code>VRECKAN_DB_*</code> settings).</p>
  </div>

  <div class="faq-item">
    <h2>How is my data stored?</h2>
    <p>All durable configuration (users, groups, apps, sessions, tokens, shares, mounts, stores, templates) lives in the configuration database. User files live in persistent storage under the configured storage path. Everything is kept under a single data root, making backup and restore straightforward.</p>
  </div>

  <div class="faq-item">
    <h2>What's the default admin account?</h2>
    <p>On first run the server creates an <code>admin</code> account (username configurable via <code>VRECKAN_BOOTSTRAP_ADMIN_USERNAME</code>, bootstrap password <code>admin1234</code> unless <code>VRECKAN_BOOTSTRAP_ADMIN_PASSWORD</code> is set). The startup log names the username and prints the password only when the well-known default <code>admin1234</code> is in use. The account is flagged must-change-password, so change the password — or set up SSO — as soon as possible.</p>
  </div>

  <div class="faq-item">
    <h2>Can I use single sign-on?</h2>
    <p>Yes. Vreckan speaks standard OIDC, so it works with authentik, Keycloak, Entra ID, and similar providers. Configure the issuer, client ID, and secret on the admin <strong>SSO</strong> page, and map provider groups to Vreckan admins and users.</p>
  </div>

  <div class="faq-item">
    <h2>Where do I report issues or contribute?</h2>
    <p>The project lives on <a href="https://github.com/vreckan-project/vreckan">GitHub</a> — open an issue or send a pull request there.</p>
  </div>
</div>
