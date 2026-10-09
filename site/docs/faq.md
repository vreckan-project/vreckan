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
    <h3>Do I need to install anything on my computer?</h3>
    <p>No. The entire client — launching apps, managing files, and the admin UI — runs in a modern web browser. You only need to run the Vreckan server on your own hardware.</p>
  </div>

  <div class="faq-item">
    <h3>What's the difference between Vreckan and SealSkin?</h3>
    <p>Vreckan is a hard fork of <a href="https://github.com/linuxserver/docker-sealskin">SealSkin</a>. It continues the project under a new name with a leaner codebase and its own roadmap — including a database-backed configuration store, a full admin UI, SSO, and built-in Let's Encrypt.</p>
  </div>

  <div class="faq-item">
    <h3>How does the end-to-end encryption work?</h3>
    <p>All management traffic between the web client and the server's API is protected by an end-to-end encrypted channel established during a cryptographic handshake. The client performs all client-side crypto, so sensitive data like launch parameters and credentials is encrypted before it leaves your browser.</p>
  </div>

  <div class="faq-item">
    <h3>How does the session proxy work?</h3>
    <p>The server serves both the encrypted API and a reverse proxy for live application traffic on the same HTTPS endpoint. When you launch an app, the server returns a unique, single-use URL; after a one-time token exchange, all subsequent traffic (HTTP and WebSockets) is proxied through the server to the app container. The internal containers are never directly exposed to the internet.</p>
  </div>

  <div class="faq-item">
    <h3>Do I need a real SSL certificate?</h3>
    <p>Not strictly — a self-signed certificate is generated automatically on first run, which is fine for a LAN or a trusted setup. For a public domain, enable <strong>Let's Encrypt</strong> (Cloudflare DNS-01 or HTTP-01) to issue and auto-renew real certificates, or drop your own certificate and key at the configured SSL path.</p>
  </div>

  <div class="faq-item">
    <h3>Can I use GPUs?</h3>
    <p>Yes. Vreckan detects GPUs on the host and can pass the matching device through into app containers. Intel GPUs work via <code>/dev/dri</code> passthrough; NVIDIA GPUs are supported through the NVIDIA Container Toolkit.</p>
  </div>

  <div class="faq-item">
    <h3>SQLite or PostgreSQL?</h3>
    <p>SQLite (the default) is ideal for development and single-user setups. For a production deployment with multiple users, run the bundled PostgreSQL service and point <code>VRECKAN_DATABASE_URL</code> at it. Both are fully supported and tested.</p>
  </div>

  <div class="faq-item">
    <h3>How is my data stored?</h3>
    <p>All durable configuration (users, groups, apps, sessions, tokens, shares, mounts, stores, templates) lives in the configuration database. User files live in persistent storage under the configured storage path. Everything is kept under a single data root, making backup and restore straightforward.</p>
  </div>

  <div class="faq-item">
    <h3>What's the default admin account?</h3>
    <p>On first run the server creates an <code>admin</code> account and prints its bootstrap password to the server logs. Sign in with it, then change the password (or set up SSO) as soon as possible.</p>
  </div>

  <div class="faq-item">
    <h3>Can I use single sign-on?</h3>
    <p>Yes. Vreckan speaks standard OIDC, so it works with authentik, Keycloak, Entra ID, and similar providers. Configure the issuer, client ID, and secret on the admin <strong>SSO</strong> page, and map provider groups to Vreckan admins and users.</p>
  </div>

  <div class="faq-item">
    <h3>Where do I report issues or contribute?</h3>
    <p>The project lives on <a href="https://github.com/vreckan-project/vreckan">GitHub</a> — open an issue or send a pull request there.</p>
  </div>
</div>
