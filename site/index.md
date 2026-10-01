---
layout: default
title: Overview
nav: overview
description: Vreckan streams powerful, containerized desktop applications to any web browser — self-hosted, end-to-end encrypted, and sandboxed.
---

<section class="hero">
  <img src="/assets/img/vreckan-icon-transparent.svg" alt="" class="hero-logo">
  <h1><span class="accent">V</span>RECKAN</h1>
  <p class="tagline">Safe Harbor for Selkies</p>
  <p class="lede">
    A self-hosted, client–server platform that streams powerful, containerized
    desktop applications directly to your web browser. Launch demanding apps,
    manage your files, and administer the platform — all from the browser, with
    nothing to install.
  </p>
  <div class="hero-actions">
    <a class="btn btn-primary" href="{{ '/docs/setup' | relative_url }}">Get Started</a>
    <a class="btn btn-ghost" href="https://github.com/vreckan-project/vreckan" target="_blank" rel="noopener">View on GitHub</a>
  </div>
</section>

<section class="section">
  <h2 class="section-title">What Vreckan does</h2>
  <p class="section-sub">Three things, done well.</p>
  <div class="grid">
    <div class="card">
      <h3><span class="glyph">⚡</span> Remote Application Access</h3>
      <p>Stream demanding desktop apps — video editors, IDEs, 3D modeling tools, office suites — to any device with a browser. Open any file or link on the web in a full-featured application, using your server's CPU, GPU, and RAM.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🛡️</span> Secure Browser &amp; File Isolation</h3>
      <p>Keep your local machine away from the public internet. All web content, downloads, and app processes run inside a sandboxed container on the server, so malware, exploits, and trackers never reach your device.</p>
    </div>
    <div class="card">
      <h3><span class="glyph">🗂️</span> Centralized Data Management</h3>
      <p>An integrated file manager keeps your data in persistent server storage, immediately available to every remote application — linked through multiple configurable home directories.</p>
    </div>
  </div>
</section>

<section class="section">
  <h2 class="section-title">How it works</h2>
  <p class="section-sub">Two components, one encrypted channel.</p>
  <div class="arch">
    <div class="arch-node">
      <div class="role">Client</div>
      <h3>Vreckan Web Client</h3>
      <p>A single-page app served by the server — nothing to install. It launches and manages sessions, browses and shares files, and handles all client-side cryptography (the E2EE handshake and authentication).</p>
    </div>
    <div class="arch-link">
      <span class="arrow">⇄</span>
      <span>E2EE channel</span>
    </div>
    <div class="arch-node">
      <div class="role">Server</div>
      <h3>Vreckan Server</h3>
      <p>The central hub for management, orchestration, and traffic proxying. A dual-port design separates the <strong>control plane</strong> (encrypted API) from the <strong>data plane</strong> (a secure reverse proxy for live app traffic), so app containers are never directly exposed.</p>
    </div>
  </div>
</section>

<section class="section">
  <h2 class="section-title">Key features</h2>
  <div class="feature-list">
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>End-to-end encrypted API</strong>
        <span>All management traffic between client and server is encrypted, protecting launch parameters and user data.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Flexible authentication</strong>
        <span>Sign in with a password or via single sign-on (OIDC), with sessions held by secure, server-issued tokens.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Dual-port architecture</strong>
        <span>A strict separation between the management control plane and the application data plane enhances security and stability.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Containerized backend</strong>
        <span>Docker runs every application in an isolated, sandboxed environment — with optional GPU passthrough.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Role-based access control</strong>
        <span>Clear separation between admins (full system control) and users (their own sessions), with fine-grained permissions.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Let's Encrypt built in</strong>
        <span>Issue and auto-renew real TLS certificates with certbot — Cloudflare DNS-01 or HTTP-01 — or fall back to a self-signed cert.</span>
      </div>
    </div>
    <div class="feature">
      <span class="check">✓</span>
      <div class="feature-body">
        <strong>Full admin UI</strong>
        <span>A complete management dashboard for users, groups, roles, apps, stores, templates, mounts, and backups — all in the browser.</span>
      </div>
    </div>
  </div>
</section>

<section class="section">
  <h2 class="section-title">A typical workflow</h2>
  <div class="card">
    <ol>
      <li><strong>Sign in</strong> — open the web client and authenticate with a password or single sign-on.</li>
      <li><strong>Choose an app</strong> — the client presents the available remote applications.</li>
      <li><strong>Encrypt</strong> — the client establishes an end-to-end encrypted channel and authenticates with a server-issued token.</li>
      <li><strong>Orchestrate</strong> — the server verifies permissions and launches a new, isolated application container.</li>
      <li><strong>Connect</strong> — the server returns a unique, single-use URL; the client opens it and all session traffic flows through the secure proxy.</li>
    </ol>
  </div>
</section>
